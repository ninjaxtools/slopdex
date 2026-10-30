//! Shared fixture command builder; no real harness, credentials, or user cache.
#![allow(dead_code)] // Individual test modules use different parts of this helper.
use serde_json::{Value, json};
use std::{path::PathBuf, process::Command, sync::OnceLock};

pub fn executable() -> PathBuf {
    static FIXTURE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    FIXTURE
        .get_or_init(|| {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("agent.rs");
            let binary = temp
                .path()
                .join(format!("agent{}", std::env::consts::EXE_SUFFIX));
            std::fs::write(&source, include_str!("agent_fixture.rs")).unwrap();
            let result = Command::new("rustc")
                .arg("--edition=2024")
                .arg(&source)
                .arg("-o")
                .arg(&binary)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "fixture compilation failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            (temp, binary)
        })
        .1
        .clone()
}

pub fn http(base: &str, _key: &str) -> Value {
    let address = base
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    json!([executable(), "http", address])
}

pub fn response(body: &Value) -> String {
    body["output_text"]
        .as_str()
        .or_else(|| body["output"][0]["content"][0]["text"].as_str())
        .unwrap()
        .to_owned()
}

pub fn request(body: &Value, model: &str) -> Value {
    json!({"model": model, "instructions": body["instructions"],
        "input": body["messages"].as_array().unwrap().iter().map(|m|
            json!({"role": m["role"], "content": [{"type": if m["role"] == "assistant" {"output_text"} else {"input_text"}, "text": m["content"]}]})).collect::<Vec<_>>()})
}
