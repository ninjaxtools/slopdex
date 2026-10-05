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
fn selected_root_does_not_rebase_explicit_config_or_saved_index_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo with spaces");
    let cwd = std::env::current_dir().unwrap();
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
            .join("workspaces")
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
    let saved = json!({"embeddingProvider": "jina", "embeddingModel": "old-model",
        "embeddingDimensions": 1024, "custom": "keep"});
    fs::write(&path, saved.to_string()).unwrap();
    for (args, expected) in [
        (
            vec!["status"],
            json!({"provider": "jina", "model": "old-model", "dimensions": 1024}),
        ),
        (
            vec!["status", "--provider", "openai"],
            json!({"provider": "openai", "model": "old-model", "dimensions": 1024}),
        ),
        (
            vec!["status", "--model", "new-model"],
            json!({"provider": "jina", "model": "new-model", "dimensions": 1024}),
        ),
        (
            vec!["status", "--dimensions", "8"],
            json!({"provider": "jina", "model": "old-model", "dimensions": 8}),
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
            "descriptionsEnabled",
            "true",
            "--format",
            "json",
        ],
        vec!["config", "set", "parallelism", "4", "--format", "json"],
        vec![
            "config",
            "set",
            "rerankerProvider",
            "openai",
            "--format",
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
    let cli = parse(&[
        "config",
        "set",
        "descriptionsEnabled",
        "true",
        "--format",
        "json",
    ]);
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
    let cli = parse(&[args, &["--format", "json"]].concat());
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
