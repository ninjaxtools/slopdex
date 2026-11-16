use super::*;

fn fixture(mock: &Mock) -> Result<Repo> {
    let repo = Repo::new()?;
    repo.write(".slopdex/config.json", &mock.config().to_string())?;
    repo.write(
        "east.rs",
        &(function("east_one", "VECTOR_EAST") + &function("east_two", "VECTOR_EAST")),
    )?;
    repo.write("west.rs", &function("west", "VECTOR_WEST"))?;
    repo.write("docs/guide.md", "# Guide\nVECTOR_MID documentation.\n")?;
    repo.write("settings.json", "{\"marker\":\"VECTOR_NORTH\"}\n")?;
    repo.write("page.html", "<main>VECTOR_WEST</main>\n")?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    engine.generate_descriptions()?;
    engine.search("east", "search-symbols", &all())?;
    Ok(repo)
}

#[test]
fn index_catalog_and_exports_cover_every_channel_without_refresh_or_model_calls() -> Result<()> {
    let mock = Mock::start()?;
    let repo = fixture(&mock)?;
    let calls = mock.count();
    let db = repo.db()?;
    let mut settings = mock.config();
    settings["defaultLimit"] = json!(1);
    repo.write(".slopdex/config.json", &settings.to_string())?;
    let before: i64 = db.query_row("PRAGMA data_version", [], |row| row.get(0))?;
    // Inspection describes what is saved, not the edited working tree.
    repo.write("east.rs", "fn unindexed() {}\n")?;
    let catalog = repo.cli_json(&["index", "list"])?;
    let expected = [
        "code",
        "docs",
        "config",
        "markup",
        "descriptions",
        "symbols",
    ];
    assert_eq!(
        catalog
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        expected
    );
    for name in expected {
        let channel = catalog
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .unwrap();
        let rows = repo.cli_json(&["index", "list", name])?;
        let rows = rows.as_array().unwrap();
        assert!(!rows.is_empty(), "{name}");
        assert_eq!(channel["entryCount"], rows.len());
        for (position, row) in rows.iter().enumerate() {
            assert_eq!(row["rank"], position + 1);
            assert_eq!(row["order"], position + 1);
            assert!(row["similarity"].is_null());
            assert!(
                row["embeddings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|embedding| embedding["vector"].as_array().unwrap().len() == 4)
            );
        }
    }
    let code = repo.cli_json(&["index", "list", "code"])?;
    assert_eq!(
        code.as_array().unwrap().len(),
        3,
        "inspection/export has no implicit result cap"
    );
    assert_eq!(
        repo.cli_json(&["index", "list", "md"])?,
        repo.cli_json(&["index", "list", "docs"])?
    );
    assert!(
        code.as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "east_one")
    );
    assert!(!code.to_string().contains("unindexed"));
    assert_eq!(
        db.query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))?,
        before
    );
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn index_queries_score_actual_vectors_with_regex_filtering_stable_ties_and_limits() -> Result<()> {
    let mock = Mock::start()?;
    let repo = fixture(&mock)?;
    let descriptions = mock.requests("/responses").len();
    let reranks = mock.requests("/rerank").len();
    let rows = repo.cli_json(&[
        "index",
        "list",
        "code",
        "-q",
        "VECTOR_WEST",
        "-e",
        "west",
        "-e",
        "east_one",
    ])?;
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], "west");
    near(&rows[0]["similarity"], 1.0);
    near(&rows[1]["similarity"], -1.0);
    let union = repo.cli_json(&[
        "index",
        "list",
        "code",
        "-q",
        "VECTOR_WEST",
        "-q",
        "east",
        "--threshold",
        "0.9",
        "--limit",
        "2",
    ])?;
    assert_eq!(union[0]["name"], "east_one");
    assert_eq!(union[1]["name"], "east_two");
    assert!(
        union
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["similarity"] == 1.0)
    );
    let regex = repo.cli_json(&["index", "list", "code", "-e", "EAST_TWO", "-i"])?;
    assert_eq!(regex.as_array().unwrap().len(), 1);
    assert_eq!(regex[0]["name"], "east_two");
    assert!(regex[0]["order"].as_u64().unwrap() > 1);
    let bounds = repo.cli_json(&[
        "index",
        "list",
        "code",
        "-q",
        "VECTOR_MID",
        "--threshold",
        "0.7-0.9",
    ])?;
    assert_eq!(bounds.as_array().unwrap().len(), 2);
    for row in bounds.as_array().unwrap() {
        near(&row["similarity"], 0.8);
    }
    assert_eq!(mock.requests("/responses").len(), descriptions);
    assert_eq!(mock.requests("/rerank").len(), reranks);
    Ok(())
}

#[test]
fn index_lists_pending_vectors_without_generating_them() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(".slopdex/config.json", &mock.config().to_string())?;
    repo.write("source.rs", &function("not_prepared", "VECTOR_EAST"))?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh_structure()?;
    drop(engine);
    let calls = mock.count();
    let catalog = repo.cli_json(&["index", "list"])?;
    assert_eq!(catalog[0]["state"], "pending");
    let code = repo.cli_json(&["index", "list", "code"])?;
    assert_eq!(code[0]["embeddings"][0]["present"], false);
    assert!(code[0]["embeddings"][0]["vector"].is_null());
    for name in ["code", "symbols"] {
        assert!(
            repo.cli_json(&["index", "list", name, "-q", "east"])?
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn index_csv_json_export_complete_entries_while_compact_text_is_truncated() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(".slopdex/config.json", &mock.config().to_string())?;
    let body = format!(
        "{} UNTRUNCATED_END",
        "📖 Unicode, \"quoted\" text. ".repeat(40)
    );
    repo.write("guide.md", &format!("# Long guide\n{body}\n"))?;
    repo.write(
        "extra.rs",
        &(function("first", "VECTOR_EAST") + &function("second", "VECTOR_EAST")),
    )?;
    repo.cli_json(&["update"])?;
    let calls = mock.count();
    let compact = repo.cli_format(&["index", "list", "docs"], "text")?;
    assert!(compact.status.success());
    let compact = String::from_utf8(compact.stdout)?;
    assert!(compact.contains('…'));
    assert!(!compact.contains("UNTRUNCATED_END"));
    let expanded = repo.cli_format(&["index", "list", "docs", "--detail", "expanded"], "text")?;
    assert!(expanded.status.success());
    assert!(String::from_utf8(expanded.stdout)?.contains("UNTRUNCATED_END"));
    let json = repo.cli_json(&["index", "list", "docs"])?;
    assert!(
        json[0]["text"]
            .as_str()
            .unwrap()
            .contains("UNTRUNCATED_END")
    );
    let csv = repo.cli_format(&["index", "list", "docs"], "csv")?;
    assert!(
        csv.status.success(),
        "{}",
        String::from_utf8_lossy(&csv.stderr)
    );
    let rows = parse_csv(&String::from_utf8(csv.stdout)?)?;
    assert_eq!(rows.len(), json.as_array().unwrap().len() + 1);
    for (fields, entry) in rows.iter().skip(1).zip(json.as_array().unwrap()) {
        assert_eq!(fields.len(), 11);
        assert_eq!(fields[8], entry["text"].as_str().unwrap());
        assert_eq!(serde_json::from_str::<Value>(&fields[9])?, entry["data"]);
        assert_eq!(
            serde_json::from_str::<Value>(&fields[10])?,
            entry["embeddings"]
        );
    }
    let limited = repo.cli_format(&["index", "list", "symbols", "--limit", "1"], "csv")?;
    assert!(limited.status.success());
    assert_eq!(parse_csv(&String::from_utf8(limited.stdout)?)?.len(), 2);
    assert_eq!(mock.count(), calls);
    Ok(())
}

fn parse_csv(input: &str) -> Result<Vec<Vec<String>>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => row.push(std::mem::take(&mut field)),
            '\r' if !quoted && chars.peek() == Some(&'\n') => {
                chars.next();
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            _ => field.push(ch),
        }
    }
    ensure!(
        !quoted && row.is_empty() && field.is_empty(),
        "incomplete CSV export"
    );
    Ok(rows)
}

#[test]
fn index_listing_validates_options_and_missing_indexes_before_creating_anything() -> Result<()> {
    let repo = Repo::new()?;
    for args in [
        vec!["index", "list"],
        vec!["index", "list", "docs"],
        vec!["index", "list", "code", "-q", "east"],
    ] {
        let output = repo.cli(&args)?;
        assert_missing_index(&output);
        assert!(!repo.index.exists());
    }
    for args in [
        vec!["index", "list", "unknown"],
        vec!["index", "list", "-q", "east"],
        vec!["index", "list", "faq", "-e", "["],
        vec!["index", "list", "faq", "--threshold", "0.9"],
        vec!["search", "east", "--output", "csv"],
        vec![
            "index",
            "list",
            "--force-reindex",
            "--yes-really-rebuild-the-index",
        ],
    ] {
        let output = repo.cli(&args)?;
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!repo.index.exists());
    }
    Ok(())
}
