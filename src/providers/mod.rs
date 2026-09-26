//! Blocking hosted providers. Construction is offline; credentials are resolved on use.
//!
//! Implementations of the provider-independent traits in [`crate::models`] live
//! in `<capability>_<provider>.rs` modules (for example, `llm_opencode_go.rs`).
//! The `llm`, `vector`, and `rerank` factories select implementations; indexing
//! and search consume their traits through [`Providers`]. HTTP transport and
//! wire protocols are shared separately from provider policy.
//!
//! The canonical `provider`, `model`, and `dimensions` embedding keys take precedence
//! over the `embeddingProvider`, `embeddingModel`, and `embeddingDimensions` aliases.
//! Endpoint overrides are `embeddingBaseUrl`, `descriptionBaseUrl`, and
//! `rerankerBaseUrl`; a base URL includes its API version, e.g. `http://localhost/v1`.
//! Embedding/reranking overrides may also include `/embeddings` or `/rerank`.
//! Optional operation-specific `*ApiKey` keys override provider-specific `*ApiKey`
//! keys and the usual environment variables. Never print this struct or its config.
//! `providerTimeoutMs` defaults to 60,000 (capped at 300,000), `providerMaxRetries`
//! defaults to 2 (maximum 5), and `retryDelayMs` defaults to 250. Backoff and numeric
//! Retry-After delays are capped at five seconds. Description failover/empty-output
//! recovery allows six total attempts, without nested HTTP retries.
//!
//! `models(None)` lists both public OpenCode catalogues. Entries have exactly the
//! shape `{ "provider": "opencode-go", "model": "model-id" }`; join with `/` for
//! a qualified reference. `Providers::models` is an alias for the free function.

use crate::models::{Llm, Rerank, Vector};
use anyhow::{Result, anyhow, bail, ensure};
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use http::{Failure, Http};
use protocol::{Protocol, auth_headers, description_request, description_text};

mod http;
mod llm;
mod llm_openai;
mod llm_opencode;
mod llm_opencode_go;
mod protocol;
mod rerank;
mod rerank_cohere;
mod rerank_jina;
mod rerank_openai;
mod vector;
mod vector_jina;
mod vector_openai;

#[cfg(test)]
mod tests;

const OPENAI: &str = "https://api.openai.com/v1";
const JINA: &str = "https://api.jina.ai/v1";
const ZEN: &str = "https://opencode.ai/zen/v1";
const GO: &str = "https://opencode.ai/zen/go/v1";
const DESCRIPTION_ATTEMPTS: usize = 6;

type ReportedCalls = Mutex<HashSet<(String, String, String)>>;
static REPORTED_CALLS: OnceLock<ReportedCalls> = OnceLock::new();

/// Decide atomically so concurrent requests report each identity only once.
fn should_report_call(
    reported: &ReportedCalls,
    verbose: bool,
    kind: &str,
    provider: &str,
    model: &str,
) -> bool {
    let first = reported
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert((kind.into(), provider.into(), model.into()));
    verbose || first
}

pub struct Providers {
    context: Arc<Context>,
    vector: Box<dyn Vector>,
    llm: Box<dyn Llm>,
    reranker: OnceLock<Box<dyn Rerank>>,
}

struct Context {
    config: Value,
    http: Http,
}

impl Providers {
    pub fn new(config: &Value) -> Result<Self> {
        ensure!(
            config.is_object(),
            "Provider configuration must be a JSON object"
        );
        let context = Arc::new(Context {
            config: config.clone(),
            http: Http::new(config)?,
        });
        Ok(Self {
            vector: vector::create(context.clone())?,
            llm: llm::create(context.clone())?,
            context,
            reranker: OnceLock::new(),
        })
    }

    pub fn vector(&self) -> &dyn Vector {
        self.vector.as_ref()
    }

    pub fn llm(&self) -> &dyn Llm {
        self.llm.as_ref()
    }

    pub fn reranker(&self) -> Result<&dyn Rerank> {
        if self.reranker.get().is_none() {
            let reranker = rerank::create(self.context.clone())?;
            let _ = self.reranker.set(reranker);
        }
        Ok(self.reranker.get().expect("reranker initialized").as_ref())
    }

    pub fn dimensions(&self) -> usize {
        self.vector().dimensions()
    }

    /// Maximum inputs for one `embed` call, including the configured batch cap.
    /// Callers must persist each returned batch immediately, including concurrent batches.
    pub fn embedding_batch_limit(&self) -> usize {
        self.vector().batch_limit()
    }

    pub fn embedding_profile(&self) -> Value {
        self.vector().profile()
    }

    /// The profile identifies the configured primary, even while fallback is active.
    pub fn description_profile(&self) -> Value {
        self.llm().profile()
    }

    pub fn embed(&self, inputs: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        self.vector().embed(inputs, query)
    }

    /// Successful fallback remains active across calls; a failure switches back.
    /// Six total attempts bound failover/empty-output retries without nesting HTTP retries.
    pub fn describe(&self, system: &str, prompt: &str) -> Result<String> {
        self.llm().describe(system, prompt)
    }

    /// Ranks every supplied candidate. Enable/disable and candidate retrieval limits
    /// belong to the caller; invoking this method explicitly requests reranking.
    pub fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<(usize, f64)>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        self.reranker()?.rerank(query, documents)
    }

    pub fn models(provider: Option<&str>) -> Result<Value> {
        models(provider)
    }
}

impl Context {
    fn report_call(&self, kind: &str, provider: &str, model: &str) {
        if should_report_call(
            REPORTED_CALLS.get_or_init(ReportedCalls::default),
            self.config["verbose"].as_bool() == Some(true),
            kind,
            provider,
            model,
        ) {
            eprintln!(
                "slopdex: notice: external model call: kind={kind} provider={} model={}",
                json!(provider),
                json!(model),
            );
        }
    }
}

/// Fetch the public, live OpenCode catalogue without requiring credentials.
/// Returns `[{"provider": "opencode" | "opencode-go", "model": "id"}, ...]`.
/// `None` fetches both providers. Duplicate IDs are removed within each provider.
pub fn models(provider: Option<&str>) -> Result<Value> {
    let providers: &[&str] = match provider {
        None => &["opencode", "opencode-go"],
        Some("opencode") => &["opencode"],
        Some("opencode-go") => &["opencode-go"],
        _ => bail!("models provider must be opencode or opencode-go"),
    };
    let http = Http::new(&json!({}))?;
    let mut result = Vec::new();
    for provider in providers {
        let base = match *provider {
            "opencode" => llm_opencode::DEFAULT_BASE,
            "opencode-go" => llm_opencode_go::DEFAULT_BASE,
            _ => unreachable!("catalogue providers validated above"),
        };
        result.extend(fetch_models(&http, provider, base)?);
    }
    Ok(Value::Array(result))
}

fn fetch_models(http: &Http, provider: &str, base: &str) -> Result<Vec<Value>> {
    let value = http.request(&endpoint(base, "models")?, None, &HeaderMap::new())?;
    let data = value["data"]
        .as_array()
        .ok_or_else(|| anyhow!("Malformed model catalogue: missing data array"))?;
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for entry in data {
        let model = entry["id"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("Malformed model catalogue: invalid model ID"))?;
        if seen.insert(model) {
            models.push(json!({"provider": provider, "model": model}));
        }
    }
    Ok(models)
}

fn string<'a>(config: &'a Value, keys: &[&str]) -> Result<Option<&'a str>> {
    for key in keys {
        if let Some(value) = config.get(*key).filter(|v| !v.is_null()) {
            return value
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(|s| Some(s.trim()))
                .ok_or_else(|| anyhow!("{key} must be a non-empty string"));
        }
    }
    Ok(None)
}

fn positive(config: &Value, keys: &[&str], default: usize) -> Result<usize> {
    for key in keys {
        if let Some(value) = config.get(*key).filter(|v| !v.is_null()) {
            return value
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| anyhow!("{key} must be a positive integer"));
        }
    }
    Ok(default)
}

fn unqualify<'a>(model: &'a str, provider: &str) -> Result<&'a str> {
    if let Some((prefix, id)) = model.split_once('/').filter(|(prefix, _)| {
        matches!(
            *prefix,
            "openai" | "jina" | "cohere" | "opencode" | "opencode-go"
        )
    }) {
        ensure!(
            prefix == provider,
            "Model reference provider must match the configured provider"
        );
        ensure!(!id.trim().is_empty(), "Model ID must not be empty");
        return Ok(id);
    }
    Ok(model)
}

fn auth_path() -> Option<PathBuf> {
    let data = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| PathBuf::from(s.trim()))
        .or_else(|| dirs::home_dir().map(|p| p.join(".local/share")))?;
    Some(data.join("opencode/auth.json"))
}

fn stored_key(path: &std::path::Path, provider: &str) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let auth: Value = serde_json::from_reader(file.take(1024 * 1024)).ok()?;
    auth.get(provider)?
        .get("key")?
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_owned())
}

fn api_key(config: &Value, operation_key: &str, provider: &str) -> Result<String> {
    let (setting, env) = match provider {
        "openai" => ("openaiApiKey", "OPENAI_API_KEY"),
        "jina" => ("jinaApiKey", "JINA_API_KEY"),
        "cohere" => ("cohereApiKey", "COHERE_API_KEY"),
        "opencode" | "opencode-go" => ("opencodeApiKey", "OPENCODE_API_KEY"),
        _ => bail!("Unsupported credential provider"),
    };
    if let Some(key) = string(config, &[operation_key, setting])? {
        return Ok(key.into());
    }
    if let Some(key) = std::env::var(env).ok().filter(|key| !key.trim().is_empty()) {
        return Ok(key.trim().into());
    }
    if provider.starts_with("opencode") {
        if let Some(key) = auth_path().and_then(|p| stored_key(&p, provider)) {
            return Ok(key);
        }
        bail!("OPENCODE_API_KEY or stored opencode/auth.json credentials are required");
    }
    bail!("{env} is required")
}

fn validate_url(base: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(base).map_err(|_| anyhow!("Invalid provider base URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "Provider base URL must be HTTP(S), without user information or a fragment"
    );
    Ok(url)
}

fn endpoint(base: &str, suffix: &str) -> Result<String> {
    let mut url = validate_url(base)?;
    let path = url.path().trim_end_matches('/').to_owned();
    let suffix = format!("/{suffix}");
    if !path.ends_with(&suffix) {
        url.set_path(&format!("{path}{suffix}"));
    } else {
        url.set_path(&path);
    }
    Ok(url.into())
}

fn truncate_bytes(value: &str, limit: usize) -> &str {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn index(value: &Value, count: usize, kind: &str) -> Result<usize> {
    value
        .as_u64()
        .and_then(|i| usize::try_from(i).ok())
        .filter(|i| *i < count)
        .ok_or_else(|| anyhow!("Invalid {kind} response index"))
}

fn normalize(value: &Value, dimensions: usize) -> Result<Vec<f32>> {
    let values = value
        .as_array()
        .ok_or_else(|| anyhow!("Embedding must be an array"))?;
    ensure!(
        values.len() == dimensions,
        "Embedding dimension mismatch: expected {dimensions}, received {}",
        values.len()
    );
    let mut vector = Vec::with_capacity(dimensions);
    let mut scale = 0.0_f64;
    for value in values {
        let number = value
            .as_f64()
            .filter(|n| n.is_finite() && n.abs() <= f32::MAX as f64)
            .ok_or_else(|| anyhow!("Embedding components must be finite f32 numbers"))?;
        scale = scale.max(number.abs());
        vector.push(number);
    }
    ensure!(scale > 0.0, "Embedding vector must have nonzero norm");
    // Scale first to avoid overflow/underflow while computing the norm.
    let norm = vector
        .iter()
        .map(|v| (v / scale).powi(2))
        .sum::<f64>()
        .sqrt();
    Ok(vector
        .into_iter()
        .map(|v| ((v / scale) / norm) as f32)
        .collect())
}

fn parse_ranking(value: &Value, count: usize, openai: bool) -> Result<Vec<(usize, f64)>> {
    let values = value
        .as_array()
        .ok_or_else(|| anyhow!("Malformed reranking response"))?;
    ensure!(
        values.len() == count,
        "Reranking response count does not match candidate count"
    );
    let mut seen = HashSet::new();
    let mut result = Vec::with_capacity(count);
    for item in values {
        let index = index(&item["index"], count, "reranking")?;
        ensure!(seen.insert(index), "Duplicate reranking response index");
        let score = item[if openai { "score" } else { "relevance_score" }]
            .as_f64()
            .filter(|s| s.is_finite() && (!openai || (0.0..=1.0).contains(s)))
            .ok_or_else(|| anyhow!("Invalid reranking score"))?;
        result.push((index, score));
    }
    result.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(result)
}

fn session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seed = format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let hash = format!("{:x}", Sha256::digest(seed.as_bytes()));
    format!(
        "{}-{}-4{}-a{}-{}",
        &hash[..8],
        &hash[8..12],
        &hash[13..16],
        &hash[17..20],
        &hash[20..32]
    )
}
