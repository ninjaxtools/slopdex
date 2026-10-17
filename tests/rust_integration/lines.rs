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

fn line_flags(bits: usize) -> Vec<&'static str> {
    [
        (1, "--include-blank-lines"),
        (2, "--exclude-comments"),
        (4, "--exclude-tests"),
    ]
    .into_iter()
    .filter_map(|(bit, flag)| (bits & bit != 0).then_some(flag))
    .collect()
}

fn line_stats_cache(repo: &Repo) -> Result<BTreeMap<String, [usize; 8]>> {
    let db = repo.db()?;
    let rows = db
        .prepare("SELECT key,value FROM global.cache WHERE kind='line-stats'")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(key, value)| Ok((key, serde_json::from_str(&value)?)))
        .collect()
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
fn lines_cli_cached_totals_match_cache_free_counts_for_all_eight_flag_combinations() -> Result<()> {
    // Intentionally non-Git: the exact LF/CRLF and unterminated bytes are fixtures.
    let repo = Repo::new()?;
    repo.write(
        "src/main.rs",
        "// Header\n\nfn production() {\n    let text = \"// λ literal\"; // trailing\n}\n#[cfg(test)]\nmod tests {\n    // Test comment\n\n    #[test]\n    fn check() {}\n}\n",
    )?;
    repo.write(
        "src/api.test.ts",
        "// Test file\n\ntest('works', () => {\n  expect(true).toBe(true);\n});\n",
    )?;
    repo.write(
        "src/tail.ts",
        "/* Header\r\n\r\n */\r\n\r\nconst text = 'λ'; // trailing",
    )?;
    repo.write("docs/guide.md", "# Guide\n\nPlain prose.\n")?;
    repo.write("src/empty.rs", "")?;
    repo.write("src/blank.rs", "\n \t\r\n \t")?;
    let files = [
        "main.rs",
        "api.test.ts",
        "tail.ts",
        "guide.md",
        "empty.rs",
        "blank.rs",
    ];
    let before = filesystem_snapshot(repo._temp.path())?;
    let mut baseline = Vec::new();
    for bits in 0..8 {
        let mut args = vec!["lines"];
        args.extend(line_flags(bits));
        baseline.push(repo.cli_json(&args)?);
    }
    assert!(!repo.index.exists());
    assert_eq!(filesystem_snapshot(repo._temp.path())?, before);

    repo.open_map(&json!({}))?.refresh_structure()?;
    assert!(line_stats_cache(&repo)?.is_empty(), "indexing is lazy");
    for (bits, direct) in baseline.iter().enumerate() {
        let mut args = vec!["lines"];
        args.extend(line_flags(bits));
        assert_eq!(counts(&repo.cli_json(&args)?), counts(direct), "{args:?}");
    }
    let cached = line_stats_cache(&repo)?;
    assert_eq!(
        cached.len(),
        files.len(),
        "one fixed array per file identity"
    );
    let mut expected: Vec<[usize; 8]> = files
        .iter()
        .map(|name| {
            std::array::from_fn(|bits| {
                node(&baseline[bits], name)
                    .and_then(|file| file["lines"].as_u64())
                    .unwrap_or(0) as usize
            })
        })
        .collect();
    let mut actual: Vec<_> = cached.values().copied().collect();
    expected.sort();
    actual.sort();
    assert_eq!(actual, expected, "cache slots use the three flag bits");
    for _ in 0..2 {
        for (bits, direct) in baseline.iter().enumerate() {
            let mut args = vec!["--no-reindex", "lines"];
            args.extend(line_flags(bits));
            assert_eq!(counts(&repo.cli_json(&args)?), counts(direct), "{args:?}");
        }
    }
    assert_eq!(line_stats_cache(&repo)?, cached);
    Ok(())
}

#[test]
fn lines_cli_cold_no_reindex_caches_saved_snapshot_and_reuses_restored_content() -> Result<()> {
    let repo = Repo::new()?;
    let original = "// saved\n\nfn original() {\n    let text = \"saved\";\n}\n";
    repo.write("source.rs", original)?;
    repo.open_map(&json!({}))?.refresh_structure()?;
    assert!(line_stats_cache(&repo)?.is_empty());
    repo.write("source.rs", "fn changed() {}\n")?;

    // A readonly cold query must reopen for the cache write, not refresh from disk.
    let saved = repo.cli_json(&[
        "--no-reindex",
        "lines",
        "--include-blank-lines",
        "--exclude-comments",
    ])?;
    assert_eq!(node(&saved, "source.rs").unwrap()["lines"], 4);
    let original_cache = line_stats_cache(&repo)?;
    assert_eq!(original_cache.len(), 1);
    assert_eq!(
        original_cache.values().next().unwrap(),
        &[4, 5, 3, 4, 4, 5, 3, 4]
    );
    let db = repo.db()?;
    let saved_hash: String =
        db.query_row("SELECT hash FROM files WHERE path='source.rs'", [], |row| {
            row.get(0)
        })?;
    assert_eq!(saved_hash, slopdex::hash(original));
    let saved_source: String = db.query_row(
        "SELECT source FROM global.sources WHERE hash=?",
        [&saved_hash],
        |row| row.get(0),
    )?;
    assert_eq!(
        saved_source, original,
        "filling totals must preserve source"
    );
    drop(db);
    let repeated = repo.cli_json(&["--no-reindex", "lines"])?;
    assert_eq!(node(&repeated, "source.rs").unwrap()["lines"], 4);
    assert_eq!(line_stats_cache(&repo)?, original_cache);

    let edited = repo.cli_json(&["lines"])?;
    assert_eq!(node(&edited, "source.rs").unwrap()["lines"], 1);
    let edited_cache = line_stats_cache(&repo)?;
    assert_eq!(edited_cache.len(), 2, "changed bytes need a new identity");
    for (key, totals) in &original_cache {
        assert_eq!(edited_cache.get(key), Some(totals));
    }
    repo.write("source.rs", original)?;
    let restored = repo.cli_json(&["lines"])?;
    assert_eq!(node(&restored, "source.rs").unwrap()["lines"], 4);
    assert_eq!(line_stats_cache(&repo)?, edited_cache);
    repo.write("source.rs", &original.replace('\n', "\r\n"))?;
    let crlf = repo.cli_json(&["lines"])?;
    assert_eq!(counts(&crlf), counts(&restored));
    let crlf_cache = line_stats_cache(&repo)?;
    assert_eq!(crlf_cache.len(), 3, "identity hashes exact source bytes");
    repo.write("source.rs", original)?;
    assert_eq!(counts(&repo.cli_json(&["lines"])?), counts(&restored));
    assert_eq!(line_stats_cache(&repo)?, crlf_cache);
    Ok(())
}

#[test]
fn lines_cli_totals_reuse_production_renames_and_workspaces_but_isolate_language_and_test_paths()
-> Result<()> {
    let first = Repo::new()?;
    let second = Repo::new()?;
    let config = json!({
        "artifactCachePath": first.home.join("cache/slopdex/global-v1.sqlite")
    });
    let source = "// Header\n\nexport function shared() {\n  return 1;\n}\n";
    first.write("src/original.ts", source)?;
    first.open_map(&config)?.refresh_structure()?;
    let original = first.cli_json(&["--no-reindex", "lines"])?;
    assert_eq!(node(&original, "original.ts").unwrap()["lines"], 4);
    let production_cache = line_stats_cache(&first)?;
    assert_eq!(production_cache.len(), 1);

    fs::rename(
        first.root.join("src/original.ts"),
        first.root.join("src/renamed.ts"),
    )?;
    let renamed = first.cli_json(&["lines"])?;
    assert_eq!(node(&renamed, "renamed.ts").unwrap()["lines"], 4);
    assert!(node(&renamed, "original.ts").is_none());
    assert_eq!(line_stats_cache(&first)?, production_cache);

    second.write("different/copied.ts", source)?;
    second.open_map(&config)?.refresh_structure()?;
    let copied = second.cli_json(&["--no-reindex", "lines"])?;
    assert_eq!(node(&copied, "copied.ts").unwrap()["lines"], 4);
    assert_eq!(line_stats_cache(&second)?, production_cache);

    fs::rename(
        first.root.join("src/renamed.ts"),
        first.root.join("src/renamed.test.ts"),
    )?;
    let test = first.cli_json(&["lines"])?;
    assert_eq!(node(&test, "renamed.test.ts").unwrap()["lines"], 4);
    let test_cache = line_stats_cache(&first)?;
    assert_eq!(
        test_cache.len(),
        2,
        "test-path classification is part of the key"
    );
    let test_totals = test_cache
        .iter()
        .find(|(key, _)| !production_cache.contains_key(*key))
        .unwrap()
        .1;
    assert_eq!(&test_totals[4..], &[0; 4]);
    assert!(production_cache.values().next().unwrap()[4] > 0);
    let excluded = first.cli_json(&["--no-reindex", "lines", "--exclude-tests"])?;
    assert!(node(&excluded, "renamed.test.ts").is_none());

    first.write("src/same.md", source)?;
    let markdown = first.cli_json(&["lines", "--exclude-comments"])?;
    assert_eq!(node(&markdown, "same.md").unwrap()["lines"], 4);
    assert_eq!(node(&markdown, "renamed.test.ts").unwrap()["lines"], 3);
    let language_cache = line_stats_cache(&first)?;
    assert_eq!(language_cache.len(), 3, "language is part of the key");
    // The other workspace still selects the production identity from the shared store.
    let production = second.cli_json(&["--no-reindex", "lines", "--exclude-tests"])?;
    assert_eq!(node(&production, "copied.ts").unwrap()["lines"], 4);
    assert_eq!(line_stats_cache(&second)?, language_cache);
    Ok(())
}

#[test]
fn lines_cli_warm_totals_skip_saved_source_reads_while_regex_queries_bypass_totals() -> Result<()> {
    let repo = Repo::new()?;
    let source = "fn wanted() {\n    work();\n}\n\n// Header\nfn other() {}\n";
    repo.write("source.rs", source)?;
    repo.open_map(&json!({}))?.refresh_structure()?;
    let selected = repo.cli_json(&["--no-reindex", "lines", "-e", "wanted"])?;
    assert_eq!(node(&selected, "source.rs").unwrap()["lines"], 3);
    assert!(
        line_stats_cache(&repo)?.is_empty(),
        "filtered misses bypass totals"
    );
    let whole = repo.cli_json(&["--no-reindex", "lines"])?;
    assert_eq!(node(&whole, "source.rs").unwrap()["lines"], 5);
    let cached = line_stats_cache(&repo)?;
    assert_eq!(cached.len(), 1);
    let selected_again = repo.cli_json(&["--no-reindex", "lines", "-e", "wanted"])?;
    assert_eq!(counts(&selected_again), counts(&selected));
    assert_eq!(line_stats_cache(&repo)?, cached);

    // Preserve the source row/key for metadata existence validation, but corrupt
    // its bytes so any attempt to load saved source fails hash validation.
    assert_eq!(
        repo.db()?.execute(
            "UPDATE global.sources SET source='corrupt saved source' WHERE hash=?",
            [slopdex::hash(source)],
        )?,
        1
    );
    let totals = cached.values().next().unwrap();
    for (bits, total) in totals.iter().enumerate() {
        let mut args = vec!["--no-reindex", "lines"];
        args.extend(line_flags(bits));
        let warm = repo.cli_json(&args)?;
        assert_eq!(
            node(&warm, "source.rs").unwrap()["lines"],
            *total,
            "{args:?}"
        );
    }
    let filtered = repo.cli(&["--no-reindex", "lines", "-e", "wanted"])?;
    assert!(
        !filtered.status.success(),
        "regex queries must still read saved source"
    );
    let error = String::from_utf8_lossy(&filtered.stderr);
    assert!(error.contains("Corrupt global source bytes"), "{error}");
    assert!(filtered.stdout.is_empty());
    assert_eq!(line_stats_cache(&repo)?, cached);
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
    assert!(
        line_stats_cache(&repo)?.is_empty(),
        "semantic and mixed queries must bypass whole-file totals"
    );
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
