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
    let refreshed = engine.faq("east", &options)?;
    assert_eq!(refreshed["description"], original["description"]);
    assert_eq!(
        mock.requests("/responses").len(),
        calls,
        "a file change does not invalidate an identical LLM request"
    );
    let files: String = repo
        .db()?
        .query_row("SELECT files FROM faq_entries", [], |row| row.get(0))?;
    assert_eq!(
        serde_json::from_str::<Value>(&files)?["code.rs"],
        slopdex::hash(function("selected", "VECTOR_EAST") + "\n// edited context\n")
    );
    // Verify disk bytes on a warm Engine even without refreshing its saved snapshot.
    fs::rename(repo.root.join("code.rs"), repo.root.join("renamed.rs"))?;
    engine.faq("east", &options)?;
    assert_eq!(mock.requests("/responses").len(), calls);
    assert_eq!(
        entries(&repo)?,
        1,
        "invalid answers retain their original question for a later refresh"
    );
    assert_eq!(
        engine.index_entries("faq", &json!({}))?[0]["data"]["valid"],
        false
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
        assert_eq!(
            mock.requests("/responses").len(),
            calls,
            "the changed reference target did not change the actual prompt"
        );
        let rows = engine.index_entries("faq", &json!({}))?;
        let expected = slopdex::hash(fs::read(repo.root.join("defs.ts"))?);
        assert!(
            rows.iter()
                .any(|row| row["data"]["valid"] == true
                    && row["data"]["files"]["defs.ts"] == expected)
        );
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
    assert_eq!(answer["question"], "east original question");
    assert_eq!(answer["query"], "east new question");
    assert_eq!(answer["functions"][0]["qualifiedName"], "changed");
    assert_eq!(
        db.query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))?,
        before
    );
    let cached_question: String =
        db.query_row("SELECT question FROM faq_entries", [], |row| row.get(0))?;
    assert_eq!(
        cached_question, "east original question",
        "the canonical question is retained while its answer refresh is deferred"
    );
    drop(reader);
    assert_eq!(engine.faq("east new question", &all())?, answer);
    assert_eq!(entries(&repo)?, 1);
    let question: String =
        db.query_row("SELECT question FROM faq_entries", [], |row| row.get(0))?;
    assert_eq!(question, "east original question");
    Ok(())
}

#[test]
fn faq_stale_paraphrases_refresh_one_canonical_question_and_reuse_exact_requests() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let source = function("selected", "VECTOR_EAST");
    repo.write("code.rs", &source)?;
    let config = mock.config();
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let options = json!({"glob":"code.rs","minSimilarity":-1});
    let original = engine.faq("east original question", &options)?;
    let id: i64 = repo
        .db()?
        .query_row("SELECT id FROM faq_entries", [], |row| row.get(0))?;
    for (index, question) in ["east paraphrase", "another eastward question"]
        .into_iter()
        .enumerate()
    {
        let edited = format!("{source}\n// edit outside the supplied search context {index}\n");
        repo.write("code.rs", &edited)?;
        engine.refresh()?;
        let answer = engine.faq(question, &options)?;
        assert_eq!(answer["query"], question);
        assert_eq!(answer["question"], "east original question");
        assert_eq!(answer["description"], original["description"]);
        assert_eq!(entries(&repo)?, 1);
        let (current_id, question, files): (i64, String, String) =
            repo.db()?
                .query_row("SELECT id,question,files FROM faq_entries", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
        assert_eq!(current_id, id);
        assert_eq!(question, "east original question");
        assert_eq!(
            serde_json::from_str::<Value>(&files)?["code.rs"],
            slopdex::hash(edited)
        );
        assert_eq!(mock.requests("/responses").len(), 1);
    }
    // A real change to the supplied source requires another LLM response, but
    // still refreshes the same question/entry rather than recording the paraphrase.
    repo.write("code.rs", &source.replace("42", "43"))?;
    engine.refresh()?;
    let changed = engine.faq("east yet another paraphrase", &options)?;
    assert_eq!(changed["question"], "east original question");
    assert_ne!(changed["description"], original["description"]);
    assert_eq!(entries(&repo)?, 1);
    let requests = mock.requests("/responses");
    assert_eq!(requests.len(), 2);
    assert!(description_prompt(&requests[1].body).starts_with("Task: east original question\n"));
    assert!(description_prompt(&requests[1].body).contains("43"));
    drop(engine);
    let mut changed_model = config;
    changed_model["descriptionModel"] = json!("alternate-description-model");
    let engine = repo.open(&changed_model)?;
    engine.faq("east original question", &options)?;
    assert_eq!(
        mock.requests("/responses").len(),
        3,
        "a different model is a different LLM request"
    );
    Ok(())
}

#[test]
fn faq_and_description_request_caches_survive_index_deletion_and_default_pruning() -> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    repo.write(".slopdex/config.json", &config.to_string())?;
    repo.write("code.rs", &function("selected", "VECTOR_EAST"))?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    engine.generate_descriptions()?;
    let options = json!({"minSimilarity":-1,"glob":"code.rs"});
    let answer = engine.faq("east", &options)?;
    let paid = mock.count();
    assert_eq!(mock.requests("/responses").len(), 3);
    drop(engine);
    fs::remove_file(&repo.index)?;
    repo.cli_json(&["cache", "prune"])?;
    assert!(!repo.index.exists());
    let global = Connection::open(config["artifactCachePath"].as_str().unwrap())?;
    assert_eq!(
        global.query_row(
            "SELECT count(*) FROM cache WHERE kind='explanation'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    assert_eq!(
        global.query_row(
            "SELECT count(*) FROM cache WHERE kind='description'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        2
    );
    drop(global);
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    engine.generate_descriptions()?;
    assert_eq!(
        engine.faq("east", &options)?["description"],
        answer["description"]
    );
    assert_eq!(entries(&repo)?, 1);
    assert_eq!(
        mock.count(),
        paid,
        "rebuilding the index must reuse all completed provider requests"
    );
    Ok(())
}

#[test]
fn faq_promotes_legacy_file_keyed_answers_using_the_matching_entries_original_question()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let config = mock.config();
    let source = function("selected", "VECTOR_EAST");
    repo.write("code.rs", &source)?;
    let mut engine = repo.open(&config)?;
    engine.refresh()?;
    let options = json!({"minSimilarity":-1,"glob":"code.rs"});
    let answer = engine.faq("east original question", &options)?;
    let request = mock.requests("/responses").pop().unwrap();
    let profile = slopdex::providers::Providers::new(&config)?.llm().profile();
    let legacy_key = slopdex::hash(json!([
        "explanation-v3",profile,config["descriptionBaseUrl"],config["descriptionFallbackModel"],config["fallbackModel"],
        request.body["instructions"],description_prompt(&request.body),{"code.rs":slopdex::hash(&source)}
    ]).to_string());
    let db = repo.db()?;
    db.execute("DELETE FROM global.cache WHERE kind='explanation'", [])?;
    db.execute(
        "INSERT INTO global.cache VALUES('explanation',?,?)",
        rusqlite::params![legacy_key, answer["description"].as_str().unwrap()],
    )?;
    repo.write("code.rs", &(source + "\n// outside the context\n"))?;
    engine.refresh()?;
    let refreshed = engine.faq("east paraphrase", &options)?;
    assert_eq!(refreshed["question"], "east original question");
    assert_eq!(refreshed["description"], answer["description"]);
    assert_eq!(mock.requests("/responses").len(), 1);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM global.cache WHERE kind='explanation'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        2,
        "the old paid response is promoted into the request-only cache"
    );
    repo.write(
        "code.rs",
        &function("selected", "VECTOR_EAST").replace("42", "43"),
    )?;
    engine.refresh()?;
    let changed = engine.faq("east another paraphrase", &options)?;
    assert_ne!(changed["description"], answer["description"]);
    assert_eq!(
        mock.requests("/responses").len(),
        2,
        "legacy promotion must not reuse a response to a different prompt"
    );
    Ok(())
}

#[test]
fn faq_request_cache_keeps_paid_answers_even_when_the_snapshot_cannot_publish_a_faq_entry()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = Repo::new()?;
    let source = function("selected", "VECTOR_EAST");
    repo.write("code.rs", &source)?;
    let mut engine = repo.open(&mock.config())?;
    engine.refresh()?;
    // Like --no-reindex, this deliberately answers from a saved, now-stale snapshot.
    repo.write("code.rs", &source.replace("42", "43"))?;
    let answer = engine.faq("east", &all())?;
    assert!(engine.index_entries("faq", &json!({}))?.is_empty());
    let calls = mock.requests("/responses").len();
    assert_eq!(engine.faq("east", &all())?, answer);
    assert_eq!(
        mock.requests("/responses").len(),
        calls,
        "request caching must not depend on source freshness or FAQ publication"
    );
    repo.write("code.rs", &source)?;
    assert_eq!(engine.faq("east", &all())?, answer);
    assert_eq!(entries(&repo)?, 1);
    assert_eq!(mock.requests("/responses").len(), calls);
    Ok(())
}
