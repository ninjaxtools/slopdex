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
    catalog: Value,
    fetches: Vec<String>,
    selections: Vec<ScriptedSelection>,
}

impl ScriptedPrompts {
    fn new(answers: &str) -> Self {
        Self {
            input: io::Cursor::new(answers.as_bytes().to_vec()),
            output: Vec::new(),
            catalog: Value::Null,
            fetches: Vec::new(),
            selections: Vec::new(),
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

    fn catalog(&mut self, provider: &str) -> Result<Value> {
        self.fetches.push(provider.to_owned());
        ensure!(!self.catalog.is_null(), "no scripted catalog supplied");
        Ok(self.catalog.clone())
    }
}

#[test]
fn wizard_can_skip_llms_and_preserves_extensions() {
    let mut config = json!({"custom": "keep"});
    let mut prompts = ScriptedPrompts::new("no\nno\n\n\n\n\n\n\n\n\n\n\n");
    configure_interactively(&mut config, &mut prompts).unwrap();
    assert_eq!(config["descriptionsEnabled"], false);
    assert_eq!(config["model"], "text-embedding-3-large");
    assert_eq!(config["dimensions"], 3072);
    assert_eq!(config["custom"], "keep");
    assert!(prompts.fetches.is_empty());
    let output = String::from_utf8(prompts.output).unwrap();
    assert!(!output.contains("Description provider"));
    assert!(!output.contains("Description model"));
}

#[test]
fn wizard_migrates_embedding_aliases_and_saves_selected_values() {
    for (answers, provider, model, dimensions) in [
        ("no\nno\n\n\n\n\n\n\n\n\n\n\n", "jina", "custom-jina", 16),
        (
            "no\nno\nopenai\ntext-embedding-3-small\n8\n\n\n\n\n\n\n\n",
            "openai",
            "text-embedding-3-small",
            8,
        ),
    ] {
        let mut config = json!({"embeddingProvider": "jina", "embeddingModel": "custom-jina",
            "embeddingDimensions": 16, "custom": "keep"});
        let mut prompts = ScriptedPrompts::new(answers);
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
fn wizard_removes_fallback_alias_when_fallback_is_disabled() {
    let mut config = json!({"descriptionProvider": "openai", "descriptionModel": "primary",
        "fallbackModel": "backup"});
    let mut prompts = ScriptedPrompts::new("yes\n\n\nno\nno\n\n\n\n\n\n\n\n\n\n\n");
    configure_interactively(&mut config, &mut prompts).unwrap();
    assert!(config.get("fallbackModel").is_none());
    assert!(config.get("descriptionFallbackModel").is_none());
    assert_eq!(
        Providers::new(&config).unwrap().description_profile()["model"],
        "primary"
    );
}

#[test]
fn wizard_published_models_are_searchable_scoped_and_defaulted() {
    let catalog = json!([
        {"provider": "opencode-go", "model": "first"},
        {"provider": "opencode-go", "model": "saved"},
        {"provider": "opencode", "model": "other-provider"}
    ]);
    let mut prompts = ScriptedPrompts::new("\n\n\n");
    assert_eq!(
        prompts
            .published(&catalog, "opencode-go", "Model", "saved", None)
            .unwrap(),
        "saved"
    );
    assert_eq!(
        prompts
            .published(&catalog, "opencode-go", "Fallback", "saved", Some("saved"))
            .unwrap(),
        "first"
    );
    assert_eq!(
        prompts
            .published(&catalog, "opencode-go", "Model", "retired", None)
            .unwrap(),
        "first"
    );
    assert!(
        prompts
            .selections
            .iter()
            .all(|selection| selection.searchable)
    );
    assert_eq!(prompts.selections[0].default, "saved");
    assert_eq!(prompts.selections[0].choices, ["first", "saved"]);
    assert_eq!(prompts.selections[1].choices, ["first"]);
    assert!(
        prompts
            .published(&catalog, "missing", "Model", "", None)
            .is_err()
    );
    assert!(
        prompts
            .published(&catalog, "opencode", "Fallback", "", Some("other-provider"))
            .is_err()
    );
}

#[test]
fn wizard_fetches_one_catalog_and_reuses_it_for_fallback() {
    let mut config = json!({"descriptionProvider": "opencode-go", "descriptionModel": "primary",
        "descriptionFallbackModel": "backup", "descriptionsEnabled": true});
    let mut prompts = ScriptedPrompts::new(&"\n".repeat(16));
    prompts.catalog = json!([
        {"provider": "opencode-go", "model": "primary"},
        {"provider": "opencode-go", "model": "backup"}
    ]);
    configure_interactively(&mut config, &mut prompts).unwrap();
    assert_eq!(prompts.fetches, ["opencode-go"]);
    assert_eq!(config["descriptionModel"], "primary");
    assert_eq!(config["descriptionFallbackModel"], "backup");
}

#[test]
fn wizard_cancellation_at_every_prompt_discards_all_changes() {
    let original = json!({"embeddingProvider": "jina", "embeddingModel": "custom",
        "embeddingDimensions": 16, "custom": "keep"});
    let answers = [
        "yes",
        "openai",
        "primary",
        "yes",
        "backup",
        "yes",
        "openai",
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
    assert!(prompts.fetches.is_empty());
}

#[test]
fn wizard_catalog_and_validation_errors_discard_changes() {
    for answers in ["yes\nopencode-go\n", "no\nno\n\n\n\n\n[\n\n\n\n\n\n"] {
        let original = json!({"embeddingProvider": "jina", "custom": "keep"});
        let mut config = original.clone();
        assert!(configure_interactively(&mut config, &mut ScriptedPrompts::new(answers)).is_err());
        assert_eq!(config, original);
    }
}

#[test]
fn filtered_config_prompts_only_matching_keys() {
    let mut config =
        json!({"descriptionProvider": "openai", "descriptionModel": "old", "parallelism": 3});
    let mut prompts = ScriptedPrompts::new("new\n");
    configure_interactively_filtered(&mut config, &mut prompts, Some("descriptionModel")).unwrap();
    assert_eq!(config["descriptionModel"], "new");
    assert_eq!(config["descriptionProvider"], "openai");
    assert_eq!(config["parallelism"], 3);
    let mut prompts = ScriptedPrompts::new("7\n");
    configure_interactively_filtered(&mut config, &mut prompts, Some("parallel")).unwrap();
    assert_eq!(config["parallelism"], 7);
}
