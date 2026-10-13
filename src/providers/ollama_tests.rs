use super::*;

fn model_list() -> Value {
    json!({"models": [{"name": "qwen3-embedding:0.6b", "digest": "fixture-digest"}]})
}

fn details() -> Value {
    json!({"capabilities": ["embedding"], "model_info": {
        "general.architecture": "qwen3", "qwen3.context_length": 32768}})
}

fn config(mock: &Mock) -> Value {
    let mut config = mock.config();
    config["provider"] = json!("ollama");
    config["model"] = json!(OLLAMA_DEFAULT_MODEL);
    config["ollamaNativeDimensions"] = json!(2);
    config["ollamaModelDigest"] = json!("fixture-digest");
    config["providerMaxRetries"] = json!(0);
    config
}

#[test]
fn ollama_construction_is_offline_and_defaults_are_explicit() {
    for (model, dimensions) in [
        ("qwen3-embedding:0.6b", 1024),
        ("qwen3-embedding:4b", 2560),
        ("qwen3-embedding:8b", 4096),
    ] {
        let p = Providers::new(&json!({"provider": "ollama", "model": model})).unwrap();
        assert_eq!(p.dimensions(), dimensions);
        assert_eq!(p.embedding_batch_limit(), 8);
        assert_eq!(p.symbol_vector().unwrap().dimensions(), 256);
        assert_eq!(
            p.embedding_profile()["endpoint"],
            "http://localhost:11434/api/embed"
        );
        assert!(p.embed(&[], true).unwrap().is_empty());
    }
    let p = Providers::new(&json!({"provider": "ollama"})).unwrap();
    assert_eq!(p.embedding_profile()["model"], OLLAMA_DEFAULT_MODEL);
    let mock = Mock::new(vec![]);
    let mut value = config(&mock);
    value["embeddingBatchSize"] = json!(1);
    let p = Providers::new(&value).unwrap();
    assert!(
        p.embed(&["a".into(), "b".into()], false)
            .unwrap_err()
            .to_string()
            .contains("exceeds batch limit")
    );
    assert!(mock.requests.lock().unwrap().is_empty());
}

#[test]
fn ollama_batches_are_ordered_normalized_and_never_send_credentials() {
    let mock = Mock::new(vec![
        (200, model_list()),
        (200, details()),
        (200, json!({"embeddings": [[3,4],[0,8]]})),
        (200, json!({"embeddings": [[5,0]]})),
        (200, json!({"embeddings": [[0,1]]})),
    ]);
    let p = Providers::new(&config(&mock)).unwrap();
    assert_eq!(
        p.embed(&["code".into(), "docs".into()], false).unwrap(),
        vec![vec![0.6, 0.8], vec![0., 1.]]
    );
    assert_eq!(
        p.embed(&["authentication".into()], true).unwrap(),
        vec![vec![1., 0.]]
    );
    p.symbol_vector()
        .unwrap()
        .embed(&["getHTTPResponse".into()], true)
        .unwrap();
    let requests = mock.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        5,
        "content and symbols share identity verification"
    );
    for r in requests.iter() {
        assert!(!r.headers.contains("authorization"));
    }
    assert_eq!(requests[2].path, "/v1/embed");
    assert_eq!(requests[2].body["input"], json!(["code", "docs"]));
    assert_eq!(requests[2].body["keep_alive"], "5m");
    assert_eq!(requests[2].body["truncate"], true);
    assert_eq!(requests[2].body["options"]["num_ctx"], 8192);
    assert!(requests[2].body.get("dimensions").is_none());
    assert_eq!(
        requests[3].body["input"],
        json!([
            "Instruct: Given a query, retrieve relevant code or documentation\nQuery:authentication"
        ])
    );
    assert!(
        requests[4].body["input"][0]
            .as_str()
            .unwrap()
            .ends_with("Query:get http response")
    );
}

#[test]
fn ollama_fixed_dimension_models_keep_native_symbol_space() {
    let p = Providers::new(
        &json!({"provider": "ollama", "model": "custom:test", "dimensions": 768,
        "ollamaNativeDimensions": 768}),
    )
    .unwrap();
    assert_eq!(p.symbol_vector().unwrap().dimensions(), 768);
    let alias = Providers::new(&json!({"embeddingProvider": "ollama", "embeddingModel": "custom:test", "embeddingDimensions": 768})).unwrap();
    assert_eq!(alias.symbol_vector().unwrap().dimensions(), 768);
    assert!(
        Providers::new(
            &json!({"provider": "ollama", "model": "custom:test", "dimensions": 256,
        "ollamaNativeDimensions": 768})
        )
        .is_err()
    );
    assert!(Providers::new(&json!({"provider": "ollama", "dimensions": 1025})).is_err());
    assert!(Providers::new(&json!({"provider": "ollama", "model": "custom:test"})).is_err());
}

#[test]
fn ollama_reduced_dimensions_are_requested_and_validated() {
    let mock = Mock::new(vec![
        (200, model_list()),
        (200, details()),
        (200, json!({"embeddings": [[4]]})),
    ]);
    let mut value = config(&mock);
    value["dimensions"] = json!(1);
    let p = Providers::new(&value).unwrap();
    assert_eq!(p.embed(&["query".into()], true).unwrap(), vec![vec![1.]]);
    assert_eq!(mock.requests.lock().unwrap()[2].body["dimensions"], 1);
}

#[test]
fn ollama_setup_checks_reduced_output_and_model_context_limits() {
    for reduced in [json!([1]), json!([1, 0])] {
        let mock = Mock::new(vec![
            (200, model_list()),
            (200, details()),
            (200, json!({"embeddings": [[1,0]]})),
            (200, json!({"embeddings": [reduced.clone()]})),
        ]);
        let mut value = config(&mock);
        value["dimensions"] = json!(1);
        let original = value.clone();
        let result = prepare_embeddings(&mut value, false);
        if reduced.as_array().unwrap().len() == 1 {
            result.unwrap();
            assert_eq!(value["ollamaNativeDimensions"], 2);
        } else {
            assert!(result.is_err());
            assert_eq!(value, original);
        }
        assert_eq!(mock.requests.lock().unwrap()[3].body["dimensions"], 1);
    }
    let mock = Mock::new(vec![
        (200, model_list()),
        (
            200,
            json!({"capabilities": ["embedding"], "model_info": {
            "general.architecture": "qwen3", "qwen3.context_length": 512}}),
        ),
        (200, json!({"embeddings": [[1,0]]})),
    ]);
    let mut value = config(&mock);
    prepare_embeddings(&mut value, false).unwrap();
    assert_eq!(value["ollamaContextLength"], 512);
}

#[test]
fn embeddinggemma_uses_distinct_query_and_document_prompts() {
    let mock = Mock::new(vec![
        (
            200,
            json!({"models": [{"name": "embeddinggemma:300m", "digest": "fixture-digest"}]}),
        ),
        (200, details()),
        (200, json!({"embeddings": [[1,0]]})),
        (200, json!({"embeddings": [[1,0]]})),
    ]);
    let mut value = config(&mock);
    value["model"] = json!("embeddinggemma:300m");
    let p = Providers::new(&value).unwrap();
    p.embed(&["document".into()], false).unwrap();
    p.embed(&["question".into()], true).unwrap();
    let requests = mock.requests.lock().unwrap();
    assert_eq!(
        requests[2].body["input"],
        json!(["title: none | text: document"])
    );
    assert_eq!(
        requests[3].body["input"],
        json!(["task: search result | query: question"])
    );
}

#[test]
fn ollama_rejects_corrupt_vectors_without_retrying() {
    for value in [
        json!({}),
        json!({"embeddings": []}),
        json!({"embeddings": [[1]]}),
        json!({"embeddings": [[0,0]]}),
        json!({"embeddings": [["bad",0]]}),
        json!({"embeddings": [[1e100,0]]}),
        json!({"embeddings": [[1,0],[0,1]]}),
    ] {
        let mock = Mock::new(vec![(200, model_list()), (200, details()), (200, value)]);
        assert!(
            Providers::new(&config(&mock))
                .unwrap()
                .embed(&["a".into()], false)
                .is_err()
        );
        assert_eq!(mock.requests.lock().unwrap().len(), 3);
    }
}

#[test]
fn ollama_prepare_discovers_dimensions_and_pins_identity_without_mutating_failure() {
    let mock = Mock::new(vec![
        (200, model_list()),
        (200, details()),
        (200, json!({"embeddings": [[1,0]]})),
    ]);
    let mut value = mock.config();
    value["provider"] = json!("ollama");
    value.as_object_mut().unwrap().remove("dimensions");
    prepare_embeddings(&mut value, false).unwrap();
    assert_eq!(value["dimensions"], 2);
    assert_eq!(value["ollamaNativeDimensions"], 2);
    assert_eq!(value["ollamaModelDigest"], "fixture-digest");
    assert_eq!(value["ollamaContextLength"], 8192);
    assert_eq!(value["model"], OLLAMA_DEFAULT_MODEL);
    // The input strategy derives from the model name; no family is persisted.
    assert!(value.get("ollamaModelFamily").is_none());
    assert!(value.get("ollamaAutoPull").is_none());
    let mock = Mock::new(vec![
        (200, model_list()),
        (200, details()),
        (200, json!({"embeddings": [[0,0]]})),
    ]);
    let mut value = config(&mock);
    let original = value.clone();
    assert!(prepare_embeddings(&mut value, false).is_err());
    assert_eq!(value, original);
}

#[test]
fn ollama_missing_models_require_explicit_download_consent() {
    let mock = Mock::new(vec![(200, json!({"models": []}))]);
    let mut value = config(&mock);
    let original = value.clone();
    let error = prepare_embeddings(&mut value, false).unwrap_err();
    assert!(error.to_string().contains("not installed"));
    assert_eq!(value, original);
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
}

#[test]
fn ollama_pull_stream_requires_success_and_checks_embedded_errors() {
    for stream in [
        "{\"status\":\"pulling\"}\n",
        "{\"error\":\"secret remote error\"}\n",
        "not json\n",
    ] {
        let mock = Mock::raw(vec![
            (200, json!({"models": []}).to_string()),
            (200, stream.into()),
        ]);
        let mut value = config(&mock);
        let original = value.clone();
        let error = prepare_embeddings(&mut value, true).unwrap_err();
        assert!(!error.to_string().contains("secret remote error"));
        assert_eq!(value, original);
        assert_eq!(mock.requests.lock().unwrap().len(), 2);
    }
}

#[test]
fn ollama_pull_stream_downloads_only_missing_models() {
    let mock = Mock::raw(vec![
        (200, json!({"models": []}).to_string()),
        (200, "{\"status\":\"downloading\",\"total\":100,\"completed\":50}\n{\"status\":\"success\"}\n".into()),
        (200, model_list().to_string()), (200, details().to_string()),
        (200, json!({"embeddings": [[1,0]]}).to_string())]);
    let mut value = config(&mock);
    prepare_embeddings(&mut value, true).unwrap();
    assert_eq!(value["ollamaAutoPull"], true);
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests[1].path, "/v1/pull");
    assert_eq!(
        requests[1].body,
        json!({"model": OLLAMA_DEFAULT_MODEL, "stream": true})
    );
    drop(requests);
    let mock = Mock::new(vec![
        (200, model_list()),
        (200, details()),
        (200, json!({"embeddings": [[1,0]]})),
    ]);
    let mut value = config(&mock);
    prepare_embeddings(&mut value, true).unwrap();
    assert!(
        !mock
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.path.ends_with("/pull"))
    );
}

#[test]
fn ollama_model_digest_changes_fail_before_inference() {
    let mock = Mock::new(vec![(200, model_list()), (200, details())]);
    let mut value = config(&mock);
    value["ollamaModelDigest"] = json!("old-digest");
    let error = Providers::new(&value)
        .unwrap()
        .embed(&["a".into()], false)
        .unwrap_err();
    assert!(error.to_string().contains("changed since setup"));
    assert_eq!(mock.requests.lock().unwrap().len(), 2);
    let p = Providers::new(&value).unwrap();
    value["ollamaModelDigest"] = json!("new-digest");
    assert_ne!(
        p.embedding_profile(),
        Providers::new(&value).unwrap().embedding_profile()
    );
}

#[test]
fn ollama_cloud_and_completion_models_never_receive_embedding_input() {
    for list in [
        json!({"models": [{"name": OLLAMA_DEFAULT_MODEL, "digest": "digest", "remote_host": "cloud"}]}),
        json!({"models": [{"name": OLLAMA_DEFAULT_MODEL, "digest": "digest", "remote_model": "remote"}]}),
    ] {
        let mock = Mock::new(vec![(200, list)]);
        assert!(
            Providers::new(&config(&mock))
                .unwrap()
                .embed(&["private code".into()], false)
                .is_err()
        );
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
    }
    for show in [
        json!({"capabilities": ["completion"]}),
        json!({"capabilities": ["embedding"], "remote_host": "cloud"}),
    ] {
        let mock = Mock::new(vec![(200, model_list()), (200, show)]);
        assert!(
            Providers::new(&config(&mock))
                .unwrap()
                .embed(&["private code".into()], false)
                .is_err()
        );
        assert_eq!(mock.requests.lock().unwrap().len(), 2);
    }
}

#[test]
fn ollama_installed_model_choices_filter_cloud_and_non_embedding_models() {
    let mock = Mock::new(vec![
        (
            200,
            json!({"models": [
        {"name": "chat:latest"}, {"name": "code:latest"}, {"name": "cloud:latest", "remote_host": "cloud"}]}),
        ),
        (200, json!({"capabilities": ["completion"]})),
        (200, details()),
    ]);
    assert_eq!(
        installed_embedding_models(&config(&mock)).unwrap(),
        vec!["code:latest"]
    );
    assert_eq!(mock.requests.lock().unwrap().len(), 3);
}

#[test]
fn ollama_context_and_input_strategy_are_profile_inputs_not_scheduling() {
    let mock = Mock::new(vec![]);
    let mut value = config(&mock);
    let before = Providers::new(&value).unwrap().embedding_profile();
    for (key, changed) in [
        ("ollamaKeepAlive", json!("10m")),
        ("embeddingBatchSize", json!(4)),
        ("embeddingParallelism", json!(2)),
        ("ollamaAutoPull", json!(true)),
    ] {
        let mut config = value.clone();
        config[key] = changed;
        assert_eq!(Providers::new(&config).unwrap().embedding_profile(), before);
    }
    value["ollamaContextLength"] = json!(4096);
    assert_ne!(Providers::new(&value).unwrap().embedding_profile(), before);
}

#[test]
fn unreachable_ollama_is_reported_only_for_transport_failures() {
    let mock = Mock::new(vec![(500, json!({"error": "busy"}))]);
    let value = config(&mock);
    let error = format!("{:#}", installed_embedding_models(&value).unwrap_err());
    assert!(error.contains("HTTP 500"), "{error}");
    assert!(!error.contains("Cannot reach Ollama"), "{error}");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut value = config(&mock);
    value["embeddingBaseUrl"] = json!(format!("http://127.0.0.1:{port}/api"));
    let error = format!("{:#}", installed_embedding_models(&value).unwrap_err());
    assert!(error.contains("Cannot reach Ollama"), "{error}");
}

#[test]
fn ollama_profiles_strip_endpoint_query_strings_and_resolve_against_them() {
    let mock = Mock::new(vec![]);
    let mut value = config(&mock);
    value["embeddingBaseUrl"] = json!(format!("{}/?token=secret", mock.base));
    let profile = Providers::new(&value).unwrap().embedding_profile();
    assert_eq!(profile["endpoint"], format!("{}/embed", mock.base));
    assert!(!profile.to_string().contains("secret"));
    // A saved profile recovers identity without a server; the model stays implicit.
    let mut minimal = json!({"provider": "ollama",
        "embeddingBaseUrl": format!("{}/?token=secret", mock.base)});
    resolve_embeddings(&mut minimal, Some(&profile.to_string())).unwrap();
    assert_eq!(minimal["ollamaModelDigest"], "fixture-digest");
    assert_eq!(minimal["ollamaNativeDimensions"], 2);
    assert!(minimal.get("model").is_none());
    assert!(mock.requests.lock().unwrap().is_empty());
}
