//! Explicit model preparation and pinned model identity for local Ollama inference.

use super::http::{Http, RequestError};
use super::{Context, endpoint, normalize, positive, string};
use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::sync::PoisonError;

pub const OLLAMA_DEFAULT_MODEL: &str = "qwen3-embedding:0.6b";
const DEFAULT_BASE: &str = "http://localhost:11434/api";

pub(super) fn model(config: &Value) -> Result<String> {
    let name = super::vector::model(config, "ollama", OLLAMA_DEFAULT_MODEL)?;
    ensure!(
        !cloud_alias(&name),
        "Ollama cloud-backed models are not supported for local embeddings"
    );
    Ok(name)
}

fn cloud_alias(name: &str) -> bool {
    name.ends_with(":cloud") || name.ends_with("-cloud")
}

fn warn_remote_endpoint(config: &Value) -> Result<()> {
    let url = super::validate_url(&url(config, "embed")?)?;
    let host = url.host_str().unwrap_or("").trim_matches(['[', ']']);
    let loopback = host.trim_end_matches('.') == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !loopback {
        crate::ui::warning(
            "slopdex: Ollama embedding endpoint is not loopback; inputs will be sent to the configured server, which may be remote",
        );
    }
    Ok(())
}

/// Native output sizes for the built-in Ollama presets. Other models are probed.
pub fn default_dimensions(model: &str) -> Option<usize> {
    match model {
        "qwen3-embedding:0.6b" => Some(1024),
        "qwen3-embedding:4b" => Some(2560),
        "qwen3-embedding:8b" => Some(4096),
        "embeddinggemma" | "embeddinggemma:300m" | "embeddinggemma:latest" => Some(768),
        _ => None,
    }
}

pub(super) fn family(model: &str) -> &'static str {
    let name = model.rsplit('/').next().unwrap_or(model);
    if name == "qwen3-embedding" || name.starts_with("qwen3-embedding:") {
        "qwen3"
    } else if name == "embeddinggemma" || name.starts_with("embeddinggemma:") {
        "embeddinggemma"
    } else {
        "generic"
    }
}

pub(super) fn supports_reduction(config: &Value) -> bool {
    model(config).is_ok_and(|m| family(&m) != "generic")
}

pub(super) fn url(config: &Value, suffix: &str) -> Result<String> {
    endpoint(
        string(config, &["embeddingBaseUrl"])?.unwrap_or(DEFAULT_BASE),
        suffix,
    )
}

/// The embed endpoint as recorded in profiles: without query parameters.
pub(super) fn profile_url(config: &Value) -> Result<String> {
    Ok(super::profile_endpoint(&url(config, "embed")?))
}

fn tags(config: &Value, http: &Http) -> Result<Vec<Value>> {
    let response = http
        .request(&url(config, "tags")?, None, &Default::default())
        .map_err(|error| {
            // Only connectivity failures mean the server is absent; HTTP errors and
            // malformed replies keep their own diagnostics.
            if error
                .downcast_ref::<RequestError>()
                .is_some_and(|request| request.transport)
            {
                error.context("Cannot reach Ollama; install/start Ollama (for example `ollama serve`) or check embeddingBaseUrl")
            } else {
                error
            }
        })?;
    response["models"]
        .as_array()
        .cloned()
        .ok_or_else(|| anyhow!("Malformed Ollama model list"))
}

fn find<'a>(models: &'a [Value], name: &str) -> Option<&'a Value> {
    let qualified = if name.rsplit('/').next().unwrap_or(name).contains(':') {
        name.to_owned()
    } else {
        format!("{name}:latest")
    };
    models.iter().find(|m| {
        m["name"] == qualified || m["model"] == qualified || m["name"] == name || m["model"] == name
    })
}

fn local(entry: &Value) -> Result<()> {
    ensure!(
        ["remote_host", "remote_model"]
            .iter()
            .all(|key| entry[*key].is_null() || entry[*key].as_str() == Some("")),
        "Ollama cloud-backed models are not supported for local embeddings"
    );
    Ok(())
}

fn show(config: &Value, http: &Http, name: &str) -> Result<Value> {
    let value = http.request(
        &url(config, "show")?,
        Some(&json!({"model": name})),
        &Default::default(),
    )?;
    local(&value)?;
    ensure!(
        value["capabilities"]
            .as_array()
            .is_some_and(|v| v.iter().any(|v| v == "embedding")),
        "Selected Ollama model does not support embeddings"
    );
    Ok(value)
}

pub(super) fn embeddings(response: &Value, count: usize) -> Result<&Vec<Value>> {
    let vectors = response["embeddings"]
        .as_array()
        .ok_or_else(|| anyhow!("Malformed Ollama embedding response: missing embeddings array"))?;
    ensure!(
        vectors.len() == count,
        "Embedding response count does not match input count"
    );
    Ok(vectors)
}

fn installed(config: &Value, http: &Http, name: &str, pull: bool) -> Result<Value> {
    let mut models = tags(config, http)?;
    if find(&models, name).is_none() {
        ensure!(
            pull || config["ollamaAutoPull"] == true,
            "Ollama model {name} is not installed; run `slopdex config embeddings --provider ollama --model {name} --pull` or `ollama pull {name}`"
        );
        let timeout = positive(config, &["ollamaPullTimeoutMs"], 1_800_000)?;
        crate::ui::info(format!("slopdex: downloading Ollama model {name}"));
        http.stream_json(
            &url(config, "pull")?,
            &json!({"model": name, "stream": true}),
            timeout,
            |event| {
                ensure!(event.get("error").is_none(), "Ollama model download failed");
                if let (Some(done), Some(total)) =
                    (event["completed"].as_u64(), event["total"].as_u64())
                {
                    crate::ui::progress(format!("Downloading Ollama model: {done}/{total} bytes"));
                }
                Ok(event["status"] == "success")
            },
        )?;
        models = tags(config, http)?;
    }
    let entry =
        find(&models, name).ok_or_else(|| anyhow!("Ollama did not install the selected model"))?;
    local(entry)?;
    ensure!(
        entry["digest"].as_str().is_some_and(|d| !d.is_empty()),
        "Ollama model has no content digest"
    );
    Ok(entry.clone())
}

/// Prepare an embedding selection without opening an index. Changes are staged;
/// model downloads may remain in Ollama's cache if later validation fails.
pub fn prepare_embeddings(config: &mut Value, pull: bool) -> Result<()> {
    if string(config, &["provider", "embeddingProvider"])?.unwrap_or("openai") != "ollama" {
        return Ok(());
    }
    let mut prepared = config.clone();
    let name = model(&prepared)?;
    warn_remote_endpoint(&prepared)?;
    let http = Http::new(&prepared)?;
    let entry = installed(&prepared, &http, &name, pull)?;
    let details = show(&prepared, &http, &name)?;
    let mut body = json!({"model": name, "input": ["slopdex embedding setup"],
        "truncate": false, "keep_alive": string(&prepared, &["ollamaKeepAlive"])?.unwrap_or("5m")});
    let response = http.request_with_notice(
        &url(&prepared, "embed")?,
        Some(&body),
        &Default::default(),
        || super::report_call(&prepared, "vectors", "ollama", &name),
    )?;
    let vectors = embeddings(&response, 1)?;
    let native = vectors[0]
        .as_array()
        .map(Vec::len)
        .filter(|n| *n > 0)
        .ok_or_else(|| anyhow!("Ollama returned an empty embedding"))?;
    normalize(&vectors[0], native)?;
    let dimensions = positive(&prepared, &["dimensions", "embeddingDimensions"], native)?;
    ensure!(
        dimensions <= native,
        "Embedding dimensions exceed the Ollama model's native dimensions ({native})"
    );
    ensure!(
        dimensions == native || supports_reduction(&prepared),
        "This Ollama model does not support reduced embedding dimensions"
    );
    if dimensions != native {
        body["dimensions"] = json!(dimensions);
        let response = http.request_with_notice(
            &url(&prepared, "embed")?,
            Some(&body),
            &Default::default(),
            || super::report_call(&prepared, "vectors", "ollama", &name),
        )?;
        normalize(&embeddings(&response, 1)?[0], dimensions)?;
    }
    let architecture = details["model_info"]["general.architecture"]
        .as_str()
        .unwrap_or("");
    let maximum = details["model_info"][format!("{architecture}.context_length")]
        .as_u64()
        .unwrap_or(8192) as usize;
    let context_length = positive(&prepared, &["ollamaContextLength"], maximum.min(8192))?;
    ensure!(
        context_length <= maximum,
        "ollamaContextLength exceeds the model context length ({maximum})"
    );
    prepared["model"] = json!(name);
    prepared["dimensions"] = json!(dimensions);
    prepared["ollamaNativeDimensions"] = json!(native);
    prepared["ollamaModelDigest"] = entry["digest"].clone();
    // The input strategy derives from the model name at construction; no saved family.
    prepared
        .as_object_mut()
        .expect("prepared config is an object")
        .remove("ollamaModelFamily");
    prepared["ollamaContextLength"] = json!(context_length);
    if pull {
        prepared["ollamaAutoPull"] = json!(true);
    }
    *config = prepared;
    Ok(())
}

/// Resolve unconfigured semantic callers before their first cache access. Provider
/// construction itself stays offline; saved identities need no server connection.
pub(crate) fn resolve_embeddings(config: &mut Value, saved_profile: Option<&str>) -> Result<()> {
    if string(config, &["provider", "embeddingProvider"])?.unwrap_or("openai") == "ollama"
        && string(config, &["ollamaModelDigest"])?.is_none()
    {
        if let Some(saved) = saved_profile {
            let profile: Value = serde_json::from_str(saved)?;
            // `model` already applies the default, so an unconfigured model matches
            // only a profile that embeds with that default; other inference settings
            // (dimensions mean native when missing) are never inherited.
            if profile["provider"] == "ollama"
                && profile["model"] == model(config)?
                && profile["endpoint"] == profile_url(config)?
                && profile["strategyVersion"] == "ollama-v1"
                && profile["modelDigest"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                && profile["nativeDimensions"].as_u64().is_some_and(|n| n > 0)
            {
                config["ollamaModelDigest"] = profile["modelDigest"].clone();
                config["ollamaNativeDimensions"] = profile["nativeDimensions"].clone();
                if config.get("ollamaContextLength").is_none() {
                    config["ollamaContextLength"] = profile["contextLength"].clone();
                }
                return Ok(());
            }
        }
        prepare_embeddings(config, false)?;
    }
    Ok(())
}

pub fn installed_embedding_models(config: &Value) -> Result<Vec<String>> {
    let http = Http::new(config)?;
    let mut result = Vec::new();
    for entry in tags(config, &http)? {
        if local(&entry).is_err() {
            continue;
        }
        let name = entry["name"]
            .as_str()
            .or_else(|| entry["model"].as_str())
            .ok_or_else(|| anyhow!("Malformed Ollama model name"))?;
        if cloud_alias(name) {
            continue;
        }
        let details = http.request(
            &url(config, "show")?,
            Some(&json!({"model": name})),
            &Default::default(),
        )?;
        if local(&details).is_ok()
            && details["capabilities"]
                .as_array()
                .is_some_and(|v| v.iter().any(|v| v == "embedding"))
        {
            result.push(name.to_owned());
        }
    }
    result.sort();
    result.dedup();
    Ok(result)
}

pub(super) fn verify(context: &Context) -> Result<()> {
    // A panic while verifying leaves the flag false; later callers simply verify again.
    let mut verified = context
        .ollama_verified
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if *verified {
        return Ok(());
    }
    let expected = string(&context.config, &["ollamaModelDigest"])?.ok_or_else(|| {
        anyhow!("Ollama model identity has not been resolved; run `slopdex config embeddings`")
    })?;
    let name = model(&context.config)?;
    warn_remote_endpoint(&context.config)?;
    let entry = installed(&context.config, &context.http, &name, false)?;
    show(&context.config, &context.http, &name)?;
    if entry["digest"] != expected {
        bail!(
            "Ollama model changed since setup; rerun `slopdex config embeddings` to select the new embedding profile"
        );
    }
    *verified = true;
    Ok(())
}
