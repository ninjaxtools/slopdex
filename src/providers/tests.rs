use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use crate::test_agent as agent;
#[path = "ollama_tests.rs"]
mod ollama_tests;

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
        Self::wire(replies.into_iter().map(|(status, body)|
            format!("HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nRetry-After: 0\r\n\r\n{body}", body.len())).collect())
    }
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
                let deadline = Instant::now() + Duration::from_secs(30);
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
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(30)))
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
        json!({"embeddingBaseUrl": self.base, "rerankerBaseUrl": self.base,
            "embeddingApiKey": "mock-secret", "rerankerApiKey": "mock-secret",
            "dimensions": 2, "retryDelayMs": 0, "providerTimeoutMs": 2000})
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}

#[test]
fn model_notices_dedupe_by_kind_provider_and_actual_model() {
    let reported = ReportedCalls::default();
    for (kind, provider, model) in [
        ("vectors", "openai", "primary"),
        ("description", "command", "primary"),
        ("reranker", "command", "primary"),
    ] {
        assert!(should_report_call(&reported, false, kind, provider, model));
        assert!(!should_report_call(&reported, false, kind, provider, model));
        assert!(should_report_call(&reported, true, kind, provider, model));
    }
    let barrier = std::sync::Barrier::new(10);
    let reported = ReportedCalls::default();
    let count = thread::scope(|scope| {
        let workers: Vec<_> = (0..10)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    usize::from(should_report_call(
                        &reported, false, "vectors", "jina", "new",
                    ))
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().unwrap())
            .sum::<usize>()
    });
    assert_eq!(count, 1);
}

#[test]
fn construction_is_offline_and_embeddings_preserve_aliases() {
    let mock = Mock::new(vec![]);
    let p = Providers::new(&json!({"embeddingProvider":"jina", "embeddingModel":"jina/jina-embeddings-v4", "embeddingDimensions":16,
        "descriptionCommand":["not-installed-agent"], "descriptionModel":"provider/arbitrary-model"})).unwrap();
    assert_eq!(p.dimensions(), 16);
    assert_eq!(p.embedding_profile()["provider"], "jina");
    assert_eq!(p.description_profile()["model"], "provider/arbitrary-model");
    assert_eq!(p.description_profile()["provider"], "command");
    assert!(p.embed(&[], false).unwrap().is_empty());
    assert!(p.rerank("", &[]).unwrap().is_empty());
    assert!(mock.requests.lock().unwrap().is_empty());
    let p = Providers::new(
        &json!({"provider":"openai", "model":"text-embedding-3-small", "dimensions":8,
        "embeddingProvider":"jina", "embeddingDimensions":16}),
    )
    .unwrap();
    assert_eq!(p.dimensions(), 8);
    assert_eq!(p.embedding_profile()["provider"], "openai");
}

#[test]
fn symbol_vectors_are_lazy_separate_and_normalize_input() {
    for provider in ["openai", "jina"] {
        let mock = Mock::new(vec![(200, json!({"data":[{"index":0,"embedding":[3,4]}]}))]);
        let mut config = mock.config();
        config["provider"] = json!(provider);
        config["dimensions"] = json!(512);
        config["symbolDimensions"] = json!(2);
        let p = Providers::new(&config).unwrap();
        assert!(p.symbol_vector.get().is_none());
        let symbol = p.symbol_vector().unwrap();
        assert_eq!(symbol.dimensions(), 2);
        assert_eq!(p.dimensions(), 512);
        assert_ne!(symbol.profile(), p.embedding_profile());
        assert!(std::ptr::eq(symbol, p.symbol_vector().unwrap()));
        assert_eq!(
            symbol.embed(&["getHTTP2Response".into()], true).unwrap(),
            vec![vec![0.6, 0.8]]
        );
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests[0].body["input"], json!(["get http 2 response"]));
        assert_eq!(requests[0].body["dimensions"], 2);
        if provider == "jina" {
            assert_eq!(requests[0].body["task"], "code.query");
        }
    }
    for invalid in [json!(0), json!(-1), json!(1.5), json!("256")] {
        let p = Providers::new(&json!({"symbolDimensions":invalid})).unwrap();
        assert!(p.symbol_vector().is_err());
        assert!(p.symbol_vector.get().is_none());
    }
}

#[test]
fn embeddings_batch_reorder_normalize_and_use_jina_tasks() {
    let mock = Mock::new(vec![
        (
            200,
            json!({"data":[{"index":1,"embedding":[0,4]},{"index":0,"embedding":[3,4]}]}),
        ),
        (200, json!({"data":[{"index":0,"embedding":[5,0]}]})),
        (200, json!({"data":[{"index":0,"embedding":[0,9]}]})),
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
    p.embed(&["c".into()], false).unwrap();
    p.embed(&["q".into()], true).unwrap();
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests[0].path, "/v1/embeddings");
    assert_eq!(requests[0].body["task"], "code.passage");
    assert_eq!(requests[2].body["task"], "code.query");
    assert_eq!(requests[0].body["embedding_type"], "float");
    assert!(
        requests[0]
            .headers
            .contains("authorization: bearer mock-secret")
    );
}

#[test]
fn embedding_limits_expose_successful_paid_batches() {
    for (provider, maximum) in [("openai", 32), ("jina", 64)] {
        for configured in [None, Some(2), Some(128)] {
            let limit = configured.unwrap_or(maximum).min(maximum);
            let data: Vec<_> = (0..limit)
                .map(|i| json!({"index":i,"embedding":[1,0]}))
                .collect();
            let mock = Mock::new(vec![(200, json!({"data":data})), (503, json!({}))]);
            let mut config = mock.config();
            config["provider"] = json!(provider);
            config["providerMaxRetries"] = json!(0);
            if let Some(n) = configured {
                config["embeddingBatchSize"] = json!(n);
            }
            let p = Providers::new(&config).unwrap();
            assert_eq!(p.embedding_batch_limit(), limit);
            let inputs = vec!["input".into(); limit + 1];
            assert!(p.embed(&inputs, false).is_err());
            assert!(mock.requests.lock().unwrap().is_empty());
            assert_eq!(p.embed(&inputs[..limit], false).unwrap().len(), limit);
            assert!(p.embed(&inputs[limit..], false).is_err());
            assert_eq!(mock.requests.lock().unwrap().len(), 2);
        }
    }
}

#[test]
fn embedding_validation_rejects_corruption_without_poisoning_next_call() {
    for data in [
        json!([]),
        json!([{"index":1,"embedding":[1,0]}]),
        json!([{"index":0,"embedding":[1]}]),
        json!([{"index":0,"embedding":["1",0]}]),
        json!([{"index":0,"embedding":[0,0]}]),
        json!([{"index":0,"embedding":[1e100,0]}]),
        json!([{"index":0,"embedding":[1,0]},{"index":0,"embedding":[0,1]}]),
    ] {
        let mock = Mock::new(vec![
            (200, json!({"data":data})),
            (200, json!({"data":[{"index":0,"embedding":[1,0]}]})),
        ]);
        let p = Providers::new(&mock.config()).unwrap();
        assert!(p.embed(&["a".into()], false).is_err());
        assert_eq!(p.embed(&["a".into()], false).unwrap(), vec![vec![1., 0.]]);
        assert_eq!(mock.requests.lock().unwrap().len(), 2);
    }
    assert_eq!(normalize(&json!([1e-200, 0]), 2).unwrap(), vec![1., 0.]);
}

#[test]
fn openai_bounds_utf8_input_and_ada_keeps_native_dimensions() {
    let mock = Mock::new(vec![(200, json!({"data":[{"index":0,"embedding":[1,0]}]}))]);
    let p = Providers::new(&mock.config()).unwrap();
    let input = "🙂x".repeat(10_000);
    p.embed(std::slice::from_ref(&input), false).unwrap();
    let requests = mock.requests.lock().unwrap();
    let sent = requests[0].body["input"][0].as_str().unwrap();
    assert!(sent.len() <= 8191 && input.starts_with(sent));
    drop(requests);
    assert!(Providers::new(&json!({"model":"text-embedding-ada-002","dimensions":256})).is_err());
    let mock = Mock::new(vec![(
        200,
        json!({"data":[{"index":0,"embedding":vec![1;1536]}]}),
    )]);
    let mut config = mock.config();
    config.as_object_mut().unwrap().remove("dimensions");
    config["model"] = json!("text-embedding-ada-002");
    config["symbolDimensions"] = json!(256);
    let p = Providers::new(&config).unwrap();
    let symbols = p.symbol_vector().unwrap();
    assert_eq!(symbols.dimensions(), 1536);
    symbols.embed(&["GLOBAL_CONSTANT".into()], false).unwrap();
    assert!(
        mock.requests.lock().unwrap()[0]
            .body
            .get("dimensions")
            .is_none()
    );
}

#[test]
fn native_rerankers_validate_rankings_preserve_credentials_and_endpoints() {
    for provider in ["cohere", "jina"] {
        for bad in [
            Value::Null,
            json!([]),
            json!([{"index":0,"relevance_score":0.1},{"index":0,"relevance_score":0.2}]),
            json!([{"index":0,"relevance_score":"secret"}]),
        ] {
            let mock = Mock::new(vec![
                (200, json!({"results":bad})),
                (
                    200,
                    json!({"results":[{"index":0,"relevance_score":0.1},{"index":1,"relevance_score":0.9}]}),
                ),
            ]);
            let mut config = mock.config();
            config["rerankerProvider"] = json!(provider);
            config["rerankerModel"] = json!(format!("{provider}/custom"));
            config["rerankerBaseUrl"] = json!(format!("{}/rerank/?route=custom", mock.base));
            let p = Providers::new(&config).unwrap();
            let docs = ["first".into(), "second".into()];
            assert!(p.rerank("q", &docs).is_err());
            assert_eq!(p.rerank("q", &docs).unwrap(), vec![(1, 0.9), (0, 0.1)]);
            let requests = mock.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].path, "/v1/rerank?route=custom");
            assert_eq!(requests[0].body["model"], "custom");
            if provider == "jina" {
                assert_eq!(requests[0].body["return_documents"], false);
            }
        }
    }
    for ranking in [
        json!([]),
        json!([{"index":1,"score":0.5}]),
        json!([{"index":0,"score":1.1}]),
        json!([{"index":0,"score":"0.5"}]),
    ] {
        assert!(parse_ranking(&ranking, 1, true).is_err());
    }
}

#[test]
fn http_retries_transient_statuses_not_auth_redirects_or_paid_invalid_json() {
    for status in [408, 409, 425, 429, 500, 502, 503, 504, 529] {
        let mock = Mock::new(vec![(status, json!({})), (200, json!({"ok":true}))]);
        let http = Http::new(&mock.config()).unwrap();
        let mut notices = 0;
        assert_eq!(
            http.request_with_notice(
                &mock.base,
                Some(&json!({"input":["λ"]})),
                &HeaderMap::new(),
                || notices += 1
            )
            .unwrap(),
            json!({"ok":true})
        );
        assert_eq!(notices, 2);
    }
    for status in [400, 401, 403, 404, 302] {
        let mock = Mock::new(vec![(status, json!({"error":"mock-secret"}))]);
        let error = Http::new(&mock.config())
            .unwrap()
            .request(&mock.base, None, &HeaderMap::new())
            .unwrap_err();
        assert!(error.to_string().contains(&status.to_string()));
        assert!(!format!("{error:#?}").contains("mock-secret"));
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
    }
    let mock = Mock::raw(vec![(200, "not JSON: mock-secret".into())]);
    let error = Http::new(&mock.config())
        .unwrap()
        .request(&mock.base, None, &HeaderMap::new())
        .unwrap_err();
    assert!(!format!("{error:#?}").contains("mock-secret"));
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
}

#[test]
fn http_short_reads_recover_and_retry_after_is_bounded() {
    let mock = Mock::wire(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort".into(),
        "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}".into(),
    ]);
    assert_eq!(
        Http::new(&mock.config())
            .unwrap()
            .request(&mock.base, None, &HeaderMap::new())
            .unwrap(),
        json!({"ok":true})
    );
    for (header, expected) in [
        ("0", Some(0)),
        ("2", Some(2)),
        ("999999", Some(5)),
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
        assert_eq!(failure.retry_after, expected.map(Duration::from_secs));
    }
}

#[test]
fn catalogue_remains_public_deduplicated_and_provider_scoped() {
    let mock = Mock::new(vec![(
        200,
        json!({"data":[{"id":"a"},{"id":"b"},{"id":"a"}]}),
    )]);
    assert_eq!(
        fetch_models(
            &Http::new(&mock.config()).unwrap(),
            "opencode-go",
            &mock.base
        )
        .unwrap(),
        vec![
            json!({"provider":"opencode-go","model":"a"}),
            json!({"provider":"opencode-go","model":"b"})
        ]
    );
    assert!(
        !mock.requests.lock().unwrap()[0]
            .headers
            .contains("authorization")
    );
    assert!(models(Some("invalid")).is_err());
    for body in [
        json!({}),
        json!({"data":[{"id":""}]}),
        json!({"data":[{"id":4}]}),
    ] {
        let mock = Mock::new(vec![(200, body)]);
        assert!(fetch_models(&Http::new(&mock.config()).unwrap(), "opencode", &mock.base).is_err());
    }
}

#[test]
fn config_validation_and_endpoint_policy_remain_offline() {
    for config in [
        Value::Null,
        json!([]),
        json!("config"),
        json!({"dimensions":0}),
        json!({"provider":"unknown"}),
        json!({"model":"jina/foreign"}),
        json!({"descriptionModel":false}),
        json!({"descriptionModel":" "}),
    ] {
        assert!(Providers::new(&config).is_err(), "{config}");
    }
    for key in ["embeddingBaseUrl", "rerankerBaseUrl"] {
        let mut config =
            json!({key:"http://gateway.example.invalid/v1","rerankerProvider":"cohere"});
        let valid = |c: &Value| {
            Providers::new(c).and_then(|p| {
                if key == "rerankerBaseUrl" {
                    p.reranker().map(|_| ())
                } else {
                    Ok(())
                }
            })
        };
        assert!(valid(&config).is_err());
        config["allowInsecureEndpoints"] = json!(true);
        assert!(valid(&config).is_ok());
    }
    for base in [
        "not a URL",
        "file:///tmp/provider",
        "https://user:secret@example.invalid/v1",
    ] {
        assert!(Providers::new(&json!({"embeddingBaseUrl":base})).is_err());
    }
    let mock = Mock::new(vec![]);
    let mut config = mock.config();
    config["embeddingBaseUrl"] = json!(format!("{}/embeddings?token=secret", mock.base));
    let p = Providers::new(&config).unwrap();
    assert!(!p.embedding_profile().to_string().contains("secret"));
    config["providerMaxRetries"] = Value::Null;
    config["retryDelayMs"] = Value::Null;
    assert_eq!(Http::new(&config).unwrap().retries, 2);
}

#[test]
fn credentials_are_operation_scoped_trimmed_and_never_routed_by_untrusted_repos() {
    let environment = |_: &str| Some(" env-secret ".into());
    let remote = json!({CONFIG_ORIGIN_KEY:CONFIG_ORIGIN_REPOSITORY,"embeddingBaseUrl":"https://gateway.example.invalid/v1"});
    let error = resolve_api_key(&remote, "embeddingApiKey", "openai", environment)
        .unwrap_err()
        .to_string();
    assert!(error.contains("repository config") && error.contains("trustedEndpointHosts"));
    assert!(!error.contains("env-secret"));
    let mut trusted = remote.clone();
    trusted["trustedEndpointHosts"] = json!(["gateway.example.invalid"]);
    assert_eq!(
        resolve_api_key(&trusted, "embeddingApiKey", "openai", environment).unwrap(),
        "env-secret"
    );
    let mut own = remote.clone();
    own["embeddingApiKey"] = json!(" operation-key ");
    own["openaiApiKey"] = json!("provider-key");
    assert_eq!(
        resolve_api_key(&own, "embeddingApiKey", "openai", environment).unwrap(),
        "operation-key"
    );
    let mut flagged = remote.clone();
    flagged[TRUST_REPOSITORY_ENDPOINTS_KEY] = json!(true);
    assert!(resolve_api_key(&flagged, "embeddingApiKey", "openai", environment).is_ok());
    for invalid in [json!(" "), json!(42), json!("key\nsecret")] {
        let mock = Mock::new(vec![]);
        let mut config = mock.config();
        config["embeddingApiKey"] = invalid;
        assert!(
            Providers::new(&config)
                .unwrap()
                .embed(&["input".into()], false)
                .is_err()
        );
        assert!(mock.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn jina_tasks_follow_model_generation_or_explicit_config() {
    use vector_jina::tasks;
    assert_eq!(
        tasks(&json!({}), "jina-embeddings-v4").unwrap(),
        Some(("code.query".into(), "code.passage".into()))
    );
    assert_eq!(
        tasks(&json!({}), "jina-embeddings-v3").unwrap(),
        Some(("retrieval.query".into(), "retrieval.passage".into()))
    );
    assert_eq!(
        tasks(&json!({}), "jina-embeddings-v2-base-code").unwrap(),
        None
    );
    assert!(tasks(&json!({}), "jina-code-embeddings-1.5b").is_err());
    assert_eq!(
        tasks(&json!({"jinaTask":"nl2code"}), "jina-code-embeddings-1.5b").unwrap(),
        Some(("nl2code.query".into(), "nl2code.passage".into()))
    );
    assert_eq!(
        tasks(&json!({"jinaTask":"none"}), "jina-embeddings-v4").unwrap(),
        None
    );
    assert!(tasks(&json!({"jinaTask":"Code Query"}), "jina-embeddings-v4").is_err());
}

#[test]
fn generative_reranking_uses_command_and_validates_every_candidate() {
    for count in [1, 100] {
        let ranking: Vec<_> = (0..count)
            .map(|index| json!({"index":index,"score":1}))
            .collect();
        let text = json!({"ranking":ranking}).to_string();
        let p=Providers::new(&json!({"rerankerProvider":"command","rerankerCommand":[agent::executable(),"emit",text]})).unwrap();
        assert_eq!(
            p.rerank("q", &vec!["doc".into(); count]).unwrap().len(),
            count
        );
    }
    for text in [
        "not JSON: secret",
        "{}",
        "{\"ranking\":[{\"index\":0,\"score\":2}]}",
    ] {
        let p=Providers::new(&json!({"rerankerProvider":"command","rerankerCommand":[agent::executable(),"emit",text]})).unwrap();
        let error = p.rerank("q", &["doc".into()]).unwrap_err();
        assert!(!format!("{error:#?}").contains("secret"));
    }
    let p = Providers::new(&json!({"rerankerProvider":"command"})).unwrap();
    assert!(p.rerank("q", &[]).unwrap().is_empty());
    assert!(
        p.rerank("q", &vec!["doc".into(); 101])
            .unwrap_err()
            .to_string()
            .contains("at most 100")
    );
    assert!(
        p.rerank("q", &["doc".into()])
            .unwrap_err()
            .to_string()
            .contains("rerankerCommand")
    );
}
