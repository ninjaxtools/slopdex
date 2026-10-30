use super::{Prompts, configure_interactively, configure_interactively_filtered};
use crate::cli::args::positive;
use crate::cli::config::{read_config, write_config};
use crate::providers::Providers;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::io::{self, Write};

struct ScriptedSelection {
    default: String,
    choices: Vec<String>,
    searchable: bool,
}

struct ScriptedPrompts {
    input: io::Cursor<Vec<u8>>,
    output: Vec<u8>,
    selections: Vec<ScriptedSelection>,
    installed: Vec<String>,
    installed_fetches: usize,
}

impl ScriptedPrompts {
    fn new(answers: &str) -> Self {
        Self {
            input: io::Cursor::new(answers.as_bytes().to_vec()),
            output: Vec::new(),
            selections: Vec::new(),
            installed: Vec::new(),
            installed_fetches: 0,
        }
    }
}

impl Prompts for ScriptedPrompts {
    fn ask(&mut self, label: &str, default: &str) -> Result<String> {
        use std::io::BufRead;

        writeln!(self.output, "{label} [{default}]")?;
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            return Err(io::Error::from(io::ErrorKind::Interrupted).into());
        }
        Ok(if line.trim().is_empty() {
            default.to_owned()
        } else {
            line.trim().to_owned()
        })
    }

    fn select(
        &mut self,
        label: &str,
        default: &str,
        choices: &[&str],
        searchable: bool,
    ) -> Result<String> {
        self.selections.push(ScriptedSelection {
            default: default.to_owned(),
            choices: choices.iter().map(|choice| (*choice).to_owned()).collect(),
            searchable,
        });
        let value = self.ask(label, default)?;
        ensure!(
            choices.contains(&value.as_str()),
            "invalid scripted selection: {value}"
        );
        Ok(value)
    }

    fn yes(&mut self, label: &str, default: bool) -> Result<bool> {
        match self
            .ask(label, if default { "yes" } else { "no" })?
            .as_str()
        {
            "yes" => Ok(true),
            "no" => Ok(false),
            value => anyhow::bail!("invalid scripted confirmation: {value}"),
        }
    }

    fn number(&mut self, label: &str, default: u64, max: Option<usize>) -> Result<usize> {
        let value = self.ask(label, &default.to_string())?;
        let n = positive(&value).map_err(anyhow::Error::msg)?;
        ensure!(
            max.is_none_or(|max| n <= max),
            "scripted number exceeds maximum"
        );
        Ok(n)
    }

    fn required(&mut self, label: &str, default: &str) -> Result<String> {
        let value = self.ask(label, default)?;
        ensure!(!value.is_empty(), "scripted value is required");
        Ok(value)
    }

    fn installed(&mut self, _config: &Value) -> Result<Vec<String>> {
        self.installed_fetches += 1;
        Ok(self.installed.clone())
    }
}

#[test]
fn ollama_setup_defaults_download_consent_but_preserves_explicit_refusal() {
    for explicit in [None, Some(false)] {
        let mut config = json!({"provider": "ollama"});
        if let Some(allowed) = explicit {
            config["ollamaAutoPull"] = json!(allowed);
        }
        super::configure_embeddings_with(&mut config, &mut ScriptedPrompts::new("\n\n\n")).unwrap();
        assert_eq!(config["ollamaAutoPull"], explicit.unwrap_or(true));
    }
}

#[test]
fn full_ollama_configuration_uses_local_batching_without_throttling_descriptions() {
    let mut config = json!({});
    configure_interactively(
        &mut config,
        &mut ScriptedPrompts::new("\n\nno\nollama\n\n\n\n\n\n\n\n\n\n\n"),
    )
    .unwrap();
    assert_eq!(config["embeddingBatchSize"], 8);
    assert_eq!(config["embeddingParallelism"], 1);
    assert!(config.get("parallelism").is_none());
}

#[test]
fn wizard_configures_description_models_without_enablement_and_preserves_extensions() {
    let mut config = json!({"custom": "keep"});
    let mut prompts = ScriptedPrompts::new(&format!("\n\nno\n{}", "\n".repeat(11)));
    configure_interactively(&mut config, &mut prompts).unwrap();
    assert_eq!(config["descriptionCommand"], json!(["opencode", "run"]));
    assert!(config.get("descriptionModel").is_none());
    assert_eq!(config["model"], "text-embedding-3-large");
    assert_eq!(config["dimensions"], 3072);
    assert_eq!(config["custom"], "keep");
    let output = String::from_utf8(prompts.output).unwrap();
    assert!(output.contains("Description agent command"));
    assert!(output.contains("Description model"));
    assert!(!output.contains("Generate file and function descriptions"));
}

#[test]
fn wizard_migrates_embedding_aliases_and_saves_selected_values() {
    for (answers, provider, model, dimensions) in [
        (
            "\n\nno\n\n\n\n\n\n\n\n\n\n\n",
            "jina",
            "jina-embeddings-v3",
            16,
        ),
        (
            "\n\nno\nopenai\ntext-embedding-3-small\n8\n\n\n\n\n\n\n\n",
            "openai",
            "text-embedding-3-small",
            8,
        ),
    ] {
        let mut config = json!({"embeddingProvider": "jina", "embeddingModel": "jina-embeddings-v3",
            "embeddingDimensions": 16, "custom": "keep"});
        let mut prompts = ScriptedPrompts::new(&(answers.to_owned() + "\n"));
        configure_interactively(&mut config, &mut prompts).unwrap();
        for alias in ["embeddingProvider", "embeddingModel", "embeddingDimensions"] {
            assert!(config.get(alias).is_none());
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        write_config(&path, &config).unwrap();
        let saved = read_config(&path).unwrap();
        assert_eq!(saved["custom"], "keep");
        assert_eq!(
            Providers::new(&saved).unwrap().embedding_profile(),
            json!({
                "provider": provider, "model": model, "dimensions": dimensions, "strategyVersion": "rust-v1"
            })
        );
    }
}

#[test]
fn wizard_command_configuration_is_offline_and_preserves_model_ids() {
    let mut config = json!({"descriptionModel":"opencode-go/deepseek-v4.1-flash"});
    let mut prompts = ScriptedPrompts::new("[\"pi\",\"--print\",\"--no-tools\"]\n\n");
    configure_interactively_filtered(&mut config, &mut prompts, Some("description")).unwrap();
    assert_eq!(
        config["descriptionCommand"],
        json!(["pi", "--print", "--no-tools"])
    );
    assert_eq!(
        config["descriptionModel"],
        "opencode-go/deepseek-v4.1-flash"
    );
}

#[test]
fn wizard_cancellation_at_every_prompt_discards_all_changes() {
    let original = json!({"embeddingProvider": "jina", "embeddingModel": "custom",
        "embeddingDimensions": 16, "custom": "keep"});
    let answers = [
        "[\"opencode\",\"run\"]",
        "primary",
        "yes",
        "command",
        "[\"codex\",\"exec\"]",
        "reranker",
        "100",
        "openai",
        "embedding",
        "8",
        "index.sqlite",
        "src/**",
        "-",
        "1024",
        "16",
        "4",
        "yes",
        "none",
    ];
    for end in 0..answers.len() {
        let input = answers[..end]
            .iter()
            .map(|answer| format!("{answer}\n"))
            .collect::<String>();
        let mut config = original.clone();
        let error =
            configure_interactively(&mut config, &mut ScriptedPrompts::new(&input)).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(config, original, "cancelled at prompt {end}");
    }
    let mut config = original;
    let mut prompts = ScriptedPrompts::new(&(answers.join("\n") + "\n"));
    configure_interactively(&mut config, &mut prompts).unwrap();
    assert_eq!(config["rerankerCandidates"], 100);
    assert_eq!(config["verbose"], true);
    assert_eq!(config["defaultLimit"], "none");
}

#[test]
fn wizard_catalog_and_validation_errors_discard_changes() {
    for answers in [
        "not-json\n",
        "[\"opencode\",\"run\"]\n\nno\n\n\n\n\n[\n\n\n\n\n\n",
    ] {
        let original = json!({"embeddingProvider": "jina", "custom": "keep"});
        let mut config = original.clone();
        assert!(configure_interactively(&mut config, &mut ScriptedPrompts::new(answers)).is_err());
        assert_eq!(config, original);
    }
}

#[test]
fn filtered_config_prompts_only_matching_keys() {
    let mut config = json!({"descriptionCommand": ["opencode","run"], "descriptionModel": "old", "parallelism": 3});
    let mut prompts = ScriptedPrompts::new("new\n");
    configure_interactively_filtered(&mut config, &mut prompts, Some("descriptionModel")).unwrap();
    assert_eq!(config["descriptionModel"], "new");
    assert_eq!(config["descriptionCommand"], json!(["opencode", "run"]));
    assert_eq!(config["parallelism"], 3);
    let mut prompts = ScriptedPrompts::new("7\n");
    configure_interactively_filtered(&mut config, &mut prompts, Some("parallel")).unwrap();
    assert_eq!(config["parallelism"], 7);
}

#[test]
fn wizard_default_limit_is_configurable_and_validation_is_staged() {
    for (answer, expected) in [
        ("\n", json!(4096)),
        ("23\n", json!(23)),
        ("none\n", json!("none")),
    ] {
        let mut config = json!({"custom": "keep"});
        let mut prompts = ScriptedPrompts::new(answer);
        configure_interactively_filtered(&mut config, &mut prompts, Some("defaultLimit")).unwrap();
        assert_eq!(config, json!({"custom": "keep", "defaultLimit": expected}));
    }
    for answer in ["0\n", "all\n"] {
        let original = json!({"defaultLimit": "none", "custom": "keep"});
        let mut config = original.clone();
        assert!(
            configure_interactively_filtered(
                &mut config,
                &mut ScriptedPrompts::new(answer),
                Some("defaultLimit")
            )
            .is_err()
        );
        assert_eq!(config, original);
    }
}

#[test]
fn focused_embeddings_offer_presets_installed_and_custom_without_description_catalogs() {
    for (answer, model, dimensions) in [
        ("qwen3-embedding:0.6b", "qwen3-embedding:0.6b", Some(1024)),
        ("qwen3-embedding:4b", "qwen3-embedding:4b", Some(2560)),
        ("qwen3-embedding:8b", "qwen3-embedding:8b", Some(4096)),
        (
            "Installed embedding models\ninstalled-embed",
            "installed-embed",
            None,
        ),
        ("Custom model\ncustom-embed:tag", "custom-embed:tag", None),
        (
            "Custom model\nembeddinggemma:300m",
            "embeddinggemma:300m",
            Some(768),
        ),
    ] {
        let mut config = json!({"provider": "openai", "model": "old", "dimensions": 8,
            "embeddingBaseUrl": "https://old.invalid/v1", "descriptionCommand": ["opencode","run"],
            "descriptionModel": "keep", "custom": true});
        let mut prompts = ScriptedPrompts::new(&format!("ollama\n{answer}\nyes\n"));
        prompts.installed = vec!["installed-embed".into(), "qwen3-embedding:4b".into()];
        super::configure_embeddings_with(&mut config, &mut prompts).unwrap();
        assert_eq!(config["provider"], "ollama");
        assert_eq!(config["model"], model);
        assert_eq!(config["dimensions"].as_u64(), dimensions);
        assert_eq!(config["ollamaAutoPull"], true);
        assert_eq!(config["descriptionModel"], "keep");
        assert_eq!(config["custom"], true);
        assert!(config.get("embeddingBaseUrl").is_none());
        let selection = &prompts.selections[1];
        assert_eq!(
            selection.choices,
            [
                "qwen3-embedding:0.6b",
                "qwen3-embedding:4b",
                "qwen3-embedding:8b",
                "Installed embedding models",
                "Custom model"
            ]
        );
        assert!(selection.searchable);
        assert_eq!(
            prompts.installed_fetches,
            usize::from(model == "installed-embed")
        );
    }
}

#[test]
fn focused_embedding_cancellation_at_every_prompt_is_staged() {
    let original = json!({"embeddingProvider": "jina", "embeddingModel": "saved",
        "embeddingDimensions": 16, "descriptionModel": "keep"});
    let answers = ["ollama", "Custom model", "my-embedding:tag", "yes"];
    for end in 0..answers.len() {
        let mut config = original.clone();
        let input = answers[..end].join("\n") + if end == 0 { "" } else { "\n" };
        let error =
            super::configure_embeddings_with(&mut config, &mut ScriptedPrompts::new(&input))
                .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(config, original, "cancelled at prompt {end}");
    }
    let mut config = original.clone();
    let error = super::configure_embeddings_with(
        &mut config,
        &mut ScriptedPrompts::new("ollama\nInstalled embedding models\n"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("No installed embedding models"));
    assert_eq!(config, original);
}

#[test]
fn full_wizard_supports_ollama_and_provider_prefix_resets_stale_fields() {
    let mut config = json!({"provider": "jina", "model": "old", "dimensions": 16,
        "embeddingBaseUrl": "https://old.invalid/v1"});
    let mut prompts = ScriptedPrompts::new(&format!(
        "\n\nno\nollama\nqwen3-embedding:4b\nyes\n{}",
        "\n".repeat(8)
    ));
    configure_interactively(&mut config, &mut prompts).unwrap();
    assert_eq!(config["provider"], "ollama");
    assert_eq!(config["dimensions"], 2560);
    assert_eq!(config["ollamaAutoPull"], true);
    let mut prompts = ScriptedPrompts::new("openai\n");
    configure_interactively_filtered(&mut config, &mut prompts, Some("provider")).unwrap();
    assert_eq!(config["model"], "text-embedding-3-large");
    assert_eq!(config["dimensions"], 3072);
    assert!(config.get("embeddingBaseUrl").is_none());
    assert!(config.get("ollamaAutoPull").is_none());
}

#[test]
fn setup_destination_defaults_to_repository_without_surprise_global_writes() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let selected = base.join("user/config.json");
    let repository = base.join("repo/.slopdex/config.json");
    let mut prompts = ScriptedPrompts::new("\n");
    assert_eq!(
        super::setup_destination_with(&selected, &repository, &mut prompts).unwrap(),
        repository
    );
    assert_eq!(prompts.selections[0].default, repository.to_string_lossy());
    let mut prompts = ScriptedPrompts::new(&format!("{}\n", selected.display()));
    assert_eq!(
        super::setup_destination_with(&selected, &repository, &mut prompts).unwrap(),
        selected
    );
    assert_eq!(
        super::setup_destination_with(&repository, &repository, &mut ScriptedPrompts::new(""))
            .unwrap(),
        repository
    );
}
