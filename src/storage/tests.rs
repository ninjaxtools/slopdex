use super::*;
use super::{snapshot::*, units::*};

fn fixture() -> Result<(tempfile::TempDir, Database)> {
    let dir = tempfile::tempdir()?;
    let db = Database::open_with_config(
        &dir.path().join("index.sqlite"),
        dir.path(),
        &json!({"artifactCachePath": dir.path().join("global # é.sqlite")}),
        false,
    )?;
    db.put_embedding("code", &[0.6, 0.8])?;
    db.put_embedding("description", &[1.0, 0.0])?;
    Ok((dir, db))
}

fn record(path: &str) -> (File, Vec<Item>) {
    let file = File {
        path: path.into(),
        hash: hash("fn example() {}"),
        source: "fn example() {}".into(),
        language: "rust".into(),
        source_mode: "working-tree".into(),
        description: Some("A description".into()),
        description_hash: Some("older-hash".into()),
        description_embedding: Some("description".into()),
        errors: vec![json!({"message": "recoverable", "startLine": 2})],
    };
    let item = Item {
        id: 0,
        path: path.into(),
        identity: format!("{path}:example"),
        kind: "function".into(),
        data: json!({"qualifiedName": "example", "startLine": 1,"sourceHash":"callable-hash",
                "embeddingInput":"fn example() {}","description":"Callable description"}),
        embedding: "code".into(),
        description_embedding: Some("description".into()),
    };
    (file, vec![item])
}

fn snapshot(db: &Database) -> Result<Value> {
    Ok(json!({
        "files": db.files()?,
        "items": db.items()?.iter().map(|i| json!([
            i.id, i.path, i.identity, i.kind, i.data, i.embedding, i.description_embedding
        ])).collect::<Vec<_>>(),
        "generation": db.generation()?,
        "checkpoint": db.meta("checkpoint")?,
        "identity": db.meta("identity")?,
        "snapshot": db.meta("snapshot")?,
        "search": db.search_cache("query")?,
    }))
}

#[test]
fn workspaces_share_artifacts_but_keep_bindings_and_query_cache_local() -> Result<()> {
    let (dir, mut first) = fixture()?;
    first.cache_put("parse", "shared-parse", "extraction")?;
    first.cache_put("explanation", "paid", "answer")?;
    first.apply(&[record("first.rs")], &[], None)?;
    first.put_search_cache("query", &[json!("first")])?;
    let mut second = Database::open_with_config(
        &dir.path().join("second.sqlite"),
        &dir.path().join("other-root"),
        &json!({"artifactCachePath": first.global_path()}),
        false,
    )?;
    assert_eq!(
        second.cache("parse", "shared-parse")?.as_deref(),
        Some("extraction")
    );
    assert_eq!(
        second.cache("explanation", "paid")?.as_deref(),
        Some("answer")
    );
    assert_eq!(second.embedding("code")?, Some(vec![0.6, 0.8]));
    assert!(second.files()?.is_empty());
    assert!(second.search_cache("query")?.is_none());
    second.apply(&[record("second.rs")], &[], None)?;
    assert_eq!(second.files()?[0].source, "fn example() {}");
    assert_eq!(first.paths()?, ["first.rs"]);
    assert_eq!(
        first
            .conn
            .query_row("SELECT count(*) FROM global.sources", [], |r| r
                .get::<_, i64>(0))?,
        1
    );
    assert!(
        first
            .conn
            .prepare("SELECT vector FROM main.embeddings")
            .is_err()
    );
    assert!(first.conn.prepare("SELECT value FROM main.cache").is_err());
    assert!(
        first
            .conn
            .prepare("SELECT content FROM main.description_content")
            .is_err()
    );
    assert!(
        first
            .conn
            .prepare("SELECT source,data FROM main.files")
            .is_err()
    );
    first.reset()?;
    assert_eq!(second.files()?.len(), 1);
    assert!(first.embedding("code")?.is_some());
    Ok(())
}

#[test]
fn overlapping_workspaces_share_unit_content_without_local_payload_copies() -> Result<()> {
    let (dir, mut first) = fixture()?;
    let original = parsed_record("code.rs", "fn example() -> i32 { 1 }")?;
    first.apply_structure(std::slice::from_ref(&original), &[], None)?;
    let mut second = Database::open_with_config(
        &dir.path().join("second.sqlite"),
        &dir.path().join("other-root"),
        &json!({"artifactCachePath": first.global_path()}),
        false,
    )?;
    second.apply_structure(std::slice::from_ref(&original), &[], None)?;
    let expected = parsed_items(&original.0, &original.1)?.remove(0).data;
    let mut keys = Vec::new();
    let mut digests = Vec::new();
    for db in [&first, &second] {
        let (data, key): (String, String) = db.conn.query_row(
            "SELECT data,content_hash FROM search_units WHERE kind='function'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let data: Value = serde_json::from_str(&data)?;
        assert!(data.get("source").is_none());
        assert!(data.get("embeddingInput").is_none());
        assert_eq!(db.items()?[0].data, expected);
        assert_eq!(
            db.items_for_profile(&json!({"model":"a"}))?[0].data,
            expected
        );
        // The digest binds the shared payload by content hash, not by root.
        let digest = db.meta("snapshot")?.unwrap();
        assert_eq!(snapshot_digest(&db.conn)?, digest);
        digests.push(digest);
        keys.push(key);
    }
    assert_eq!(keys[0], keys[1]);
    assert_eq!(digests[0], digests[1]);
    assert_eq!(
        first.conn.query_row(
            "SELECT count(*) FROM global.cache WHERE kind='unit-content'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    let content = unit_content(&first.conn, &keys[0])?;
    assert_eq!(content["source"], expected["source"]);
    assert_eq!(content["embeddingInput"], expected["embeddingInput"]);
    first.reset()?;
    assert_eq!(second.items()?[0].data, expected);
    let path = second.path.clone();
    drop(second);
    let readonly = Database::open_readonly(&path, &dir.path().join("other-root"))?;
    assert_eq!(readonly.items()?[0].data, expected);
    Ok(())
}

#[test]
fn missing_corrupt_or_invalid_unit_content_fails_item_reads() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    let original = parsed_record("code.rs", "fn example() -> i32 { 1 }")?;
    db.apply_structure(std::slice::from_ref(&original), &[], None)?;
    let key: String = db
        .conn
        .query_row("SELECT content_hash FROM search_units", [], |row| {
            row.get(0)
        })?;
    db.conn.execute(
        "UPDATE global.cache SET value='corrupt' WHERE kind='unit-content' AND key=?",
        [&key],
    )?;
    for error in [
        db.items().unwrap_err(),
        db.items_for_profile(&json!({})).unwrap_err(),
    ] {
        assert!(
            error
                .to_string()
                .contains("Corrupt global unit content artifact")
        );
    }
    db.conn.execute(
        "DELETE FROM global.cache WHERE kind='unit-content' AND key=?",
        [&key],
    )?;
    db.cache_put("parse", &key, "wrong namespace")?;
    for error in [
        db.items().unwrap_err(),
        db.items_for_profile(&json!({})).unwrap_err(),
    ] {
        assert!(
            error
                .to_string()
                .contains("Missing global unit content artifact")
        );
    }
    for payload in ["{", "null", "[]", "{\"path\":\"other.rs\"}"] {
        let key = hash(payload);
        db.cache_put("unit-content", &key, payload)?;
        db.conn
            .execute("UPDATE search_units SET content_hash=?", [&key])?;
        assert!(
            db.items()
                .unwrap_err()
                .to_string()
                .contains("Invalid global unit content artifact"),
            "{payload}"
        );
    }
    db.apply_structure(std::slice::from_ref(&original), &[], None)?;
    assert_eq!(
        db.items()?[0].data,
        parsed_items(&original.0, &original.1)?[0].data
    );
    Ok(())
}

#[test]
fn store_binding_reopens_readonly_and_conflicts_require_a_new_index() -> Result<()> {
    let (dir, mut db) = fixture()?;
    db.apply(&[record("code.rs")], &[], None)?;
    let path = db.path.clone();
    let global = db.global_path().to_owned();
    drop(db);
    let db = Database::open(
        &path,
        dir.path(),
        &json!({"artifactCachePath":"ignored-profile"}),
        false,
    )?;
    assert_eq!(db.global_path(), global);
    drop(db);
    let readonly = Database::open_readonly(&path, dir.path())?;
    assert_eq!(readonly.files()?[0].source, "fn example() {}");
    assert!(readonly.put_embedding("readonly", &[1.0]).is_err());
    assert!(readonly.cache_put("parse", "readonly", "value").is_err());
    assert!(readonly.set_meta("readonly", "value").is_err());
    let config = json!({"artifactCachePath":dir.path().join("new-global.sqlite")});
    for force in [false, true] {
        let error = Database::open_with_config(&path, dir.path(), &config, force)
            .err()
            .unwrap();
        assert!(error.to_string().contains("Global artifact store conflict"));
        assert!(error.to_string().contains("new index path"));
    }
    assert!(!dir.path().join("new-global.sqlite").exists());
    let fresh =
        Database::open_with_config(&dir.path().join("new.sqlite"), dir.path(), &config, false)?;
    assert_ne!(fresh.global_path(), global);
    assert!(fresh.embedding("code")?.is_none());
    Ok(())
}

#[test]
#[cfg(unix)]
fn configured_store_alias_reopens_the_same_canonical_binding() -> Result<()> {
    let (dir, db) = fixture()?;
    let path = db.path.clone();
    let global = db.global_path().to_owned();
    drop(db);
    let alias = dir.path().join("store-alias.sqlite");
    std::os::unix::fs::symlink(&global, &alias)?;
    let config = json!({"artifactCachePath":alias});
    let db = Database::open_with_config(&path, dir.path(), &config, false)?;
    assert_eq!(db.global_path(), global);
    drop(db);
    let db = Database::open_readonly_with_config(&path, dir.path(), &config)?;
    assert_eq!(db.global_path(), global);
    Ok(())
}

#[test]
fn workspace_incarnation_survives_reopen_but_changes_on_each_reset() -> Result<()> {
    let (dir, db) = fixture()?;
    let path = db.path.clone();
    let first = db.meta("incarnation")?.unwrap();
    assert_eq!(first.len(), 32);
    assert!(
        first
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    drop(db);
    let db = Database::open(&path, dir.path(), &json!({}), false)?;
    assert_eq!(db.meta("incarnation")?.as_deref(), Some(first.as_str()));
    db.reset()?;
    let second = db.meta("incarnation")?.unwrap();
    assert_ne!(second, first);
    db.reset()?;
    let third = db.meta("incarnation")?.unwrap();
    assert_ne!(third, second);
    drop(db);
    let db = Database::open(&path, &dir.path().join("other-root"), &json!({}), true)?;
    assert_ne!(db.meta("incarnation")?.as_deref(), Some(third.as_str()));
    Ok(())
}

#[test]
fn snapshot_registry_is_stable_immutable_and_survives_root_reset() -> Result<()> {
    let (dir, mut first) = fixture()?;
    first.set_meta("selection_policy", "all-rust")?;
    first.set_meta("discovery_policy", "first-root-fingerprints")?;
    first.set_meta("dirty_paths", "[\"a.rs\"]")?;
    first.apply(&[record("a.rs"), record("z.rs")], &[], Some("commit-a"))?;
    let digest = first.meta("snapshot")?.unwrap();
    assert_eq!(digest.len(), 64);
    assert_eq!(hash(snapshot_manifest(&first.conn)?), digest);
    assert_eq!(snapshot_digest(&first.conn)?, digest);
    let mut second = Database::open_with_config(
        &dir.path().join("second.sqlite"),
        &dir.path().join("second-root"),
        &json!({"artifactCachePath": first.global_path()}),
        false,
    )?;
    second.set_meta("selection_policy", "all-rust")?;
    second.set_meta("discovery_policy", "second-root-fingerprints")?;
    second.set_meta("dirty_paths", "[]")?;
    second.apply(&[record("z.rs")], &[], None)?;
    second.apply(&[record("a.rs")], &[], Some("commit-b"))?;
    assert_eq!(second.meta("snapshot")?.as_deref(), Some(digest.as_str()));
    second.apply(&[], &[], Some("another-checkpoint"))?;
    assert_eq!(second.meta("snapshot")?.as_deref(), Some(digest.as_str()));
    second.set_meta("selection_policy", "selected-rust")?;
    second.apply(&[], &[], Some("another-checkpoint"))?;
    assert_ne!(second.meta("snapshot")?.as_deref(), Some(digest.as_str()));
    assert_eq!(
        second.meta("snapshot_policy")?.as_deref(),
        Some("selected-rust")
    );
    second
        .conn
        .execute("DELETE FROM metadata WHERE key='selection_policy'", [])?;
    second.apply(&[], &[], Some("another-checkpoint"))?;
    assert!(second.meta("snapshot_policy")?.is_none());
    let path = first.path.clone();
    drop(first);
    let mut first = Database::open(&path, &dir.path().join("new-root"), &json!({}), true)?;
    assert!(first.paths()?.is_empty());
    assert!(first.meta("snapshot")?.is_none());
    // Only digests are published: no manifest body is ever stored.
    assert_eq!(
        first
            .conn
            .query_row("SELECT count(*) FROM global.snapshots", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    // The digest is a pure function of published content: republishing the
    // same bindings under another root and generation reproduces it.
    first.set_meta("selection_policy", "all-rust")?;
    first.apply(&[record("z.rs"), record("a.rs")], &[], Some("commit-c"))?;
    assert_eq!(first.meta("snapshot")?.as_deref(), Some(digest.as_str()));
    assert_eq!(
        first.conn.query_row(
            "SELECT source FROM global.sources WHERE hash=?",
            [hash("fn example() {}")],
            |r| r.get::<_, String>(0)
        )?,
        "fn example() {}"
    );
    Ok(())
}

#[test]
fn source_hash_mismatch_rolls_back_live_publication() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    db.apply(&[record("old.rs")], &[], None)?;
    let before = snapshot(&db)?;
    let mut invalid = record("new.rs");
    invalid.0.hash = "incorrect".into();
    assert!(
        db.apply(&[invalid], &["old.rs".into()], None)
            .unwrap_err()
            .to_string()
            .contains("Source content hash mismatch")
    );
    assert_eq!(snapshot(&db)?, before);
    Ok(())
}

#[test]
fn unchanged_publication_does_not_recompute_the_snapshot_digest() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    db.set_meta("selection_policy", "all-rust")?;
    db.apply(&[record("code.rs")], &[], Some("commit"))?;
    let digest = db.meta("snapshot")?;
    // Make a digest-only table unavailable: unchanged publication must
    // consult only its checkpoint and policy, not traverse published rows.
    db.conn
        .execute("ALTER TABLE symbol_names RENAME TO unread_names", [])?;
    let writes = db.conn.total_changes();
    assert!(!db.apply(&[], &[], Some("commit"))?);
    assert_eq!(db.conn.total_changes(), writes);
    assert_eq!(db.meta("snapshot")?, digest);
    db.conn
        .execute("ALTER TABLE unread_names RENAME TO symbol_names", [])?;
    Ok(())
}

#[test]
fn file_records_load_metadata_without_source_bytes() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    let record = record("code.rs");
    db.apply(std::slice::from_ref(&record), &[], None)?;
    let file = db.file_records()?.remove(0);
    let mut expected = record.0.clone();
    expected.source.clear();
    assert_eq!(serde_json::to_value(file)?, serde_json::to_value(expected)?);
    assert_eq!(db.source(&record.0.hash)?, record.0.source);
    db.conn
        .execute("UPDATE global.sources SET source='corrupt'", [])?;
    assert!(db.file_records()?[0].source.is_empty());
    assert!(
        db.source(&record.0.hash)
            .unwrap_err()
            .to_string()
            .contains("Corrupt global source bytes")
    );
    Ok(())
}

#[test]
fn missing_or_corrupt_global_source_is_an_error() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    db.apply(&[record("code.rs")], &[], None)?;
    db.conn
        .execute("UPDATE global.sources SET source='corrupt'", [])?;
    assert!(
        db.files()
            .unwrap_err()
            .to_string()
            .contains("Corrupt global source bytes")
    );
    db.conn.execute("DELETE FROM global.sources", [])?;
    for error in [db.files().unwrap_err(), db.file_records().unwrap_err()] {
        let message = error.to_string();
        assert!(
            message.contains("Missing global source artifact"),
            "{message}"
        );
        assert!(message.contains("code.rs"), "{message}");
    }
    assert!(
        db.source(&hash("fn example() {}"))
            .unwrap_err()
            .to_string()
            .contains("Missing global source artifact")
    );
    assert_eq!(db.paths()?, ["code.rs"]);
    assert!(db.embedding("code")?.is_some());
    Ok(())
}

#[test]
fn rejected_local_bindings_leave_precommitted_reusable_artifacts() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    let mut changed = parsed_record("new.rs", "fn example() -> i32 { 7 }")?;
    changed.0.description = Some("New file prose".into());
    changed.0.description_hash = Some(changed.0.hash.clone());
    changed.1.callables[0].description = Some("New callable prose".into());
    changed.1.structure.nodes[0].description = Some("New symbol prose".into());
    let payload = unit_payload(&parsed_items(&changed.0, &changed.1)?[0].data)?;
    db.conn.execute_batch("CREATE TRIGGER reject_binding BEFORE INSERT ON files BEGIN SELECT RAISE(ABORT, 'binding rejected'); END;")?;
    let before = snapshot(&db)?;
    assert!(
        db.apply_structure(std::slice::from_ref(&changed), &[], Some("new"))
            .unwrap_err()
            .to_string()
            .contains("binding rejected")
    );
    assert!(db.conn.is_autocommit());
    assert_eq!(snapshot(&db)?, before);
    // Observe through an independent connection: these were committed
    // before the rejected workspace transaction, not rolled back with it.
    let global = cache::open_store(db.global_path(), true)?;
    assert_eq!(
        global.query_row(
            "SELECT source FROM sources WHERE hash=?",
            [&changed.0.hash],
            |row| row.get::<_, String>(0)
        )?,
        changed.0.source
    );
    assert_eq!(
        global.query_row(
            "SELECT value FROM cache WHERE kind='unit-content' AND key=?",
            [hash(&payload)],
            |row| row.get::<_, String>(0)
        )?,
        payload
    );
    for text in ["New file prose", "New callable prose", "New symbol prose"] {
        assert_eq!(
            global.query_row(
                "SELECT content FROM description_content WHERE hash=?",
                [hash(text)],
                |row| row.get::<_, String>(0)
            )?,
            text
        );
    }
    let counts = global.query_row("SELECT (SELECT count(*) FROM sources),(SELECT count(*) FROM cache WHERE kind='unit-content'),(SELECT count(*) FROM description_content)", [], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)))?;
    db.conn.execute_batch("DROP TRIGGER reject_binding")?;
    assert!(db.apply_structure(&[changed], &[], Some("new"))?);
    assert_eq!(global.query_row("SELECT (SELECT count(*) FROM sources),(SELECT count(*) FROM cache WHERE kind='unit-content'),(SELECT count(*) FROM description_content)", [], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)))?, counts);
    Ok(())
}

#[test]
fn artifacts_commit_before_a_rejected_snapshot_pointer_and_retry_reuses_them() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    let original = parsed_record("code.rs", "fn example() -> i32 { 1 }")?;
    db.apply_structure(&[original], &[], Some("old"))?;
    db.put_search_cache("query", &[json!("old result")])?;
    let before = snapshot(&db)?;
    let old_digest = db.meta("snapshot")?.unwrap();
    let changed = parsed_record("code.rs", "fn example() -> i32 { 2 }")?;
    db.conn.execute_batch("CREATE TRIGGER reject_snapshot_pointer BEFORE UPDATE ON metadata WHEN OLD.key='snapshot' BEGIN SELECT RAISE(ABORT, 'snapshot pointer rejected'); END;")?;
    assert!(
        db.apply_structure(std::slice::from_ref(&changed), &[], Some("new"))
            .unwrap_err()
            .to_string()
            .contains("snapshot pointer rejected")
    );
    assert!(db.conn.is_autocommit());
    assert_eq!(snapshot(&db)?, before);
    assert_eq!(db.meta("snapshot")?.as_deref(), Some(old_digest.as_str()));
    // The reusable content of the rejected publication is already durable.
    let global = cache::open_store(db.global_path(), true)?;
    assert!(global.query_row(
        "SELECT EXISTS(SELECT 1 FROM sources WHERE hash=?)",
        [&changed.0.hash],
        |row| row.get::<_, bool>(0)
    )?);
    let content_hash = hash(unit_payload(
        &parsed_items(&changed.0, &changed.1)?[0].data,
    )?);
    assert!(global.query_row(
        "SELECT EXISTS(SELECT 1 FROM cache WHERE kind='unit-content' AND key=?)",
        [&content_hash],
        |row| row.get::<_, bool>(0)
    )?);
    let artifacts = |global: &Connection| {
        global.query_row("SELECT (SELECT count(*) FROM sources),(SELECT count(*) FROM cache),(SELECT count(*) FROM description_content),(SELECT count(*) FROM snapshots)", [], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?))
            })
    };
    let committed = artifacts(&global)?;
    assert_eq!(committed.3, 0);
    db.conn
        .execute_batch("DROP TRIGGER reject_snapshot_pointer")?;
    assert!(db.apply_structure(&[changed], &[], Some("new"))?);
    let digest = db.meta("snapshot")?.unwrap();
    assert_ne!(digest, old_digest);
    assert_eq!(snapshot_digest(&db.conn)?, digest);
    assert_eq!(
        db.conn.query_row(
            "SELECT content_hash FROM search_units WHERE kind='function'",
            [],
            |row| row.get::<_, String>(0)
        )?,
        content_hash
    );
    assert_eq!(artifacts(&global)?, committed);
    assert!(db.search_cache("query")?.is_none());
    Ok(())
}

/// Every table the digest covers is populated, with text that exercises
/// JSON escaping, NULL columns and multi-row ordering.
fn digest_fixture(db: &mut Database) -> Result<()> {
    db.set_meta(
        "selection_policy",
        "policy \"quoted\" \\ back\tslash \u{2028} é 🦀",
    )?;
    let mut rust = parsed_record(
        "src/Café \"q\".rs",
        "use std::collections::HashMap as Map;\n/// Says \"hi\"\t\\ \u{1} \u{7f} \u{2028} 🦀.\nstruct Café { value: i32 }\nimpl Café {\n    /// Run the service.\n    fn run(&self) -> i32 { self.value }\n    fn other(&self) -> &'static str { \"a\\\\b\\n\" }\n}\n",
    )?;
    rust.0.errors = vec![
        json!({"message":"first \"problem\"\n","startLine":2,"code":"parse-error"}),
        json!({"message":null,"nested":{"é":[1,-2,3.5,true]}}),
    ];
    let markdown = parsed_record(
        "docs/Guide.md",
        "# Title\n\nBody \\ text.\n\n## Second\n\nMore.\n",
    )?;
    let plain = parsed_record(
        "b.rs",
        "fn zero() {}\nfn zero_two() -> u64 { 18446744073709551615 }\n",
    )?;
    db.apply_structure(&[rust, markdown, plain], &[], Some("commit"))?;
    let mut described = record("a.rs");
    described.1[0].data["description"] = json!("Callable \"description\" \u{1f}");
    db.apply(&[described, record("z.rs")], &[], Some("commit"))?;
    db.conn.execute(
        "INSERT INTO unit_embeddings(unit_id,role,profile_key,input_hash,embedding_key)
             SELECT id,'code','profile \"a\"',embedding_input_hash,'code' FROM search_units",
        [],
    )?;
    db.conn.execute(
            "INSERT INTO unit_embeddings(unit_id,role,profile_key,input_hash,embedding_key)
             SELECT id,'description','b',embedding_input_hash,'description' FROM search_units WHERE path='a.rs'",
            [],
        )?;
    Ok(())
}

#[test]
fn streamed_snapshot_digest_equals_the_hash_of_the_materialized_manifest() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    // An empty workspace is the degenerate document.
    let empty = snapshot_manifest(&db.conn)?;
    assert_eq!(
        empty,
        r#"{"descriptions":[],"diagnostics":[],"embeddings":[],"files":[],"names":[],"parseBindings":[],"policy":[],"schema":1,"symbols":[],"units":[]}"#
    );
    assert_eq!(snapshot_digest(&db.conn)?, hash(&empty));

    digest_fixture(&mut db)?;
    let manifest = snapshot_manifest(&db.conn)?;
    let parsed: Value = serde_json::from_str(&manifest)?;
    for (section, minimum) in [
        ("descriptions", 3),
        ("diagnostics", 3),
        ("embeddings", 3),
        ("files", 5),
        ("names", 2),
        ("parseBindings", 3),
        ("policy", 1),
        ("symbols", 5),
        ("units", 5),
    ] {
        assert!(
            parsed[section].as_array().unwrap().len() >= minimum,
            "{section}: {}",
            parsed[section]
        );
    }
    assert_eq!(parsed["schema"], 1);
    // NULL and integer columns and escaped text all occur in the fixture.
    for expected in [",null,", "\\\\", "\\t", "\\\"", "\u{2028}", "🦀"] {
        assert!(manifest.contains(expected), "{expected:?}");
    }
    let mut streamed = Vec::new();
    write_snapshot_manifest(&db.conn, &mut streamed)?;
    assert_eq!(String::from_utf8(streamed)?, manifest);
    let digest = snapshot_digest(&db.conn)?;
    assert_eq!(digest, hash(&manifest));
    // A dirty publication stores exactly this digest.
    db.apply(&[record("published.rs")], &[], Some("commit"))?;
    let manifest = snapshot_manifest(&db.conn)?;
    assert_eq!(db.meta("snapshot")?, Some(hash(&manifest)));
    assert_ne!(hash(&manifest), digest);
    Ok(())
}

#[test]
fn repeated_publications_never_grow_the_snapshot_table() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    let mut digests = HashSet::new();
    for round in 0..6 {
        let source = format!("fn example() -> i32 {{ {round} }}");
        db.apply_structure(&[parsed_record("code.rs", &source)?], &[], None)?;
        db.apply(&[record(&format!("file-{round}.rs"))], &[], None)?;
        digests.insert(db.meta("snapshot")?.unwrap());
        assert_eq!(
            db.conn
                .query_row("SELECT count(*) FROM global.snapshots", [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
    }
    assert_eq!(digests.len(), 6);
    // The table itself stays, so binaries that still write it keep working.
    let global = cache::open_store(db.global_path(), false)?;
    global.execute(
        "INSERT INTO snapshots VALUES('older-binary','manifest')",
        [],
    )?;
    db.apply(&[record("after.rs")], &[], None)?;
    assert_eq!(
        global.query_row("SELECT count(*) FROM snapshots", [], |r| r.get::<_, i64>(0))?,
        1
    );
    Ok(())
}

#[test]
fn initialized_index_reopens_without_writing_or_waiting_for_a_writer() -> Result<()> {
    let (dir, mut db) = fixture()?;
    db.apply(&[record("code.rs")], &[], Some("commit"))?;
    let path = db.path.clone();
    let root = dir.path().to_owned();
    let global = db.global_path().to_owned();
    let incarnation = db.meta("incarnation")?;
    // Another connection holds the worktree database's writer lock.
    let writer = Connection::open(&path)?;
    let tx =
        rusqlite::Transaction::new_unchecked(&writer, rusqlite::TransactionBehavior::Immediate)?;
    tx.execute("INSERT INTO metadata VALUES('pending','not committed')", [])?;
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = (|| -> Result<Option<String>> {
            let mut incarnation = None;
            for config in [json!({}), json!({"artifactCachePath": global})] {
                let reopened = Database::open_with_config(&path, &root, &config, false)?;
                ensure!(
                    reopened.conn.total_changes() == 0,
                    "A warm writable open wrote to the index"
                );
                ensure!(
                    reopened.meta("pending")?.is_none(),
                    "Uncommitted rows must remain invisible"
                );
                ensure!(
                    reopened.paths()? == ["code.rs"],
                    "Published rows are readable"
                );
                incarnation = reopened.meta("incarnation")?;
                Database::open_readonly_with_config(&path, &root, &config)?;
                let existing = Database::open_existing_with_config(&path, &root, &config)?;
                ensure!(
                    existing.conn.total_changes() == 0,
                    "Opening an existing index must not write during shared access"
                );
                ensure!(
                    existing.paths()? == ["code.rs"] && existing.meta("pending")?.is_none(),
                    "Existing-index opens must read the committed workspace snapshot"
                );
            }
            Ok(incarnation)
        })();
        sender.send(result).expect("opener result receiver");
    });
    let result = receiver.recv_timeout(std::time::Duration::from_secs(5));
    // Release the writer and join even on timeout, so a regression fails
    // without stranding a worker for SQLite's full busy timeout.
    drop(tx);
    worker.join().expect("index opener");
    let reopened =
        result.context("Opening an initialized index waited for the worktree writer")??;
    assert_eq!(reopened, incarnation);
    Ok(())
}

#[test]
fn concurrent_cold_index_opens_share_one_initialization() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("root");
    std::fs::create_dir(&root)?;
    let indexes = [dir.path().join("indexes")];
    std::fs::create_dir(&indexes[0])?;
    #[cfg(unix)]
    let indexes = {
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&indexes[0], &alias)?;
        [indexes[0].clone(), alias]
    };
    for round in 0..6 {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let path = indexes[worker % indexes.len()].join(format!("index-{round}.sqlite"));
                let config =
                    json!({"artifactCachePath": dir.path().join(format!("global-{round}.sqlite"))});
                let (barrier, root) = (barrier.clone(), root.clone());
                std::thread::spawn(move || {
                    // Start barrier before any fallible setup.
                    barrier.wait();
                    Database::open_with_config(&path, &root, &config, false)
                })
            })
            .collect();
        // Join everyone before propagating an error; successful connections
        // stay open while their peers finish opening the same cold index.
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().expect("index opener"))
            .collect();
        let databases = results.into_iter().collect::<Result<Vec<_>>>()?;
        let incarnation = databases[0].meta("incarnation")?.unwrap();
        for (worker, db) in databases.iter().enumerate() {
            assert_eq!(
                db.meta("incarnation")?.as_deref(),
                Some(incarnation.as_str())
            );
            assert_eq!(db.global_path(), databases[0].global_path());
            assert_eq!(
                db.conn
                    .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))?,
                "wal"
            );
            db.set_meta(&format!("worker-{worker}"), "wrote")?;
        }
        assert_eq!(
            databases[0].conn.query_row(
                "SELECT count(*) FROM metadata WHERE key LIKE 'worker-%'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            8
        );
    }
    Ok(())
}

#[test]
fn missing_description_content_is_a_descriptive_error_for_every_reader() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    let parsed = parsed_record(
        "code.rs",
        "fn first() {}\n\n/// Run the service.\nfn run() -> i32 { 1 }\n",
    )?;
    db.apply_structure(&[parsed], &[], None)?;
    db.apply(&[record("described.rs")], &[], None)?;
    assert_eq!(db.symbol_descriptions("code.rs")?.len(), 1);
    assert_eq!(db.file_records()?.len(), 2);
    let items = db.items()?.len();
    let (callable, text): (String, String) = db.conn.query_row(
            "SELECT d.content_hash,c.content FROM descriptions d JOIN global.description_content c ON c.hash=d.content_hash WHERE d.scope='callable' AND d.path='code.rs'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
    let file: String = db.conn.query_row(
        "SELECT content_hash FROM descriptions WHERE scope='file' AND path='described.rs'",
        [],
        |r| r.get(0),
    )?;
    let expect = |error: anyhow::Error, key: &str| {
        assert_eq!(
            error.to_string(),
            format!("Missing global description content {key}; rebuild the index")
        );
    };
    db.conn.execute(
        "DELETE FROM global.description_content WHERE hash=?",
        [&callable],
    )?;
    expect(db.symbol_descriptions("code.rs").unwrap_err(), &callable);
    expect(db.items().unwrap_err(), &callable);
    expect(
        db.items_for_profile(&json!({"model":"a"})).unwrap_err(),
        &callable,
    );
    assert_eq!(db.file_records()?.len(), 2);
    db.put_description_content(&callable, &text)?;
    assert_eq!(db.items()?.len(), items);
    db.conn.execute(
        "DELETE FROM global.description_content WHERE hash=?",
        [&file],
    )?;
    expect(db.file_records().unwrap_err(), &file);
    expect(db.files().unwrap_err(), &file);
    Ok(())
}

#[test]
fn late_publication_failure_rolls_back_metadata_and_cache_deletion() -> Result<()> {
    let (dir, mut db) = fixture()?;
    db.apply(&[record("old.rs")], &[], Some("old-commit"))?;
    db.put_search_cache("query", &[json!({"id": 1})])?;
    let before = snapshot(&db)?;
    // Fail after all live rows, generation and checkpoint have been written.
    db.conn.execute_batch("CREATE TRIGGER abort_cache BEFORE DELETE ON search_cache BEGIN SELECT RAISE(ABORT, 'late failure'); END;")?;
    db.cache_put("parse", "completed", "paid artifact")?;
    db.put_embedding("completed", &[0.0, 1.0])?;
    let error = db
        .apply(&[record("new.rs")], &["old.rs".into()], Some("new-commit"))
        .unwrap_err();
    assert!(error.to_string().contains("late failure"));
    assert!(db.conn.is_autocommit());
    assert_eq!(snapshot(&db)?, before);
    drop(db);
    let mut db = Database::open(
        &dir.path().join("index.sqlite"),
        dir.path(),
        &json!({}),
        false,
    )?;
    assert_eq!(snapshot(&db)?, before);
    assert_eq!(
        db.cache("parse", "completed")?.as_deref(),
        Some("paid artifact")
    );
    assert_eq!(db.embedding("completed")?, Some(vec![0.0, 1.0]));
    db.conn.execute_batch("DROP TRIGGER abort_cache")?;
    assert!(db.apply(&[record("new.rs")], &["old.rs".into()], Some("new-commit"))?);
    assert_eq!(db.generation()?, 2);
    assert_eq!(db.meta("checkpoint")?.as_deref(), Some("new-commit"));
    assert!(db.search_cache("query")?.is_none());
    Ok(())
}

#[test]
fn failed_reset_rolls_back_and_leaves_connection_reusable() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    db.apply(&[record("old.rs")], &[], Some("commit"))?;
    db.put_search_cache("query", &[json!("cached")])?;
    let before = snapshot(&db)?;
    db.conn.execute_batch("CREATE TRIGGER abort_reset BEFORE DELETE ON metadata BEGIN SELECT RAISE(ABORT, 'reset failure'); END;")?;
    assert!(
        db.reset()
            .unwrap_err()
            .to_string()
            .contains("reset failure")
    );
    assert!(
        db.conn.is_autocommit(),
        "failed reset must release its transaction"
    );
    assert_eq!(snapshot(&db)?, before);
    db.conn.execute_batch("DROP TRIGGER abort_reset")?;
    db.reset()?;
    assert!(db.files()?.is_empty());
    assert!(db.items()?.is_empty());
    assert_eq!(
        db.meta("identity")?,
        before["identity"].as_str().map(str::to_owned)
    );
    assert!(db.meta("checkpoint")?.is_none());
    assert_eq!(db.generation()?, 0);
    assert!(db.search_cache("query")?.is_none());
    assert!(db.embedding("code")?.is_some());
    Ok(())
}

#[test]
fn incompatible_identity_rejection_preserves_snapshot_and_force_resets_live_state() -> Result<()> {
    for change_root in [false, true] {
        let (dir, mut db) = fixture()?;
        db.apply(&[record("old.rs")], &[], Some("commit"))?;
        db.cache_put("description", "paid", "retained")?;
        db.put_search_cache("query", &[json!(1)])?;
        let before = snapshot(&db)?;
        let path = db.path.clone();
        drop(db);
        let root = if change_root {
            dir.path().join("other")
        } else {
            dir.path().to_owned()
        };
        let profile = if change_root {
            json!({})
        } else {
            json!({"model": "other"})
        };
        if !change_root {
            for force in [false, true] {
                let db = Database::open(&path, &root, &profile, force)?;
                assert_eq!(snapshot(&db)?, before);
                assert_eq!(
                    db.cache("description", "paid")?.as_deref(),
                    Some("retained")
                );
            }
            continue;
        }
        let error = Database::open(&path, &root, &profile, false)
            .err()
            .expect("identity mismatch");
        assert!(error.to_string().contains("Incompatible index"));
        let db = Database::open(&path, dir.path(), &json!({}), false)?;
        assert_eq!(snapshot(&db)?, before);
        drop(db);
        let db = Database::open(&path, &root, &profile, true)?;
        assert!(db.files()?.is_empty());
        assert!(db.items()?.is_empty());
        assert_eq!(db.generation()?, 0);
        assert!(db.meta("checkpoint")?.is_none());
        assert!(db.search_cache("query")?.is_none());
        assert_eq!(
            db.cache("description", "paid")?.as_deref(),
            Some("retained")
        );
        assert_eq!(db.embedding("code")?, Some(vec![0.6, 0.8]));
        let identity = db.meta("identity")?;
        drop(db);
        assert_eq!(
            Database::open(&path, &root, &profile, false)?.meta("identity")?,
            identity
        );
    }
    Ok(())
}

#[test]
fn checkpoint_only_updates_preserve_generation_and_search_cache() -> Result<()> {
    let (dir, mut db) = fixture()?;
    assert!(!db.apply(&[], &[], None)?);
    assert!(db.meta("generation")?.is_none());
    db.apply(&[record("code.rs")], &[], Some("first"))?;
    db.put_search_cache("query", &[json!({"score": 0.75})])?;
    let before = snapshot(&db)?;
    for checkpoint in [Some("first"), Some("second"), None, None] {
        assert!(!db.apply(&[], &[], checkpoint)?);
        assert_eq!(db.generation()?, 1);
        assert_eq!(db.meta("checkpoint")?.as_deref(), checkpoint);
        let mut expected = before.clone();
        expected["checkpoint"] = json!(checkpoint);
        assert_eq!(snapshot(&db)?, expected);
    }
    drop(db);
    let db = Database::open(
        &dir.path().join("index.sqlite"),
        dir.path(),
        &json!({}),
        false,
    )?;
    assert_eq!(db.generation()?, 1);
    assert!(db.meta("checkpoint")?.is_none());
    assert_eq!(
        db.search_cache("query")?,
        Some(vec![json!({"score": 0.75})])
    );
    Ok(())
}

#[test]
fn generation_corruption_and_exhaustion_fail_without_publication() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    db.apply(&[record("old.rs")], &[], Some("old"))?;
    db.put_search_cache("query", &[json!(1)])?;
    for invalid in [
        "not-a-number",
        "-1",
        "18446744073709551616",
        "18446744073709551615",
    ] {
        db.set_meta("generation", invalid)?;
        assert!(
            db.apply(&[record("new.rs")], &["old.rs".into()], Some("new"))
                .is_err(),
            "{invalid}"
        );
        assert!(db.conn.is_autocommit());
        assert_eq!(db.files()?[0].path, "old.rs");
        assert_eq!(db.items()?.len(), 1);
        assert_eq!(db.meta("generation")?.as_deref(), Some(invalid));
        assert_eq!(db.meta("checkpoint")?.as_deref(), Some("old"));
        assert_eq!(db.search_cache("query")?, Some(vec![json!(1)]));
    }
    // Exhaustion does not prevent checkpoint-only updates.
    assert!(!db.apply(&[], &[], Some("new"))?);
    assert_eq!(db.generation()?, u64::MAX);
    Ok(())
}

#[test]
fn embeddings_validate_before_writing_and_preserve_exact_first_value() -> Result<()> {
    let (dir, db) = fixture()?;
    let vector = [0.0, -0.0, f32::MIN_POSITIVE, f32::MAX, -3.25];
    db.put_embedding("exact", &vector)?;
    db.put_embedding("exact", &[9.0])?;
    for invalid in [
        vec![],
        vec![f32::NAN],
        vec![f32::INFINITY],
        vec![f32::NEG_INFINITY],
    ] {
        assert!(db.put_embedding("invalid", &invalid).is_err());
        assert!(db.put_embedding("exact", &invalid).is_err());
        assert!(db.embedding("invalid")?.is_none());
    }
    drop(db);
    let db = Database::open(
        &dir.path().join("index.sqlite"),
        dir.path(),
        &json!({}),
        false,
    )?;
    let stored = db.embedding("exact")?.unwrap();
    assert_eq!(
        stored.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        vector.map(f32::to_bits)
    );
    assert!(db.embedding("missing")?.is_none());
    Ok(())
}

#[test]
fn corrupt_embedding_blobs_are_errors_not_cache_misses() -> Result<()> {
    let (_dir, db) = fixture()?;
    assert!(!db.embedding_exists("missing")?);
    let cases = [
        (vec![], "Invalid stored vector length"),
        (vec![0, 0, 0], "Invalid stored vector length"),
        (vec![0; 5], "Invalid stored vector length"),
        (f32::NAN.to_le_bytes().to_vec(), "Non-finite stored vector"),
        (
            f32::INFINITY.to_le_bytes().to_vec(),
            "Non-finite stored vector",
        ),
        (
            f32::NEG_INFINITY.to_le_bytes().to_vec(),
            "Non-finite stored vector",
        ),
    ];
    for (blob, message) in cases {
        db.conn.execute(
            "INSERT OR REPLACE INTO global.embeddings VALUES('corrupt', ?)",
            [blob],
        )?;
        assert!(db.embedding_exists("corrupt")?);
        assert_eq!(db.embedding("corrupt").unwrap_err().to_string(), message);
        assert_eq!(db.embedding("code")?, Some(vec![0.6, 0.8]));
    }
    Ok(())
}

#[test]
fn projection_presence_defers_vector_validation_until_embedding_read() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    let record = record("code.rs");
    db.apply(std::slice::from_ref(&record), &[], None)?;
    let profile = json!({"model":"a"});
    let code_key = Database::embedding_key(&profile, false, "fn example() {}");
    let file_key = Database::embedding_key(&profile, false, "A description");
    let callable_key = Database::embedding_key(&profile, false, "Callable description");
    for key in [&code_key, &file_key, &callable_key] {
        db.conn.execute(
            "INSERT INTO global.embeddings VALUES(?,?)",
            params![key, vec![0_u8; 3]],
        )?;
        assert!(db.embedding_exists(key)?);
        assert!(db.embedding(key).is_err());
    }
    let item = db.items_for_profile(&profile)?.remove(0);
    assert_eq!(item.embedding, code_key);
    assert_eq!(item.description_embedding, Some(callable_key));
    assert_eq!(
        db.files_for_profile(&profile)?[0].description_embedding,
        Some(file_key)
    );
    Ok(())
}

#[test]
fn corrupt_json_records_fail_reads_and_can_be_repaired() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    db.apply(&[record("code.rs")], &[], None)?;
    assert!(
        db.conn
            .prepare("SELECT data,source FROM main.files")
            .is_err()
    );
    for value in ["{", "[", "not-json"] {
        db.conn.execute("UPDATE diagnostics SET data=?", [value])?;
        assert!(db.files().is_err(), "{value}");
    }
    db.apply(&[record("code.rs")], &[], None)?;
    for value in ["{", "null", "[]"] {
        db.conn.execute("UPDATE search_units SET data=?", [value])?;
        assert!(db.items().is_err(), "{value}");
    }
    for value in ["{", "null", "{}"] {
        db.conn.execute(
            "INSERT OR REPLACE INTO search_cache VALUES('query', ?)",
            [value],
        )?;
        assert!(
            db.search_cache("query")
                .unwrap_err()
                .to_string()
                .contains("Corrupt search cache")
        );
    }
    db.apply(&[record("code.rs")], &[], None)?;
    assert_eq!(db.files()?.len(), 1);
    assert_eq!(db.items()?.len(), 1);
    assert!(db.search_cache("query")?.is_none());
    Ok(())
}

#[test]
fn artifacts_are_namespaced_and_reopen_with_complete_live_records() -> Result<()> {
    let (dir, mut db) = fixture()?;
    let record = record("space/é.rs");
    db.apply(std::slice::from_ref(&record), &[], Some("commit"))?;
    db.cache_put("parse", "same-key", "old")?;
    db.cache_put("description", "same-key", "description")?;
    db.cache_put("parse", "same-key", "new")?;
    let before = snapshot(&db)?;
    drop(db);
    let db = Database::open(
        &dir.path().join("index.sqlite"),
        dir.path(),
        &json!({}),
        false,
    )?;
    assert_eq!(snapshot(&db)?, before);
    assert_eq!(
        serde_json::to_value(&db.files()?[0])?,
        serde_json::to_value(record.0)?
    );
    assert_eq!(
        db.items()?[0].description_embedding.as_deref(),
        Some("description")
    );
    assert_eq!(db.cache("parse", "same-key")?.as_deref(), Some("new"));
    assert_eq!(
        db.cache("description", "same-key")?.as_deref(),
        Some("description")
    );
    assert!(db.cache("other", "same-key")?.is_none());
    let profile = json!({"model": "a", "dimensions": 2});
    let document = Database::embedding_key(&profile, false, "input");
    let keys = [
        document.clone(),
        Database::embedding_key(&profile, true, "input"),
        Database::embedding_key(&json!({"model": "b", "dimensions": 2}), false, "input"),
        Database::embedding_key(&profile, false, "input "),
    ];
    assert_eq!(
        keys.iter().collect::<std::collections::HashSet<_>>().len(),
        keys.len()
    );
    assert_eq!(document, Database::embedding_key(&profile, false, "input"));
    Ok(())
}

#[test]
fn missing_global_embedding_references_abort_publication_and_retry_cleanly() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    for description in [false, true] {
        let mut record = record("code.rs");
        if description {
            record.1[0].description_embedding = Some("missing".into());
        } else {
            record.1[0].embedding = "missing".into();
        }
        assert!(db.apply(&[record], &[], Some("commit")).is_err());
        assert!(db.files()?.is_empty());
        assert!(db.items()?.is_empty());
        assert_eq!(db.generation()?, 0);
        assert!(db.meta("checkpoint")?.is_none());
        assert!(db.conn.is_autocommit());
    }
    assert!(db.apply(&[record("code.rs")], &[], Some("commit"))?);
    assert_eq!(db.generation()?, 1);
    assert_eq!(db.items()?.len(), 1);
    Ok(())
}

#[test]
fn unprefixed_schema_reopens_and_retains_artifacts_on_reset() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("index.sqlite");
    let profile = json!({"model": "test", "dimensions": 2});
    let db = Database::open_with_config(
        &path,
        dir.path(),
        &json!({"artifactCachePath": dir.path().join("global.sqlite")}),
        false,
    )?;
    let tables: Vec<String> = db.conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
    assert_eq!(
        tables,
        [
            "descriptions",
            "diagnostics",
            "files",
            "metadata",
            "search_cache",
            "search_units",
            "symbol_names",
            "symbols",
            "unit_embeddings"
        ]
    );
    db.put_embedding("vector", &[0.6, 0.8])?;
    db.cache_put("parse", "source", "parsed")?;
    db.put_search_cache("query", &[json!({"id": 1})])?;
    db.set_meta("generation", "3")?;
    drop(db);

    let db = Database::open(&path, dir.path(), &profile, false)?;
    assert_eq!(db.generation()?, 3);
    assert!(db.search_cache("query")?.is_some());
    db.reset()?;
    assert_eq!(db.embedding("vector")?, Some(vec![0.6, 0.8]));
    assert_eq!(db.cache("parse", "source")?.as_deref(), Some("parsed"));
    assert_eq!(db.generation()?, 0);
    assert!(db.search_cache("query")?.is_none());
    Ok(())
}

#[test]
fn old_layouts_require_rebuilding_even_with_force() -> Result<()> {
    for schema in [
        "CREATE TABLE rust_metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO rust_metadata VALUES('identity','old');",
        "CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO metadata VALUES('schema_version','11');",
        "CREATE TABLE functions(id INTEGER PRIMARY KEY);",
        "CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO metadata VALUES('identity','{\"schema\":2,\"root\":\"old\",\"embedding\":{}}');",
        "CREATE TABLE files(path TEXT PRIMARY KEY,data TEXT NOT NULL); CREATE TABLE items(id INTEGER PRIMARY KEY);",
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("index.sqlite");
        let conn = Connection::open(&path)?;
        conn.execute_batch(schema)?;
        let before: Vec<Option<String>> = conn
            .prepare("SELECT sql FROM sqlite_master ORDER BY name")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let before_bytes = std::fs::read(&path)?;
        for force in [false, true] {
            let error = Database::open(&path, dir.path(), &json!({}), force)
                .err()
                .expect("old layout must be rejected");
            assert!(error.to_string().contains("Unsupported index table layout"));
            let after: Vec<Option<String>> = conn
                .prepare("SELECT sql FROM sqlite_master ORDER BY name")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            assert_eq!(before, after);
            assert_eq!(std::fs::read(&path)?, before_bytes);
        }
    }
    Ok(())
}

fn parsed_record(path: &str, source: &str) -> Result<(File, ParsedFile)> {
    let mut file = record(path).0;
    file.source = source.into();
    file.hash = hash(source);
    file.language = crate::parse::language_for_path(path).unwrap().into();
    file.description = None;
    file.description_hash = None;
    file.description_embedding = None;
    file.errors.clear();
    Ok((file, crate::parse::parse(path, source)?))
}

#[test]
fn structure_roundtrips_full_metadata_and_links_pending_units() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    let mut record = parsed_record(
        "code.rs",
        "use std::collections::HashMap as Map;\nstruct Café { value: i32 }\nimpl Café {\n    /// Run the service.\n    fn run(&self) -> i32 { self.value }\n}\n",
    )?;
    // Storage preserves the extraction in full; presentation limits belong
    // to the renderer, never to the normalized source of truth.
    record.1.structure.nodes[0].signature = "é".repeat(20_000);
    record.1.structure.nodes[0].names = vec!["Map".into(), "HashMap".into(), "Map".into()];
    record.1.structure.nodes[0]
        .attributes
        .push("#[cfg(feature = \"test\")]".into());
    assert!(
        record
            .1
            .structure
            .nodes
            .iter()
            .any(|node| node.declaration_start_line == Some(5))
    );
    assert!(
        record
            .1
            .structure
            .nodes
            .iter()
            .any(|node| { node.declaration_start_byte == record.0.source.find("fn run") })
    );
    db.apply_structure(std::slice::from_ref(&record), &[], Some("commit"))?;
    assert_eq!(db.structure("code.rs")?, record.1.structure);
    assert!(db.structure_current("code.rs", &record.0.hash, STRUCTURE_PARSER_VERSION)?);
    assert!(!db.structure_current("code.rs", "different", STRUCTURE_PARSER_VERSION)?);
    assert_eq!(db.missing_structure_paths("next-version")?, ["code.rs"]);
    assert!(
        db.missing_structure_paths(STRUCTURE_PARSER_VERSION)?
            .is_empty()
    );
    assert_eq!(db.paths()?, ["code.rs"]);
    let items = db.items_for_profile(&json!({"model":"a"}))?;
    assert_eq!(items.len(), record.1.callables.len());
    assert!(items.iter().all(|i| i.embedding.is_empty()));
    let linked: i64 = db.conn.query_row(
        "SELECT count(*) FROM search_units WHERE symbol_id IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(linked as usize, items.len());
    assert_eq!(
        items[0].identity,
        hash(
            json!([
                "code.rs",
                record.1.callables[0].qualified_name,
                record.1.callables[0].kind,
                0
            ])
            .to_string()
        )
    );
    let mut file = db.files()?.remove(0);
    file.source_mode = "git-head".into();
    db.apply(&[(file, items)], &[], Some("commit"))?;
    assert_eq!(
        db.structure("code.rs")?,
        record.1.structure,
        "semantic apply must reuse stored structure"
    );
    assert_eq!(db.files()?[0].source_mode, "git-head");
    // File-local symbol IDs overlap; the bulk name load must stay scoped
    // to its file and preserve name order, duplicates and empty lists.
    let mut other = parsed_record("other.rs", "struct Other; impl Other { fn run() {} }")?;
    other.1.structure.nodes[0].names.clear();
    db.apply_structure(std::slice::from_ref(&other), &[], Some("commit"))?;
    assert_eq!(db.structure("code.rs")?, record.1.structure);
    assert_eq!(db.structure("other.rs")?, other.1.structure);
    Ok(())
}

#[test]
fn structure_reads_metadata_without_declaration_start_byte() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    let mut record = parsed_record("code.rs", "/// API.\nfn run() {}\n")?;
    db.apply_structure(std::slice::from_ref(&record), &[], None)?;
    db.conn.execute(
        "UPDATE symbols SET metadata=json_remove(metadata,'$.declarationStartByte') WHERE path=?",
        [&record.0.path],
    )?;
    for node in &mut record.1.structure.nodes {
        node.declaration_start_byte = None;
    }
    assert_eq!(db.structure("code.rs")?, record.1.structure);
    Ok(())
}

#[test]
fn projections_reuse_durable_artifacts_and_never_return_stale_or_other_profile_vectors()
-> Result<()> {
    let (dir, mut db) = fixture()?;
    let a = json!({"model":"a","dimensions":2});
    let b = json!({"model":"b","dimensions":2});
    let original = parsed_record("code.rs", "fn example() -> i32 { 1 }")?;
    db.apply_structure(std::slice::from_ref(&original), &[], None)?;
    let id = db.items()?[0].id;
    let input = original.1.callables[0].embedding_input.clone();
    let code_a = Database::embedding_key(&a, false, &input);
    db.put_embedding(&code_a, &[1.0, 0.0])?;
    assert_eq!(
        db.items_for_profile(&a)?[0].embedding,
        code_a,
        "cache lookup requires no published association"
    );
    assert!(db.items_for_profile(&b)?[0].embedding.is_empty());
    let text = "Returns one";
    let mut file = original.0.clone();
    file.description = Some("Example file".into());
    file.description_hash = Some(file.hash.clone());
    for profile in [&a, &b] {
        db.set_projection_profile(profile)?;
        let key = Database::embedding_key(profile, false, &input);
        db.put_embedding(&key, &[1.0, 0.0])?;
        let description_key = Database::embedding_key(profile, false, text);
        db.put_embedding(&description_key, &[0.0, 1.0])?;
        let file_key =
            Database::embedding_key(profile, false, file.description.as_deref().unwrap());
        db.put_embedding(&file_key, &[0.6, 0.8])?;
        file.description_embedding = Some(file_key);
        let mut items = db.items_for_profile(profile)?;
        items[0].data["description"] = json!(text);
        items[0].description_embedding = Some(description_key.clone());
        db.apply(&[(file.clone(), items)], &[], None)?;
        assert_eq!(db.items_for_profile(profile)?[0].embedding, key);
        assert_eq!(
            db.items_for_profile(profile)?[0]
                .description_embedding
                .as_deref(),
            Some(description_key.as_str())
        );
    }
    let associations: i64 = db
        .conn
        .query_row("SELECT count(*) FROM unit_embeddings", [], |r| r.get(0))?;
    assert_eq!(associations, 4);
    assert_eq!(db.items_for_profile(&a)?[0].embedding, code_a);
    assert_eq!(
        db.files_for_profile(&a)?[0].description_embedding,
        Some(Database::embedding_key(&a, false, "Example file"))
    );
    let shifted = parsed_record("code.rs", "\n\nfn example() -> i32 { 1 }")?;
    db.apply_structure(&[shifted], &[], None)?;
    // Even an unchanged callable has new full-file generation context.
    // Code vectors survive, but generated description bindings do not.
    for profile in [&a, &b] {
        let item = db.items_for_profile(profile)?.remove(0);
        assert_eq!(item.id, id);
        assert!(item.data["description"].is_null());
        assert_eq!(
            item.embedding,
            Database::embedding_key(profile, false, &input)
        );
        assert!(item.description_embedding.is_none());
        assert!(
            db.embedding(&Database::embedding_key(profile, false, text))?
                .is_some()
        );
    }
    assert!(db.items()?[0].description_embedding.is_none());
    assert!(db.symbol_descriptions("code.rs")?.is_empty());
    assert_eq!(
        db.conn.query_row(
            "SELECT count(*) FROM descriptions WHERE scope='callable'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    let changed = parsed_record("code.rs", "fn example() -> i32 { 2 }")?;
    db.apply_structure(&[changed], &[], Some("edited"))?;
    for profile in [&a, &b] {
        let item = db.items_for_profile(profile)?.remove(0);
        assert_eq!(item.id, id);
        assert!(item.embedding.is_empty());
        assert!(item.description_embedding.is_none());
        assert!(item.data["description"].is_null());
    }
    assert!(db.items()?[0].embedding.is_empty());
    assert_eq!(db.files()?[0].description.as_deref(), Some("Example file"));
    assert_eq!(
        db.files()?[0].description_hash,
        Some(original.0.hash.clone())
    );
    assert_eq!(
        db.conn
            .query_row("SELECT count(*) FROM unit_embeddings", [], |r| r
                .get::<_, i64>(0))?,
        associations
    );
    db.cache_put("rerank", "paid", "cached response")?;
    drop(db);
    let mut db = Database::open(&dir.path().join("index.sqlite"), dir.path(), &a, true)?;
    assert_eq!(db.items()?[0].id, id);
    db.apply_structure(&[original], &[], None)?;
    assert_eq!(db.items_for_profile(&a)?[0].embedding, code_a);
    db.reset()?;
    assert_eq!(
        db.cache("rerank", "paid")?.as_deref(),
        Some("cached response")
    );
    assert!(db.embedding(&code_a)?.is_some());
    drop(db);
    let db = Database::open(&dir.path().join("index.sqlite"), dir.path(), &b, false)?;
    assert!(db.paths()?.is_empty());
    assert!(db.embedding(&code_a)?.is_some());
    Ok(())
}

#[test]
fn structural_publication_rolls_back_symbols_units_diagnostics_and_checkpoint() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    let original = parsed_record("code.rs", "struct A; impl A { fn run() {} }")?;
    db.apply_structure(std::slice::from_ref(&original), &[], Some("old"))?;
    db.put_search_cache("query", &[json!("old result")])?;
    let before = snapshot(&db)?;
    db.conn.execute_batch("CREATE TRIGGER abort_structure BEFORE DELETE ON search_cache BEGIN SELECT RAISE(ABORT, 'late structure failure'); END;")?;
    db.cache_put("description", "paid", "survives")?;
    let mut changed = parsed_record("code.rs", "struct B; impl B { fn run() { let x = 2; } }")?;
    changed.1.errors.push(crate::parse::Diagnostic {
        message: "new diagnostic".into(),
        start_line: 1,
        end_line: 1,
    });
    let read_error = json!({"code":"read-error","message":"using saved source","path":"code.rs"});
    changed.0.errors.push(read_error.clone());
    assert!(
        db.apply_structure(std::slice::from_ref(&changed), &[], Some("new"))
            .is_err()
    );
    assert!(db.conn.is_autocommit());
    assert_eq!(snapshot(&db)?, before);
    assert_eq!(db.structure("code.rs")?, original.1.structure);
    assert_eq!(
        db.cache("description", "paid")?.as_deref(),
        Some("survives")
    );
    db.conn.execute_batch("DROP TRIGGER abort_structure")?;
    // A deferred FK failure at commit must roll back the entire publication.
    let mut invalid = changed.clone();
    invalid.1.structure.nodes[0].parent_id = Some(999_999);
    assert!(
        db.apply_structure(&[invalid], &[], Some("invalid"))
            .is_err()
    );
    assert!(db.conn.is_autocommit());
    assert_eq!(snapshot(&db)?, before);
    assert_eq!(db.structure("code.rs")?, original.1.structure);
    db.apply_structure(&[changed], &[], Some("new"))?;
    assert_eq!(db.files()?[0].errors[0], read_error);
    assert_eq!(db.files()?[0].errors[1]["message"], "new diagnostic");
    assert_eq!(db.generation()?, 2);
    assert_eq!(db.meta("checkpoint")?.as_deref(), Some("new"));
    db.apply_structure(&[], &["code.rs".into()], None)?;
    assert!(db.paths()?.is_empty());
    for table in [
        "symbols",
        "symbol_names",
        "search_units",
        "unit_embeddings",
        "descriptions",
        "diagnostics",
    ] {
        assert_eq!(
            db.conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
    }
    Ok(())
}

#[test]
fn markdown_and_empty_structures_are_versioned_without_embeddings() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    let markdown = parsed_record("readme.md", "# Overview\n\nHello.\n\n## Detail\n\nWorld.\n")?;
    let empty = parsed_record("empty.rs", "// nothing to declare\n")?;
    db.apply_structure_with_version(&[markdown.clone(), empty.clone()], &[], None, "parser-2")?;
    assert!(db.structure("empty.rs")?.nodes.is_empty());
    assert!(db.structure_current("empty.rs", &empty.0.hash, "parser-2")?);
    assert!(db.structure("missing.rs").is_err());
    assert_eq!(db.structure("readme.md")?, markdown.1.structure);
    for (ordinal, item) in db.items()?.iter().enumerate() {
        assert_eq!(
            item.identity,
            hash(json!(["readme.md", "markdown", ordinal]).to_string())
        );
        assert_eq!(item.kind, "markdown");
        assert!(item.embedding.is_empty());
    }
    let mut semantic = empty.0.clone();
    semantic.source = "fn new() {}".into();
    semantic.hash = hash(&semantic.source);
    db.apply(&[(semantic, vec![])], &[], None)?;
    assert!(
        db.structure("empty.rs").is_err(),
        "old structures must not be presented for new source"
    );
    assert_eq!(db.missing_structure_paths("parser-2")?, ["empty.rs"]);
    Ok(())
}

#[test]
fn projection_selection_and_mismatched_associations_fail_atomically() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    let record = parsed_record("code.rs", "fn example() {}")?;
    db.apply_structure(std::slice::from_ref(&record), &[], None)?;
    let a = json!({"model":"a"});
    let b = json!({"model":"b"});
    db.set_projection_profile(&a)?;
    db.put_search_cache("query", &[json!("a result")])?;
    db.conn.execute_batch("CREATE TRIGGER abort_profile BEFORE DELETE ON search_cache BEGIN SELECT RAISE(ABORT, 'profile failure'); END;")?;
    assert!(db.set_projection_profile(&b).is_err());
    assert_eq!(db.meta("active_embedding_profile")?, Some(a.to_string()));
    assert!(db.search_cache("query")?.is_some());
    db.conn.execute_batch("DROP TRIGGER abort_profile")?;
    let before = snapshot(&db)?;
    let input = &record.1.callables[0].embedding_input;
    let wrong_key = Database::embedding_key(&b, false, input);
    db.put_embedding(&wrong_key, &[0.6, 0.8])?;
    let mut items = db.items_for_profile(&b)?;
    assert_eq!(items[0].embedding, wrong_key);
    assert!(
        db.apply(&[(record.0.clone(), items.clone())], &[], Some("wrong"))
            .is_err()
    );
    assert_eq!(snapshot(&db)?, before);
    let right_key = Database::embedding_key(&a, false, input);
    db.put_embedding(&right_key, &[0.6, 0.8])?;
    items[0].embedding = right_key;
    db.apply(&[(record.0, items)], &[], Some("right"))?;
    db.set_projection_profile(&b)?;
    assert_eq!(db.meta("active_embedding_profile")?, Some(b.to_string()));
    assert_eq!(db.structure("code.rs")?, record.1.structure);
    assert_eq!(db.items_for_profile(&b)?[0].embedding, wrong_key);
    Ok(())
}

#[test]
fn source_descriptions_publish_without_vectors_and_hydrate_for_each_profile() -> Result<()> {
    let (dir, mut db) = fixture()?;
    let mut record = parsed_record(
        "code.rs",
        "const LIMIT: usize = 10;\nstruct Settings;\nfn example() {}\n",
    )?;
    record.1.description = Some("Configuration utilities".into());
    record.1.callables[0].description = Some("Runs the example".into());
    for node in &mut record.1.structure.nodes {
        node.description = Some(
            match node.kind.as_str() {
                "constant" => "Maximum number of requests",
                "struct" => "Request configuration",
                "function" => "Runs the example",
                kind => panic!("unexpected declaration: {kind}"),
            }
            .into(),
        );
    }
    db.apply_structure(std::slice::from_ref(&record), &[], None)?;
    assert_eq!(db.structure("code.rs")?, record.1.structure);
    let file = db.files()?.remove(0);
    assert_eq!(file.hash, hash(&record.0.source));
    assert_eq!(file.description.as_deref(), Some("Configuration utilities"));
    assert_eq!(
        file.description_hash,
        Some(format!("source:{}", hash("Configuration utilities")))
    );
    assert!(file.description_embedding.is_none());
    let items = db.items()?;
    assert_eq!(items.len(), 3);
    assert_eq!(
        items
            .iter()
            .filter(|item| item.kind == "symbol-description")
            .count(),
        2
    );
    assert!(items.iter().all(|item| item.embedding.is_empty()
        && item.description_embedding.is_none()
        && item.data["sourceDescription"] == true));
    let descriptions = db.symbol_descriptions("code.rs")?;
    for node in &record.1.structure.nodes {
        assert_eq!(descriptions.get(&node.id), node.description.as_ref());
    }
    let a = json!({"model":"a"});
    let b = json!({"model":"b"});
    for text in std::iter::once("Configuration utilities").chain(
        items
            .iter()
            .map(|item| item.data["description"].as_str().unwrap()),
    ) {
        db.put_embedding(&Database::embedding_key(&a, false, text), &[1.0, 0.0])?;
    }
    assert!(db.files_for_profile(&a)?[0].description_embedding.is_some());
    assert!(db.files_for_profile(&b)?[0].description_embedding.is_none());
    assert!(
        db.items_for_profile(&a)?
            .iter()
            .all(|item| item.description_embedding.is_some())
    );
    assert!(
        db.items_for_profile(&b)?
            .iter()
            .all(|item| item.description_embedding.is_none())
    );
    // Publish the same description-only units through the engine contract,
    // omitting byte offsets and code input; declaration links must survive.
    db.set_projection_profile(&a)?;
    let mut items = db.items_for_profile(&a)?;
    for item in &mut items {
        if item.kind == "symbol-description" {
            item.data.as_object_mut().unwrap().remove("startByte");
            item.data.as_object_mut().unwrap().remove("endByte");
        }
    }
    db.apply(&[(file, items)], &[], None)?;
    assert_eq!(db.symbol_descriptions("code.rs")?, descriptions);
    assert_eq!(db.conn.query_row(
            "SELECT count(*) FROM unit_embeddings u JOIN search_units s ON s.id=u.unit_id WHERE s.kind='symbol-description' AND u.role='code'",
            [], |r| r.get::<_, i64>(0)
        )?, 0);
    drop(db);
    let db = Database::open_readonly(&dir.path().join("index.sqlite"), dir.path())?;
    assert_eq!(db.symbol_descriptions("code.rs")?, descriptions);
    assert_eq!(
        db.conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
        4
    );
    assert!(
        db.items()?
            .iter()
            .all(|item| item.description_embedding.is_some())
    );
    Ok(())
}

#[test]
fn source_prose_supersedes_generated_prose_and_comment_removal_invalidates_it() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    let original = parsed_record("code.rs", "fn example() -> i32 { 1 }")?;
    db.apply_structure(std::slice::from_ref(&original), &[], None)?;
    let mut file = db.files()?.remove(0);
    file.description = Some("Generated file prose".into());
    file.description_hash = Some(file.hash.clone());
    file.description_embedding = Some("description".into());
    let mut items = db.items()?;
    items[0].data["description"] = json!("Generated callable prose");
    items[0].description_embedding = Some("description".into());
    db.apply(&[(file, items)], &[], None)?;

    let mut commented = original.clone();
    commented.1.description = Some("Source file prose".into());
    commented.1.callables[0].description = Some("Source callable prose".into());
    commented.1.structure.nodes[0].description = Some("Source callable prose".into());
    db.apply_structure(std::slice::from_ref(&commented), &[], None)?;
    let item = db.items()?.remove(0);
    let id = item.id;
    assert_eq!(item.data["description"], "Source callable prose");
    assert_eq!(item.data["sourceDescription"], true);
    assert!(item.description_embedding.is_none());
    assert_eq!(
        db.files()?[0].description.as_deref(),
        Some("Source file prose")
    );
    assert!(db.files()?[0].description_embedding.is_none());

    // Semantic generation cannot overwrite canonical declaration prose.
    let mut generated = item;
    generated.data["description"] = json!("Replacement generated prose");
    generated.data["sourceDescription"] = json!(false);
    generated.description_embedding = Some("description".into());
    let mut generated_file = db.files()?.remove(0);
    generated_file.description = Some("Replacement generated file prose".into());
    generated_file.description_hash = Some(generated_file.hash.clone());
    generated_file.description_embedding = Some("description".into());
    db.apply(&[(generated_file, vec![generated])], &[], None)?;
    assert_eq!(db.items()?[0].data["description"], "Source callable prose");
    assert!(db.items()?[0].description_embedding.is_none());
    assert_eq!(
        db.files()?[0].description.as_deref(),
        Some("Source file prose")
    );
    assert!(db.files()?[0].description_embedding.is_none());

    // Source prose remains authoritative even when executable code changes.
    let mut edited = parsed_record("code.rs", "\nfn example() -> i32 { 2 }")?;
    edited.1.description = commented.1.description.clone();
    edited.1.callables[0].description = commented.1.callables[0].description.clone();
    edited.1.structure.nodes[0].description = commented.1.structure.nodes[0].description.clone();
    db.apply_structure(std::slice::from_ref(&edited), &[], None)?;
    assert_eq!(db.items()?[0].id, id);
    assert_eq!(db.items()?[0].data["description"], "Source callable prose");

    // A removed comment must not be retained merely because callable source
    // hashes match. The engine may pass the previously hydrated file slot.
    edited.0 = db.files()?.remove(0);
    edited.1.description = None;
    edited.1.callables[0].description = None;
    edited.1.structure.nodes[0].description = None;
    db.apply_structure(&[edited], &[], None)?;
    let item = db.items()?.remove(0);
    assert_eq!(item.id, id);
    assert!(item.data["description"].is_null());
    assert_eq!(item.data["sourceDescription"], false);
    assert!(item.description_embedding.is_none());
    let file = db.files()?.remove(0);
    assert!(file.description.is_none());
    assert!(file.description_hash.is_none());
    assert!(file.description_embedding.is_none());
    assert!(db.symbol_descriptions("code.rs")?.is_empty());
    Ok(())
}

#[test]
fn symbol_description_occurrences_and_vectors_survive_structural_edits() -> Result<()> {
    let (_dir, mut db) = fixture()?;
    let mut record = parsed_record("code.ts", "type A = string;\ntype A = number;\n")?;
    assert_eq!(record.1.structure.nodes.len(), 2);
    record.1.structure.nodes[0].description = Some("First overload".into());
    record.1.structure.nodes[1].description = Some("Second overload".into());
    db.apply_structure(std::slice::from_ref(&record), &[], None)?;
    let profile = json!({"model":"a"});
    db.set_projection_profile(&profile)?;
    let mut items = db.items_for_profile(&profile)?;
    for item in &mut items {
        let text = item.data["description"].as_str().unwrap();
        let key = Database::embedding_key(&profile, false, text);
        db.put_embedding(&key, &[1.0, 0.0])?;
        item.description_embedding = Some(key);
    }
    let second_id = items[1].id;
    let second_identity = items[1].identity.clone();
    assert_eq!(
        second_identity,
        hash(
            json!([
                "code.ts",
                "symbol-description",
                "A",
                record.1.structure.nodes[1].kind,
                1
            ])
            .to_string()
        )
    );
    db.apply(&[(db.files()?.remove(0), items)], &[], None)?;
    record.1.structure.nodes[0].description = None;
    db.apply_structure(std::slice::from_ref(&record), &[], None)?;
    let second = db.items()?.remove(0);
    assert_eq!(second.id, second_id);
    assert_eq!(second.identity, second_identity);
    assert!(second.embedding.is_empty());
    assert!(second.description_embedding.is_some());
    assert_eq!(db.symbol_descriptions("code.ts")?.len(), 1);
    record.1.structure.nodes[1].description = Some("Updated second overload".into());
    db.apply_structure(std::slice::from_ref(&record), &[], None)?;
    let second = db.items_for_profile(&profile)?.remove(0);
    assert_eq!(second.id, second_id);
    assert_eq!(second.data["description"], "Updated second overload");
    assert!(second.description_embedding.is_none());
    record.1.structure.nodes[1].description = None;
    db.apply_structure(&[record], &[], None)?;
    assert!(db.items()?.is_empty());
    assert!(db.symbol_descriptions("code.ts")?.is_empty());
    assert_eq!(
        db.conn.query_row(
            "SELECT count(*) FROM descriptions WHERE scope='callable'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}
