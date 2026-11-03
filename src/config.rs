//! Typed configuration parsed once at the Engine and CLI boundaries.
//!
//! The on-disk JSON format is unchanged. [`Config`] keeps the canonical
//! effective JSON (`raw`) only as the documented identity input for
//! provider/cache profile hashes and search-cache keys, whose exact
//! serialization (unknown keys, null-vs-absent, number spelling) is part of
//! persisted identities. Every core option is also extracted into typed
//! fields at parse time; engine code consumes those fields instead of
//! repeated `config["..."]` reads. Mutations that must stay visible to
//! identity hashes go through [`Config::set`]/`refresh_typed`, which update
//! the raw JSON and the typed fields together.

use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde_json::{Value, json};

use crate::limits::{self, ResultLimit};

/// Default `maxFileSize` when the key is absent or not an unsigned integer,
/// matching the historical `as_u64().unwrap_or(1_048_576)` reads.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 1_048_576;
/// Default `faqThreshold` (strict-greater comparison lives in `engine::faq`).
pub const DEFAULT_FAQ_THRESHOLD: f64 = 0.7;
/// Default `rerankerCandidates` when absent or not an unsigned integer.
pub const DEFAULT_RERANKER_CANDIDATES: u64 = 10;

/// A configuration value plus its typed core options.
///
/// Construct with [`Config::from_value`] to validate at a boundary, or with
/// `Config::from(value)` for an unchecked parse whose validation happens
/// when an [`Engine`](crate::engine::Engine) opens (the engine constructors
/// accept `impl Into<Config>`, so existing `Value` call sites keep working).
#[derive(Clone)]
pub struct Config {
    /// Canonical effective JSON. Retained only as identity/profile input.
    raw: Value,
    max_file_size: u64,
    // `None` marks a non-string element: glob compilation must still report
    // "Glob must be a string" in element order, as the old Value-based
    // compilation did.
    include: Vec<Option<String>>,
    exclude: Vec<Option<String>>,
    parallelism: Option<u64>,
    embedding_parallelism: Option<u64>,
    embedding_is_ollama: bool,
    default_limit: ResultLimit,
    faq_threshold: Option<f64>,
    reranking_enabled: bool,
    reranker_candidates: u64,
    no_reindex: bool,
    force_reindex: bool,
    verbose: bool,
    artifact_cache_path: Option<PathBuf>,
}

impl std::fmt::Debug for Config {
    // The raw JSON can carry API keys; never render it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("max_file_size", &self.max_file_size)
            .field("include", &self.include)
            .field("exclude", &self.exclude)
            .field("parallelism", &self.parallelism)
            .field("embedding_parallelism", &self.embedding_parallelism)
            .field("embedding_is_ollama", &self.embedding_is_ollama)
            .field("faq_threshold", &self.faq_threshold)
            .field("reranking_enabled", &self.reranking_enabled)
            .field("reranker_candidates", &self.reranker_candidates)
            .field("no_reindex", &self.no_reindex)
            .field("force_reindex", &self.force_reindex)
            .field("verbose", &self.verbose)
            .field("artifact_cache_path", &self.artifact_cache_path)
            .finish_non_exhaustive()
    }
}

fn patterns(raw: &Value, key: &str) -> Vec<Option<String>> {
    // Matches discovery::globs: a non-array selects nothing, while a
    // non-string element is an error reported when globs are compiled.
    let Some(values) = raw.get(key).and_then(Value::as_array) else {
        return Vec::new();
    };
    values
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

fn flag(raw: &Value, key: &str) -> bool {
    raw.get(key).and_then(Value::as_bool).unwrap_or(false)
}

impl Config {
    fn parse_typed(raw: &Value) -> Self {
        let include = patterns(raw, "include");
        let exclude = patterns(raw, "exclude");
        Self {
            raw: raw.clone(),
            max_file_size: raw
                .get("maxFileSize")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_MAX_FILE_SIZE),
            include,
            exclude,
            parallelism: raw.get("parallelism").and_then(Value::as_u64),
            embedding_parallelism: raw.get("embeddingParallelism").and_then(Value::as_u64),
            embedding_is_ollama: raw.get("provider") == Some(&json!("ollama"))
                || raw.get("embeddingProvider") == Some(&json!("ollama")),
            // An invalid defaultLimit is rejected by validate(); the lenient
            // fallback only matters for unchecked From<Value> parses that an
            // Engine open validates before any use.
            default_limit: raw
                .get("defaultLimit")
                .and_then(|value| ResultLimit::from_value(value).ok())
                .unwrap_or(ResultLimit::Count(limits::DEFAULT_LIMIT)),
            faq_threshold: raw
                .get("faqThreshold")
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite() && (-1.0..=1.0).contains(value)),
            reranking_enabled: flag(raw, "rerankingEnabled"),
            reranker_candidates: raw
                .get("rerankerCandidates")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_RERANKER_CANDIDATES),
            no_reindex: flag(raw, "noReindex"),
            force_reindex: flag(raw, "forceReindex"),
            verbose: flag(raw, "verbose"),
            artifact_cache_path: raw
                .get("artifactCachePath")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
        }
    }

    /// Parse and validate a configuration value at a boundary. Error strings
    /// and order match the previous Engine-open validation exactly: numeric
    /// limits (including `defaultLimit`) first, then `faqThreshold`.
    pub fn from_value(raw: Value) -> Result<Self> {
        let config = Self::parse_typed(&raw);
        config.validate()?;
        Ok(config)
    }

    /// The validation previously open-coded in `Engine::open_internal`.
    pub fn validate(&self) -> Result<()> {
        limits::validate_config(&self.raw)?;
        // Same check (and message) as engine::faq::threshold on config only.
        if let Some(value) = self.raw.get("faqThreshold") {
            value
                .as_f64()
                .filter(|number| number.is_finite() && (-1.0..=1.0).contains(number))
                .context("faqThreshold must be a finite number between -1 and 1")?;
        }
        Ok(())
    }

    /// Canonical JSON, retained only as identity/profile input (provider and
    /// cache profiles, search-cache keys, redacted rendering).
    pub fn raw(&self) -> &Value {
        &self.raw
    }

    /// Consume the config, returning its canonical JSON unchanged.
    pub fn into_raw(self) -> Value {
        self.raw
    }

    pub(crate) fn raw_mut(&mut self) -> &mut Value {
        &mut self.raw
    }

    /// Re-extract typed fields after a raw mutation (e.g. the provider
    /// layer's `resolve_embeddings`, which rewrites model/dimension keys).
    pub(crate) fn refresh_typed(&mut self) {
        let parsed = Self::parse_typed(&self.raw);
        *self = parsed;
    }

    /// Write one key to the raw JSON and the typed view together.
    pub(crate) fn set(&mut self, key: &str, value: Value) {
        self.raw[key] = value;
        self.refresh_typed();
    }

    pub fn max_file_size(&self) -> u64 {
        self.max_file_size
    }

    pub fn include_patterns(&self) -> Vec<&str> {
        self.include.iter().flatten().map(String::as_str).collect()
    }

    pub fn exclude_patterns(&self) -> Vec<&str> {
        self.exclude.iter().flatten().map(String::as_str).collect()
    }

    fn glob_set(patterns: &[Option<String>]) -> Result<GlobSet> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let pattern = pattern.as_deref().context("Glob must be a string")?;
            builder.add(Glob::new(pattern)?);
        }
        Ok(builder.build()?)
    }

    pub(crate) fn include_globs(&self) -> Result<GlobSet> {
        Self::glob_set(&self.include)
    }

    pub(crate) fn exclude_globs(&self) -> Result<GlobSet> {
        Self::glob_set(&self.exclude)
    }

    /// File-preparation parallelism: configured value or 10, clamped to the
    /// same 256 cap the engine always applied.
    pub fn parallelism(&self) -> Result<usize> {
        let parallelism = self.parallelism.unwrap_or(10) as usize;
        ensure!(parallelism > 0, "parallelism must be positive");
        Ok(parallelism.min(256))
    }

    /// Embedding parallelism: `embeddingParallelism`, else `parallelism`,
    /// else 1 for Ollama providers (either spelling) and 10 otherwise.
    pub fn embedding_parallelism(&self) -> Result<usize> {
        let default = if self.embedding_is_ollama { 1 } else { 10 };
        let value = self
            .embedding_parallelism
            .or(self.parallelism)
            .unwrap_or(default) as usize;
        ensure!(value > 0, "embeddingParallelism must be positive");
        Ok(value.min(256))
    }

    pub fn embedding_is_ollama(&self) -> bool {
        self.embedding_is_ollama
    }

    /// Resolve an output limit: per-call `limit` option wins, then the
    /// configured `defaultLimit`, then the built-in default.
    pub(crate) fn resolve_limit(&self, options: &Value) -> Result<ResultLimit> {
        match options.get("limit") {
            Some(value) => ResultLimit::from_value(value),
            None => Ok(self.default_limit),
        }
    }

    /// FAQ cutoff: per-call `faqThreshold` option wins over the configured
    /// value; both are validated with the historical error message. The
    /// strict-greater-than comparison lives in the FAQ search itself.
    pub fn faq_threshold(&self, options: &Value) -> Result<f64> {
        if let Some(value) = options.get("faqThreshold") {
            return value
                .as_f64()
                .filter(|number| number.is_finite() && (-1.0..=1.0).contains(number))
                .context("faqThreshold must be a finite number between -1 and 1");
        }
        if self.raw.get("faqThreshold").is_some() && self.faq_threshold.is_none() {
            bail!("faqThreshold must be a finite number between -1 and 1");
        }
        Ok(self.faq_threshold.unwrap_or(DEFAULT_FAQ_THRESHOLD))
    }

    pub fn reranking_enabled(&self) -> bool {
        self.reranking_enabled
    }

    pub fn reranker_candidates(&self) -> u64 {
        self.reranker_candidates
    }

    pub fn no_reindex(&self) -> bool {
        self.no_reindex
    }

    pub fn force_reindex(&self) -> bool {
        self.force_reindex
    }

    pub fn verbose(&self) -> bool {
        self.verbose
    }

    pub fn artifact_cache_path(&self) -> Option<&PathBuf> {
        self.artifact_cache_path.as_ref()
    }

    /// Validated `crossSearchExclusions`, checked lazily at cross-search
    /// time exactly as before (same error strings via the shared validator).
    pub fn cross_search_exclusions(&self) -> Result<std::collections::BTreeSet<&str>> {
        crate::engine::cross_search_exclusions(&self.raw)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::parse_typed(&json!({}))
    }
}

impl From<Value> for Config {
    /// Unchecked parse; validation runs at `Engine` open or `validate()`.
    fn from(raw: Value) -> Self {
        Self::parse_typed(&raw)
    }
}

impl From<Config> for Value {
    fn from(config: Config) -> Self {
        config.raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_historical_value_reads() -> Result<()> {
        let config = Config::from_value(json!({}))?;
        assert_eq!(config.max_file_size(), 1_048_576);
        assert_eq!(config.parallelism()?, 10);
        assert_eq!(config.embedding_parallelism()?, 10);
        assert_eq!(config.faq_threshold(&json!({}))?, 0.7);
        assert_eq!(config.reranker_candidates(), 10);
        assert!(!config.reranking_enabled() && !config.no_reindex() && !config.force_reindex());
        assert!(config.artifact_cache_path().is_none());
        assert_eq!(config.resolve_limit(&json!({}))?.count(), Some(4096));
        Ok(())
    }

    #[test]
    fn typed_fields_parse_core_options_and_aliases() -> Result<()> {
        let config = Config::from_value(json!({
            "maxFileSize": 2048, "include": ["src/**"], "exclude": ["gen/**"],
            "parallelism": 4, "embeddingProvider": "ollama", "defaultLimit": "none",
            "faqThreshold": 0.25, "rerankingEnabled": true, "rerankerCandidates": 33,
            "artifactCachePath": "/tmp/store.sqlite",
        }))?;
        assert_eq!(config.max_file_size(), 2048);
        assert_eq!(config.include_patterns(), ["src/**"]);
        assert_eq!(config.exclude_patterns(), ["gen/**"]);
        assert!(config.include_globs()?.is_match("src/lib.rs"));
        assert_eq!(config.parallelism()?, 4);
        // embeddingParallelism falls back to parallelism before the Ollama default.
        assert_eq!(config.embedding_parallelism()?, 4);
        assert!(config.embedding_is_ollama());
        assert_eq!(config.faq_threshold(&json!({}))?, 0.25);
        assert_eq!(config.faq_threshold(&json!({"faqThreshold": -0.5}))?, -0.5);
        assert!(config.reranking_enabled());
        assert_eq!(config.reranker_candidates(), 33);
        assert_eq!(
            config.artifact_cache_path(),
            Some(&PathBuf::from("/tmp/store.sqlite"))
        );
        assert_eq!(config.resolve_limit(&json!({}))?.count(), None);
        assert_eq!(config.resolve_limit(&json!({"limit": 3}))?.count(), Some(3));
        Ok(())
    }

    #[test]
    fn ollama_embedding_parallelism_defaults_to_one() -> Result<()> {
        let config = Config::from_value(json!({"provider": "ollama"}))?;
        assert_eq!(config.embedding_parallelism()?, 1);
        assert_eq!(config.parallelism()?, 10);
        Ok(())
    }

    #[test]
    fn validation_preserves_engine_error_strings_and_order() {
        for (value, message) in [
            (
                json!({"maxFileSize": null}),
                "maxFileSize must be an integer between 1 and 67108864",
            ),
            (
                json!({"defaultLimit": 0}),
                "defaultLimit must be a positive integer or \"none\"",
            ),
            (
                json!({"faqThreshold": 2.0}),
                "faqThreshold must be a finite number between -1 and 1",
            ),
            (
                json!({"faqThreshold": "x"}),
                "faqThreshold must be a finite number between -1 and 1",
            ),
        ] {
            let error = Config::from_value(value).unwrap_err();
            assert!(format!("{error:#}").contains(message), "{error:#}");
        }
        // Numeric validation runs before the FAQ check, as Engine open did.
        let error = Config::from_value(json!({"parallelism": 0, "faqThreshold": 9.0})).unwrap_err();
        assert!(format!("{error:#}").contains("parallelism must be an integer between 1 and 256"));
        // HTTP retry null-means-unset contract is untouched.
        assert!(Config::from_value(json!({"providerMaxRetries": null})).is_ok());
    }

    #[test]
    fn raw_json_is_preserved_byte_for_byte_for_identity() -> Result<()> {
        let raw = json!({"unknownKey": [1, null, "x"], "agentModel": null, "dimensions": 1024});
        let config = Config::from_value(raw.clone())?;
        assert_eq!(config.raw().to_string(), raw.to_string());
        assert_eq!(Value::from(config), raw);
        Ok(())
    }

    #[test]
    fn set_updates_raw_and_typed_views_together() -> Result<()> {
        let mut config = Config::from_value(json!({}))?;
        config.set("artifactCachePath", json!("/tmp/a.sqlite"));
        config.set("maxFileSize", json!(99));
        assert_eq!(config.raw()["artifactCachePath"], "/tmp/a.sqlite");
        assert_eq!(
            config.artifact_cache_path(),
            Some(&PathBuf::from("/tmp/a.sqlite"))
        );
        assert_eq!(config.max_file_size(), 99);
        Ok(())
    }

    #[test]
    fn glob_errors_keep_legacy_messages_and_debug_redacts_raw() {
        let config = Config::from(json!({"include": [1], "openaiApiKey": "secret-value"}));
        let error = config.include_globs().unwrap_err();
        assert!(format!("{error:#}").contains("Glob must be a string"));
        assert!(!format!("{config:?}").contains("secret-value"));
    }
}
