use super::{effective_config, read_config, run_config, write_config};
use crate::cli::{
    args::Command,
    test_support::parse,
    workspace::{config_path, index_path},
};
use crate::{cache, providers::Providers};
use serde_json::{Value, json};
use std::{fs, path::Path};

#[test]
fn embedding_setup_saves_only_embedding_flags_and_resolved_identity() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("selected/config.json");
    let original = json!({"provider": "openai", "model": "old", "dimensions": 8,
        "embeddingBaseUrl": "https://old.invalid/v1", "descriptionModel": "keep",
        "artifactCachePath": temp.path().join("artifacts.sqlite"), "custom": {"keep": true}});
    write_config(&path, &original).unwrap();
    for command in [vec!["update", "--setup"], vec!["config", "embeddings"]] {
        let args = [
            command,
            vec![
                "--provider",
                "ollama",
                "--model",
                "qwen3-embedding:4b",
                "--pull",
                "--no-reindex",
                "--verbose",
                "--description-model",
                "transient",
            ],
        ]
        .concat();
        let cli = parse(&args);
        cli.validate().unwrap();
        super::save_embeddings(
            &cli.global,
            &path,
            |_| panic!("explicit flags must not prompt"),
            |config, pull| {
                assert!(pull);
                assert_eq!(config["provider"], "ollama");
                assert_eq!(config["model"], "qwen3-embedding:4b");
                assert_eq!(config["dimensions"], 2560);
                assert_eq!(config["ollamaAutoPull"], true);
                assert!(config.get("embeddingBaseUrl").is_none());
                config["ollamaModelDigest"] = json!("sha256:resolved");
                config["ollamaNativeDimensions"] = json!(2560);
                config["ollamaContextLength"] = json!(8192);
                Ok(())
            },
        )
        .unwrap();
        let saved = read_config(&path).unwrap();
        assert_eq!(saved["ollamaModelDigest"], "sha256:resolved");
        assert_eq!(saved["descriptionModel"], "keep");
        assert_eq!(saved["custom"], original["custom"]);
        assert_eq!(saved["artifactCachePath"], original["artifactCachePath"]);
        for key in ["verbose", "noReindex", "forceReindex", "ignoreErrors"] {
            assert!(saved.get(key).is_none(), "unrelated flag saved: {key}");
        }
        write_config(&path, &original).unwrap();
    }
}

#[test]
fn embedding_setup_failure_and_cancellation_never_write() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    let original = json!({"embeddingProvider": "jina", "custom": "keep"});
    fs::write(&path, original.to_string()).unwrap();
    let bytes = fs::read(&path).unwrap();
    let global = parse(&["update", "--setup"]).global;
    assert!(
        super::save_embeddings(
            &global,
            &path,
            |config| {
                config["provider"] = json!("ollama");
                Err(std::io::Error::from(std::io::ErrorKind::Interrupted).into())
            },
            |_, _| panic!("cancelled wizard must not prepare")
        )
        .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    let global = parse(&["update", "--setup", "--provider", "ollama", "--pull"]).global;
    assert!(
        super::save_embeddings(
            &global,
            &path,
            |_| panic!("unexpected wizard"),
            |config, _| {
                config["ollamaModelDigest"] = json!("partial");
                anyhow::bail!("injected smoke embedding failure")
            }
        )
        .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert!(
        super::save_embeddings(
            &global,
            &path,
            |_| panic!("unexpected wizard"),
            |config, _| {
                config["dimensions"] = json!(0);
                Ok(())
            }
        )
        .is_err(),
        "invalid prepared metadata must not be saved"
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    let missing = temp.path().join("not-created/config.json");
    assert!(
        super::save_embeddings(
            &global,
            &missing,
            |_| panic!("unexpected wizard"),
            |_, _| { anyhow::bail!("injected server error") }
        )
        .is_err()
    );
    assert!(!missing.parent().unwrap().exists());
}

#[test]
fn embedding_setup_preserves_explicit_dimensions_refreshes_pin_and_skips_hosted_preparation() {
    let global = parse(&[
        "config",
        "embeddings",
        "--provider",
        "ollama",
        "--model",
        "qwen3-embedding:8b",
        "--dimensions",
        "256",
    ])
    .global;
    let saved = json!({"provider": "ollama", "model": "qwen3-embedding:0.6b",
        "dimensions": 1024, "embeddingBaseUrl": "http://localhost:11434/api",
        "ollamaModelDigest": "old", "ollamaNativeDimensions": 1024,
        "ollamaModelFamily": "legacy", "ollamaContextLength": 2048});
    let config = super::stage_embeddings(
        &saved,
        &global,
        |_| panic!("unexpected wizard"),
        |config, pull| {
            assert!(!pull);
            assert_eq!(config["dimensions"], 256);
            assert_eq!(config["embeddingBaseUrl"], saved["embeddingBaseUrl"]);
            for key in [
                "ollamaModelDigest",
                "ollamaNativeDimensions",
                "ollamaModelFamily",
                "ollamaContextLength",
            ] {
                assert!(config.get(key).is_none(), "stale field: {key}");
            }
            config["ollamaModelDigest"] = json!("new");
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(config["ollamaModelDigest"], "new");
    let global = parse(&["config", "embeddings", "--provider", "ollama"]).global;
    let refreshed = super::stage_embeddings(
        &config,
        &global,
        |_| panic!("unexpected wizard"),
        |config, _| {
            config["ollamaModelDigest"] = json!("refreshed");
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(refreshed["ollamaModelDigest"], "refreshed");
    let global = parse(&["config", "embeddings", "--provider", "jina"]).global;
    let hosted = super::stage_embeddings(
        &saved,
        &global,
        |_| panic!("unexpected wizard"),
        |_, _| panic!("hosted setup must not prepare or require credentials"),
    )
    .unwrap();
    assert_eq!(hosted["model"], "jina-embeddings-v4");
    assert_eq!(hosted["dimensions"], 1024);
    assert!(hosted.get("embeddingBaseUrl").is_none());
    assert!(hosted.get("ollamaModelDigest").is_none());
}

#[test]
fn config_embeddings_noninteractive_consumes_global_flags_without_an_index() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    let result = config_action_json(
        &path,
        &[
            "config",
            "embeddings",
            "--provider",
            "jina",
            "--model",
            "custom-jina",
            "--dimensions",
            "16",
        ],
    );
    assert_eq!(result["provider"], "jina");
    assert_eq!(result["model"], "custom-jina");
    assert_eq!(result["dimensions"], 16);
    assert!(!temp.path().join("index.sqlite").exists());
    assert_eq!(read_config(&path).unwrap()["model"], "custom-jina");
}

#[test]
fn ollama_advanced_fields_are_validated_and_selection_edits_invalidate_identity() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    config_action_json(&path, &["config", "set", "provider", "ollama"]);
    for (key, value, invalid) in [
        ("ollamaAutoPull", "true", "\"true\""),
        ("ollamaKeepAlive", "5m", "0"),
        ("ollamaModelDigest", "sha256:abc", "\"\""),
        ("embeddingParallelism", "2", "0"),
        ("ollamaNativeDimensions", "1024", "-1"),
        ("ollamaContextLength", "8192", "1.5"),
        ("ollamaPullTimeoutMs", "1800000", "\"3000\""),
    ] {
        config_action_json(&path, &["config", "set", key, value]);
        let bytes = fs::read(&path).unwrap();
        let cli = parse(&["config", "set", key, "--", invalid]);
        let Command::Config { args } = cli.command else {
            panic!()
        };
        assert!(
            run_config(&cli.global, &path, &args, &mut Vec::new()).is_err(),
            "{key}"
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    config_action_json(&path, &["config", "set", "dimensions", "16"]);
    config_action_json(&path, &["config", "set", "model", "other"]);
    let saved = read_config(&path).unwrap();
    for key in [
        "dimensions",
        "ollamaModelDigest",
        "ollamaNativeDimensions",
        "ollamaModelFamily",
        "ollamaContextLength",
    ] {
        assert!(saved.get(key).is_none(), "stale model field: {key}");
    }
    assert_eq!(saved["ollamaKeepAlive"], "5m");
    assert_eq!(saved["embeddingParallelism"], 2);
    config_action_json(
        &path,
        &[
            "config",
            "set",
            "embeddingBaseUrl",
            "http://localhost:11434/api",
        ],
    );
    config_action_json(&path, &["config", "set", "provider", "openai"]);
    let saved = read_config(&path).unwrap();
    assert_eq!(saved["model"], "text-embedding-3-large");
    assert_eq!(saved["dimensions"], 3072);
    assert!(saved.get("embeddingBaseUrl").is_none());
    assert!(saved.get("ollamaAutoPull").is_none());
}

#[test]
fn selected_root_does_not_rebase_explicit_config_or_saved_index_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo with spaces");
    let cwd = std::env::current_dir().unwrap();
    write_config(&root.join(".slopdex/config.json"), &json!({})).unwrap();
    assert_eq!(
        config_path(&root, None).unwrap(),
        root.join(".slopdex/config.json")
    );
    assert_eq!(
        config_path(&root, Some(Path::new("settings/custom.json"))).unwrap(),
        cwd.join("settings/custom.json")
    );
    let config_file = temp.path().join("elsewhere/config.json");
    write_config(&config_file, &json!({"indexPath": "indexes/saved.sqlite"})).unwrap();
    let config = read_config(&config_file).unwrap();
    assert_eq!(
        index_path(&root, None, &config).unwrap(),
        cwd.join("indexes/saved.sqlite")
    );
    let explicit = temp.path().join("override.sqlite");
    assert_eq!(
        index_path(&root, Some(&explicit), &config).unwrap(),
        explicit
    );
    assert_eq!(config_path(&root, Some(&config_file)).unwrap(), config_file);
    let cli = parse(&["status", "--index", "relative/override.sqlite"]);
    let effective = effective_config(&cli.global, &config_file).unwrap();
    assert_eq!(
        effective["indexPath"],
        json!(cwd.join("relative/override.sqlite"))
    );
    assert_eq!(read_config(&config_file).unwrap(), config);
}

#[test]
fn config_round_trip_preserves_unknown_fields_and_resolves_cwd_paths() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("nested/config.json");
    let config = json!({"provider": "jina", "dimensions": 1024, "custom": {"keep": true}});
    write_config(&path, &config).unwrap();
    assert_eq!(read_config(&path).unwrap(), config);
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    assert_eq!(
        index_path(&root, Some(Path::new("custom.sqlite")), &config).unwrap(),
        std::env::current_dir().unwrap().join("custom.sqlite")
    );
    assert_eq!(
        index_path(&root, None, &config).unwrap(),
        cache::directory()
            .unwrap()
            .join("worktrees-v1")
            .join(crate::hash(
                root.canonicalize().unwrap().as_os_str().as_encoded_bytes()
            ))
            .join("index.sqlite")
    );
}

#[test]
fn explicit_embedding_cli_options_override_config_aliases() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    let saved = json!({"embeddingProvider": "jina", "embeddingModel": "jina-embeddings-v3",
        "embeddingDimensions": 1024, "custom": "keep"});
    fs::write(&path, saved.to_string()).unwrap();
    for (args, expected) in [
        (
            vec!["status"],
            json!({"provider": "jina", "model": "jina-embeddings-v3", "dimensions": 1024}),
        ),
        (
            vec!["status", "--provider", "openai"],
            json!({"provider": "openai", "model": "text-embedding-3-large", "dimensions": 3072}),
        ),
        (
            vec!["status", "--model", "jina-embeddings-v4"],
            json!({"provider": "jina", "model": "jina-embeddings-v4", "dimensions": 1024}),
        ),
        (
            vec!["status", "--dimensions", "8"],
            json!({"provider": "jina", "model": "jina-embeddings-v3", "dimensions": 8}),
        ),
        (
            vec![
                "status",
                "--provider",
                "openai",
                "--model",
                "text-embedding-3-small",
                "--dimensions",
                "8",
            ],
            json!({"provider": "openai", "model": "text-embedding-3-small", "dimensions": 8}),
        ),
    ] {
        let config = effective_config(&parse(&args).global, &path).unwrap();
        let profile = Providers::new(&config).unwrap().embedding_profile();
        for key in ["provider", "model", "dimensions"] {
            assert_eq!(profile[key], expected[key], "{args:?}: {key}");
        }
        for alias in ["embeddingProvider", "embeddingModel", "embeddingDimensions"] {
            assert!(config.get(alias).is_none());
        }
        assert_eq!(config["custom"], "keep");
    }
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
        saved
    );
}

#[test]
fn canonical_config_wins_conflicting_aliases() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    fs::write(
        &path,
        json!({"provider": "openai", "model": "new-model", "dimensions": 8,
        "embeddingProvider": "jina", "embeddingModel": "old-model", "embeddingDimensions": 1024,
        "descriptionFallbackModel": "new-backup", "fallbackModel": "old-backup"})
        .to_string(),
    )
    .unwrap();
    let config = effective_config(&parse(&["status"]).global, &path).unwrap();
    assert_eq!(config["provider"], "openai");
    assert_eq!(config["model"], "new-model");
    assert_eq!(config["dimensions"], 8);
    assert_eq!(config["descriptionFallbackModel"], "new-backup");
    assert!(config.get("fallbackModel").is_none());
}

#[test]
fn config_set_keeps_json_stdout_machine_readable() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    write_config(&path, &json!({"custom": "keep"})).unwrap();
    for args in [
        vec![
            "config",
            "set",
            "descriptionProvider",
            "openai",
            "--output",
            "json",
        ],
        vec!["config", "set", "parallelism", "4", "--output", "json"],
        vec![
            "config",
            "set",
            "rerankerProvider",
            "openai",
            "--output",
            "json",
        ],
    ] {
        let cli = parse(&args);
        let Command::Config { args } = cli.command else {
            panic!()
        };
        let mut out = Vec::new();
        run_config(&cli.global, &path, &args, &mut out).unwrap();
        let result: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(result["configPath"], json!(path));
        assert_eq!(read_config(&path).unwrap()["custom"], "keep");
    }
}

#[test]
fn config_default_limit_accepts_counts_and_none_and_rejects_invalid_values() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    for (input, expected) in [("17", json!(17)), ("none", json!("none"))] {
        let result = config_action_json(&path, &["config", "set", "defaultLimit", input]);
        assert_eq!(result["defaultLimit"], expected);
        let config = effective_config(&parse(&["map"]).global, &path).unwrap();
        assert_eq!(config["defaultLimit"], expected);
    }
    let original = fs::read(&path).unwrap();
    for invalid in ["0", "-1", "1.5", "null", "true", "\"17\"", "all"] {
        let cli = parse(&["config", "set", "defaultLimit", "--", invalid]);
        let Command::Config { args } = cli.command else {
            panic!()
        };
        let mut out = Vec::new();
        let error = run_config(&cli.global, &path, &args, &mut out).unwrap_err();
        assert!(error.to_string().contains("defaultLimit"), "{error}");
        assert!(out.is_empty());
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::write(&path, json!({"defaultLimit": serde_json::from_str::<Value>(invalid).unwrap_or(json!(invalid))}).to_string()).unwrap();
        assert!(effective_config(&parse(&["map"]).global, &path).is_err());
        fs::write(&path, &original).unwrap();
    }
}

#[test]
fn config_set_supports_nested_values_and_rejects_invalid_values_without_writing() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    let result = config_action_json(&path, &["config", "set", "artifactS3.bucket", "my-bucket"]);
    assert_eq!(result["artifactS3.bucket"], "my-bucket");
    assert_eq!(
        read_config(&path).unwrap()["artifactS3"]["bucket"],
        "my-bucket"
    );
    let original = fs::read(&path).unwrap();
    let cli = parse(&["config", "set", "parallelism", "0"]);
    let Command::Config { args } = cli.command else {
        panic!()
    };
    assert!(run_config(&cli.global, &path, &args, &mut Vec::new()).is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[test]
fn removed_description_setting_is_rejected_without_writing() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    write_config(&path, &json!({"custom": "keep"})).unwrap();
    let original = fs::read(&path).unwrap();
    for value in ["true", "false", "null"] {
        let cli = parse(&["config", "set", "descriptionsEnabled", value]);
        let Command::Config { args } = cli.command else {
            panic!()
        };
        let mut out = Vec::new();
        let error = run_config(&cli.global, &path, &args, &mut out).unwrap_err();
        assert!(error.to_string().contains("has been removed"));
        assert!(out.is_empty());
        assert_eq!(fs::read(&path).unwrap(), original);
    }
    fs::write(&path, r#"{"descriptionsEnabled": false}"#).unwrap();
    let error = effective_config(&parse(&["status"]).global, &path).unwrap_err();
    assert!(error.to_string().contains("slopdex generate descriptions"));
}

#[test]
fn config_read_distinguishes_missing_files_from_invalid_or_unreadable_files() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    assert_eq!(read_config(&path).unwrap(), json!({}));
    for bytes in [
        b"".as_slice(),
        b"{\"provider\":",
        b"{} {}",
        b"{\"model\":\"\xff\"}",
        b"null",
        b"[]",
        b"true",
        b"42",
        b"\"openai\"",
    ] {
        fs::write(&path, bytes).unwrap();
        let error = read_config(&path).unwrap_err();
        assert!(format!("{error:#}").contains(&path.display().to_string()));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    let error = read_config(temp.path()).unwrap_err();
    assert!(error.to_string().contains("read config"));
}

#[test]
fn invalid_config_settings_cannot_partially_persist_a_config_action() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    let cli = parse(&["config", "set", "defaultLimit", "5", "--output", "json"]);
    let Command::Config { args } = cli.command else {
        panic!()
    };
    let mut cases = Vec::new();
    for key in ["provider", "descriptionProvider"] {
        for value in [Value::Null, json!(false), json!("unsupported")] {
            cases.push((key, value));
        }
    }
    for key in [
        "model",
        "indexPath",
        "descriptionModel",
        "descriptionFallbackModel",
    ] {
        for value in [Value::Null, json!(42), json!(" \t\n")] {
            cases.push((key, value));
        }
    }
    for key in [
        "dimensions",
        "maxFileSize",
        "embeddingBatchSize",
        "parallelism",
    ] {
        for value in [json!(0), json!(-1), json!(1.5), json!("4"), Value::Null] {
            cases.push((key, value));
        }
    }
    for key in ["rerankingEnabled", "verbose"] {
        for value in [json!("false"), json!(0), Value::Null] {
            cases.push((key, value));
        }
    }
    for key in ["include", "exclude"] {
        for value in [json!("src/**"), json!(["src/**", 1]), json!(["["])] {
            cases.push((key, value));
        }
    }
    cases.extend([
        ("rerankerProvider", json!("unknown")),
        ("rerankerModel", json!("  ")),
        ("rerankerCandidates", json!(0)),
        ("rerankerCandidates", json!(101)),
        ("rerankerCandidates", json!("10")),
    ]);
    for (key, value) in cases {
        let mut config = json!({"rerankingEnabled": true, "rerankerProvider": "openai",
            "custom": {"keep": [1, 2]}});
        config[key] = value;
        let original = serde_json::to_vec(&config).unwrap();
        fs::write(&path, &original).unwrap();
        let mut out = Vec::new();
        let error = run_config(&cli.global, &path, &args, &mut out).unwrap_err();
        assert!(format!("{error:#}").contains(key), "{key}: {error:#}");
        assert!(out.is_empty(), "reported success for invalid {key}");
        assert_eq!(fs::read(&path).unwrap(), original, "modified invalid {key}");
    }
    let absent = temp.path().join("new/config.json");
    assert!(write_config(&absent, &json!({"dimensions": 0})).is_err());
    assert!(!absent.parent().unwrap().exists());
}

#[test]
fn failed_config_replacement_preserves_destination_and_allows_retry() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    // A nonempty directory forces failure after the temporary file is written.
    fs::create_dir(&path).unwrap();
    fs::write(path.join("keep"), b"original").unwrap();
    let config = json!({"parallelism": 3});
    let error = write_config(&path, &config).unwrap_err();
    assert!(error.to_string().contains("save config"));
    assert_eq!(fs::read(path.join("keep")).unwrap(), b"original");
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    fs::remove_file(path.join("keep")).unwrap();
    fs::remove_dir(&path).unwrap();
    write_config(&path, &config).unwrap();
    assert_eq!(read_config(&path).unwrap(), config);
    assert!(fs::read(&path).unwrap().ends_with(b"\n"));
}

fn config_action_json(path: &Path, args: &[&str]) -> Value {
    let cli = parse(&[args, &["--output", "json"]].concat());
    let Command::Config { args } = cli.command else {
        panic!()
    };
    let mut out = Vec::new();
    run_config(&cli.global, path, &args, &mut out).unwrap();
    serde_json::from_slice(&out).unwrap()
}

#[test]
fn unrelated_config_edits_durably_normalize_aliases_without_saving_global_overrides() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    fs::write(
        &path,
        json!({"provider": null, "embeddingProvider": "jina",
        "model": "canonical", "embeddingModel": "obsolete", "dimensions": null,
        "embeddingDimensions": 16, "descriptionFallbackModel": "backup", "fallbackModel": null,
        "custom": {"nested": [null, "λ"]}})
        .to_string(),
    )
    .unwrap();
    let result = config_action_json(
        &path,
        &[
            "config",
            "set",
            "parallelism",
            "4",
            "--provider",
            "openai",
            "--model",
            "transient",
            "--dimensions",
            "8",
            "--no-reindex",
        ],
    );
    assert_eq!(result, json!({"configPath": path, "parallelism": 4}));
    let expected = json!({"provider": "jina", "model": "canonical", "dimensions": 16,
        "descriptionFallbackModel": "backup", "parallelism": 4,
        "custom": {"nested": [null, "λ"]}});
    // Inspect raw JSON: read_config would hide aliases accidentally left on disk.
    let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved, expected);
    config_action_json(&path, &["config", "set", "parallelism", "4"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
        expected
    );
    let effective = effective_config(&parse(&["status"]).global, &path).unwrap();
    assert_eq!(effective["provider"], "jina");
    assert_eq!(effective["dimensions"], 16);
    assert_eq!(effective["noReindex"], false);
}

#[test]
fn config_set_preserves_other_settings_and_accepts_json_values() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    write_config(
        &path,
        &json!({"rerankingEnabled": true, "rerankerProvider": "openai",
        "rerankerModel": "custom-model", "rerankerCandidates": 37, "custom": "keep"}),
    )
    .unwrap();
    assert_eq!(
        config_action_json(&path, &["config", "set", "rerankingEnabled", "false"]),
        json!({"configPath": path, "rerankingEnabled": false})
    );
    let disabled = read_config(&path).unwrap();
    assert_eq!(disabled["rerankerModel"], "custom-model");
    assert_eq!(disabled["rerankerCandidates"], 37);
    config_action_json(&path, &["config", "set", "include", "[\"src/**\"]"]);
    config_action_json(&path, &["config", "set", "rerankerModel", "replacement"]);
    let restored = read_config(&path).unwrap();
    assert_eq!(restored["rerankerModel"], "replacement");
    assert_eq!(restored["rerankerCandidates"], 37);
    assert_eq!(restored["include"], json!(["src/**"]));
    assert_eq!(restored["custom"], "keep");
}

#[test]
fn config_excludes_current_symbol_hashes_offline_and_without_duplicates() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo with spaces");
    fs::create_dir(&root).unwrap();
    let path = temp.path().join("settings/config.json");
    let source = "struct First;\nimpl First { pub fn run() { println!(\"first\"); } }\nstruct Second;\nimpl Second { pub fn run() { println!(\"second\"); } }\npub fn other() {}\n";
    fs::write(root.join("sample.rs"), source).unwrap();
    let parsed = crate::parse::parse("sample.rs", source).unwrap();
    let symbol_hash = |name: &str| {
        parsed
            .callables
            .iter()
            .find(|callable| callable.qualified_name == name)
            .unwrap()
            .source_hash
            .clone()
    };
    let existing = crate::hash("previous exclusion");
    write_config(
        &path,
        &json!({"custom": {"keep": true}, "crossSearchExclusions": [existing]}),
    )
    .unwrap();
    let expected = json!([existing, symbol_hash("First.run"), symbol_hash("other")]);
    for added in [2, 0] {
        let result = config_action_json(
            &path,
            &[
                "config",
                "exclude-cross-search",
                "sample.rs:First.run",
                "sample.rs:other",
                "sample.rs:First.run",
                "--root",
                root.to_str().unwrap(),
                "--provider",
                "jina",
            ],
        );
        assert_eq!(result["total"], 3);
        assert_eq!(result["added"], added);
        assert!(result.get("crossSearchExclusions").is_none());
        let saved = read_config(&path).unwrap();
        assert_eq!(saved["crossSearchExclusions"], expected);
        assert_eq!(saved["custom"], json!({"keep": true}));
        assert!(saved.get("provider").is_none());
    }
    let absolute = format!("{}:Second.run", root.join("sample.rs").display());
    let result = config_action_json(&path, &["config", "exclude-cross-search", &absolute]);
    assert_eq!(result["total"], 4);
    assert_eq!(result["added"], 1);
    assert_eq!(
        read_config(&path).unwrap()["crossSearchExclusions"][3],
        symbol_hash("Second.run")
    );
    assert!(!root.join(".slopdex").exists());
    config_action_json(&path, &["config", "set", "crossSearchExclusions", "[]"]);
    assert_eq!(
        read_config(&path).unwrap()["crossSearchExclusions"],
        json!([])
    );
}

#[test]
fn config_excludes_all_ambiguous_symbol_matches_without_duplicate_hashes() {
    for (file, source, symbol, qualified_names) in [
        (
            "sample.rs",
            "struct First;\nimpl First { fn run() { println!(\"first\"); } }\nstruct Second;\nimpl Second { fn run() { println!(\"second\"); } }\nstruct Third;\nimpl Third { fn run() { println!(\"first\"); } }\nfn other() {}\n",
            "run",
            ["First.run", "Second.run", "Third.run"],
        ),
        (
            "result_ownership.test.ts",
            "test('first', () => { const cancel = () => 1; });\ntest('second', () => { const cancel = () => 2; });\ntest('third', () => { const cancel = () => 1; });\nfunction other() {}\n",
            "cancel",
            ["cancel", "cancel", "cancel"],
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(file), source).unwrap();
        let parsed = crate::parse::parse(file, source).unwrap();
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let matches: Vec<_> = parsed
            .callables
            .iter()
            .filter(|callable| callable.name == symbol)
            .collect();
        assert_eq!(
            matches
                .iter()
                .map(|callable| callable.qualified_name.as_str())
                .collect::<Vec<_>>(),
            qualified_names
        );
        assert_ne!(matches[0].source_hash, matches[1].source_hash);
        assert_eq!(matches[0].source_hash, matches[2].source_hash);
        let path = temp.path().join("config.json");
        let existing = crate::hash("previous exclusion");
        write_config(
            &path,
            &json!({"custom": "keep", "crossSearchExclusions": [existing, matches[0].source_hash]}),
        )
        .unwrap();
        let expected = json!([existing, matches[0].source_hash, matches[1].source_hash]);
        let selector = format!("{file}:{symbol}");
        for added in [1, 0] {
            let result = config_action_json(
                &path,
                &[
                    "config",
                    "exclude-cross-search",
                    &selector,
                    &selector,
                    "--root",
                    temp.path().to_str().unwrap(),
                ],
            );
            assert_eq!(result["total"], 3);
            assert_eq!(result["added"], added);
            assert!(result.get("crossSearchExclusions").is_none());
            let saved = read_config(&path).unwrap();
            assert_eq!(saved["crossSearchExclusions"], expected);
            assert_eq!(saved["custom"], "keep");
        }
    }
}

#[test]
fn invalid_cross_search_selectors_do_not_partially_save_exclusions() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("sample.rs");
    fs::write(
        &source,
        "struct First;\nimpl First { fn run() {} }\nstruct Second;\nimpl Second { fn run() {} }\nfn good() {}\nconst VALUE: u32 = 1;\n",
    )
    .unwrap();
    let path = temp.path().join("config.json");
    write_config(&path, &json!({"custom": "keep"})).unwrap();
    let original = fs::read(&path).unwrap();
    let good = format!("{}:good", source.display());
    for (selectors, message) in [
        (vec![], "usage:"),
        (vec!["missing-separator".into()], "expected <file>:<symbol>"),
        (vec![":good".into()], "expected <file>:<symbol>"),
        (
            vec![format!("{}:", source.display())],
            "expected <file>:<symbol>",
        ),
        (
            vec![format!("{}:VALUE", source.display())],
            "no callable symbol",
        ),
        (
            vec![format!("{}:missing", source.display())],
            "no callable symbol",
        ),
        (
            vec![good.clone(), format!("{}:missing", source.display())],
            "no callable symbol",
        ),
        (
            vec![
                format!("{}:run", source.display()),
                format!("{}:missing", source.display()),
            ],
            "no callable symbol",
        ),
        (
            vec![format!("{}:good", temp.path().join("missing.rs").display())],
            "read source",
        ),
    ] {
        let cli = parse(&["config", "exclude-cross-search"]);
        let args: Vec<_> = std::iter::once("exclude-cross-search".to_owned())
            .chain(selectors)
            .collect();
        let mut out = Vec::new();
        let error = run_config(&cli.global, &path, &args, &mut out).unwrap_err();
        assert!(error.to_string().contains(message), "{error:#}");
        assert!(out.is_empty());
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}

#[test]
fn config_exclusion_dispatch_creates_config_without_an_index() {
    let temp = tempfile::tempdir().unwrap();
    let source = "pub fn run() { println!(\"first\"); }\n";
    fs::write(temp.path().join("sample.rs"), source).unwrap();
    let path = temp.path().join(".slopdex/config.json");
    let cli = parse(&[
        "config",
        "exclude-cross-search",
        "sample.rs:run",
        "--root",
        temp.path().to_str().unwrap(),
        "--config",
        path.to_str().unwrap(),
        "--output",
        "json",
    ]);
    let mut out = Vec::new();
    super::super::run_cli(&cli, &mut out).unwrap();
    let old_hash = crate::parse::parse("sample.rs", source).unwrap().callables[0]
        .source_hash
        .clone();
    let result: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        Path::new(result["configPath"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        path.canonicalize().unwrap()
    );
    assert_eq!(result["total"], 1);
    assert_eq!(result["added"], 1);
    assert!(result.get("crossSearchExclusions").is_none());
    assert_eq!(
        read_config(&path).unwrap()["crossSearchExclusions"],
        json!([old_hash])
    );

    let changed = source.replace("first", "changed");
    fs::write(temp.path().join("sample.rs"), &changed).unwrap();
    super::super::run_cli(&cli, &mut Vec::new()).unwrap();
    let new_hash = crate::parse::parse("sample.rs", &changed)
        .unwrap()
        .callables[0]
        .source_hash
        .clone();
    assert_ne!(old_hash, new_hash);
    assert_eq!(
        read_config(&path).unwrap()["crossSearchExclusions"],
        json!([old_hash, new_hash])
    );
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn cross_search_exclusions_require_an_array_of_code_hashes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    write_config(&path, &json!({"custom": "keep"})).unwrap();
    let original = fs::read(&path).unwrap();
    let cli = parse(&["config"]);
    for invalid in [
        json!(null),
        json!(crate::hash("code")),
        json!([1]),
        json!([""]),
        json!(["not-a-hash"]),
        json!(["g".repeat(64)]),
    ] {
        let args = vec![
            "set".into(),
            "crossSearchExclusions".into(),
            invalid.to_string(),
        ];
        let error = run_config(&cli.global, &path, &args, &mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("crossSearchExclusions"));
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}

#[test]
fn config_set_rejects_unknown_keys_with_suggestions_and_accepts_documented_keys() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    write_config(&path, &json!({"custom": "keep"})).unwrap();
    let original = fs::read(&path).unwrap();
    for (key, suggestion) in [
        ("descriptonModel", "descriptionModel"),
        ("embeddingapikey", "embeddingApiKey"),
        ("verbosity", "verbose"),
        ("parallelism.limit", "parallelism"),
    ] {
        let cli = parse(&["config", "set", key, "value"]);
        let Command::Config { args } = cli.command else {
            panic!()
        };
        let mut out = Vec::new();
        let error = run_config(&cli.global, &path, &args, &mut out)
            .unwrap_err()
            .to_string();
        assert!(error.contains(suggestion), "{key}: {error}");
        assert!(out.is_empty());
        assert_eq!(fs::read(&path).unwrap(), original, "{key}");
    }
    let error = super::check_settable_key("zzzzzzzz")
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown configuration key zzzzzzzz"));
    assert!(!error.contains("did you mean"), "{error}");
    for key in wizard_keys().iter().chain(super::ADVANCED_KEYS) {
        super::check_settable_key(key).unwrap_or_else(|e| panic!("{key}: {e}"));
    }
    super::check_settable_key("artifactS3.pathStyle").unwrap();
    assert!(super::check_settable_key("configOrigin").is_err());
    assert!(super::check_settable_key("trustRepositoryEndpoints").is_err());
    assert_eq!(
        config_action_json(
            &path,
            &["config", "set", "trustedEndpointHosts", "[\"gw.invalid\"]"]
        )["trustedEndpointHosts"],
        json!(["gw.invalid"])
    );
    let cli = parse(&["config", "set", "trustedEndpointHosts", "[\"\"]"]);
    let Command::Config { args } = cli.command else {
        panic!()
    };
    assert!(run_config(&cli.global, &path, &args, &mut Vec::new()).is_err());
}

fn wizard_keys() -> &'static [&'static str] {
    super::wizard::CONFIG_KEYS
}

#[test]
fn provider_switch_keeps_the_embedding_api_key_and_resets_documented_keys() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    write_config(
        &path,
        &json!({"provider": "openai", "model": "text-embedding-3-small", "dimensions": 1536,
            "embeddingApiKey": "keep-secret", "openaiApiKey": "also-keep",
            "embeddingBaseUrl": "https://old.invalid/v1", "embeddingParallelism": 3,
            "ollamaKeepAlive": "1m", "ollamaAutoPull": true, "ollamaPullTimeoutMs": 5,
            "ollamaModelDigest": "d", "ollamaNativeDimensions": 4, "ollamaContextLength": 8}),
    )
    .unwrap();
    config_action_json(&path, &["config", "set", "provider", "jina"]);
    let saved = read_config(&path).unwrap();
    assert_eq!(saved["embeddingApiKey"], "keep-secret");
    assert_eq!(saved["openaiApiKey"], "also-keep");
    assert_eq!(saved["provider"], "jina");
    assert_eq!(saved["model"], "jina-embeddings-v4");
    assert_eq!(saved["dimensions"], 1024);
    for key in [
        "embeddingBaseUrl",
        "embeddingParallelism",
        "ollamaKeepAlive",
        "ollamaAutoPull",
        "ollamaPullTimeoutMs",
        "ollamaModelDigest",
        "ollamaNativeDimensions",
        "ollamaContextLength",
    ] {
        assert!(saved.get(key).is_none(), "{key} should reset");
    }
}

#[test]
fn config_renderings_redact_api_keys() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    let result = config_action_json(
        &path,
        &["config", "set", "embeddingApiKey", "sk-live-secret"],
    );
    assert_eq!(result["embeddingApiKey"], "<redacted>");
    assert_eq!(
        read_config(&path).unwrap()["embeddingApiKey"],
        "sk-live-secret"
    );
    let result = config_action_json(&path, &["config", "set", "openaiApiKey", "sk-other-secret"]);
    assert_eq!(result["openaiApiKey"], "<redacted>");
    assert!(!result.to_string().contains("secret"));
    let dumped = config_action_json(
        &path,
        &[
            "config",
            "embeddings",
            "--provider",
            "openai",
            "--model",
            "text-embedding-3-small",
        ],
    );
    assert_eq!(dumped["embeddingApiKey"], "<redacted>");
    assert_eq!(dumped["openaiApiKey"], "<redacted>");
    assert_eq!(dumped["model"], "text-embedding-3-small");
    assert!(!dumped.to_string().contains("secret"));
    assert_eq!(
        read_config(&path).unwrap()["openaiApiKey"],
        "sk-other-secret"
    );
    assert_eq!(
        super::redacted(
            &json!({"a": {"xApiKey": "s", "n": null}, "list": [{"rerankerApiKey": 1}], "ApiKeyName": "visible", "nullApiKey": null})
        ),
        json!({"a": {"xApiKey": "<redacted>", "n": null}, "list": [{"rerankerApiKey": "<redacted>"}], "ApiKeyName": "visible", "nullApiKey": null})
    );
}

#[cfg(unix)]
#[test]
fn config_files_keep_their_mode_and_secrets_become_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    write_config(&path, &json!({"parallelism": 2})).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    write_config(&path, &json!({"parallelism": 3})).unwrap();
    assert_eq!(mode(&path), 0o640, "existing mode is preserved");
    write_config(&path, &json!({"parallelism": 3, "openaiApiKey": "secret"})).unwrap();
    assert_eq!(mode(&path), 0o600, "secrets tighten to owner-only");
    write_config(&path, &json!({"parallelism": 4})).unwrap();
    assert_eq!(mode(&path), 0o600, "a tightened mode stays");
    let fresh = temp.path().join("fresh/config.json");
    write_config(&fresh, &json!({"embeddingApiKey": "secret"})).unwrap();
    assert_eq!(mode(&fresh), 0o600);
    fs::set_permissions(&fresh, fs::Permissions::from_mode(0o400)).unwrap();
    write_config(&fresh, &json!({"embeddingApiKey": "rotated"})).unwrap();
    assert_eq!(mode(&fresh), 0o400, "never wider than before");
    assert_eq!(read_config(&fresh).unwrap()["embeddingApiKey"], "rotated");
}

#[test]
fn repository_config_marking_adopts_only_the_user_trust_list() {
    use crate::providers::{CONFIG_ORIGIN_KEY, TRUST_REPOSITORY_ENDPOINTS_KEY};
    let temp = tempfile::tempdir().unwrap();
    let user = temp.path().join("user/config.json");
    let repository = json!({"embeddingBaseUrl": "https://gw.invalid/v1",
        "trustedEndpointHosts": ["gw.invalid"], TRUST_REPOSITORY_ENDPOINTS_KEY: true});
    let mut config = repository.clone();
    super::mark_repository_config_from(&mut config, &parse(&["status"]).global, &user).unwrap();
    assert_eq!(config[CONFIG_ORIGIN_KEY], "repository");
    assert!(config.get("trustedEndpointHosts").is_none(), "{config}");
    assert!(config.get(TRUST_REPOSITORY_ENDPOINTS_KEY).is_none());
    assert_eq!(config["embeddingBaseUrl"], repository["embeddingBaseUrl"]);

    write_config(&user, &json!({"trustedEndpointHosts": ["other.invalid"]})).unwrap();
    let mut config = repository.clone();
    super::mark_repository_config_from(
        &mut config,
        &parse(&["--trust-repo-endpoints", "status"]).global,
        &user,
    )
    .unwrap();
    assert_eq!(config["trustedEndpointHosts"], json!(["other.invalid"]));
    assert_eq!(config[TRUST_REPOSITORY_ENDPOINTS_KEY], true);

    fs::write(&user, r#"{"trustedEndpointHosts": "gw.invalid"}"#).unwrap();
    let mut config = repository.clone();
    let error = super::mark_repository_config_from(&mut config, &parse(&["status"]).global, &user)
        .unwrap_err()
        .to_string();
    assert!(error.contains("trustedEndpointHosts"), "{error}");
    assert!(
        effective_config(&parse(&["status"]).global, &user).is_err(),
        "the user config itself rejects an invalid trust list"
    );
}
