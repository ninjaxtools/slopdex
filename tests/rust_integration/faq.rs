use super::*;

fn entries(repo: &Repo) -> Result<i64> {
    Ok(repo
        .db()?
        .query_row("SELECT count(*) FROM faq_entries", [], |row| row.get(0))?)
}

#[test]
fn faq_matches_question_or_plain_answer_without_repeating_search_and_survives_restart() -> Result<()>
{
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write("code.rs", &function("selected", "VECTOR_EAST"))?;
    let mut config = mock.config();
    config["rerankingEnabled"] = json!(true);
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let options = json!({"glob":"code.rs", "minSimilarity":-1});
    let original = engine.faq("VECTOR_NORTH original question", &options)?;
    assert_eq!(original["question"], "VECTOR_NORTH original question");
    let db = repo.db()?;
    let (question_key, answer_key, files): (String, String, String) = db.query_row(
        "SELECT question_embedding,answer_embedding,files FROM faq_entries",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_ne!(question_key, answer_key);
    assert_eq!(
        serde_json::from_str::<Value>(&files)?["code.rs"],
        slopdex::hash(function("selected", "VECTOR_EAST"))
    );
    let inputs = mock.embedding_inputs();
    assert!(inputs.contains(&"VECTOR_NORTH original question".into()));
    assert!(inputs.contains(&original["description"].as_str().unwrap().to_owned()));
    assert!(
        !inputs.iter().any(|input| input.contains("*** code.rs")),
        "map text must never be embedded as the answer"
    );
    assert_eq!(
        shared_pointer(&repo, "faq")?["membership"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
    let responses = mock.requests("/responses").len();
    let reranks = mock.requests("/rerank").len();
    let by_question = engine.faq("VECTOR_NORTH paraphrased question", &options)?;
    assert_eq!(by_question["description"], original["description"]);
    assert_eq!(by_question["searchResults"], original["searchResults"]);
    // This query is orthogonal to the stored question, but identical to its answer vector.
    let by_answer = engine.faq("eastward answer paraphrase", &options)?;
    assert_eq!(by_answer["question"], original["question"]);
    assert_eq!(by_answer["query"], "eastward answer paraphrase");
    assert_eq!(mock.requests("/responses").len(), responses);
    assert_eq!(mock.requests("/rerank").len(), reranks);
    assert_eq!(entries(&repo)?, 1);
    drop(engine);
    db.execute("DELETE FROM search_cache", [])?;
    let engine = repo.open(&config)?;
    let calls = mock.count();
    assert_eq!(
        engine.faq("eastward answer paraphrase", &options)?,
        by_answer
    );
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn faq_cutoff_is_configurable_strict_and_does_not_change_context_identity() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write("code.rs", &function("selected", "VECTOR_EAST"))?;
    let mut config = mock.config();
    config["faqThreshold"] = json!(0.7);
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let original = engine.faq("east", &all())?;
    assert_eq!(
        engine.faq("VECTOR_MID similar", &all())?["question"],
        "east"
    );
    let mut options = all();
    options["faqThreshold"] = json!(0.99);
    let distinct = engine.faq("VECTOR_MID different", &options)?;
    assert_eq!(distinct["question"], "VECTOR_MID different");
    assert_ne!(distinct["description"], original["description"]);
    // A score exactly one is not strictly above one.
    options["faqThreshold"] = json!(1);
    assert_eq!(
        engine.faq("east but force a semantic miss", &options)?["question"],
        "east but force a semantic miss"
    );
    assert_eq!(entries(&repo)?, 3);
    let calls = mock.count();
    for value in [json!(2), json!(-2), json!("0.9"), json!(null)] {
        assert!(engine.faq("east", &json!({"faqThreshold":value})).is_err());
    }
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn faq_invalidates_changed_deleted_and_renamed_files_but_not_unrelated_edits() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write("code.rs", &function("selected", "VECTOR_EAST"))?;
    repo.write("other.rs", &function("other", "VECTOR_WEST"))?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    let options = json!({"glob":"code.rs", "minSimilarity":-1});
    let original = engine.faq("east", &options)?;
    let calls = mock.requests("/responses").len();
    repo.write("other.rs", &function("unrelated_change", "VECTOR_WEST"))?;
    engine.refresh()?;
    assert_eq!(
        engine.faq("east again", &options)?["description"],
        original["description"]
    );
    assert_eq!(mock.requests("/responses").len(), calls);
    // Even an edit outside the selected callable invalidates its entire file.
    repo.write(
        "code.rs",
        &(function("selected", "VECTOR_EAST") + "\n// edited context\n"),
    )?;
    engine.refresh()?;
    engine.faq("east", &options)?;
    assert_eq!(mock.requests("/responses").len(), calls + 1);
    // Verify disk bytes on a warm Engine even without refreshing its saved snapshot.
    fs::rename(repo.root.join("code.rs"), repo.root.join("renamed.rs"))?;
    engine.faq("east", &options)?;
    assert_eq!(mock.requests("/responses").len(), calls + 2);
    assert_eq!(
        entries(&repo)?,
        0,
        "deleted/renamed dependencies cannot remain searchable"
    );
    engine.refresh()?;
    assert!(
        engine.faq("east", &options)?["files"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    Ok(())
}

#[test]
fn faq_tracks_expanded_callers_callees_and_reference_only_files() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write("entry.ts", "import { helper, LIMIT } from './defs';\nexport function entry() { /* VECTOR_EAST */ return helper() + LIMIT; }\n")?;
    repo.write(
        "defs.ts",
        "export const LIMIT = 1;\nexport function helper() { /* VECTOR_NORTH */ return LIMIT; }\n",
    )?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    for (callers, callees, refs) in [(2, 2, "none"), (0, 0, "cross-file")] {
        let options = json!({"glob":"entry.ts", "minSimilarity":-1,"callers":callers,"callees":callees,"expandCallers":0,"expandCallees":0,"refs":refs});
        let answer = engine.faq("east", &options)?;
        assert!(
            answer["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file["path"] == "defs.ts")
        );
        let calls = mock.requests("/responses").len();
        repo.write("defs.ts", &format!("export const LIMIT = {};\nexport function helper() {{ /* VECTOR_NORTH */ return LIMIT; }}\n", calls + 10))?;
        engine.refresh()?;
        engine.faq("east", &options)?;
        assert_eq!(mock.requests("/responses").len(), calls + 1);
    }
    Ok(())
}

#[test]
fn faq_never_publishes_partial_embeddings_and_reuses_completed_llm_work() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write("code.rs", &function("selected", "VECTOR_EAST"))?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    mock.fail_on(Some("answer grounded"));
    assert!(engine.faq("east", &all()).is_err());
    let calls = mock.requests("/responses").len();
    mock.fail_on(None);
    engine.faq("east", &all())?;
    assert_eq!(mock.requests("/responses").len(), calls);
    assert_eq!(entries(&repo)?, 1);
    Ok(())
}

#[test]
fn faq_cli_replaces_describe_and_threshold_override_reuses_saved_references() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write(".slopdex/config.json", &config.to_string())?;
    repo.write("code.rs", &function("selected", "VECTOR_EAST"))?;
    repo.cli_json(&["update"])?;
    let first = repo.cli_json(&["faq", "east", "--threshold", "-1"])?;
    let second = repo.cli_json(&[
        "faq",
        "VECTOR_MID rephrase",
        "--threshold",
        "-1",
        "--faq-threshold",
        "0.7",
    ])?;
    assert_eq!(second["question"], "east");
    assert_eq!(second["description"], first["description"]);
    assert_eq!(second["searchResults"], first["searchResults"]);
    assert_eq!(mock.requests("/responses").len(), 1);
    assert!(!repo.cli(&["describe", "east"])?.status.success());
    for value in ["NaN", "2", "-2", "0.8-0.9"] {
        assert!(
            !repo
                .cli(&["faq", "east", "--faq-threshold", value])?
                .status
                .success()
        );
    }
    repo.cli_json(&["config", "set", "faqThreshold", "0.75"])?;
    Ok(())
}

#[test]
fn faq_selects_best_entry_and_isolates_filters_and_embedding_profiles() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write("one.rs", &function("one", "VECTOR_EAST"))?;
    repo.write("two.rs", &function("two", "VECTOR_EAST"))?;
    let config = mock.config();
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let mut options = all();
    options["faqThreshold"] = json!(1);
    let north = engine.faq("VECTOR_NORTH question", &options)?;
    let west = engine.faq("VECTOR_WEST question", &options)?;
    options["faqThreshold"] = json!(0.9);
    let calls = mock.requests("/responses").len();
    assert_eq!(
        engine.faq("VECTOR_WEST best question", &options)?["description"],
        west["description"]
    );
    assert_eq!(
        engine.faq("VECTOR_NORTH best question", &options)?["description"],
        north["description"]
    );
    assert_eq!(mock.requests("/responses").len(), calls);
    options["glob"] = json!("one.rs");
    let filtered = engine.faq("VECTOR_NORTH question", &options)?;
    assert_eq!(filtered["files"].as_array().unwrap().len(), 1);
    assert_eq!(mock.requests("/responses").len(), calls + 1);
    drop(engine);
    let mut changed = config.clone();
    changed["dimensions"] = json!(2);
    let mut engine = repo.open(&changed)?;
    engine.refresh()?;
    engine.faq("VECTOR_NORTH question", &options)?;
    assert_eq!(entries(&repo)?, 4);
    drop(engine);
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let calls = mock.count();
    assert_eq!(engine.faq("VECTOR_NORTH question", &options)?, filtered);
    assert_eq!(mock.count(), calls);
    Ok(())
}

#[test]
fn faq_live_vectors_survive_pruning_without_a_sidecar_and_reset_clears_entries() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    repo.write(".slopdex/config.json", &mock.config().to_string())?;
    repo.write("code.rs", &function("selected", "VECTOR_EAST"))?;
    repo.cli_json(&["update"])?;
    let args = ["--no-reindex", "faq", "east", "--threshold", "-1"];
    let answer = repo.cli_json(&args)?;
    let calls = mock.count();
    let mut pointer = repo.index.as_os_str().to_os_string();
    pointer.push(".faq.shared.json");
    fs::remove_file(PathBuf::from(pointer))?;
    repo.cli_json(&["cache", "prune", "--prune-vectors"])?;
    assert_eq!(repo.cli_json(&args)?, answer);
    assert_eq!(
        mock.count(),
        calls,
        "SQLite membership roots both vectors even without the derived pointer"
    );
    let engine = repo.open(&mock.config())?;
    drop(engine);
    let db = slopdex::storage::Database::open_with_config(
        &repo.index,
        &repo.root,
        &repo.isolated_config(&mock.config()),
        false,
    )?;
    db.reset()?;
    assert_eq!(entries(&repo)?, 0);
    Ok(())
}

#[test]
fn faq_readers_defer_writes_without_allowing_stale_semantic_hits() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write("code.rs", &function("original", "VECTOR_EAST"))?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    engine.faq("east original question", &all())?;
    repo.write("code.rs", &function("changed", "VECTOR_EAST"))?;
    engine.refresh()?;
    let reader = Engine::open_map_readonly(&repo.root, &repo.index, repo.isolated_config(&config))?;
    let db = repo.db()?;
    let before: i64 = db.query_row("PRAGMA data_version", [], |row| row.get(0))?;
    let answer = engine.faq("east new question", &all())?;
    assert_eq!(answer["question"], "east new question");
    assert_eq!(answer["functions"][0]["qualifiedName"], "changed");
    assert_eq!(
        db.query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))?,
        before
    );
    let cached_question: String =
        db.query_row("SELECT question FROM faq_entries", [], |row| row.get(0))?;
    assert_eq!(
        cached_question, "east original question",
        "physical deletion is deferred while a reader is open"
    );
    drop(reader);
    assert_eq!(engine.faq("east new question", &all())?, answer);
    assert_eq!(entries(&repo)?, 1);
    let question: String =
        db.query_row("SELECT question FROM faq_entries", [], |row| row.get(0))?;
    assert_eq!(question, "east new question");
    Ok(())
}
