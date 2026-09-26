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
        Self::wire(
            replies
                .into_iter()
                .map(|(status, body)| {
                    format!("HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nRetry-After: 0\r\n\r\n{body}", body.len())
                })
                .collect(),
        )
    }

    // Raw wire replies allow deterministic short reads and custom response headers.
    fn wire(replies: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            for reply in replies {
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
                stream.write_all(reply.as_bytes()).unwrap();
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

#[test]
fn configuration_rejects_invalid_values_and_unsafe_urls_offline() {
    for config in [Value::Null, json!([]), json!("config")] {
        assert!(Providers::new(&config).is_err());
    }
    let mock = Mock::new(vec![]);
    for (key, value) in [
        ("providerMaxRetries", json!(6)),
        ("providerMaxRetries", json!(-1)),
        ("providerMaxRetries", json!(1.5)),
        ("retryDelayMs", json!(-1)),
        ("retryDelayMs", json!("0")),
        ("providerTimeoutMs", json!(0)),
        ("embeddingBatchSize", json!(0)),
        ("dimensions", json!("2")),
        ("provider", json!("unknown")),
        ("model", json!("jina/foreign-model")),
        ("model", json!("openai/")),
        ("descriptionProvider", json!("unknown")),
        ("descriptionModel", json!(false)),
        ("descriptionModel", json!("  ")),
    ] {
        let mut config = mock.config();
        config[key] = value;
        assert!(Providers::new(&config).is_err(), "accepted {key}: {config}");
    }
    for base in [
        "not a URL",
        "file:///tmp/provider",
        "https://user:url-secret@example.invalid/v1",
        "https://example.invalid/v1#url-secret",
    ] {
        for key in ["embeddingBaseUrl", "descriptionBaseUrl"] {
            let mut config = mock.config();
            config[key] = json!(base);
            let error = Providers::new(&config).err().expect("invalid URL accepted");
            assert!(!format!("{error:#?}").contains("url-secret"));
        }
    }
    // Null canonical settings defer to aliases, which are trimmed and unqualified.
    let mut config = mock.config();
    config["provider"] = Value::Null;
    config["embeddingProvider"] = json!(" jina ");
    config["model"] = Value::Null;
    config["embeddingModel"] = json!(" jina/jina-embeddings-v4 ");
    config["dimensions"] = Value::Null;
    config["embeddingDimensions"] = json!(8);
    let p = Providers::new(&config).unwrap();
    assert_eq!(p.embedding_profile()["provider"], "jina");
    assert_eq!(p.embedding_profile()["model"], "jina-embeddings-v4");
    assert_eq!(p.dimensions(), 8);
    assert!(mock.requests.lock().unwrap().is_empty());
}

#[test]
fn configured_credentials_are_trimmed_and_operation_scoped() {
    for operation_key in [Some(" operation-key "), None] {
        let mock = Mock::new(vec![
            (200, json!({"data": [{"index": 0, "embedding": [1, 0]}]})),
            (200, response("description")),
            (200, response(r#"{"ranking":[{"index":0,"score":1}]}"#)),
        ]);
        let mut config = mock.config();
        config["openaiApiKey"] = json!(" provider-key ");
        config["embeddingApiKey"] = json!(operation_key);
        config["descriptionApiKey"] = json!(" description-key ");
        config["rerankerApiKey"] = Value::Null;
        config["rerankerProvider"] = json!("openai");
        let p = Providers::new(&config).unwrap();
        p.embed(&["input".into()], false).unwrap();
        p.describe("system", "prompt").unwrap();
        p.rerank("query", &["document".into()]).unwrap();
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let embedding_key = if operation_key.is_some() {
            "operation-key"
        } else {
            "provider-key"
        };
        for (request, key) in
            requests
                .iter()
                .zip([embedding_key, "description-key", "provider-key"])
        {
            assert!(
                request
                    .headers
                    .contains(&format!("authorization: bearer {key}\r\n"))
            );
        }
    }
}

#[test]
fn invalid_credentials_fail_before_sending_in_every_protocol() {
    for model in ["gpt-5", "deepseek-v4", "claude-sonnet", "gemini-3.8-flash"] {
        for (key, expected) in [
            (
                json!("secret\r\nx-injected: true"),
                "valid HTTP header value",
            ),
            (json!("  "), "descriptionApiKey must be a non-empty string"),
            (json!(42), "descriptionApiKey must be a non-empty string"),
        ] {
            let mock = Mock::new(vec![]);
            let mut config = mock.config();
            config["descriptionProvider"] = json!("opencode");
            config["descriptionModel"] = json!(model);
            config["descriptionApiKey"] = key;
            config["opencodeApiKey"] = json!("valid-but-must-not-mask-invalid-override");
            let p = Providers::new(&config).unwrap();
            let error = p.describe("system", "prompt").unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert!(!format!("{error:#?}").contains("secret"));
            assert!(mock.requests.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn description_urls_preserve_query_values_and_encode_model_path_segments() {
    for (model, suffix, path, output) in [
        ("gpt-5", "/responses/", "/v1/responses", response("ok")),
        (
            "deepseek-v4",
            "/chat/completions/",
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        ),
        (
            "claude-sonnet",
            "/messages/",
            "/v1/messages",
            json!({"content": [{"type": "text", "text": "ok"}]}),
        ),
        (
            "gemini-custom/variant?x#y",
            "/",
            "/v1/models/gemini-custom%2Fvariant%3Fx%23y:generateContent",
            json!({"candidates": [{"content": {"parts": [{"text": "ok"}]}}]}),
        ),
    ] {
        let mock = Mock::new(vec![(200, output)]);
        let mut config = mock.config();
        config["descriptionProvider"] = json!("opencode");
        config["descriptionModel"] = json!(model);
        config["descriptionBaseUrl"] = json!(format!("{}{suffix}?route=tenant/", mock.base));
        assert_eq!(
            Providers::new(&config).unwrap().describe("s", "p").unwrap(),
            "ok"
        );
        assert_eq!(
            mock.requests.lock().unwrap()[0].path,
            format!("{path}?route=tenant/")
        );
    }
}

#[test]
fn description_protocols_join_only_public_text_in_order() {
    for (model, output) in [
        (
            "gpt-5",
            json!({"output_text": " first second ", "output": [{"type": "message", "content": [{"type": "output_text", "text": "duplicate"}]}]}),
        ),
        (
            "gpt-5",
            json!({"output": [
                {"type": "reasoning", "content": [{"type": "output_text", "text": "private"}]},
                {"type": "message", "content": [{"type": "refusal", "text": "ignored"}, {"type": "output_text", "text": " first "}]},
                {"type": "message", "content": [{"type": "output_text", "text": 42}, {"type": "output_text", "text": "second "}]}
            ]}),
        ),
        (
            "deepseek-v4",
            json!({"choices": [{"message": {"content": [{"type": "text", "text": " first "}, {"type": "image_url", "text": "ignored"}, {"type": "text", "text": "second "}]}}, {"message": {"content": "other candidate"}}]}),
        ),
        (
            "claude-sonnet",
            json!({"content": [{"type": "thinking", "text": "private"}, {"type": "text", "text": " first "}, {"type": "tool_use", "text": "ignored"}, {"type": "text", "text": "second "}]}),
        ),
        (
            "gemini-3.8-flash",
            json!({"candidates": [{"content": {"parts": [{"thought": true, "text": "private"}, {"text": " first "}, {"inlineData": {}}, {"text": "second "}]}}, {"content": {"parts": [{"text": "other candidate"}]}}]}),
        ),
    ] {
        let mock = Mock::new(vec![(200, output)]);
        let mut config = mock.config();
        config["descriptionProvider"] = json!("opencode");
        config["descriptionModel"] = json!(model);
        assert_eq!(
            Providers::new(&config).unwrap().describe("s", "p").unwrap(),
            "first second",
            "{model}"
        );
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
    }
}

#[test]
fn incomplete_descriptions_retry_even_when_transport_retries_are_disabled() {
    for (model, incomplete, complete) in [
        (
            "gpt-5",
            json!({"status": "incomplete", "output_text": "partial"}),
            response("complete"),
        ),
        (
            "gpt-5",
            json!({"status": "failed", "output_text": "partial"}),
            response("complete"),
        ),
        (
            "gpt-5",
            json!({"status": "cancelled", "output_text": "partial"}),
            response("complete"),
        ),
        (
            "deepseek-v4",
            json!({"choices": [{"finish_reason": "length", "message": {"content": "partial"}}]}),
            json!({"choices": [{"finish_reason": "stop", "message": {"content": "complete"}}]}),
        ),
        (
            "claude-sonnet",
            json!({"stop_reason": "max_tokens", "content": [{"type": "text", "text": "partial"}]}),
            json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": "complete"}]}),
        ),
        (
            "gemini-3.8-flash",
            json!({"candidates": [{"finishReason": "MAX_TOKENS", "content": {"parts": [{"text": "partial"}]}}]}),
            json!({"candidates": [{"finishReason": "STOP", "content": {"parts": [{"text": "complete"}]}}]}),
        ),
    ] {
        let mock = Mock::new(vec![(200, incomplete), (200, complete)]);
        let mut config = mock.config();
        config["descriptionProvider"] = json!("opencode");
        config["descriptionModel"] = json!(model);
        config["providerMaxRetries"] = json!(0);
        assert_eq!(
            Providers::new(&config).unwrap().describe("s", "p").unwrap(),
            "complete",
            "{model}"
        );
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].body, requests[1].body);
    }
}

#[test]
fn malformed_and_error_descriptions_fail_over_without_leaking_remote_content() {
    for bad_body in [
        "not JSON: remote-secret".to_owned(),
        json!({"error": {"message": "remote-secret"}, "output_text": "must not succeed"})
            .to_string(),
    ] {
        for fallback in [false, true] {
            let mock = Mock::raw(vec![
                (200, bad_body.clone()),
                (200, response("recovered").to_string()),
                (200, response("still recovered").to_string()),
            ]);
            let mut config = mock.config();
            config["descriptionModel"] = json!("primary");
            if fallback {
                config["descriptionFallbackModel"] = json!("backup");
            }
            let p = Providers::new(&config).unwrap();
            if fallback {
                assert_eq!(p.describe("s", "p").unwrap(), "recovered");
                assert_eq!(p.describe("s", "p").unwrap(), "still recovered");
                let requests = mock.requests.lock().unwrap();
                let models: Vec<_> = requests
                    .iter()
                    .map(|r| r.body["model"].as_str().unwrap())
                    .collect();
                assert_eq!(models, ["primary", "backup", "backup"]);
            } else {
                let error = p.describe("s", "p").unwrap_err();
                assert!(!format!("{error:#?}").contains("remote-secret"));
                assert_eq!(mock.requests.lock().unwrap().len(), 1);
                assert_eq!(p.describe("s", "p").unwrap(), "recovered");
            }
        }
    }
}

#[test]
fn model_scoped_forbidden_fails_over_with_new_auth_but_the_same_session() {
    let output = json!({"content": [{"type": "text", "text": "ok"}]});
    let mock = Mock::new(vec![(403, json!({})), (200, output.clone()), (200, output)]);
    let mut config = mock.config();
    config["descriptionProvider"] = json!("opencode");
    config["descriptionModel"] = json!("gpt-5");
    config["descriptionFallbackModel"] = json!("claude-sonnet");
    let p = Providers::new(&config).unwrap();
    assert_eq!(p.describe("s", "p").unwrap(), "ok");
    assert_eq!(p.describe("s", "p").unwrap(), "ok");
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[0]
            .headers
            .contains("authorization: bearer mock-secret")
    );
    for request in &requests[1..] {
        assert_eq!(request.path, "/v1/messages");
        assert!(request.headers.contains("x-api-key: mock-secret"));
        assert!(!request.headers.contains("authorization:"));
    }
    let sessions: Vec<_> = requests
        .iter()
        .map(|r| {
            r.headers
                .lines()
                .find_map(|line| line.strip_prefix("x-opencode-session: "))
                .unwrap()
        })
        .collect();
    assert!(!sessions[0].is_empty());
    assert_eq!(sessions[0], sessions[1]);
    assert_ne!(sessions[1], sessions[2]);
}

#[test]
fn http_transient_statuses_replay_the_same_request_once() {
    for status in [408, 409, 425, 500, 502, 504, 529] {
        let mock = Mock::new(vec![(status, json!({})), (200, json!({"ok": true}))]);
        let mut config = mock.config();
        config["providerMaxRetries"] = json!(1);
        let http = Http::new(&config).unwrap();
        let headers = auth_headers("test-key", Protocol::Responses).unwrap();
        let body = json!({"input": ["λ", "two"], "model": "custom"});
        assert_eq!(
            http.request(&mock.base, Some(&body), &headers).unwrap(),
            json!({"ok": true})
        );
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "HTTP {status}");
        for request in requests.iter() {
            assert_eq!(request.path, "/v1");
            assert_eq!(request.body, body);
            assert!(request.headers.contains("authorization: bearer test-key"));
            assert!(request.headers.contains("accept: application/json"));
        }
    }
}

#[test]
fn http_short_response_body_recovers_but_complete_invalid_json_does_not_retry() {
    for declared_length in [100, 13] {
        let mock = Mock::wire(vec![
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\nremote-secret"
            ),
            "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}"
                .into(),
        ]);
        let http = Http::new(&mock.config()).unwrap();
        let result = http.request(&mock.base, None, &HeaderMap::new());
        if declared_length == 100 {
            assert_eq!(result.unwrap(), json!({"ok": true}));
        } else {
            let error = result.unwrap_err();
            assert!(error.to_string().contains("malformed JSON"));
            assert!(!format!("{error:#?}").contains("remote-secret"));
            assert_eq!(mock.requests.lock().unwrap().len(), 1);
            assert_eq!(
                http.request(&mock.base, None, &HeaderMap::new()).unwrap(),
                json!({"ok": true})
            );
        }
        assert_eq!(mock.requests.lock().unwrap().len(), 2);
    }
}

#[test]
fn http_retry_after_is_bounded_and_invalid_values_are_ignored() {
    // Inspect a single real HTTP exchange rather than sleeping or asserting elapsed time.
    for (header, expected) in [
        ("0", Some(0)),
        ("2", Some(2)),
        ("999999", Some(5)),
        ("18446744073709551616", None),
        ("-1", None),
        ("Wed, 21 Oct 2015 07:28:00 GMT", None),
    ] {
        let mock = Mock::wire(vec![format!(
            "HTTP/1.1 429 Limited\r\nContent-Length: 0\r\nRetry-After: {header}\r\nConnection: close\r\n\r\n"
        )]);
        let failure = Http::new(&mock.config())
            .unwrap()
            .once(&mock.base, None, &HeaderMap::new())
            .unwrap_err();
        assert!(failure.retryable);
        assert_eq!(
            failure.retry_after,
            expected.map(Duration::from_secs),
            "{header}"
        );
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
    }
}

#[test]
fn rerankers_reject_corrupt_rankings_and_recover_on_the_next_call() {
    for provider in ["openai", "jina", "cohere"] {
        let score = if provider == "openai" {
            "score"
        } else {
            "relevance_score"
        };
        let valid = json!([{"index": 1, (score): 0.5}, {"index": 0, (score): 0.5}]);
        let wrap = |ranking: Value| {
            if provider == "openai" {
                response(&json!({"ranking": ranking}).to_string())
            } else {
                json!({"results": ranking})
            }
        };
        let mut invalid = vec![
            wrap(Value::Null),
            wrap(json!([{"index": 0, (score): 0.5}])),
            wrap(json!([{"index": 0, (score): 0.5}, {"index": 0, (score): 0.5}])),
            wrap(json!([{"index": -1, (score): 0.5}, {"index": 1, (score): 0.5}])),
            wrap(json!([{"index": 0, (score): "remote-secret"}, {"index": 1, (score): 0.5}])),
            wrap(json!([{"index": 0, (score): 0.5}, {"index": 2, (score): 0.5}])),
        ];
        if provider == "openai" {
            invalid.extend([
                response("not JSON: remote-secret"),
                json!({"status": "incomplete", "output_text": json!({"ranking": valid}).to_string()}),
                wrap(json!([{"index": 0, "score": -0.1}, {"index": 1, "score": 1}])),
            ]);
        }
        for output in invalid {
            let mock = Mock::new(vec![(200, output), (200, wrap(valid.clone()))]);
            let mut config = mock.config();
            config["rerankerProvider"] = json!(provider);
            let p = Providers::new(&config).unwrap();
            let documents = ["first".into(), "second".into()];
            let error = p.rerank("query", &documents).unwrap_err();
            assert!(!format!("{error:#?}").contains("remote-secret"));
            assert_eq!(mock.requests.lock().unwrap().len(), 1);
            assert_eq!(
                p.rerank("query", &documents).unwrap(),
                vec![(0, 0.5), (1, 0.5)]
            );
            assert_eq!(mock.requests.lock().unwrap().len(), 2);
        }
    }
}

#[test]
fn reranking_validates_lazy_configuration_and_candidate_limits_before_http() {
    let mock = Mock::new(vec![]);
    for (settings, expected) in [
        (json!({}), "rerankerProvider is required"),
        (
            json!({"rerankerProvider": "unsupported"}),
            "Unsupported reranker provider",
        ),
        (
            json!({"rerankerProvider": "cohere", "rerankerModel": "jina/foreign"}),
            "provider must match",
        ),
        (
            json!({"rerankerProvider": "jina", "rerankerBaseUrl": "file:///tmp/provider"}),
            "base URL must be HTTP(S)",
        ),
        (
            json!({"rerankerProvider": "openai", "rerankerCandidates": 101}),
            "rerankerCandidates must not exceed 100",
        ),
        (
            json!({"rerankerProvider": "openai", "rerankerCandidates": 0}),
            "rerankerCandidates must be a positive integer",
        ),
        (
            json!({"rerankerProvider": "openai", "rerankerCandidates": "10"}),
            "rerankerCandidates must be a positive integer",
        ),
    ] {
        let mut config = mock.config();
        config
            .as_object_mut()
            .unwrap()
            .extend(settings.as_object().unwrap().clone());
        let p = Providers::new(&config).unwrap();
        assert!(p.rerank("query", &[]).unwrap().is_empty());
        for _ in 0..2 {
            let error = p.rerank("query", &["document".into()]).unwrap_err();
            assert!(error.to_string().contains(expected), "{settings}: {error}");
        }
    }
    let mut config = mock.config();
    config["rerankerProvider"] = json!("openai");
    let p = Providers::new(&config).unwrap();
    assert_eq!(p.reranker().unwrap().candidate_limit(), Some(100));
    assert!(
        p.rerank("query", &vec!["document".into(); 101])
            .unwrap_err()
            .to_string()
            .contains("at most 100")
    );
    assert!(mock.requests.lock().unwrap().is_empty());
}

#[test]
fn openai_reranking_bounds_utf8_documents_and_requests_every_candidate() {
    for count in [1, 100] {
        let ranking: Vec<_> = (0..count)
            .map(|index| json!({"index": index, "score": 1}))
            .collect();
        let mock = Mock::new(vec![(
            200,
            response(&json!({"ranking": ranking}).to_string()),
        )]);
        let mut config = mock.config();
        config["rerankerProvider"] = json!("openai");
        let p = Providers::new(&config).unwrap();
        let document = format!("x{}", "🙂".repeat(4000));
        assert_eq!(
            p.rerank("query λ", &vec![document.clone(); count])
                .unwrap()
                .len(),
            count
        );
        let requests = mock.requests.lock().unwrap();
        let body = &requests[0].body;
        let prompt: Value =
            serde_json::from_str(body["input"][0]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(prompt["query"], "query λ");
        assert_eq!(prompt["resultCount"], count);
        let candidates = prompt["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), count);
        let mut total_bytes = 0;
        for (index, candidate) in candidates.iter().enumerate() {
            assert_eq!(candidate["index"], index);
            let sent = candidate["document"].as_str().unwrap();
            assert!(!sent.is_empty() && document.starts_with(sent));
            assert!(sent.len() <= 12_000);
            total_bytes += sent.len();
        }
        assert!(total_bytes <= 80_000);
        let schema = &body["text"]["format"]["schema"]["properties"]["ranking"];
        assert_eq!(schema["minItems"], count);
        assert_eq!(schema["maxItems"], count);
        assert_eq!(schema["items"]["properties"]["index"]["maximum"], count - 1);
        assert!(body.get("max_output_tokens").is_none());
    }
}

#[test]
fn malformed_embeddings_do_not_poison_later_batches_or_retry_paid_responses() {
    for provider in ["openai", "jina"] {
        for output in [
            json!({}),
            json!({"data": {"index": 0, "embedding": [1, 0]}}),
            json!({"data": [{"embedding": [1, 0]}]}),
            json!({"data": [{"index": -1, "embedding": [1, 0]}]}),
            json!({"data": [{"index": "0", "embedding": [1, 0]}]}),
            json!({"data": [{"index": 0, "embedding": null}]}),
            json!({"data": [{"index": 0, "embedding": [true, 0]}]}),
        ] {
            let mock = Mock::new(vec![
                (200, output),
                (
                    200,
                    json!({"data": [{"index": 1, "embedding": [-1e-200, 0]}, {"index": 0, "embedding": [3e38, 3e38]}]}),
                ),
            ]);
            let mut config = mock.config();
            config["provider"] = json!(provider);
            let p = Providers::new(&config).unwrap();
            assert!(p.embed(&["bad".into()], false).is_err());
            assert_eq!(mock.requests.lock().unwrap().len(), 1);
            let vectors = p.embed(&["large".into(), "tiny".into()], true).unwrap();
            assert_eq!(vectors[1], vec![-1., 0.]);
            for component in &vectors[0] {
                assert!((*component - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
            }
            assert_eq!(mock.requests.lock().unwrap().len(), 2);
        }
    }
}
