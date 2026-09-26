use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

fn adapter_protocol(provider: &str, model: &str) -> Protocol {
    match provider {
        "openai" => <llm_openai::OpenAi as llm::Adapter>::protocol(model),
        "opencode" => <llm_opencode::OpenCode as llm::Adapter>::protocol(model),
        "opencode-go" => <llm_opencode_go::OpenCodeGo as llm::Adapter>::protocol(model),
        _ => panic!("unsupported test provider: {provider}"),
    }
}

struct Request {
    path: String,
    headers: String,
    body: Value,
}
struct Mock {
    base: String,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Mock {
    fn new(replies: Vec<(u16, Value)>) -> Self {
        Self::raw(
            replies
                .into_iter()
                .map(|(status, body)| (status, body.to_string()))
                .collect(),
        )
    }

    fn raw(replies: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            for (status, body) in replies {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    if stopped.load(Ordering::Relaxed) {
                        return;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "mock timed out waiting for a request"
                    );
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(e) => panic!("mock accept failed: {e}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut headers = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    assert!(!line.is_empty(), "unexpected EOF reading request headers");
                    if let Some((_, value)) = line
                        .split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                    headers.push_str(&line.to_ascii_lowercase());
                }
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).unwrap();
                captured.lock().unwrap().push(Request {
                    path: first.split_whitespace().nth(1).unwrap().into(),
                    headers,
                    body: if bytes.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_slice(&bytes).unwrap()
                    },
                });
                write!(stream, "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nRetry-After: 0\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        Self {
            base,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn config(&self) -> Value {
        json!({"embeddingBaseUrl": self.base, "descriptionBaseUrl": self.base, "rerankerBaseUrl": self.base,
            "embeddingApiKey": "mock-secret", "descriptionApiKey": "mock-secret", "rerankerApiKey": "mock-secret",
            "dimensions": 2, "retryDelayMs": 0, "providerTimeoutMs": 2000})
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            if !thread::panicking() {
                worker.join().unwrap();
            } else {
                let _ = worker.join();
            }
        }
    }
}

fn response(text: &str) -> Value {
    json!({"status": "completed", "output": [{"type": "message", "content": [{"type": "output_text", "text": text}]}]})
}

#[test]
fn model_notices_dedupe_by_kind_provider_and_actual_model() {
    let reported = ReportedCalls::default();
    for (kind, provider, model) in [
        ("vectors", "openai", "primary"),
        ("descriptions", "openai", "primary"),
        ("reranking", "openai", "primary"),
        ("descriptions", "opencode", "primary"),
        ("descriptions", "opencode", "fallback"),
    ] {
        assert!(should_report_call(&reported, false, kind, provider, model));
        assert!(!should_report_call(&reported, false, kind, provider, model));
        for _ in 0..3 {
            assert!(should_report_call(&reported, true, kind, provider, model));
        }
        assert!(!should_report_call(&reported, false, kind, provider, model));
    }
    // A verbose first call still counts as reported for later quiet callers.
    assert!(should_report_call(
        &reported, true, "vectors", "jina", "new"
    ));
    assert!(!should_report_call(
        &reported, false, "vectors", "jina", "new"
    ));
}

#[test]
fn concurrent_model_notices_report_once_unless_verbose() {
    for verbose in [false, true] {
        let reported = ReportedCalls::default();
        let barrier = std::sync::Barrier::new(10);
        let count = thread::scope(|scope| {
            let workers: Vec<_> = (0..10)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        usize::from(should_report_call(
                            &reported,
                            verbose,
                            "descriptions",
                            "openai",
                            "primary",
                        ))
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|w| w.join().unwrap())
                .sum::<usize>()
        });
        assert_eq!(count, if verbose { 10 } else { 1 });
    }
}

#[test]
fn http_notices_run_for_each_outgoing_attempt() {
    let mock = Mock::new(vec![(503, json!({})), (200, json!({"ok": true}))]);
    let http = Http::new(&mock.config()).unwrap();
    let mut notices = 0;
    let result = http
        .request_with_notice(&mock.base, Some(&json!({})), &HeaderMap::new(), || {
            notices += 1;
        })
        .unwrap();
    assert_eq!(result, json!({"ok": true}));
    assert_eq!(notices, 2);
    assert_eq!(mock.requests.lock().unwrap().len(), notices);
}

#[test]
fn constructor_is_offline_and_supports_legacy_and_qualified_config() {
    let mock = Mock::new(vec![]);
    let config = json!({"provider": "jina", "model": "jina-embeddings-v4", "dimensions": 8,
        "descriptionModel": "opencode-go/deepseek-v4", "descriptionFallbackModel": "opencode-go/muse-spark-1.3-contributor",
        "descriptionBaseUrl": mock.base});
    let p = Providers::new(&config).unwrap();
    assert_eq!(p.dimensions(), 8);
    assert_eq!(
        p.embedding_profile(),
        json!({"provider": "jina", "model": "jina-embeddings-v4", "dimensions": 8, "strategyVersion": "rust-v1"})
    );
    assert_eq!(p.description_profile()["model"], "deepseek-v4");
    assert_eq!(p.description_profile()["provider"], "opencode-go");
    assert!(p.embed(&[], false).unwrap().is_empty());
    assert!(p.rerank("", &[]).unwrap().is_empty());
    assert!(mock.requests.lock().unwrap().is_empty());
    assert!(Providers::new(&json!({"descriptionProvider": "opencode", "descriptionFallbackModel": "opencode-go/foo"})).is_err());
    assert!(Providers::new(&json!({"dimensions": 0})).is_err());
    let aliases = Providers::new(
        &json!({"embeddingProvider": "jina", "embeddingModel": "jina-embeddings-v4", "embeddingDimensions": 16}),
    )
    .unwrap();
    assert_eq!(aliases.embedding_profile()["provider"], "jina");
    assert_eq!(aliases.dimensions(), 16);
    let canonical = Providers::new(&json!({
        "provider": "openai", "model": "text-embedding-3-small", "dimensions": 8,
        "embeddingProvider": "jina", "embeddingModel": "jina/jina-embeddings-v4",
        "embeddingDimensions": 16
    }))
    .unwrap();
    assert_eq!(
        canonical.embedding_profile(),
        json!({"provider": "openai",
        "model": "text-embedding-3-small", "dimensions": 8, "strategyVersion": "rust-v1"})
    );
}

#[test]
fn embeddings_batch_reorder_normalize_and_use_jina_tasks() {
    let mock = Mock::new(vec![
        (
            200,
            json!({"data": [{"index": 1, "embedding": [0, 4]}, {"index": 0, "embedding": [3, 4]}]}),
        ),
        (200, json!({"data": [{"index": 0, "embedding": [5, 0]}]})),
        (200, json!({"data": [{"index": 0, "embedding": [0, 9]}]})),
    ]);
    let mut config = mock.config();
    config["provider"] = json!("jina");
    config["embeddingBatchSize"] = json!(2);
    config["embeddingBaseUrl"] = json!(format!("{}/embeddings/", mock.base));
    let p = Providers::new(&config).unwrap();
    assert_eq!(
        p.embed(&["a".into(), "b".into()], false).unwrap(),
        vec![vec![0.6, 0.8], vec![0., 1.]]
    );
    assert_eq!(p.embed(&["c".into()], false).unwrap(), vec![vec![1., 0.]]);
    p.embed(&["query".into()], true).unwrap();
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].path, "/v1/embeddings");
    assert_eq!(requests[0].body["task"], "code.passage");
    assert_eq!(requests[2].body["task"], "code.query");
    assert_eq!(requests[0].body["embedding_type"], "float");
    assert!(requests[0].body.get("encoding_format").is_none());
    assert!(
        requests[0]
            .headers
            .contains("authorization: bearer mock-secret")
    );
}

#[test]
fn embedding_limit_caps_requests_and_never_hides_successful_batches() {
    for (provider, maximum) in [("openai", 32), ("jina", 64)] {
        for configured in [None, Some(2), Some(128)] {
            let limit = configured.unwrap_or(maximum).min(maximum);
            let data: Vec<_> = (0..limit)
                .map(|i| json!({"index": i, "embedding": [1, 0]}))
                .collect();
            let mock = Mock::new(vec![
                (200, json!({"data": data})),
                (503, json!({"error": "unavailable"})),
            ]);
            let mut config = mock.config();
            config["provider"] = json!(provider);
            config["providerMaxRetries"] = json!(0);
            if let Some(size) = configured {
                config["embeddingBatchSize"] = json!(size);
            }
            let p = Providers::new(&config).unwrap();
            assert_eq!(p.embedding_batch_limit(), limit);
            let inputs = vec!["input".to_owned(); limit + 1];
            let error = p.embed(&inputs, false).unwrap_err();
            assert!(error.to_string().contains("exceeds batch limit"));
            assert!(mock.requests.lock().unwrap().is_empty());

            // The caller receives the first paid result before the next can
            // fail, so it has an opportunity to persist it durably.
            let first = p.embed(&inputs[..limit], false).unwrap();
            assert_eq!(first.len(), limit);
            assert_eq!(mock.requests.lock().unwrap().len(), 1);
            assert!(p.embed(&inputs[limit..], false).is_err());
            let requests = mock.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].body["input"].as_array().unwrap().len(), limit);
        }
    }
}

#[test]
fn openai_embedding_input_is_bounded_on_utf8_boundaries() {
    let mock = Mock::new(vec![(
        200,
        json!({"data": [{"index": 0, "embedding": [1, 0]}]}),
    )]);
    let p = Providers::new(&mock.config()).unwrap();
    let input = "🙂x".repeat(10_000);
    p.embed(std::slice::from_ref(&input), false).unwrap();
    let requests = mock.requests.lock().unwrap();
    let sent = requests[0].body["input"][0].as_str().unwrap();
    assert!(sent.len() <= 8191 && input.starts_with(sent));
    assert_eq!(requests[0].body["encoding_format"], "float");
    assert_eq!(requests[0].body["dimensions"], 2);
}

#[test]
fn embedding_response_validation_rejects_corruption() {
    for data in [
        json!([]),
        json!([{"index": 1, "embedding": [1, 0]}]),
        json!([{"index": 0, "embedding": [1]}]),
        json!([{"index": 0, "embedding": ["1", 0]}]),
        json!([{"index": 0, "embedding": [0, 0]}]),
        json!([{"index": 0, "embedding": [1e100, 0]}]),
    ] {
        let mock = Mock::new(vec![(200, json!({"data": data}))]);
        assert!(
            Providers::new(&mock.config())
                .unwrap()
                .embed(&["a".into()], false)
                .is_err()
        );
    }
    let mock = Mock::new(vec![(
        200,
        json!({"data": [{"index": 0, "embedding": [1, 0]}, {"index": 0, "embedding": [0, 1]}]}),
    )]);
    assert!(
        Providers::new(&mock.config())
            .unwrap()
            .embed(&["a".into(), "b".into()], false)
            .is_err()
    );
    assert_eq!(normalize(&json!([1e-200, 0]), 2).unwrap(), vec![1., 0.]);
}

#[test]
fn descriptions_use_all_four_wire_protocols_and_headers() {
    for (provider, model, path, output, auth) in [
        (
            "openai",
            "gpt-5.6-luna",
            "/v1/responses",
            response(" hello "),
            "authorization: bearer mock-secret",
        ),
        (
            "opencode",
            "deepseek-v4",
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "hello"}}]}),
            "authorization: bearer mock-secret",
        ),
        (
            "opencode-go",
            "qwen3.8-max",
            "/v1/messages",
            json!({"content": [{"type": "thinking", "thinking": "private"}, {"type": "text", "text": "hello"}]}),
            "x-api-key: mock-secret",
        ),
        (
            "opencode",
            "gemini-3.8-flash",
            "/v1/models/gemini-3.8-flash:generateContent",
            json!({"candidates": [{"content": {"parts": [{"thought": true, "text": "private"}, {"text": "hello"}]}}]}),
            "x-goog-api-key: mock-secret",
        ),
    ] {
        let mock = Mock::new(vec![(200, output)]);
        let mut config = mock.config();
        config["descriptionProvider"] = json!(provider);
        config["descriptionModel"] = json!(model);
        assert_eq!(
            Providers::new(&config)
                .unwrap()
                .describe("instructions", "question")
                .unwrap(),
            "hello"
        );
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests[0].path, path);
        assert!(requests[0].headers.contains(auth));
        assert!(!requests[0].path.contains("mock-secret"));
        assert_eq!(
            requests[0].headers.contains("x-opencode-session:"),
            provider != "openai"
        );
        let body = &requests[0].body;
        match adapter_protocol(provider, model) {
            Protocol::Responses => {
                assert_eq!(body["store"], false);
                assert_eq!(body["instructions"], "instructions");
                assert_eq!(body["input"][0]["content"][0]["text"], "question");
            }
            Protocol::Chat => {
                assert_eq!(body["messages"][0]["content"], "instructions");
                assert_eq!(body["messages"][1]["content"], "question");
            }
            Protocol::Messages => {
                assert_eq!(body["system"], "instructions");
                assert!(
                    requests[0]
                        .headers
                        .contains("anthropic-version: 2023-06-01")
                );
            }
            Protocol::Gemini => {
                assert_eq!(
                    body["systemInstruction"]["parts"][0]["text"],
                    "instructions"
                );
                assert_eq!(body["contents"][0]["parts"][0]["text"], "question");
            }
        }
    }
}

#[test]
fn catalogue_model_families_select_the_existing_registry_protocols() {
    for model in ["gpt-5", "grok-4", "muse-spark-1", "unknown"] {
        assert_eq!(adapter_protocol("opencode-go", model), Protocol::Responses);
    }
    for model in [
        "big-pickle",
        "deepseek-v4",
        "glm-5",
        "hy3",
        "kimi-k3",
        "ling-2",
        "longcat-1",
        "mimo-v2",
        "nemotron-3",
        "omen-1",
    ] {
        assert_eq!(adapter_protocol("opencode", model), Protocol::Chat);
    }
    assert_eq!(adapter_protocol("opencode", "minimax-m3"), Protocol::Chat);
    assert_eq!(
        adapter_protocol("opencode-go", "minimax-m3"),
        Protocol::Messages
    );
    assert_eq!(
        adapter_protocol("opencode", "claude-sonnet"),
        Protocol::Messages
    );
    assert_eq!(
        adapter_protocol("openai", "deepseek-custom"),
        Protocol::Responses
    );
}

#[test]
fn fallback_is_sticky_and_switches_back_including_wire_protocol() {
    let mock = Mock::new(vec![
        (400, json!({"error": "model unavailable"})),
        (200, response("fallback")),
        (200, response("fallback again")),
        (503, json!({"error": "unavailable"})),
        (
            200,
            json!({"choices": [{"message": {"content": "primary"}}]}),
        ),
    ]);
    let mut config = mock.config();
    config["descriptionProvider"] = json!("opencode-go");
    config["descriptionModel"] = json!("deepseek-v4");
    config["descriptionFallbackModel"] = json!("muse-spark-1.3-contributor");
    let p = Providers::new(&config).unwrap();
    assert_eq!(p.describe("system", "one").unwrap(), "fallback");
    assert_eq!(p.describe("system", "two").unwrap(), "fallback again");
    assert_eq!(p.describe("system", "three").unwrap(), "primary");
    assert_eq!(p.description_profile()["model"], "deepseek-v4");
    let requests = mock.requests.lock().unwrap();
    let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/v1/chat/completions",
            "/v1/responses",
            "/v1/responses",
            "/v1/responses",
            "/v1/chat/completions"
        ]
    );
}

#[test]
fn description_empty_retry_and_failover_are_bounded() {
    let mock = Mock::new(vec![(200, response("")), (200, response("recovered"))]);
    assert_eq!(
        Providers::new(&mock.config())
            .unwrap()
            .describe("s", "p")
            .unwrap(),
        "recovered"
    );
    assert_eq!(mock.requests.lock().unwrap().len(), 2);
    for fallback in [false, true] {
        let mock = Mock::new(vec![(200, response("")); 6]);
        let mut config = mock.config();
        if fallback {
            config["descriptionFallbackModel"] = json!("backup");
        }
        assert!(Providers::new(&config).unwrap().describe("s", "p").is_err());
        assert_eq!(mock.requests.lock().unwrap().len(), 6);
    }
}

#[test]
fn description_failover_does_not_retry_permanently_failed_models() {
    let mock = Mock::new(vec![
        (404, json!({"error": "unknown primary"})),
        (503, json!({"error": "fallback temporarily unavailable"})),
        (200, response("fallback recovered")),
        (200, response("still fallback")),
    ]);
    let mut config = mock.config();
    config["descriptionModel"] = json!("primary");
    config["descriptionFallbackModel"] = json!("backup");
    let p = Providers::new(&config).unwrap();
    assert_eq!(p.describe("s", "p").unwrap(), "fallback recovered");
    assert_eq!(p.describe("s", "p").unwrap(), "still fallback");
    let requests = mock.requests.lock().unwrap();
    let models: Vec<_> = requests
        .iter()
        .map(|r| r.body["model"].as_str().unwrap())
        .collect();
    assert_eq!(models, ["primary", "backup", "backup", "backup"]);
    drop(requests);

    let mock = Mock::new(vec![(400, json!({})); DESCRIPTION_ATTEMPTS]);
    let mut config = mock.config();
    config["descriptionFallbackModel"] = json!("backup");
    assert!(Providers::new(&config).unwrap().describe("s", "p").is_err());
    assert_eq!(mock.requests.lock().unwrap().len(), 2);
}

#[test]
fn description_failover_stops_on_shared_auth_errors_and_redirects() {
    for status in [401, 302, 307] {
        let mock = Mock::new(vec![
            (status, json!({"error": "mock-secret"}));
            DESCRIPTION_ATTEMPTS
        ]);
        let mut config = mock.config();
        config["descriptionFallbackModel"] = json!("backup");
        let error = Providers::new(&config)
            .unwrap()
            .describe("s", "p")
            .unwrap_err();
        assert!(error.to_string().contains(&status.to_string()));
        assert!(!format!("{error:#?}").contains("mock-secret"));
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
    }
}

#[test]
fn description_transient_retries_respect_limits_without_nested_retries() {
    for (fallback, retries, expected) in [(false, 0, 1), (false, 2, 3), (true, 5, 6)] {
        let mock = Mock::new(vec![(503, json!({})); DESCRIPTION_ATTEMPTS + 1]);
        let mut config = mock.config();
        config["providerMaxRetries"] = json!(retries);
        if fallback {
            config["descriptionFallbackModel"] = json!("backup");
        }
        assert!(Providers::new(&config).unwrap().describe("s", "p").is_err());
        assert_eq!(mock.requests.lock().unwrap().len(), expected);
    }
}

#[test]
fn http_retries_transient_statuses_but_not_auth_or_bad_json() {
    let vector = json!({"data": [{"index": 0, "embedding": [1, 0]}]});
    let mock = Mock::new(vec![(429, json!({})), (503, json!({})), (200, vector)]);
    Providers::new(&mock.config())
        .unwrap()
        .embed(&["a".into()], false)
        .unwrap();
    assert_eq!(mock.requests.lock().unwrap().len(), 3);
    for status in [400, 401, 403, 404, 302] {
        let mock = Mock::new(vec![
            (status, json!({"error": "mock-secret"})),
            (200, json!({})),
        ]);
        let error = Providers::new(&mock.config())
            .unwrap()
            .embed(&["a".into()], false)
            .unwrap_err();
        assert!(error.to_string().contains(&status.to_string()));
        assert!(!format!("{error:#?}").contains("mock-secret"));
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
    }
    let mock = Mock::raw(vec![(200, "not JSON: mock-secret".into())]);
    let error = Providers::new(&mock.config())
        .unwrap()
        .embed(&["a".into()], false)
        .unwrap_err();
    assert!(!format!("{error:#?}").contains("mock-secret"));
    let mock = Mock::new(vec![(503, json!({})); 3]);
    assert!(
        Providers::new(&mock.config())
            .unwrap()
            .embed(&["a".into()], false)
            .is_err()
    );
    assert_eq!(mock.requests.lock().unwrap().len(), 3);
}

#[test]
fn stalled_http_request_respects_timeout_without_leaking_url_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!(
        "http://{}/v1?token=url-secret",
        listener.local_addr().unwrap()
    );
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((_stream, _)) => {
                    thread::sleep(Duration::from_millis(500));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("mock accept failed: {error}"),
            }
        }
    });
    let p = Providers::new(
        &json!({"embeddingBaseUrl": base, "embeddingApiKey": "header-secret",
        "providerTimeoutMs": 100, "providerMaxRetries": 0}),
    )
    .unwrap();
    let start = Instant::now();
    let error = p.embed(&["input".into()], false).unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(3));
    let detail = format!("{error:#?}");
    assert!(detail.contains("timed out"));
    assert!(!detail.contains("url-secret") && !detail.contains("header-secret"));
    worker.join().unwrap();
}

#[test]
fn all_rerankers_validate_and_return_descending_scores() {
    for provider in ["cohere", "jina", "openai"] {
        let output = if provider == "openai" {
            response(
                &json!({"ranking": [{"index": 0, "score": 0.1}, {"index": 1, "score": 0.9}]})
                    .to_string(),
            )
        } else {
            json!({"results": [{"index": 0, "relevance_score": 0.1}, {"index": 1, "relevance_score": 0.9}]})
        };
        let mock = Mock::new(vec![(200, output)]);
        let mut config = mock.config();
        config["rerankerProvider"] = json!(provider);
        let p = Providers::new(&config).unwrap();
        assert_eq!(
            p.rerank("query", &["first".into(), "second".into()])
                .unwrap(),
            vec![(1, 0.9), (0, 0.1)]
        );
        let requests = mock.requests.lock().unwrap();
        if provider == "openai" {
            assert_eq!(requests[0].path, "/v1/responses");
            assert_eq!(requests[0].body["reasoning"]["effort"], "high");
            assert_eq!(requests[0].body["text"]["format"]["type"], "json_schema");
        } else {
            assert_eq!(requests[0].path, "/v1/rerank");
            assert_eq!(requests[0].body["top_n"], 2);
            assert_eq!(requests[0].body["query"], "query");
            if provider == "jina" {
                assert_eq!(requests[0].body["return_documents"], false);
            }
        }
    }
    for ranking in [
        json!([]),
        json!([{"index": 1, "score": 0.5}]),
        json!([{"index": 0, "score": 1.1}]),
        json!([{"index": 0, "score": "0.5"}]),
    ] {
        assert!(parse_ranking(&ranking, 1, true).is_err());
    }
    assert!(
        parse_ranking(
            &json!([{"index": 0, "score": 0.5}, {"index": 0, "score": 0.6}]),
            2,
            true
        )
        .is_err()
    );
}

#[test]
fn catalogue_is_public_deduplicated_and_has_documented_shape() {
    let mock = Mock::new(vec![(
        200,
        json!({"data": [{"id": "deepseek-v4"}, {"id": "qwen3.8-max"}, {"id": "deepseek-v4"}]}),
    )]);
    let http = Http::new(&mock.config()).unwrap();
    assert_eq!(
        fetch_models(&http, "opencode-go", &mock.base).unwrap(),
        vec![
            json!({"provider": "opencode-go", "model": "deepseek-v4"}),
            json!({"provider": "opencode-go", "model": "qwen3.8-max"}),
        ]
    );
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests[0].path, "/v1/models");
    assert!(!requests[0].headers.contains("authorization"));
    assert!(models(Some("invalid")).is_err());
    for body in [
        json!({}),
        json!({"data": [{"id": ""}]}),
        json!({"data": [{"id": 4}]}),
    ] {
        let mock = Mock::new(vec![(200, body)]);
        assert!(fetch_models(&http, "opencode", &mock.base).is_err());
    }
}

#[test]
fn stored_auth_is_scoped_to_the_selected_provider() {
    let path = std::env::temp_dir().join(format!("slopdex-provider-auth-{}", session_id()));
    std::fs::write(&path, json!({"opencode": {"type": "api", "key": "zen-key"}, "opencode-go": {"type": "api", "key": "go-key"}}).to_string()).unwrap();
    assert_eq!(stored_key(&path, "opencode").as_deref(), Some("zen-key"));
    assert_eq!(stored_key(&path, "opencode-go").as_deref(), Some("go-key"));
    assert_eq!(stored_key(&path, "openai"), None);
    std::fs::write(&path, "invalid secret-bearing JSON").unwrap();
    assert_eq!(stored_key(&path, "opencode"), None);
    std::fs::remove_file(path).unwrap();
}
