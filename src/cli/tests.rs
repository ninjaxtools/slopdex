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
            "--format",
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
