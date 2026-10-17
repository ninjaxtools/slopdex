use super::*;

fn node<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    match value {
        Value::Object(object) => {
            if value["name"] == name {
                return Some(value);
            }
            object.values().find_map(|value| node(value, name))
        }
        Value::Array(values) => values.iter().find_map(|value| node(value, name)),
        _ => None,
    }
}

fn counts(value: &Value) -> Vec<(String, u64)> {
    let mut result = Vec::new();
    if let (Some(name), Some(lines)) = (value["name"].as_str(), value["lines"].as_u64()) {
        result.push((name.to_owned(), lines));
    }
    match value {
        Value::Object(object) => result.extend(object.values().flat_map(counts)),
        Value::Array(values) => result.extend(values.iter().flat_map(counts)),
        _ => {}
    }
    result.sort();
    result
}

#[test]
fn lines_cli_full_file_counts_formats_globs_and_exclusions_match_indexed_counts() -> Result<()> {
    let repo = Repo::new()?;
    let config = json!({
        "artifactCachePath": repo.home.join("cache/slopdex/global-v1.sqlite"),
        "defaultLimit": 1
    });
    let settings = repo.home.join("settings.json");
    fs::write(&settings, config.to_string())?;
    repo.write("src/main.rs", "// Header\n\nfn private() {\n    let text = \"// literal\"; // trailing\n}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn check() {}\n}\n")?;
    repo.write("src/api.test.ts", "test('works', () => {});\n")?;
    repo.write("src/small.ts", "const value = 1;")?;
    repo.write(
        "docs/guide.md",
        "Plain prose without headings.\n\nMore prose.\n",
    )?;
    repo.write("config/settings.yaml", "# comment\nport: 80\n")?;
    repo.write("src/empty.rs", "")?;
    let before = filesystem_snapshot(repo._temp.path())?;
    let mut direct = Vec::new();
    for indexed in [false, true] {
        if indexed {
            repo.open_map(&config)?.refresh_structure()?;
        }
        for extra in [
            vec![],
            vec!["--include-blank-lines"],
            vec!["--formats", "all"],
            vec!["--exclude-tests", "--exclude-comments"],
            vec!["-g", "*.ts", "-g", "!*.test.ts"],
        ] {
            let mut args = vec!["--config", settings.to_str().unwrap(), "lines"];
            args.extend(&extra);
            let value = repo.cli_json(&args)?;
            if !indexed {
                direct.push(value.clone());
                assert!(!repo.index.exists());
            } else {
                assert_eq!(counts(&value), counts(&direct.remove(0)), "{args:?}");
            }
            if extra.is_empty() || extra[0] == "--include-blank-lines" {
                let include_blanks = !extra.is_empty();
                assert_eq!(
                    node(&value, "main.rs").unwrap()["lines"],
                    if include_blanks { 10 } else { 9 }
                );
                assert_eq!(
                    node(&value, "guide.md").unwrap()["lines"],
                    if include_blanks { 3 } else { 2 }
                );
                assert_eq!(node(&value, "empty.rs").unwrap()["lines"], 0);
                assert!(
                    node(&value, "small.ts").is_some(),
                    "defaultLimit must not limit files"
                );
                assert!(node(&value, "settings.yaml").is_none());
            } else if extra[0] == "--formats" {
                assert_eq!(node(&value, "settings.yaml").unwrap()["lines"], 2);
            } else if extra[0] == "--exclude-tests" {
                assert_eq!(node(&value, "main.rs").unwrap()["lines"], 3);
                assert!(node(&value, "api.test.ts").is_none());
            } else {
                assert!(node(&value, "small.ts").is_some());
                assert!(node(&value, "api.test.ts").is_none());
                assert!(node(&value, "main.rs").is_none());
            }
        }
        if !indexed {
            assert_eq!(
                filesystem_snapshot(repo._temp.path())?,
                before,
                "local counts must not create caches"
            );
        }
    }
    let output = repo.cli_format(
        &[
            "--config",
            settings.to_str().unwrap(),
            "lines",
            "src",
            "--limit",
            "1",
        ],
        "text",
    )?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout)?;
    assert!(text.contains("main.rs"), "{text}");
    assert!(!text.contains("small.ts"), "{text}");
    Ok(())
}

#[test]
fn lines_cli_regex_counts_full_unique_matching_source_without_ancestor_bodies() -> Result<()> {
    let repo = Repo::new()?;
    repo.write(
        "src/api.ts",
        "export class Api {\n  wanted() {\n    const x = 1;\n    return x;\n  }\n  other() {}\n}\n",
    )?;
    repo.write(
        "docs/guide.md",
        "# Guide\n\nParent body.\n\n## Child\n\nChild body.\n",
    )?;
    for indexed in [false, true] {
        if indexed {
            repo.open_map(&json!({}))?.refresh_structure()?;
        }
        let selected = repo.cli_json(&["lines", "-e", "WANTED", "-i"])?;
        assert_eq!(node(&selected, "api.ts").unwrap()["lines"], 4);
        assert!(node(&selected, "guide.md").is_none());
        let overlap = repo.cli_json(&["lines", "-e", "Api", "-e", "wanted"])?;
        assert_eq!(node(&overlap, "api.ts").unwrap()["lines"], 7);
        for include in [false, true] {
            let mut args = vec!["lines", "-e", "^Guide$"];
            if include {
                args.push("--include-blank-lines");
            }
            let selected = repo.cli_json(&args)?;
            assert_eq!(
                node(&selected, "guide.md").unwrap()["lines"],
                if include { 3 } else { 2 }
            );
            assert!(node(&selected, "api.ts").is_none());
        }
    }
    Ok(())
}

#[test]
fn lines_cli_blank_line_policy_applies_to_totals_and_limits() -> Result<()> {
    let repo = Repo::new()?;
    repo.write("a.rs", "fn compact() {}\n \t\n\n \t")?;
    repo.write("b.rs", "fn substantial() {\n work();\n}\n")?;
    repo.write("blank.rs", "\n \t\r\n \t")?;
    for indexed in [false, true] {
        if indexed {
            repo.open_map(&json!({}))?.refresh_structure()?;
        }
        for include in [false, true] {
            let mut args = vec!["lines"];
            if include {
                args.push("--include-blank-lines");
            }
            let tree = repo.cli_json(&args)?;
            assert_eq!(tree["lines"], if include { 10 } else { 4 });
            assert_eq!(
                node(&tree, "a.rs").unwrap()["lines"],
                if include { 4 } else { 1 }
            );
            assert_eq!(
                node(&tree, "blank.rs").unwrap()["lines"],
                if include { 3 } else { 0 }
            );
            args.extend(["--limit", "1"]);
            let limited = repo.cli_json(&args)?;
            assert_eq!(limited["lines"], tree["lines"]);
            assert_eq!(node(&limited, "a.rs").is_some(), include);
            assert_eq!(node(&limited, "b.rs").is_some(), !include);
            assert!(node(&limited, "blank.rs").is_none());
        }
    }
    Ok(())
}

#[test]
fn lines_cli_semantic_queries_create_only_structure_and_union_with_regex_and_reuse_snapshot()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write(".slopdex/config.json", &config.to_string())?;
    repo.write(
        "src/north.ts",
        "function VectorNorth() {\n \t\n  return 1;\n}\n",
    )?;
    repo.write("src/west.ts", "function VectorWest() {\n  return 2;\n}\n")?;
    repo.write("src/other.ts", "function Ordinary() {\n  return 3;\n}\n")?;
    let selected = repo.cli_json(&["lines", "-q", "VECTOR_NORTH", "-e", "Ordinary"])?;
    assert_eq!(node(&selected, "north.ts").unwrap()["lines"], 3);
    assert_eq!(node(&selected, "other.ts").unwrap()["lines"], 3);
    assert!(node(&selected, "west.ts").is_none());
    assert!(repo.index.exists());
    assert!(mock.requests("/responses").is_empty());
    let calls = mock.count();
    let included = repo.cli_json(&[
        "--no-reindex",
        "lines",
        "-q",
        "VECTOR_NORTH",
        "-e",
        "Ordinary",
        "--include-blank-lines",
    ])?;
    assert_eq!(node(&included, "north.ts").unwrap()["lines"], 4);
    assert_eq!(node(&included, "other.ts").unwrap()["lines"], 3);
    assert_eq!(
        mock.count(),
        calls,
        "blank-line policy must reuse semantic selection vectors"
    );
    repo.write("src/north.ts", "function Changed() {}\n")?;
    let saved = repo.cli_json(&[
        "--no-reindex",
        "lines",
        "-q",
        "VECTOR_NORTH",
        "-e",
        "Ordinary",
    ])?;
    assert_eq!(counts(&saved), counts(&selected));
    assert_eq!(
        mock.count(),
        calls,
        "cached name queries must not call providers again"
    );
    let repeated = repo.cli_json(&[
        "--no-reindex",
        "lines",
        "-q",
        "VECTOR_NORTH",
        "-q",
        "VECTOR_WEST",
    ])?;
    assert_eq!(node(&repeated, "north.ts").unwrap()["lines"], 3);
    assert_eq!(node(&repeated, "west.ts").unwrap()["lines"], 3);
    assert!(node(&repeated, "other.ts").is_none());
    Ok(())
}

#[test]
fn lines_cli_multiple_workspaces_share_tree_and_global_limit() -> Result<()> {
    let repo = Repo::new()?;
    repo.write("a.rs", "fn a() {}\n")?;
    let external = repo._temp.path().canonicalize()?.join("external");
    fs::create_dir_all(external.join("src"))?;
    fs::write(external.join("src/b.rs"), "fn b() {\n}\n")?;
    let value = repo.cli_json(&["lines", "a.rs", external.to_str().unwrap()])?;
    assert_eq!(node(&value, "a.rs").unwrap()["lines"], 1);
    assert_eq!(node(&value, "b.rs").unwrap()["lines"], 2);
    let limited = repo.cli_json(&["lines", "a.rs", external.to_str().unwrap(), "--limit", "1"])?;
    // Directory and its file tie: either the directory wins, or the file is
    // displayed under that directory. Neither case may expand unrelated files.
    assert!(node(&limited, "external").is_some());
    assert!(node(&limited, "a.rs").is_none());
    let overlap = repo.cli_json(&[
        "lines",
        external.to_str().unwrap(),
        external.join("src/b.rs").to_str().unwrap(),
    ])?;
    assert_eq!(
        overlap["lines"], 2,
        "overlapping workspace paths must not count the file twice"
    );
    Ok(())
}

#[test]
fn lines_cli_no_reindex_uses_saved_whole_files_and_never_ignores_queries() -> Result<()> {
    let repo = Repo::new()?;
    repo.write("source.rs", "fn original() {\n}\n")?;
    let missing = repo.cli(&["--no-reindex", "lines", "-q", "original"])?;
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("run `slopdex update` first"));
    assert!(missing.stdout.is_empty());
    assert!(!repo.index.exists());
    repo.open_map(&json!({}))?.refresh_structure()?;
    repo.write("source.rs", "fn changed() {}\n")?;
    let saved = repo.cli_json(&["--no-reindex", "lines"])?;
    assert_eq!(node(&saved, "source.rs").unwrap()["lines"], 2);
    let refreshed = repo.cli_json(&["lines"])?;
    assert_eq!(node(&refreshed, "source.rs").unwrap()["lines"], 1);
    Ok(())
}

#[cfg(unix)]
#[test]
fn lines_cli_resolves_paths_through_symlinked_working_directory() -> Result<()> {
    let repo = Repo::new()?;
    repo.write("src/source.rs", "fn source() {\n}\n")?;
    let link = repo._temp.path().canonicalize()?.join("linked");
    std::os::unix::fs::symlink(&repo.root, &link)?;
    let output = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .current_dir(&link)
        .args(["lines", "src/source.rs", "--output", "json"])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(node(&value, "source.rs").unwrap()["lines"], 2);
    Ok(())
}
