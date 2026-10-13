//! Configuration loading, validation, persistence, and command handling.

use super::args::{Format, Global};
use super::io::print_json;
use super::workspace::{absolute, root_path, user_config_path};
use crate::providers::{
    CONFIG_ORIGIN_KEY, CONFIG_ORIGIN_REPOSITORY, TRUST_REPOSITORY_ENDPOINTS_KEY,
};
use crate::{engine::cross_search_exclusions, parse, ui};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

mod wizard;

/// Documented settings beyond the interactive wizard's keys. `config set` accepts
/// exactly these, the wizard keys, documented aliases, and dotted `artifactS3.*`.
const ADVANCED_KEYS: &[&str] = &[
    "embeddingProvider",
    "embeddingModel",
    "embeddingDimensions",
    "fallbackModel",
    "embeddingBaseUrl",
    "descriptionBaseUrl",
    "rerankerBaseUrl",
    "embeddingApiKey",
    "descriptionApiKey",
    "rerankerApiKey",
    "openaiApiKey",
    "jinaApiKey",
    "cohereApiKey",
    "opencodeApiKey",
    "artifactCachePath",
    "artifactS3",
    "symbolDimensions",
    "jinaTask",
    "providerTimeoutMs",
    "providerMaxRetries",
    "retryDelayMs",
    "ollamaAutoPull",
    "ollamaKeepAlive",
    "ollamaModelDigest",
    "ollamaNativeDimensions",
    "ollamaContextLength",
    "ollamaPullTimeoutMs",
    "crossSearchExclusions",
    "trustedEndpointHosts",
    "allowInsecureEndpoints",
];

fn is_secret_key(key: &str) -> bool {
    key.ends_with("ApiKey")
}

/// Configuration as every CLI rendering shows it: credential values are masked.
pub(super) fn redacted(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    let shown = if is_secret_key(key) && !value.is_null() {
                        json!("<redacted>")
                    } else {
                        redacted(value)
                    };
                    (key.clone(), shown)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redacted).collect()),
        other => other.clone(),
    }
}

fn contains_secret(config: &Value) -> bool {
    config.as_object().is_some_and(|object| {
        object
            .iter()
            .any(|(key, value)| is_secret_key(key) && !value.is_null())
    })
}

fn known_top_level_key(key: &str) -> bool {
    wizard::CONFIG_KEYS.contains(&key) || ADVANCED_KEYS.contains(&key)
}

fn edit_distance(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.to_ascii_lowercase().chars().collect();
    let right: Vec<char> = right.to_ascii_lowercase().chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (i, l) in left.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, r) in right.iter().enumerate() {
            let substitution = previous[j] + usize::from(l != r);
            current.push(substitution.min(previous[j + 1] + 1).min(current[j] + 1));
        }
        previous = current;
    }
    previous[right.len()]
}

fn closest_keys(key: &str) -> Vec<&'static str> {
    let lowered = key.to_ascii_lowercase();
    let mut candidates: Vec<(usize, &str)> = wizard::CONFIG_KEYS
        .iter()
        .chain(ADVANCED_KEYS)
        .copied()
        .filter_map(|candidate| {
            let distance = edit_distance(key, candidate);
            let related = candidate.to_ascii_lowercase().contains(&lowered)
                || lowered.contains(&candidate.to_ascii_lowercase());
            (distance <= 3 || related).then_some((distance, candidate))
        })
        .collect();
    candidates.sort();
    candidates.into_iter().take(3).map(|(_, key)| key).collect()
}

fn check_settable_key(key: &str) -> Result<()> {
    let top = key.split('.').next().unwrap_or(key);
    ensure!(
        top != "descriptionsEnabled",
        "descriptionsEnabled has been removed; remove it from config and run `slopdex generate descriptions` to generate descriptions explicitly"
    );
    if !known_top_level_key(top) {
        let suggestions = closest_keys(top);
        let hint = if suggestions.is_empty() {
            String::new()
        } else {
            format!("; did you mean {}?", suggestions.join(", "))
        };
        anyhow::bail!(
            "unknown configuration key {top}{hint} (see docs/reference.md for documented settings)"
        );
    }
    ensure!(
        !key.contains('.') || top == "artifactS3",
        "{top} does not take nested keys; set {top} to a JSON value instead"
    );
    Ok(())
}

/// Mark settings loaded from `<root>/.slopdex/config.json` so provider policy can
/// refuse to send the user's own credentials to repository-chosen endpoints. Only
/// the user config may grant trust; a repository's own list is discarded.
pub(super) fn mark_repository_config(config: &mut Value, global: &Global) -> Result<()> {
    mark_repository_config_from(config, global, &user_config_path()?)
}

fn mark_repository_config_from(config: &mut Value, global: &Global, user: &Path) -> Result<()> {
    let object = config.as_object_mut().context("config must be an object")?;
    object.insert(CONFIG_ORIGIN_KEY.into(), json!(CONFIG_ORIGIN_REPOSITORY));
    object.remove("trustedEndpointHosts");
    object.remove(TRUST_REPOSITORY_ENDPOINTS_KEY);
    if global.trust_repo_endpoints {
        object.insert(TRUST_REPOSITORY_ENDPOINTS_KEY.into(), json!(true));
    }
    let user = read_config(user).with_context(|| format!("read user config {}", user.display()))?;
    if let Some(hosts) = user.get("trustedEndpointHosts") {
        validate_trusted_hosts(hosts)?;
        config["trustedEndpointHosts"] = hosts.clone();
    }
    Ok(())
}

fn validate_trusted_hosts(value: &Value) -> Result<()> {
    ensure!(
        value.as_array().is_some_and(|hosts| hosts
            .iter()
            .all(|host| host.as_str().is_some_and(|s| !s.trim().is_empty()))),
        "trustedEndpointHosts must be an array of non-empty host strings"
    );
    Ok(())
}

fn read_config(path: &Path) -> Result<Value> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(json!({})),
        Err(error) => return Err(error).with_context(|| format!("read config {}", path.display())),
    };
    let mut value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse config {}", path.display()))?;
    ensure!(
        value.is_object(),
        "config must contain a JSON object: {}",
        path.display()
    );
    normalize_config_aliases(&mut value);
    Ok(value)
}

fn normalize_config_aliases(config: &mut Value) {
    let object = config.as_object_mut().expect("config must be an object");
    for (alias, canonical) in [
        ("embeddingProvider", "provider"),
        ("embeddingModel", "model"),
        ("embeddingDimensions", "dimensions"),
        ("fallbackModel", "descriptionFallbackModel"),
    ] {
        if let Some(value) = object.remove(alias).filter(|value| !value.is_null())
            && object.get(canonical).is_none_or(Value::is_null)
        {
            object.insert(canonical.into(), value);
        }
    }
}

pub(super) fn effective_config(global: &Global, path: &Path) -> Result<Value> {
    let mut config = read_config(path)?;
    apply_embedding_overrides(&mut config, global);
    for (key, value) in [
        ("descriptionProvider", &global.description_provider),
        ("descriptionModel", &global.description_model),
        (
            "descriptionFallbackModel",
            &global.description_fallback_model,
        ),
    ] {
        if let Some(value) = value {
            config[key] = json!(value);
        }
    }
    if let Some(value) = global.reranker_candidates {
        config["rerankerCandidates"] = json!(value);
    }
    if let Some(value) = &global.index {
        config["indexPath"] = json!(absolute(value)?);
    }
    if global.verbose {
        config["verbose"] = json!(true);
    }
    config["noReindex"] = json!(global.no_reindex);
    config["forceReindex"] = json!(global.force_reindex);
    config["ignoreErrors"] = json!(global.ignore_errors);
    config["rebuildOnDivergence"] = json!(global.rebuild_on_divergence);
    validate_config(&config)?;
    Ok(config)
}

fn embedding_defaults(provider: &str) -> (&'static str, u64) {
    match provider {
        "jina" => ("jina-embeddings-v4", 1024),
        "ollama" => (
            crate::providers::OLLAMA_DEFAULT_MODEL,
            crate::providers::ollama_model_dimensions(crate::providers::OLLAMA_DEFAULT_MODEL)
                .expect("default Ollama model has known dimensions") as u64,
        ),
        _ => ("text-embedding-3-large", 3072),
    }
}

fn known_dimensions(provider: &str, model: &str) -> Option<u64> {
    if provider == "ollama" {
        return crate::providers::ollama_model_dimensions(model)
            .map(|dimensions| dimensions as u64);
    }
    match (provider, model) {
        ("openai", "text-embedding-3-small") => Some(1536),
        ("openai", "text-embedding-3-large") => Some(3072),
        ("openai", "text-embedding-ada-002") => Some(1536),
        ("jina", "jina-embeddings-v4") => Some(1024),
        _ => None,
    }
}

fn select_embeddings(config: &mut Value, provider: Option<&str>, model: Option<&str>) {
    let previous = config["provider"].as_str().unwrap_or("openai");
    let provider_changed = provider.is_some_and(|provider| provider != previous);
    if provider_changed {
        let object = config.as_object_mut().unwrap();
        // Credentials (`embeddingApiKey`) survive a provider switch; only
        // provider-shaped settings and resolved identity are reset.
        for key in [
            "model",
            "dimensions",
            "embeddingBaseUrl",
            "ollamaAutoPull",
            "ollamaKeepAlive",
            "embeddingParallelism",
            "ollamaPullTimeoutMs",
        ] {
            object.remove(key);
        }
    }
    let model_changed = model.is_some_and(|model| config["model"].as_str() != Some(model));
    if provider_changed || model_changed {
        let object = config.as_object_mut().unwrap();
        for key in [
            "dimensions",
            "ollamaModelDigest",
            "ollamaNativeDimensions",
            "ollamaModelFamily",
            "ollamaContextLength",
        ] {
            object.remove(key);
        }
    }
    if let Some(provider) = provider {
        config["provider"] = json!(provider);
        if provider_changed {
            config["model"] = json!(embedding_defaults(provider).0);
        }
    }
    if let Some(model) = model {
        config["model"] = json!(model);
    }
    if (provider_changed || model_changed)
        && let Some(dimensions) = known_dimensions(
            config["provider"].as_str().unwrap_or("openai"),
            config["model"].as_str().unwrap_or(""),
        )
    {
        config["dimensions"] = json!(dimensions);
    }
}

fn apply_embedding_overrides(config: &mut Value, global: &Global) {
    select_embeddings(config, global.provider.as_deref(), global.model.as_deref());
    if let Some(dimensions) = global.dimensions {
        config["dimensions"] = json!(dimensions);
    }
}

fn validate_config(config: &Value) -> Result<()> {
    ensure!(config.is_object(), "config must be a JSON object");
    crate::limits::validate_config(config)?;
    cross_search_exclusions(config)?;
    ensure!(
        config.get("descriptionsEnabled").is_none(),
        "descriptionsEnabled has been removed; remove it from config and run `slopdex generate descriptions` to generate descriptions explicitly"
    );
    for (key, allowed) in [
        ("provider", &["openai", "jina", "ollama"][..]),
        (
            "descriptionProvider",
            &["openai", "opencode", "opencode-go"][..],
        ),
    ] {
        if let Some(value) = config.get(key) {
            ensure!(
                value.as_str().is_some_and(|value| allowed.contains(&value)),
                "unsupported {key}: {value}"
            );
        }
    }
    for key in [
        "model",
        "indexPath",
        "artifactCachePath",
        "descriptionModel",
        "descriptionFallbackModel",
        "ollamaKeepAlive",
        "ollamaModelDigest",
        "jinaTask",
    ] {
        if let Some(value) = config.get(key) {
            ensure!(
                value.as_str().is_some_and(|s| !s.trim().is_empty()),
                "{key} must be a non-empty string"
            );
        }
    }
    if let Some(hosts) = config.get("trustedEndpointHosts") {
        validate_trusted_hosts(hosts)?;
    }
    for key in [
        "dimensions",
        "maxFileSize",
        "embeddingBatchSize",
        "parallelism",
        "embeddingParallelism",
        "ollamaNativeDimensions",
        "ollamaContextLength",
        "ollamaPullTimeoutMs",
    ] {
        if let Some(value) = config.get(key) {
            ensure!(
                value.as_u64().is_some_and(|n| n > 0),
                "{key} must be a positive integer"
            );
        }
    }
    for key in [
        "rerankingEnabled",
        "verbose",
        "ollamaAutoPull",
        "allowInsecureEndpoints",
    ] {
        if let Some(value) = config.get(key) {
            ensure!(value.is_boolean(), "{key} must be a boolean");
        }
    }
    for key in ["include", "exclude"] {
        if let Some(value) = config.get(key) {
            ensure!(
                value
                    .as_array()
                    .is_some_and(|a| a.iter().all(Value::is_string)),
                "{key} must be an array of strings"
            );
            for pattern in value.as_array().unwrap() {
                globset::Glob::new(pattern.as_str().unwrap())
                    .with_context(|| format!("invalid {key} glob {pattern}"))?;
            }
        }
    }
    if config["rerankingEnabled"] == true {
        ensure!(
            matches!(
                config["rerankerProvider"].as_str(),
                Some("cohere" | "jina" | "openai")
            ),
            "unsupported rerankerProvider"
        );
        if let Some(model) = config.get("rerankerModel") {
            ensure!(
                model.as_str().is_some_and(|s| !s.trim().is_empty()),
                "rerankerModel must be a non-empty string"
            );
        }
        if config["rerankerProvider"] == "openai"
            && let Some(count) = config.get("rerankerCandidates")
        {
            ensure!(
                count.as_u64().is_some_and(|n| (1..=100).contains(&n)),
                "rerankerCandidates must be between 1 and 100"
            );
        }
    }
    if let Some(s3) = config.get("artifactS3") {
        ensure!(s3.is_object(), "artifactS3 must be an object");
        for key in ["bucket", "endpoint", "region", "prefix"] {
            if let Some(value) = s3.get(key) {
                ensure!(
                    value.as_str().is_some_and(|text| !text.trim().is_empty()),
                    "artifactS3.{key} must be a non-empty string"
                );
            }
        }
        ensure!(s3["bucket"].is_string(), "artifactS3.bucket is required");
        if let Some(value) = s3.get("pathStyle") {
            ensure!(value.is_boolean(), "artifactS3.pathStyle must be a boolean");
        }
    }
    Ok(())
}

/// The replacement file's permission bits: an existing file keeps its mode, and a
/// file holding any `*ApiKey` is owner-only (never wider than it already was).
/// `None` leaves a brand-new, secret-free file to the process umask.
#[cfg(unix)]
fn config_mode(path: &Path, config: &Value) -> Result<Option<u32>> {
    use std::os::unix::fs::PermissionsExt;
    let existing = match fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions().mode() & 0o777),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    Ok(if contains_secret(config) {
        Some(existing.map_or(0o600, |mode| mode & 0o600))
    } else {
        existing
    })
}

fn write_config(path: &Path, config: &Value) -> Result<()> {
    validate_config(config)?;
    let parent = path.parent().context("config path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .context("config path has no filename")?
            .to_string_lossy(),
        std::process::id()
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    let mode = config_mode(path, config)?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
    }
    let mut file = options
        .open(&temp)
        .with_context(|| format!("create temporary config {}", temp.display()))?;
    let result = (|| -> Result<()> {
        // The creation mode is subject to umask; apply the exact mode explicitly.
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        serde_json::to_writer_pretty(&mut file, config)?;
        writeln!(file)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("save config {}", path.display()))
}

fn exclude_cross_search(global: &Global, config: &mut Value, args: &[String]) -> Result<Value> {
    ensure!(
        args.len() >= 2,
        "usage: slopdex config exclude-cross-search <file>:<symbol> [<file>:<symbol> ...]"
    );
    validate_config(config)?;
    let root = root_path(global.root.as_deref())?;
    let mut hashes = config
        .get("crossSearchExclusions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let previous_count = hashes.len();
    for selector in &args[1..] {
        // Skip a Windows drive prefix when finding the file/symbol separator.
        let start = if selector.as_bytes().get(1) == Some(&b':')
            && matches!(selector.as_bytes().get(2), Some(b'/' | b'\\'))
        {
            2
        } else {
            0
        };
        let separator = selector[start..]
            .find(':')
            .map(|index| index + start)
            .context("expected <file>:<symbol> for cross-search exclusion")?;
        let (file, symbol) = (&selector[..separator], &selector[separator + 1..]);
        ensure!(
            !file.is_empty() && !symbol.is_empty(),
            "expected <file>:<symbol> for cross-search exclusion"
        );
        let path = root.join(file);
        let source =
            fs::read_to_string(&path).with_context(|| format!("read source {}", path.display()))?;
        let parsed = parse::parse(file, &source)?;
        let qualified = parsed
            .callables
            .iter()
            .any(|callable| callable.qualified_name == symbol);
        let matches: Vec<_> = parsed
            .callables
            .iter()
            .filter(|callable| {
                if qualified {
                    callable.qualified_name == symbol
                } else {
                    callable.name == symbol
                }
            })
            .collect();
        ensure!(
            !matches.is_empty(),
            "no callable symbol found for {selector}"
        );
        for callable in matches {
            let hash = json!(callable.source_hash);
            if !hashes.contains(&hash) {
                hashes.push(hash);
            }
        }
    }
    config["crossSearchExclusions"] = json!(hashes);
    Ok(json!({"total": hashes.len(), "added": hashes.len() - previous_count}))
}

fn embedding_setup_interactive(global: &Global) -> bool {
    global.provider.is_none() && global.model.is_none() && global.dimensions.is_none()
}

// Both wizard responses and preparation are injected in tests: no model download,
// server access, index open, or per-user artifact store is needed to test staging.
fn stage_embeddings(
    saved: &Value,
    global: &Global,
    configure: impl FnOnce(&mut Value) -> Result<()>,
    prepare: impl FnOnce(&mut Value, bool) -> Result<()>,
) -> Result<Value> {
    let mut config = saved.clone();
    normalize_config_aliases(&mut config);
    apply_embedding_overrides(&mut config, global);
    if embedding_setup_interactive(global) {
        configure(&mut config)?;
    }
    if config["provider"].is_null() {
        config["provider"] = json!("openai");
    }
    if config["model"].is_null() {
        let provider = config["provider"]
            .as_str()
            .context("provider must be a string")?;
        config["model"] = json!(embedding_defaults(provider).0);
    }
    ensure!(
        !global.pull || config["provider"] == "ollama",
        "--pull is only available for the ollama embedding provider"
    );
    if global.pull {
        config["ollamaAutoPull"] = json!(true);
    }
    validate_config(&config)?;
    if config["provider"] == "ollama" {
        prepare(&mut config, global.pull)?;
    }
    validate_config(&config)?;
    Ok(config)
}

fn save_embeddings(
    global: &Global,
    path: &Path,
    configure: impl FnOnce(&mut Value) -> Result<()>,
    prepare: impl FnOnce(&mut Value, bool) -> Result<()>,
) -> Result<()> {
    let config = stage_embeddings(&read_config(path)?, global, configure, prepare)?;
    write_config(path, &config)
}

fn cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::Interrupted | io::ErrorKind::UnexpectedEof
        )
    })
}

/// Save embedding-only setup before an update, or return None after cancellation.
pub(super) fn setup_embeddings(
    global: &Global,
    root: &Path,
    selected: &Path,
) -> Result<Option<PathBuf>> {
    let interactive = embedding_setup_interactive(global);
    if interactive {
        ensure!(
            io::stdin().is_terminal() && ui::terminal(),
            "interactive embedding setup requires a terminal; pass --provider and --model for noninteractive setup"
        );
        cliclack::intro("slopdex embedding setup")?;
    }
    let result = (|| {
        let path = if interactive && global.config.is_none() {
            wizard::setup_destination(selected, &root.join(".slopdex/config.json"))?
        } else {
            selected.to_owned()
        };
        save_embeddings(
            global,
            &path,
            wizard::configure_embeddings,
            |config, pull| {
                ui::spin("Preparing embeddings", || {
                    crate::providers::prepare_embeddings(config, pull)
                })
            },
        )?;
        Ok(path)
    })();
    match result {
        Ok(path) => {
            if interactive {
                cliclack::outro(format!("Updated {}", path.display()))?;
            }
            Ok(Some(path))
        }
        Err(error) if interactive && cancelled(&error) => {
            cliclack::outro_cancel("Embedding setup cancelled; no settings saved")?;
            Ok(None)
        }
        Err(error) => {
            if interactive {
                cliclack::outro_cancel("Embedding setup failed; no settings saved")?;
            }
            Err(error)
        }
    }
}

pub(super) fn run_config(
    global: &Global,
    path: &Path,
    args: &[String],
    out: &mut impl Write,
) -> Result<()> {
    if args.first().is_some_and(|arg| arg == "embeddings") {
        ensure!(
            args.len() == 1,
            "usage: slopdex config embeddings [--provider <provider>] [--model <model>] [--dimensions <count>] [--pull]"
        );
        let root = root_path(global.root.as_deref())?;
        if let Some(path) = setup_embeddings(global, &root, path)? {
            if global.format == Some(Format::Json) {
                let mut config = redacted(&read_config(&path)?);
                config["configPath"] = json!(path);
                print_json(out, &config)?;
            } else if !embedding_setup_interactive(global) {
                writeln!(out, "Updated {}: embedding settings", path.display())?;
            }
        }
        return Ok(());
    }
    let mut config = read_config(path)?;
    let action = args.first().map(String::as_str);
    let interactive = !matches!(action, Some("set" | "exclude-cross-search"));
    let changed = if action == Some("exclude-cross-search") {
        exclude_cross_search(global, &mut config, args)?
    } else if !interactive {
        ensure!(args.len() == 3, "usage: slopdex config set <key> <value>");
        let key = &args[1];
        ensure!(
            key.split('.').all(|part| !part.is_empty()),
            "config set expects a non-empty key path"
        );
        check_settable_key(key)?;
        let value = serde_json::from_str::<Value>(&args[2]).unwrap_or_else(|_| json!(args[2]));
        if let Some(value) = value.as_str() {
            match key.as_str() {
                "provider" => select_embeddings(&mut config, Some(value), None),
                "model" => select_embeddings(&mut config, None, Some(value)),
                _ => {}
            }
        }
        let mut target = &mut config;
        let mut parts = key.split('.').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                target[part] = value.clone();
            } else {
                if target.get(part).is_none() {
                    target[part] = json!({});
                }
                ensure!(
                    target[part].is_object(),
                    "{part} must be an object to set {key}"
                );
                target = &mut target[part];
            }
        }
        json!({key: value})
    } else {
        ensure!(args.len() <= 1, "usage: slopdex config [prefix]");
        let prefix = args.first().map(String::as_str);
        ensure!(
            prefix.is_none_or(|prefix| wizard::CONFIG_KEYS
                .iter()
                .any(|key| key.starts_with(prefix))),
            "no configuration keys start with {}",
            prefix.unwrap_or_default()
        );
        ensure!(
            io::stdin().is_terminal() && ui::terminal(),
            "interactive config requires a terminal"
        );
        cliclack::intro("slopdex configuration")?;
        let configured = wizard::configure(&mut config, prefix).and_then(|()| {
            if config["provider"] == "ollama"
                && prefix.is_none_or(|prefix| {
                    ["provider", "model", "dimensions"]
                        .iter()
                        .any(|key| key.starts_with(prefix))
                })
            {
                crate::providers::prepare_embeddings(&mut config, false)?;
            }
            Ok(())
        });
        if let Err(error) = configured {
            // cliclack 0.5.6 returns Interrupted for Esc/Ctrl-C; it does not exit.
            if cancelled(&error) {
                cliclack::outro_cancel("Configuration cancelled; no settings saved")?;
                return Ok(());
            }
            cliclack::outro_cancel("Configuration failed; no settings saved")?;
            return Err(error);
        }
        config.clone()
    };
    write_config(path, &config)?;
    if interactive {
        let summary = [
            "descriptionProvider",
            "descriptionModel",
            "descriptionFallbackModel",
            "rerankingEnabled",
            "rerankerProvider",
            "rerankerModel",
            "rerankerCandidates",
            "provider",
            "model",
            "dimensions",
            "indexPath",
            "include",
            "exclude",
            "maxFileSize",
            "embeddingBatchSize",
            "embeddingParallelism",
            "parallelism",
            "defaultLimit",
            "verbose",
        ]
        .iter()
        .filter_map(|key| config.get(*key).map(|value| format!("{key}: {value}")))
        .collect::<Vec<_>>()
        .join("\n");
        cliclack::note("Saved settings", summary)?;
        cliclack::outro(format!("Updated {}", path.display()))?;
        if global.format != Some(Format::Json) {
            return Ok(());
        }
    }
    // Every rendering of saved settings masks credential values.
    let changed = redacted(&changed);
    if global.format == Some(Format::Json) {
        let mut result = changed;
        result["configPath"] = json!(path);
        print_json(out, &result)
    } else {
        let settings = if action == Some("exclude-cross-search") {
            format!(
                "{} cross-search exclusions ({} added)",
                changed["total"], changed["added"]
            )
        } else {
            changed
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| {
                    let value = value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string());
                    format!("{key}={value}")
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        let message = format!("Updated {}: {settings}", path.display());
        if ui::terminal() && io::stdout().is_terminal() {
            cliclack::log::success(message)?;
        } else {
            writeln!(out, "{message}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
