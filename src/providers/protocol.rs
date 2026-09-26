//! Description wire formats and protocol-specific authentication headers.

use anyhow::{Result, anyhow};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::{Value, json};

use super::{Failure, endpoint, validate_url};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Protocol {
    Responses,
    Chat,
    Messages,
    Gemini,
}

pub(super) fn auth_headers(key: &str, protocol: Protocol) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    let (name, value) = match protocol {
        Protocol::Messages => ("x-api-key", key.to_owned()),
        Protocol::Gemini => ("x-goog-api-key", key.to_owned()),
        _ => (AUTHORIZATION.as_str(), format!("Bearer {key}")),
    };
    let mut value = HeaderValue::from_str(&value)
        .map_err(|_| anyhow!("API key is not a valid HTTP header value"))?;
    value.set_sensitive(true);
    headers.insert(
        reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
        value,
    );
    if protocol == Protocol::Messages {
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    }
    Ok(headers)
}

pub(super) fn description_request(
    base: &str,
    model: &str,
    protocol: Protocol,
    system: &str,
    prompt: &str,
) -> Result<(String, Value)> {
    Ok(match protocol {
        Protocol::Responses => (
            endpoint(base, "responses")?,
            json!({"model": model, "instructions": system,
            "input": [{"role": "user", "content": [{"type": "input_text", "text": prompt}]}],
            "store": false, "max_output_tokens": 4096}),
        ),
        Protocol::Chat => (
            endpoint(base, "chat/completions")?,
            json!({"model": model,
            "messages": [{"role": "system", "content": system}, {"role": "user", "content": prompt}], "max_tokens": 4096}),
        ),
        Protocol::Messages => (
            endpoint(base, "messages")?,
            json!({"model": model, "system": system,
            "messages": [{"role": "user", "content": [{"type": "text", "text": prompt}]}], "max_tokens": 4096}),
        ),
        Protocol::Gemini => {
            let mut url = validate_url(base)?;
            {
                let mut segments = url
                    .path_segments_mut()
                    .map_err(|_| anyhow!("Invalid Gemini base URL"))?;
                segments
                    .pop_if_empty()
                    .push("models")
                    .push(&format!("{model}:generateContent"));
            }
            (
                url.into(),
                json!({"systemInstruction": {"parts": [{"text": system}]},
                "contents": [{"role": "user", "parts": [{"text": prompt}]}], "generationConfig": {"maxOutputTokens": 4096}}),
            )
        }
    })
}

pub(super) fn description_text(
    value: &Value,
    protocol: Protocol,
) -> std::result::Result<String, Failure> {
    if !value["error"].is_null() {
        return Err(Failure::new("Provider returned an error object", false));
    }
    let mut parts = Vec::new();
    match protocol {
        Protocol::Responses => {
            if matches!(
                value["status"].as_str(),
                Some("failed" | "cancelled" | "incomplete")
            ) {
                return Err(Failure::empty(
                    "Provider returned an incomplete description",
                ));
            }
            if let Some(text) = value["output_text"].as_str() {
                parts.push(text);
            } else if let Some(output) = value["output"].as_array() {
                for message in output {
                    if message["type"] == "message" {
                        collect_text(&message["content"], "output_text", &mut parts);
                    }
                }
            }
        }
        Protocol::Chat => {
            let choice = &value["choices"][0];
            if choice["finish_reason"] == "length" {
                return Err(Failure::empty(
                    "Provider returned an incomplete description",
                ));
            }
            if let Some(text) = choice["message"]["content"].as_str() {
                parts.push(text);
            } else {
                collect_text(&choice["message"]["content"], "text", &mut parts);
            }
        }
        Protocol::Messages => {
            if value["stop_reason"] == "max_tokens" {
                return Err(Failure::empty(
                    "Provider returned an incomplete description",
                ));
            }
            collect_text(&value["content"], "text", &mut parts);
        }
        Protocol::Gemini => {
            let candidate = &value["candidates"][0];
            if candidate["finishReason"] == "MAX_TOKENS" {
                return Err(Failure::empty(
                    "Provider returned an incomplete description",
                ));
            }
            if let Some(content) = candidate["content"]["parts"].as_array() {
                for part in content {
                    if let Some(text) = part["text"].as_str().filter(|_| part["thought"] != true) {
                        parts.push(text);
                    }
                }
            }
        }
    }
    let text = parts.join("").trim().to_owned();
    if text.is_empty() {
        Err(Failure::empty("Provider returned an empty description"))
    } else {
        Ok(text)
    }
}

fn collect_text<'a>(value: &'a Value, kind: &str, parts: &mut Vec<&'a str>) {
    if let Some(content) = value.as_array() {
        for part in content {
            if let Some(text) = part["text"].as_str().filter(|_| part["type"] == kind) {
                parts.push(text);
            }
        }
    }
}
