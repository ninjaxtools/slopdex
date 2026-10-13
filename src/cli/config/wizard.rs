//! Interactive configuration prompts and staged wizard updates.

use super::{
    embedding_defaults, known_dimensions, normalize_config_aliases, select_embeddings,
    validate_config,
};
use crate::cli::args::positive;
use crate::limits::{self, ResultLimit};
use crate::providers::Providers;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn reranker_default(provider: &str) -> &'static str {
    match provider {
        "jina" => "jina-reranker-v3.5",
        "openai" => "gpt-5.6-luna",
        _ => "rerank-v4.0-pro",
    }
}

pub(super) fn configure(saved: &mut Value, prefix: Option<&str>) -> Result<()> {
    configure_interactively_filtered(saved, &mut CliclackPrompts, prefix)
}

pub(super) fn configure_embeddings(saved: &mut Value) -> Result<()> {
    configure_embeddings_with(saved, &mut CliclackPrompts)
}

pub(super) fn setup_destination(selected: &Path, repository: &Path) -> Result<PathBuf> {
    setup_destination_with(selected, repository, &mut CliclackPrompts)
}

fn setup_destination_with(
    selected: &Path,
    repository: &Path,
    prompts: &mut impl Prompts,
) -> Result<PathBuf> {
    if selected == repository {
        return Ok(selected.to_owned());
    }
    let selected_label = selected.to_string_lossy();
    let repository_label = repository.to_string_lossy();
    let choice = prompts.choice(
        "Save embeddings to repository settings or currently selected settings?",
        &repository_label,
        &[&repository_label, &selected_label],
    )?;
    Ok(if choice == repository_label {
        repository
    } else {
        selected
    }
    .to_owned())
}

trait Prompts {
    fn ask(&mut self, label: &str, default: &str) -> Result<String>;
    fn yes(&mut self, label: &str, default: bool) -> Result<bool>;
    fn number(&mut self, label: &str, default: u64, max: Option<usize>) -> Result<usize>;
    fn required(&mut self, label: &str, default: &str) -> Result<String>;
    fn catalog(&mut self, provider: &str) -> Result<Value>;
    fn installed(&mut self, config: &Value) -> Result<Vec<String>>;
    fn select(
        &mut self,
        label: &str,
        default: &str,
        choices: &[&str],
        searchable: bool,
    ) -> Result<String>;

    fn choice(&mut self, label: &str, default: &str, choices: &[&str]) -> Result<String> {
        self.select(label, default, choices, false)
    }

    fn published(
        &mut self,
        catalog: &Value,
        provider: &str,
        label: &str,
        current: &str,
        excluded: Option<&str>,
    ) -> Result<String> {
        let models: Vec<&str> = array(catalog)
            .iter()
            .filter(|row| text(row, "provider") == provider)
            .map(|row| text(row, "model"))
            .filter(|model| !model.is_empty() && Some(*model) != excluded)
            .collect();
        ensure!(
            !models.is_empty(),
            "{provider} returned no selectable models"
        );
        let default = if models.contains(&current) {
            current
        } else {
            models[0]
        };
        self.select(label, default, &models, true)
    }
}

struct CliclackPrompts;

impl Prompts for CliclackPrompts {
    fn ask(&mut self, label: &str, default: &str) -> Result<String> {
        let value: String = cliclack::input(label)
            .required(false)
            .default_input(default)
            .interact()?;
        Ok(value.trim().to_owned())
    }

    fn select(
        &mut self,
        label: &str,
        default: &str,
        choices: &[&str],
        searchable: bool,
    ) -> Result<String> {
        let mut prompt = cliclack::select(label).max_rows(10);
        for choice in choices {
            let hint = match *choice {
                "openai" if label == "Embedding provider" => {
                    "Remote API; requires OpenAI credentials"
                }
                "jina" if label == "Embedding provider" => "Remote API; requires Jina credentials",
                "ollama" => "Local inference; no cloud fallback",
                "qwen3-embedding:0.6b" => {
                    "Small / fastest · about 0.6 GB download · 1024 dimensions"
                }
                "qwen3-embedding:4b" => "Balanced · about 2.5 GB download · 2560 dimensions",
                "qwen3-embedding:8b" => "Largest · about 4.7 GB download · 4096 dimensions",
                "Custom model" => "Enter an Ollama embedding model tag",
                "Installed embedding models" => {
                    "Query Ollama for installed embedding-capable models"
                }
                _ => "",
            };
            prompt = prompt.item(choice.to_string(), choice, hint);
        }
        prompt = prompt.initial_value(default.to_owned());
        if searchable {
            prompt = prompt.filter_mode();
        }
        Ok(prompt.interact()?)
    }

    fn yes(&mut self, label: &str, default: bool) -> Result<bool> {
        Ok(cliclack::confirm(label).initial_value(default).interact()?)
    }

    fn number(&mut self, label: &str, default: u64, max: Option<usize>) -> Result<usize> {
        Ok(cliclack::input(label)
            .default_input(&default.to_string())
            .validate(move |value: &String| {
                let n = positive(value)?;
                if let Some(max) = max
                    && n > max
                {
                    return Err(format!("Enter a positive integer no greater than {max}"));
                }
                Ok(())
            })
            .interact::<usize>()?)
    }

    fn required(&mut self, label: &str, default: &str) -> Result<String> {
        let value: String = cliclack::input(label)
            .default_input(default)
            .validate(|value: &String| {
                if value.trim().is_empty() {
                    Err("A value is required")
                } else {
                    Ok(())
                }
            })
            .interact()?;
        Ok(value.trim().to_owned())
    }

    fn catalog(&mut self, provider: &str) -> Result<Value> {
        crate::ui::spin("Fetching published models", || {
            Providers::models(Some(provider))
        })
    }

    fn installed(&mut self, config: &Value) -> Result<Vec<String>> {
        match crate::providers::installed_embedding_models(config) {
            Ok(models) => Ok(models),
            Err(error) => {
                cliclack::log::warning(format!(
                    "Cannot list installed Ollama models: {error:#}; presets and custom tags remain available"
                ))?;
                Ok(Vec::new())
            }
        }
    }
}

pub(super) const CONFIG_KEYS: &[&str] = &[
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
];

fn selected(prefix: Option<&str>, key: &str) -> bool {
    prefix.is_none_or(|prefix| key.starts_with(prefix))
}

fn ollama_model(config: &Value, prompts: &mut impl Prompts) -> Result<String> {
    let mut choices = vec![
        "qwen3-embedding:0.6b".to_owned(),
        "qwen3-embedding:4b".to_owned(),
        "qwen3-embedding:8b".to_owned(),
    ];
    let current = config["model"].as_str().unwrap_or("qwen3-embedding:0.6b");
    if !choices.iter().any(|choice| choice == current) {
        choices.push(current.to_owned());
    }
    choices.push("Installed embedding models".to_owned());
    choices.push("Custom model".to_owned());
    let model = prompts.select(
        "Ollama embedding model",
        current,
        &choices.iter().map(String::as_str).collect::<Vec<_>>(),
        true,
    )?;
    if model == "Installed embedding models" {
        let models = prompts.installed(config)?;
        ensure!(
            !models.is_empty(),
            "No installed embedding models available; start Ollama and install an embedding model, or choose a preset/custom tag"
        );
        let choices: Vec<_> = models.iter().map(String::as_str).collect();
        let default = if choices.contains(&current) {
            current
        } else {
            choices[0]
        };
        prompts.select("Installed Ollama embedding model", default, &choices, true)
    } else if model == "Custom model" {
        prompts.required("Ollama embedding model tag", current)
    } else {
        Ok(model)
    }
}

fn prompt_embeddings(
    config: &mut Value,
    prompts: &mut impl Prompts,
    prefix: Option<&str>,
    focused: bool,
) -> Result<()> {
    let provider = if selected(prefix, "provider") {
        prompts.choice(
            "Embedding provider",
            config["provider"].as_str().unwrap_or("openai"),
            &["openai", "jina", "ollama"],
        )?
    } else {
        config["provider"].as_str().unwrap_or("openai").to_owned()
    };
    if selected(prefix, "provider") {
        select_embeddings(config, Some(&provider), None);
    }
    if selected(prefix, "model") {
        let model = if provider == "ollama" {
            ollama_model(config, prompts)?
        } else {
            prompts.required(
                "Embedding model",
                config["model"]
                    .as_str()
                    .unwrap_or(embedding_defaults(&provider).0),
            )?
        };
        select_embeddings(config, None, Some(&model));
    }
    if selected(prefix, "dimensions") && (provider != "ollama" || prefix.is_some()) {
        let dimensions = config["dimensions"]
            .as_u64()
            .or_else(|| known_dimensions(&provider, text(config, "model")))
            .unwrap_or(embedding_defaults(&provider).1);
        config["dimensions"] = json!(prompts.number("Embedding dimensions", dimensions, None)?);
    }
    if provider == "ollama" && (focused || selected(prefix, "provider")) {
        config["ollamaAutoPull"] = json!(prompts.yes(
            "Download missing Ollama models automatically?",
            config["ollamaAutoPull"].as_bool().unwrap_or(true),
        )?);
    }
    Ok(())
}

fn configure_embeddings_with(saved: &mut Value, prompts: &mut impl Prompts) -> Result<()> {
    let mut config = saved.clone();
    normalize_config_aliases(&mut config);
    prompt_embeddings(&mut config, prompts, None, true)?;
    validate_config(&config)?;
    *saved = config;
    Ok(())
}

#[cfg(test)]
fn configure_interactively(saved: &mut Value, prompts: &mut impl Prompts) -> Result<()> {
    configure_interactively_filtered(saved, prompts, None)
}

fn configure_interactively_filtered(
    saved: &mut Value,
    prompts: &mut impl Prompts,
    prefix: Option<&str>,
) -> Result<()> {
    // Stage even alias migration locally so cancellation and validation failures
    // leave the caller's configuration untouched.
    let mut config = saved.clone();
    normalize_config_aliases(&mut config);
    let existing = config.clone();
    if prefix.is_none()
        || prefix.is_some_and(|p| {
            [
                "descriptionProvider",
                "descriptionModel",
                "descriptionFallbackModel",
            ]
            .iter()
            .any(|key| key.starts_with(p))
        })
    {
        let provider = if selected(prefix, "descriptionProvider") {
            prompts.choice(
                "Description provider",
                existing["descriptionProvider"]
                    .as_str()
                    .unwrap_or("opencode-go"),
                &["opencode-go", "opencode", "openai"],
            )?
        } else {
            existing["descriptionProvider"]
                .as_str()
                .unwrap_or("opencode-go")
                .to_owned()
        };
        let same = existing["descriptionProvider"] == provider;
        let current = if same {
            text(&existing, "descriptionModel")
        } else {
            ""
        };
        let catalog = if provider == "openai"
            || !selected(prefix, "descriptionModel")
                && !selected(prefix, "descriptionFallbackModel")
        {
            None
        } else {
            Some(prompts.catalog(&provider)?)
        };
        let model = if !selected(prefix, "descriptionModel") {
            current.to_owned()
        } else if let Some(catalog) = &catalog {
            prompts.published(catalog, &provider, "Description model", current, None)?
        } else {
            prompts.required(
                "Description model",
                if current.is_empty() {
                    "gpt-5.6-luna"
                } else {
                    current
                },
            )?
        };
        if selected(prefix, "descriptionProvider") {
            config["descriptionProvider"] = json!(provider);
        }
        if selected(prefix, "descriptionModel") {
            config["descriptionModel"] = json!(model);
        }
        if selected(prefix, "descriptionFallbackModel")
            && prompts.yes(
                "Configure a fallback description model?",
                existing.get("descriptionFallbackModel").is_some(),
            )?
        {
            let current = if same {
                text(&existing, "descriptionFallbackModel")
            } else {
                ""
            };
            let fallback = if let Some(catalog) = &catalog {
                prompts.published(
                    catalog,
                    &provider,
                    "Fallback description model",
                    current,
                    Some(&model),
                )?
            } else {
                prompts.required("Fallback description model", current)?
            };
            config["descriptionFallbackModel"] = json!(fallback);
        } else if selected(prefix, "descriptionFallbackModel") {
            config
                .as_object_mut()
                .unwrap()
                .remove("descriptionFallbackModel");
        }
    }
    let reranking = if selected(prefix, "rerankingEnabled") {
        prompts.yes(
            "Enable second-stage reranking for searches?",
            existing["rerankingEnabled"].as_bool().unwrap_or(false),
        )?
    } else {
        existing["rerankingEnabled"].as_bool().unwrap_or(false)
    };
    if selected(prefix, "rerankingEnabled") {
        config["rerankingEnabled"] = json!(reranking);
    }
    if (reranking && prefix.is_none())
        || prefix.is_some_and(|p| {
            ["rerankerProvider", "rerankerModel", "rerankerCandidates"]
                .iter()
                .any(|key| key.starts_with(p))
        })
    {
        let provider = if selected(prefix, "rerankerProvider") {
            prompts.choice(
                "Reranker provider",
                existing["rerankerProvider"].as_str().unwrap_or("cohere"),
                &["cohere", "jina", "openai"],
            )?
        } else {
            existing["rerankerProvider"]
                .as_str()
                .unwrap_or("cohere")
                .to_owned()
        };
        let same = existing["rerankerProvider"] == provider;
        let default = if same {
            existing["rerankerModel"]
                .as_str()
                .unwrap_or(reranker_default(&provider))
        } else {
            reranker_default(&provider)
        };
        if selected(prefix, "rerankerModel") {
            config["rerankerModel"] = json!(prompts.required("Reranker model", default)?);
        }
        if selected(prefix, "rerankerProvider") {
            config["rerankerProvider"] = json!(provider);
        }
        if provider == "openai" || (prefix.is_some() && selected(prefix, "rerankerCandidates")) {
            let default = if same {
                existing["rerankerCandidates"].as_u64().unwrap_or(10)
            } else {
                10
            };
            if selected(prefix, "rerankerCandidates") {
                config["rerankerCandidates"] = json!(prompts.number(
                    "Embedding-ranked reranker candidates",
                    default,
                    Some(100)
                )?);
            }
        } else if selected(prefix, "rerankerProvider") {
            config.as_object_mut().unwrap().remove("rerankerCandidates");
        }
    }
    prompt_embeddings(&mut config, prompts, prefix, false)?;
    if selected(prefix, "indexPath") {
        let index = prompts.ask(
            "Index path (enter '-' for default)",
            text(&existing, "indexPath"),
        )?;
        if index.is_empty() || index == "-" {
            config.as_object_mut().unwrap().remove("indexPath");
        } else {
            config["indexPath"] = json!(index);
        }
    }
    for (key, label) in [
        ("include", "Include globs"),
        ("exclude", "Additional exclude globs"),
    ] {
        if !selected(prefix, key) {
            continue;
        }
        let default = array(&existing[key])
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let value = prompts.ask(&format!("{label} (comma-separated; '-' clears)"), &default)?;
        let patterns: Vec<_> = value
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "-")
            .collect();
        config[key] = json!(patterns);
    }
    for (key, label, default) in [
        (
            "maxFileSize",
            "Maximum source file size in bytes",
            1_048_576,
        ),
        (
            "embeddingBatchSize",
            "Embedding batch size",
            if config["provider"] == "ollama" {
                8
            } else {
                32
            },
        ),
        ("parallelism", "Concurrent provider request limit", 10),
    ] {
        if !selected(prefix, key) {
            continue;
        }
        if key == "parallelism" && prefix.is_none() && config["provider"] == "ollama" {
            let default = existing
                .get("embeddingParallelism")
                .or_else(|| existing.get("parallelism"))
                .and_then(Value::as_u64)
                .unwrap_or(1);
            config["embeddingParallelism"] =
                json!(prompts.number("Concurrent embedding request limit", default, None)?);
            continue;
        }
        config[key] =
            json!(prompts.number(label, existing[key].as_u64().unwrap_or(default), None)?);
    }
    if selected(prefix, "verbose") {
        config["verbose"] = json!(prompts.yes(
            "Log every external model request?",
            existing["verbose"].as_bool().unwrap_or(false)
        )?);
    }
    if selected(prefix, "defaultLimit") {
        let default = limits::resolve(&existing, &json!({}))?.value();
        let default = default
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| default.to_string());
        let limit = prompts.required(
            "Default result limit (positive integer or 'none')",
            &default,
        )?;
        config["defaultLimit"] = limit
            .parse::<ResultLimit>()
            .map_err(anyhow::Error::msg)?
            .value();
    }
    validate_config(&config)?;
    *saved = config;
    Ok(())
}

#[cfg(test)]
#[path = "wizard_tests.rs"]
mod tests;
