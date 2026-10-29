//! Blocking hosted and local providers. Construction is offline; credentials are resolved on use.
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
//! Hosted base URLs off the provider default and off loopback require `https`
//! (or `allowInsecureEndpoints`) and are named once per process on stderr; when the
//! configuration came from the repository file (`configOrigin: "repository"`), such
//! endpoints never receive environment/stored credentials unless the host is in
//! the user config's `trustedEndpointHosts` or `--trust-repo-endpoints` is passed.
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
mod ollama;
mod protocol;
mod rerank;
mod rerank_openai;
mod vector;
mod vector_jina;
mod vector_ollama;
mod vector_openai;

pub(crate) use ollama::resolve_embeddings;
pub use ollama::{
    OLLAMA_DEFAULT_MODEL, default_dimensions as ollama_model_dimensions,
    installed_embedding_models, prepare_embeddings,
};

#[cfg(test)]
mod tests;

const OPENAI: &str = "https://api.openai.com/v1";
const JINA: &str = "https://api.jina.ai/v1";
const COHERE: &str = "https://api.cohere.com/v2";
const ZEN: &str = "https://opencode.ai/zen/v1";
const GO: &str = "https://opencode.ai/zen/go/v1";
const DESCRIPTION_ATTEMPTS: usize = 6;

/// Reserved effective-configuration keys set by the CLI loader; never saved.
/// `configOrigin: "repository"` marks settings loaded from `<root>/.slopdex/config.json`.
pub const CONFIG_ORIGIN_KEY: &str = "configOrigin";
pub const CONFIG_ORIGIN_REPOSITORY: &str = "repository";
/// `--trust-repo-endpoints`: allow repository-configured remote endpoints to use
/// environment/stored credentials for this invocation.
pub const TRUST_REPOSITORY_ENDPOINTS_KEY: &str = "trustRepositoryEndpoints";

type ReportedCalls = Mutex<HashSet<(String, String, String)>>;
static REPORTED_CALLS: OnceLock<ReportedCalls> = OnceLock::new();
type ReportedEndpoints = Mutex<HashSet<(String, String)>>;
static REPORTED_ENDPOINTS: OnceLock<ReportedEndpoints> = OnceLock::new();

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
    symbol_vector: OnceLock<Box<dyn Vector>>,
    llm: Box<dyn Llm>,
    reranker: OnceLock<Box<dyn Rerank>>,
}

struct Context {
    config: Value,
    http: Http,
    ollama_verified: Arc<Mutex<bool>>,
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
            ollama_verified: Arc::new(Mutex::new(false)),
        });
        Ok(Self {
            vector: vector::create(context.clone())?,
            symbol_vector: OnceLock::new(),
            llm: llm::create(context.clone())?,
            context,
            reranker: OnceLock::new(),
        })
    }

    pub fn vector(&self) -> &dyn Vector {
        self.vector.as_ref()
    }

    /// A separate, lazily constructed embedding space for normalized symbol names.
    pub(crate) fn symbol_vector(&self) -> Result<&dyn Vector> {
        if self.symbol_vector.get().is_none() {
            let mut config = self.context.config.clone();
            let dimensions = positive(&config, &["symbolDimensions"], self.dimensions().min(256))?;
            // Models without a dimension-reduction contract retain their space.
            config["dimensions"] = json!(if self.vector.profile()["model"]
                == "text-embedding-ada-002"
                || (string(&config, &["provider", "embeddingProvider"])? == Some("ollama")
                    && !ollama::supports_reduction(&config))
            {
                self.dimensions()
            } else {
                dimensions
            });
            let context = Arc::new(Context {
                http: Http::new(&config)?,
                config,
                ollama_verified: self.context.ollama_verified.clone(),
            });
            let vector = Box::new(SymbolVector {
                inner: vector::create(context)?,
            });
            let _ = self.symbol_vector.set(vector);
        }
        Ok(self
            .symbol_vector
            .get()
            .expect("symbol vector initialized")
            .as_ref())
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

struct SymbolVector {
    inner: Box<dyn Vector>,
}

impl Vector for SymbolVector {
    fn profile(&self) -> Value {
        let mut profile = self.inner.profile();
        profile["symbolNormalizationVersion"] = json!(crate::symbols::NORMALIZATION_VERSION);
        profile
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn batch_limit(&self) -> usize {
        self.inner.batch_limit()
    }

    fn embed(&self, inputs: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        let normalized: Vec<_> = inputs
            .iter()
            .map(|s| crate::symbols::normalize(s))
            .collect();
        self.inner.embed(&normalized, query)
    }
}

impl Context {
    fn report_call(&self, kind: &str, provider: &str, model: &str) {
        report_call(&self.config, kind, provider, model);
    }
}

fn report_call(config: &Value, kind: &str, provider: &str, model: &str) {
    if should_report_call(
        REPORTED_CALLS.get_or_init(ReportedCalls::default),
        config["verbose"].as_bool() == Some(true),
        kind,
        provider,
        model,
    ) {
        crate::ui::info(format!(
            "slopdex: notice: external model call: kind={kind} provider={} model={}",
            json!(provider),
            json!(model),
        ));
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
            "openai" | "jina" | "cohere" | "opencode" | "opencode-go" | "ollama"
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
    resolve_api_key(
        config,
        operation_key,
        provider,
        |name| std::env::var(name).ok(),
        |provider| auth_path().and_then(|p| stored_key(&p, provider)),
    )
}

/// Credential lookup with injectable process state, so policy is testable
/// without mutating the environment of parallel tests.
fn resolve_api_key(
    config: &Value,
    operation_key: &str,
    provider: &str,
    environment: impl Fn(&str) -> Option<String>,
    stored: impl Fn(&str) -> Option<String>,
) -> Result<String> {
    let (setting, env) = match provider {
        "openai" => ("openaiApiKey", "OPENAI_API_KEY"),
        "jina" => ("jinaApiKey", "JINA_API_KEY"),
        "cohere" => ("cohereApiKey", "COHERE_API_KEY"),
        "opencode" | "opencode-go" => ("opencodeApiKey", "OPENCODE_API_KEY"),
        _ => bail!("Unsupported credential provider"),
    };
    // Keys written into the selected config file travel with its endpoints.
    if let Some(key) = string(config, &[operation_key, setting])? {
        return Ok(key.into());
    }
    if let Some(key) = environment(env).filter(|key| !key.trim().is_empty()) {
        guard_foreign_credential(config, operation_key, provider, env)?;
        return Ok(key.trim().into());
    }
    if provider.starts_with("opencode") {
        if let Some(key) = stored(provider) {
            guard_foreign_credential(config, operation_key, provider, "stored opencode/auth.json")?;
            return Ok(key);
        }
        bail!("OPENCODE_API_KEY or stored opencode/auth.json credentials are required");
    }
    bail!("{env} is required")
}

/// The base-URL setting and provider default behind an operation's credential.
fn operation_endpoint(operation_key: &str, provider: &str) -> Result<(&'static str, &'static str)> {
    Ok(match operation_key {
        "embeddingApiKey" => (
            "embeddingBaseUrl",
            if provider == "jina" { JINA } else { OPENAI },
        ),
        "descriptionApiKey" => (
            "descriptionBaseUrl",
            match provider {
                "opencode" => ZEN,
                "opencode-go" => GO,
                _ => OPENAI,
            },
        ),
        "rerankerApiKey" => (
            "rerankerBaseUrl",
            match provider {
                "cohere" => COHERE,
                "jina" => JINA,
                _ => OPENAI,
            },
        ),
        _ => bail!("Unsupported credential operation"),
    })
}

fn operation_name(operation_key: &str) -> &'static str {
    match operation_key {
        "embeddingApiKey" => "embedding",
        "descriptionApiKey" => "description",
        _ => "reranking",
    }
}

/// A cloned repository's `.slopdex/config.json` must not route the user's
/// environment or stored credentials to a remote host it chose. Keys that the
/// repository file supplies itself, loopback hosts, and the provider defaults
/// are exempt; the user config's `trustedEndpointHosts` or
/// `--trust-repo-endpoints` allow a specific host.
fn guard_foreign_credential(
    config: &Value,
    operation_key: &str,
    provider: &str,
    source: &str,
) -> Result<()> {
    if config[CONFIG_ORIGIN_KEY] != CONFIG_ORIGIN_REPOSITORY
        || config[TRUST_REPOSITORY_ENDPOINTS_KEY] == true
    {
        return Ok(());
    }
    let (key, default) = operation_endpoint(operation_key, provider)?;
    let Some(url) = remote_custom_endpoint(config, key, default)? else {
        return Ok(());
    };
    let host = host_label(&url);
    let bare = url.host_str().unwrap_or("");
    let trusted = config["trustedEndpointHosts"]
        .as_array()
        .is_some_and(|hosts| {
            hosts.iter().filter_map(Value::as_str).any(|entry| {
                let entry = entry.trim();
                entry.eq_ignore_ascii_case(&host) || entry.eq_ignore_ascii_case(bare)
            })
        });
    ensure!(
        trusted,
        "The repository config routes {} requests to {host}, but the {source} credential did not come from that file. \
         To allow this, add {host:?} to trustedEndpointHosts in your user config \
         (slopdex --config \"$XDG_CONFIG_HOME/slopdex/config.json\" config set trustedEndpointHosts '[\"{host}\"]'), \
         run with --trust-repo-endpoints, or set {operation_key} in the repository config",
        operation_name(operation_key)
    );
    Ok(())
}

fn is_loopback(url: &reqwest::Url) -> bool {
    let host = url.host_str().unwrap_or("").trim_matches(['[', ']']);
    host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn host_label(url: &reqwest::Url) -> String {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    }
}

/// The configured base URL for `key` when its scheme/host/port differ from the
/// provider default and the host is not loopback. Loopback proxies cannot
/// exfiltrate off this machine; path differences on the default host are routing.
fn remote_custom_endpoint(
    config: &Value,
    key: &str,
    default: &str,
) -> Result<Option<reqwest::Url>> {
    let Some(base) = string(config, &[key])? else {
        return Ok(None);
    };
    let url = validate_url(base)?;
    let standard = validate_url(default)?;
    let same = url.scheme() == standard.scheme()
        && url.host_str().map(str::to_ascii_lowercase)
            == standard.host_str().map(str::to_ascii_lowercase)
        && url.port_or_known_default() == standard.port_or_known_default();
    if same || is_loopback(&url) {
        return Ok(None);
    }
    Ok(Some(url))
}

/// Hosted operations pointed away from the provider default: require TLS off
/// loopback unless `allowInsecureEndpoints` opts in, and name the host once per
/// process so a repository-supplied endpoint is never silent.
fn check_hosted_endpoint(config: &Value, operation: &str, key: &str, default: &str) -> Result<()> {
    let Some(url) = remote_custom_endpoint(config, key, default)? else {
        return Ok(());
    };
    let host = host_label(&url);
    ensure!(
        url.scheme() == "https" || config["allowInsecureEndpoints"] == true,
        "{key} must use https for non-loopback host {host}; set allowInsecureEndpoints to true to permit plain HTTP"
    );
    let first = REPORTED_ENDPOINTS
        .get_or_init(ReportedEndpoints::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert((operation.to_owned(), host.clone()));
    if first {
        let origin = if config[CONFIG_ORIGIN_KEY] == CONFIG_ORIGIN_REPOSITORY {
            " (set by the repository config)"
        } else {
            ""
        };
        crate::ui::warning(format!(
            "slopdex: warning: {operation} requests use custom endpoint host {host}{origin}"
        ));
    }
    Ok(())
}

/// Identity form of an endpoint for profiles and status output: query strings
/// carry routing/tenant parameters, not model identity, and are never persisted.
pub(super) fn profile_endpoint(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut url) => {
            url.set_query(None);
            url.set_fragment(None);
            url.into()
        }
        Err(_) => url.to_owned(),
    }
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

pub(crate) fn session_id() -> String {
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
