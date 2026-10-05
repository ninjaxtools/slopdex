//! Configuration loading, validation, persistence, and command handling.

use super::args::{Format, Global};
use super::io::print_json;
use super::workspace::absolute;
use crate::ui;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::Path,
};

mod wizard;

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
    for (key, value) in [
        ("provider", &global.provider),
        ("model", &global.model),
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
    if let Some(value) = global.dimensions {
        config["dimensions"] = json!(value);
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

fn validate_config(config: &Value) -> Result<()> {
    ensure!(config.is_object(), "config must be a JSON object");
    for (key, allowed) in [
        ("provider", &["openai", "jina"][..]),
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
    ] {
        if let Some(value) = config.get(key) {
            ensure!(
                value.as_str().is_some_and(|s| !s.trim().is_empty()),
                "{key} must be a non-empty string"
            );
        }
    }
    for key in [
        "dimensions",
        "maxFileSize",
        "embeddingBatchSize",
        "parallelism",
    ] {
        if let Some(value) = config.get(key) {
            ensure!(
                value.as_u64().is_some_and(|n| n > 0),
                "{key} must be a positive integer"
            );
        }
    }
    for key in ["descriptionsEnabled", "rerankingEnabled", "verbose"] {
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
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .with_context(|| format!("create temporary config {}", temp.display()))?;
    let result = (|| -> Result<()> {
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

pub(super) fn run_config(
    global: &Global,
    path: &Path,
    args: &[String],
    out: &mut impl Write,
) -> Result<()> {
    let mut config = read_config(path)?;
    let interactive = args.first().is_none_or(|arg| arg != "set");
    let changed = if !interactive {
        ensure!(args.len() == 3, "usage: slopdex config set <key> <value>");
        let key = &args[1];
        ensure!(
            key.split('.').all(|part| !part.is_empty()),
            "config set expects a non-empty key path"
        );
        let value = serde_json::from_str::<Value>(&args[2]).unwrap_or_else(|_| json!(args[2]));
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
        if let Err(error) = wizard::configure(&mut config, prefix) {
            // cliclack 0.5.6 returns Interrupted for Esc/Ctrl-C; it does not exit.
            if error.downcast_ref::<io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::UnexpectedEof
                )
            }) {
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
            "descriptionsEnabled",
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
            "parallelism",
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
    if global.format == Some(Format::Json) {
        let mut result = changed;
        result["configPath"] = json!(path);
        print_json(out, &result)
    } else {
        let settings = changed
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
            .join(" ");
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
