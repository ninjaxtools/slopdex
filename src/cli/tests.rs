use super::{test_support::parse, *};

#[test]
fn external_source_saved_config_paths_use_its_directory() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let current = dir.path().join("current");
    let external = dir.path().join("external");
    std::fs::create_dir(&current)?;
    std::fs::create_dir_all(external.join(".slopdex"))?;
    std::fs::write(external.join("code.rs"), "pub fn cached() {}\n")?;
    let config = json!({"artifactCachePath": dir.path().join("artifacts.sqlite")});
    let index = external.join("saved.sqlite");
    let mut engine = Engine::open_map(&external, &index, config.clone())?;
    engine.refresh_structure()?;
    drop(engine);
    let mut saved = config;
    saved["indexPath"] = json!("saved.sqlite");
    std::fs::write(external.join(".slopdex/config.json"), saved.to_string())?;
    std::fs::write(external.join("code.rs"), "pub fn changed() {}\n")?;
    let mut out = Vec::new();
    run_cli(
        &parse(&[
            "--root",
            current.to_str().unwrap(),
            "--output",
            "json",
            "--no-reindex",
            "map",
            external.join("code.rs").to_str().unwrap(),
        ]),
        &mut out,
    )?;
    let rows: Value = serde_json::from_slice(&out)?;
    assert_eq!(rows[0]["path"], "code.rs");
    assert_eq!(rows[0]["nodes"][0]["name"], "cached");
    Ok(())
}

#[test]
fn symbol_search_requires_existing_index_even_without_refresh() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let index = dir.path().join("missing.sqlite");
    for command in [
        vec!["search-symbols", "query"],
        vec!["search", "query", "--symbols"],
    ] {
        for no_reindex in [false, true] {
            let mut argv = command.clone();
            argv.extend([
                "--root",
                dir.path().to_str().unwrap(),
                "--index",
                index.to_str().unwrap(),
            ]);
            if no_reindex {
                argv.push("--no-reindex");
            }
            let mut out = Vec::new();
            let error = run_cli(&parse(&argv), &mut out).unwrap_err();
            assert!(
                error.to_string().contains("run `slopdex update` first"),
                "{error}"
            );
            assert!(out.is_empty());
            assert!(!index.exists());
        }
    }
    Ok(())
}

#[test]
fn map_cli_uses_config_limits_explicit_overrides_and_actual_omission_notices() -> Result<()> {
    for indexed in [false, true] {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("code.rs"),
            "pub fn first() {}\npub fn second() {}\npub fn third() {}\n",
        )?;
        let index = dir.path().join("index.sqlite");
        let config_file = dir.path().join("config.json");
        let mut config =
            json!({"artifactCachePath": dir.path().join("artifacts.sqlite"), "defaultLimit": 2});
        if indexed {
            let mut engine = Engine::open_map(dir.path(), &index, config.clone())?;
            engine.refresh_structure()?;
        }
        for (default, explicit, expected) in [
            (json!(2), None, 2),
            (json!(2), Some("1"), 1),
            (json!(2), Some("none"), 3),
            (json!("none"), None, 3),
            (json!("none"), Some("1"), 1),
            (json!(3), None, 3),
        ] {
            config["defaultLimit"] = default;
            std::fs::write(&config_file, config.to_string())?;
            for format in ["json", "text"] {
                let mut argv = vec![
                    "--root",
                    dir.path().to_str().unwrap(),
                    "--config",
                    config_file.to_str().unwrap(),
                    "--index",
                    index.to_str().unwrap(),
                    "--output",
                    format,
                    "--no-reindex",
                    "map",
                ];
                if let Some(explicit) = explicit {
                    argv.extend(["--limit", explicit]);
                }
                let mut out = Vec::new();
                run_cli(&parse(&argv), &mut out)?;
                if format == "json" {
                    let rows: Value = serde_json::from_slice(&out)?;
                    assert_eq!(
                        rows[0]["nodes"].as_array().unwrap().len(),
                        expected,
                        "{argv:?}"
                    );
                } else {
                    let output = String::from_utf8(out)?;
                    assert_eq!(
                        output.contains("results omitted by limit"),
                        expected < 3,
                        "{argv:?}: {output}"
                    );
                }
            }
        }
    }
    Ok(())
}

#[test]
fn unindexed_map_cli_builtin_limit_is_4096_declarations() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = (0..4097)
        .map(|index| format!("pub fn f{index}() {{}}\n"))
        .collect::<String>();
    std::fs::write(dir.path().join("code.rs"), source)?;
    let index = dir.path().join("missing.sqlite");
    let config_file = dir.path().join("config.json");
    std::fs::write(
        &config_file,
        json!({"artifactCachePath": dir.path().join("artifacts.sqlite")}).to_string(),
    )?;
    let mut out = Vec::new();
    run_cli(
        &parse(&[
            "--root",
            dir.path().to_str().unwrap(),
            "--index",
            index.to_str().unwrap(),
            "--config",
            config_file.to_str().unwrap(),
            "--output",
            "json",
            "map",
        ]),
        &mut out,
    )?;
    let rows: Value = serde_json::from_slice(&out)?;
    assert_eq!(rows[0]["nodes"].as_array().unwrap().len(), 4096);
    assert!(!index.exists());
    Ok(())
}

#[test]
fn multi_workspace_map_shares_declaration_budget_without_counting_ancestor_context() -> Result<()> {
    for indexed in [[false, false], [true, true], [true, false], [false, true]] {
        let dir = tempfile::tempdir()?;
        let current = dir.path().join("current");
        std::fs::create_dir(&current)?;
        let roots = [dir.path().join("first"), dir.path().join("second")];
        for (i, root) in roots.iter().enumerate() {
            std::fs::create_dir_all(root.join(".slopdex"))?;
            let source = if i == 0 {
                "pub struct A;\nimpl A {\n pub fn one(&self) {}\n pub fn two(&self) {}\n}\n"
            } else {
                "pub struct B;\nimpl B {\n pub fn three(&self) {}\n pub fn four(&self) {}\n}\n"
            };
            std::fs::write(root.join("api.rs"), source)?;
            let index = root.join("index.sqlite");
            let config = json!({"artifactCachePath": root.join("artifacts.sqlite"), "indexPath": index, "defaultLimit": if i == 0 { 2 } else { 99 }});
            std::fs::write(root.join(".slopdex/config.json"), config.to_string())?;
            if indexed[i] {
                let mut engine = Engine::open_map(root, &index, config)?;
                engine.refresh_structure()?;
            }
        }
        for (explicit, regexp, expected, omitted) in [
            (None, None, 2, true),
            (Some("3"), None, 3, true),
            (Some("4"), None, 4, false),
            (Some("none"), None, 4, false),
            (Some("2"), Some("^A"), 2, false),
        ] {
            for format in ["json", "text"] {
                let mut argv = vec![
                    "--root",
                    current.to_str().unwrap(),
                    "--output",
                    format,
                    "--no-reindex",
                    "map",
                    roots[0].to_str().unwrap(),
                    roots[1].to_str().unwrap(),
                    "-k",
                    "methods",
                ];
                if let Some(explicit) = explicit {
                    argv.extend(["--limit", explicit]);
                }
                if let Some(regexp) = regexp {
                    argv.extend(["-e", regexp]);
                }
                let mut out = Vec::new();
                run_cli(&parse(&argv), &mut out)?;
                if format == "json" {
                    let rows: Vec<Value> = serde_json::from_slice(&out)?;
                    let nodes: Vec<_> = rows
                        .iter()
                        .flat_map(|row| row["nodes"].as_array().unwrap())
                        .collect();
                    assert_eq!(
                        nodes.iter().filter(|node| node["kind"] == "method").count(),
                        expected,
                        "{argv:?}"
                    );
                    assert!(
                        nodes.len() > expected,
                        "ancestor context must remain: {argv:?}"
                    );
                } else {
                    let output = String::from_utf8(out)?;
                    assert_eq!(
                        output.matches("results omitted by limit").count(),
                        usize::from(omitted),
                        "{argv:?}: {output}"
                    );
                }
            }
        }
    }
    Ok(())
}

#[test]
fn unindexed_multi_workspace_map_builtin_limit_is_global() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let current = dir.path().join("current");
    std::fs::create_dir(&current)?;
    let roots = [dir.path().join("first"), dir.path().join("second")];
    for root in &roots {
        std::fs::create_dir_all(root.join(".slopdex"))?;
        let source = (0..2049)
            .map(|index| format!("pub fn f{index}() {{}}\n"))
            .collect::<String>();
        std::fs::write(root.join("code.rs"), source)?;
        std::fs::write(root.join(".slopdex/config.json"), json!({"indexPath": root.join("missing.sqlite"), "artifactCachePath": root.join("artifacts.sqlite")}).to_string())?;
    }
    let mut out = Vec::new();
    run_cli(
        &parse(&[
            "--root",
            current.to_str().unwrap(),
            "--output",
            "json",
            "map",
            roots[0].to_str().unwrap(),
            roots[1].to_str().unwrap(),
        ]),
        &mut out,
    )?;
    let rows: Vec<Value> = serde_json::from_slice(&out)?;
    assert_eq!(
        rows.iter()
            .map(|row| row["nodes"].as_array().unwrap().len())
            .sum::<usize>(),
        4096
    );
    Ok(())
}
