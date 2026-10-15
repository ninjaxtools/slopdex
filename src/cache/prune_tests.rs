use super::*;
use crate::{parse, storage::File};
use serde_json::json;

fn generation(path: &str, source: &str) -> Result<(DescriptionArtifact, Vec<(String, String)>)> {
    DescriptionArtifact::new(
        "Generated summary".into(),
        &DescriptionGeneration {
            scope: "callable".into(),
            path: path.into(),
            symbol: Some("alpha".into()),
            source_hash: hash(source),
            file_hash: hash(source),
            file_description: Some("Context".into()),
            profile: json!({"model":"local-or-remote"}),
            settings: json!({}),
            system: "Summarize".into(),
            prompt: source.into(),
            messages: vec![crate::models::Message::user(source.into())],
            regenerate: false,
        },
    )
}

fn publish(db: &mut Database, source: &str) -> Result<()> {
    db.apply_structure(
        &[(
            File {
                path: "code.rs".into(),
                hash: hash(source),
                source: source.into(),
                language: "rust".into(),
                source_mode: "file".into(),
                description: None,
                description_hash: None,
                description_embedding: None,
                errors: Vec::new(),
            },
            parse::parse("code.rs", source)?,
        )],
        &[],
        None,
    )?;
    Ok(())
}

#[test]
fn prune_retains_complete_paid_context_and_live_authoritative_graph() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let base = temp.path().canonicalize()?;
    let live = base.join("live");
    let dead = base.join("dead");
    std::fs::create_dir(&live)?;
    std::fs::create_dir(&dead)?;
    let config = json!({"artifactCachePath":base.join("global.sqlite")});
    let live_index = base.join("live.sqlite");
    let dead_index = base.join("dead.sqlite");
    let source = "/// A source-only description.\npub fn alpha() {}\n";
    let mut db = Database::open_with_config(&live_index, &live, &config, false)?;
    publish(&mut db, source)?;
    let mut gone = Database::open_with_config(&dead_index, &dead, &config, false)?;
    publish(&mut gone, "pub fn deleted() {}\n")?;
    let (artifact, contents) = generation("old.rs", "old source")?;
    db.put_description_artifact(
        &hash("old generation"),
        &serde_json::to_string(&artifact)?,
        &contents,
    )?;
    for key in ["query", "symbol-name", "old-profile", "interrupted"] {
        db.put_embedding(&hash(key), &[1.0, 0.0])?;
    }
    db.cache_put("explanation", &hash("explanation"), "Paid explanation")?;
    db.cache_put("rerank", &hash("ranking"), "[0.5]")?;
    db.cache_put(
        "future-provider-kind",
        &hash("future"),
        "unknown must survive",
    )?;
    db.cache_put("parse", &hash("unused parse"), "{}")?;
    db.cache_put("callgraph", &hash("unused graph"), "{}")?;
    db.put_description_content(&hash("orphan"), "orphan")?;
    drop(gone);
    drop(db);
    std::fs::remove_dir(&dead)?;
    let report = prune(&config, false)?;
    assert_eq!(report.worktrees_removed, 1);
    assert!(report.artifact_rows_deleted > 0);
    assert!(!dead_index.exists());
    let db = Database::open_readonly_with_config(&live_index, &live, &config)?;
    assert_eq!(db.files()?[0].source, source);
    assert!(
        db.structure("code.rs")?
            .nodes
            .iter()
            .any(|node| node.description.as_deref() == Some("A source-only description."))
    );
    assert!(!db.items()?.is_empty(), "unit-content remains readable");
    assert!(db.cache("description", &hash("old generation"))?.is_some());
    for (key, content) in contents {
        assert_eq!(
            db.description_content(&key)?.as_deref(),
            Some(content.as_str())
        );
    }
    for key in ["query", "symbol-name", "old-profile", "interrupted"] {
        assert_eq!(db.embedding(&hash(key))?, Some(vec![1.0, 0.0]));
    }
    assert!(db.cache("explanation", &hash("explanation"))?.is_some());
    assert!(db.cache("rerank", &hash("ranking"))?.is_some());
    assert!(db.cache("future-provider-kind", &hash("future"))?.is_some());
    assert!(db.cache("parse", &hash("unused parse"))?.is_none());
    assert!(db.cache("callgraph", &hash("unused graph"))?.is_none());
    assert!(db.description_content(&hash("orphan"))?.is_none());
    Ok(())
}

#[test]
fn provider_prune_flags_are_independent_and_preserve_live_bindings() -> Result<()> {
    for (vectors, descriptions) in [(true, false), (false, true), (true, true)] {
        let temp = tempfile::tempdir()?;
        let base = temp.path().canonicalize()?;
        let config = json!({"artifactCachePath":base.join("global.sqlite")});
        let index = base.join("index.sqlite");
        let source = "pub fn alpha() {}\n";
        let mut db = Database::open_with_config(&index, &base, &config, false)?;
        publish(&mut db, source)?;
        let live_key = hash("bound vector");
        let unused_key = hash("query vector");
        for key in [&live_key, &unused_key] {
            db.put_embedding(key, &[1.0, 0.0])?;
        }
        db.conn.execute("INSERT INTO unit_embeddings SELECT id,'code','profile',embedding_input_hash,? FROM search_units", [&live_key])?;
        let mut records = Vec::new();
        for (path, text) in [("code.rs", source), ("deleted.rs", "old source")] {
            let (artifact, contents) = generation(path, text)?;
            let key = hash(path);
            db.put_description_artifact(&key, &serde_json::to_string(&artifact)?, &contents)?;
            records.push((key, contents));
        }
        drop(db);
        let report = prune_with_options(
            &config,
            PruneOptions {
                prune_vectors: vectors,
                prune_descriptions: descriptions,
                ..PruneOptions::default()
            },
        )?;
        assert_eq!(report.vector_rows_deleted, u64::from(vectors));
        assert_eq!(report.description_rows_deleted, u64::from(descriptions));
        let db = Database::open_readonly_with_config(&index, &base, &config)?;
        assert!(db.embedding(&live_key)?.is_some());
        assert_eq!(db.embedding(&unused_key)?.is_none(), vectors);
        assert!(db.cache("description", &records[0].0)?.is_some());
        assert_eq!(
            db.cache("description", &records[1].0)?.is_none(),
            descriptions
        );
        for (key, content) in &records[0].1 {
            assert_eq!(
                db.description_content(key)?.as_deref(),
                Some(content.as_str())
            );
        }
        assert_eq!(db.files()?[0].source, source);
    }
    Ok(())
}

#[test]
fn prune_ignores_and_removes_obsolete_pins() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let config = json!({"artifactCachePath":temp.path().join("global.sqlite")});
    let path = store_path(&config)?;
    let conn = open_store(&path, false)?;
    conn.execute_batch(
        "DROP TABLE workspaces;
        CREATE TABLE legacy_artifacts(kind TEXT NOT NULL,key TEXT NOT NULL,PRIMARY KEY(kind,key));
        INSERT INTO legacy_artifacts VALUES('source','old'),('unit-content','old'),('description-content','old'),('embedding','old');
        INSERT INTO sources VALUES('old','old source');
        INSERT INTO cache VALUES('unit-content','old','old payload');
        INSERT INTO description_content VALUES('old','old description');
        INSERT INTO embeddings VALUES('old',X'0000803F');",
    )?;
    let report = prune_with_options(
        &config,
        PruneOptions {
            prune_vectors: true,
            prune_descriptions: true,
            ..PruneOptions::default()
        },
    )?;
    assert_eq!(report.vector_rows_deleted, 1);
    assert_eq!(report.artifact_rows_deleted, 3);
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='legacy_artifacts'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    assert!(
        serde_json::to_value(&report)?
            .get("legacyArtifactsRetained")
            .is_none()
    );
    for table in ["sources", "cache", "description_content", "embeddings"] {
        assert_eq!(
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                .get::<_, i64>(0))?,
            0
        );
    }
    Ok(())
}

#[test]
fn obsolete_pins_do_not_change_default_provider_retention_or_opt_in_pruning() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let config = json!({"artifactCachePath":temp.path().join("global.sqlite")});
    let db = Database::open_with_config(
        &temp.path().join("index.sqlite"),
        temp.path(),
        &config,
        false,
    )?;
    let store = db.global_path().to_owned();
    let vector = hash("old unbound provider vector");
    db.put_embedding(&vector, &[1.0, 0.0])?;
    let (artifact, contents) = generation("untracked.rs", "old source")?;
    let key = hash("old generated description");
    db.put_description_artifact(&key, &serde_json::to_string(&artifact)?, &contents)?;
    db.conn.execute_batch("CREATE TABLE global.legacy_artifacts(kind TEXT NOT NULL,key TEXT NOT NULL,PRIMARY KEY(kind,key));
        INSERT INTO global.sources VALUES('old','old source');
        INSERT INTO global.cache VALUES('unit-content','old','old payload');
        INSERT INTO global.description_content VALUES('old','unused description content');
        INSERT INTO global.legacy_artifacts SELECT 'source',hash FROM global.sources;
        INSERT INTO global.legacy_artifacts SELECT 'unit-content',key FROM global.cache WHERE kind='unit-content';
        INSERT INTO global.legacy_artifacts SELECT 'description-content',hash FROM global.description_content;
        INSERT INTO global.legacy_artifacts SELECT 'embedding',key FROM global.embeddings;")?;
    drop(db);
    let report = prune(&config, false)?;
    assert_eq!(report.artifact_rows_deleted, 3);
    assert_eq!(
        (report.vector_rows_deleted, report.description_rows_deleted),
        (0, 0)
    );
    let conn = open_store(&store, true)?;
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='legacy_artifacts'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM embeddings WHERE key=?",
            [&vector],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    let text: String = conn.query_row(
        "SELECT value FROM cache WHERE kind='description' AND key=?",
        [&key],
        |r| r.get(0),
    )?;
    assert_eq!(DescriptionArtifact::decode(&text)?.text, artifact.text);
    for (hash, value) in &contents {
        assert_eq!(
            conn.query_row(
                "SELECT content FROM description_content WHERE hash=?",
                [hash],
                |r| r.get::<_, String>(0)
            )?,
            *value
        );
    }
    drop(conn);
    let report = prune_with_options(
        &config,
        PruneOptions {
            prune_vectors: true,
            prune_descriptions: true,
            ..PruneOptions::default()
        },
    )?;
    assert_eq!(
        (report.vector_rows_deleted, report.description_rows_deleted),
        (1, 1)
    );
    let conn = open_store(&store, true)?;
    assert_eq!(
        conn.query_row("SELECT count(*) FROM embeddings", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM description_content", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM cache", [], |r| r.get::<_, i64>(0))?,
        0
    );
    Ok(())
}

#[test]
fn prune_waits_for_store_users_before_touching_a_snapshot() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let config = json!({"artifactCachePath":temp.path().join("global.sqlite")});
    let db = Database::open_with_config(
        &temp.path().join("index.sqlite"),
        temp.path(),
        &config,
        false,
    )?;
    db.conn.execute(
        "INSERT INTO global.snapshots VALUES('pending','manifest')",
        [],
    )?;
    let artifacts = Artifacts::open(&config)?;
    let result = prune_within(&config, PruneOptions::default(), Duration::from_millis(30));
    assert!(format!("{:#}", result.unwrap_err()).contains("is busy"));
    assert_eq!(
        db.conn
            .query_row("SELECT count(*) FROM global.snapshots", [], |r| r
                .get::<_, i64>(0))?,
        1
    );
    drop(db);
    assert!(
        prune_within(&config, PruneOptions::default(), Duration::from_millis(30)).is_err(),
        "Artifacts alone also excludes cleanup"
    );
    drop(artifacts);
    assert_eq!(prune(&config, false)?.snapshot_rows_deleted, 1);
    Ok(())
}

#[test]
fn prune_keeps_live_symbol_vectors_and_shared_or_busy_bases() -> Result<()> {
    use fs2::FileExt;
    let temp = tempfile::tempdir()?;
    let config = json!({"artifactCachePath":temp.path().join("global.sqlite")});
    let index = temp.path().join("index.sqlite");
    let db = Database::open_with_config(&index, temp.path(), &config, false)?;
    let symbol = hash("live symbol");
    let delta = hash("live delta");
    for key in [&symbol, &delta] {
        db.put_embedding(key, &[1.0, 0.0])?;
    }
    let store = db.global_path().to_owned();
    drop(db);
    let contract = hash("contract");
    let live = hash("live base");
    let dead = hash("unused base");
    let busy = hash("busy base");
    let directory = PathBuf::from(format!("{}.indexes", store.display())).join(&contract);
    std::fs::create_dir_all(&directory)?;
    for base in [&live, &dead, &busy] {
        for suffix in [".base.json", ".usearch", ".usearch.manifest.json"] {
            std::fs::write(directory.join(format!("{base}{suffix}")), "derived")?;
        }
    }
    std::fs::write(
        directory.join(format!("{dead}.usearch.tmp-in-progress")),
        "pending",
    )?;
    let busy_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(format!("{busy}.lock")))?;
    FileExt::lock_exclusive(&busy_lock)?;
    std::fs::write(format!("{}.symbols.shared.json", index.display()), json!({
        "contract":contract,"base_id":live,"membership":{"1":symbol},"delta":{(delta):hash("vector")}
    }).to_string())?;
    let report = prune_with_options(
        &config,
        PruneOptions {
            prune_vectors: true,
            ..PruneOptions::default()
        },
    )?;
    assert_eq!(report.vector_rows_deleted, 0);
    assert_eq!(report.index_files_removed, 3);
    assert!(directory.join(format!("{live}.base.json")).exists());
    assert!(directory.join(format!("{busy}.base.json")).exists());
    assert!(!directory.join(format!("{dead}.base.json")).exists());
    assert!(directory.join(format!("{dead}.lock")).exists());
    assert!(
        directory
            .join(format!("{dead}.usearch.tmp-in-progress"))
            .exists()
    );
    FileExt::unlock(&busy_lock)?;
    assert_eq!(prune(&config, false)?.index_files_removed, 3);
    Ok(())
}

#[test]
fn prune_discovers_unregistered_default_indexes_and_keeps_shared_live_content() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let base = temp.path().canonicalize()?;
    let config = json!({"artifactCachePath":base.join("global.sqlite")});
    let source = "pub fn shared() {}\n";
    let mut indexes = Vec::new();
    for name in ["live", "deleted"] {
        let root = base.join(name);
        std::fs::create_dir(&root)?;
        let index = base
            .join("worktrees-v1")
            .join(hash(root.as_os_str().as_encoded_bytes()))
            .join("index.sqlite");
        std::fs::create_dir_all(index.parent().unwrap())?;
        let mut db = Database::open_with_config(&index, &root, &config, false)?;
        publish(&mut db, source)?;
        db.conn.execute("DELETE FROM global.workspaces", [])?;
        indexes.push((root, index));
    }
    std::fs::remove_dir(&indexes[1].0)?;
    assert_eq!(prune(&config, false)?.worktrees_removed, 1);
    assert!(!indexes[1].1.exists());
    let db = Database::open_readonly_with_config(&indexes[0].1, &indexes[0].0, &config)?;
    assert_eq!(db.files()?[0].source, source);
    assert!(!db.items()?.is_empty());
    Ok(())
}

#[test]
fn prune_never_deletes_a_store_or_another_index_with_a_sidecar_like_name() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let base = temp.path().canonicalize()?;
    let gone = base.join("gone");
    let live = base.join("live");
    for root in [&gone, &live] {
        std::fs::create_dir(root)?;
    }
    let gone_index = base.join("index.sqlite");
    let live_index = base.join("index.sqlite.symbols.shared.json");
    let store = base.join("index.sqlite.code.shared.json");
    let config = json!({"artifactCachePath":store});
    let mut dead_db = Database::open_with_config(&gone_index, &gone, &config, false)?;
    publish(&mut dead_db, "pub fn gone() {}\n")?;
    let mut live_db = Database::open_with_config(&live_index, &live, &config, false)?;
    publish(&mut live_db, "pub fn alive() {}\n")?;
    live_db.put_embedding(&hash("paid"), &[1.0, 0.0])?;
    drop((dead_db, live_db));
    std::fs::remove_dir(&gone)?;
    assert_eq!(prune(&config, false)?.worktrees_removed, 1);
    assert!(!gone_index.exists());
    assert!(store.exists() && live_index.exists());
    let db = Database::open_readonly_with_config(&live_index, &live, &config)?;
    assert_eq!(db.files()?[0].source, "pub fn alive() {}\n");
    assert!(db.embedding(&hash("paid"))?.is_some());
    Ok(())
}

#[test]
fn prune_leaves_foreign_store_indexes_untouched_and_unlocked() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let base = temp.path().canonicalize()?;
    let config = json!({"artifactCachePath":base.join("selected.sqlite")});
    drop(Artifacts::open(&config)?);
    let root = base.join("foreign");
    std::fs::create_dir(&root)?;
    let index = base
        .join("worktrees-v1")
        .join(hash(root.as_os_str().as_encoded_bytes()))
        .join("index.sqlite");
    std::fs::create_dir_all(index.parent().unwrap())?;
    let foreign = json!({"artifactCachePath":base.join("foreign.sqlite")});
    let mut db = Database::open_with_config(&index, &root, &foreign, false)?;
    publish(&mut db, "pub fn foreign() {}\n")?;
    drop(db);
    std::fs::remove_dir(&root)?;
    assert_eq!(prune(&config, false)?.worktrees_removed, 0);
    assert!(index.exists());
    assert!(!PathBuf::from(format!("{}.lock", index.display())).exists());
    let db = Database::open_readonly_with_config(&index, &root, &foreign)?;
    assert!(!db.files()?.is_empty());
    Ok(())
}

#[test]
fn busy_index_defers_global_reachability_cleanup() -> Result<()> {
    use fs2::FileExt;
    let temp = tempfile::tempdir()?;
    let base = temp.path().canonicalize()?;
    let root = base.join("gone");
    std::fs::create_dir(&root)?;
    let config = json!({"artifactCachePath":base.join("global.sqlite")});
    let index = base.join("index.sqlite");
    let mut db = Database::open_with_config(&index, &root, &config, false)?;
    publish(&mut db, "pub fn gone() {}\n")?;
    let store = db.global_path().to_owned();
    drop(db);
    std::fs::remove_dir(&root)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(format!("{}.lock", index.display()))?;
    FileExt::lock_exclusive(&lock)?;
    let report = prune(&config, false)?;
    assert_eq!((report.worktrees_removed, report.worktrees_busy), (0, 1));
    assert_eq!(report.artifact_rows_deleted, 0);
    assert!(index.exists());
    assert_eq!(
        open_store(&store, true)?
            .query_row("SELECT count(*) FROM sources", [], |r| r.get::<_, i64>(0))?,
        1
    );
    FileExt::unlock(&lock)?;
    assert_eq!(prune(&config, false)?.worktrees_removed, 1);
    assert!(!index.exists());
    Ok(())
}

#[test]
#[cfg(unix)]
fn removed_root_alias_does_not_make_the_registered_physical_worktree_missing() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let base = temp.path().canonicalize()?;
    let root = base.join("physical");
    let alias = base.join("alias");
    std::fs::create_dir(&root)?;
    match std::os::unix::fs::symlink(&root, &alias) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let config = json!({"artifactCachePath":base.join("global.sqlite")});
    let index = base.join("index.sqlite");
    let mut db = Database::open_with_config(&index, &alias, &config, false)?;
    publish(&mut db, "pub fn alive() {}\n")?;
    drop(db);
    std::fs::remove_file(&alias)?;
    let report = prune(&config, false)?;
    assert_eq!(report.worktrees_removed, 0);
    assert_eq!(report.artifact_rows_deleted, 0);
    let db = Database::open_readonly_with_config(&index, &alias, &config)?;
    assert!(!db.files()?.is_empty());
    let registered: String = db
        .conn
        .query_row("SELECT root FROM global.workspaces", [], |r| r.get(0))?;
    assert_eq!(Path::new(&registered).canonicalize()?, root.canonicalize()?);
    Ok(())
}

#[test]
fn restored_root_is_marked_before_global_sweeping() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let base = temp.path().canonicalize()?;
    let root = base.join("restored");
    std::fs::create_dir(&root)?;
    let config = json!({"artifactCachePath":base.join("global.sqlite")});
    let index = base.join("index.sqlite");
    let mut db = Database::open_with_config(&index, &root, &config, false)?;
    publish(&mut db, "pub fn restored() {}\n")?;
    let vector = hash("live vector");
    db.put_embedding(&vector, &[1.0, 0.0])?;
    db.conn.execute("INSERT INTO unit_embeddings SELECT id,'code','profile',embedding_input_hash,? FROM search_units", [&vector])?;
    let store = db.global_path().to_owned();
    drop(db);
    std::fs::remove_dir(&root)?;
    let conn = open_store(&store, false)?;
    let maintenance = registry::store_usage(&store, true, Duration::ZERO)?;
    let report = super::prune::cleanup_with(
        &conn,
        &store,
        PruneOptions {
            prune_vectors: true,
            ..PruneOptions::default()
        },
        &base.join("worktrees-v1"),
        |path| {
            std::fs::create_dir(path)?;
            Ok(())
        },
    )?;
    assert_eq!(report.worktrees_removed, 0);
    assert_eq!(report.artifact_rows_deleted, 0);
    assert_eq!(report.vector_rows_deleted, 0);
    drop(maintenance);
    let db = Database::open_readonly_with_config(&index, &root, &config)?;
    assert_eq!(db.files()?[0].source, "pub fn restored() {}\n");
    assert!(!db.items()?.is_empty());
    assert!(db.embedding(&vector)?.is_some());
    Ok(())
}

#[test]
fn a_custom_index_that_also_names_a_lifetime_lock_is_never_unlinked() -> Result<()> {
    use fs2::FileExt;
    let temp = tempfile::tempdir()?;
    let base = temp.path().canonicalize()?;
    let live = base.join("live");
    let gone = base.join("gone");
    for root in [&live, &gone] {
        std::fs::create_dir(root)?;
    }
    let config = json!({"artifactCachePath":base.join("global.sqlite")});
    let index = base.join("index.sqlite");
    let lock_path = base.join("index.sqlite.lock");
    let mut first = Database::open_with_config(&index, &live, &config, false)?;
    let mut second = Database::open_with_config(&lock_path, &gone, &config, false)?;
    publish(&mut first, "pub fn live() {}\n")?;
    publish(&mut second, "pub fn gone() {}\n")?;
    drop((first, second));
    std::fs::remove_dir(&gone)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)?;
    FileExt::lock_exclusive(&lock)?;
    let report = prune(&config, false)?;
    assert_eq!(report.worktrees_removed, 0);
    assert_eq!(report.worktrees_busy, 2);
    assert!(index.exists() && lock_path.exists());
    let same_domain = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)?;
    assert!(FileExt::try_lock_exclusive(&same_domain).is_err());
    FileExt::unlock(&lock)?;
    Ok(())
}

#[test]
fn description_pruning_keeps_generation_context_for_a_live_stale_file_answer() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let base = temp.path().canonicalize()?;
    let config = json!({"artifactCachePath":base.join("global.sqlite")});
    let index = base.join("index.sqlite");
    let mut db = Database::open_with_config(&index, &base, &config, false)?;
    let source = "pub fn original() {}\n";
    publish(&mut db, source)?;
    let (mut artifact, contents) = generation("code.rs", source)?;
    artifact.generation.scope = "file".into();
    artifact.generation.symbol = None;
    let key = hash("file generation");
    db.put_description_artifact(&key, &serde_json::to_string(&artifact)?, &contents)?;
    let answer_hash = hash(&artifact.text);
    db.put_description_content(&answer_hash, &artifact.text)?;
    db.conn.execute(
        "INSERT INTO descriptions VALUES('file','code.rs','',?,?,NULL)",
        params![hash(source), answer_hash],
    )?;
    publish(&mut db, "pub fn edited() {}\n")?;
    assert_eq!(
        db.files()?[0].description.as_deref(),
        Some(artifact.text.as_str())
    );
    drop(db);
    let report = prune_with_options(
        &config,
        PruneOptions {
            prune_descriptions: true,
            ..PruneOptions::default()
        },
    )?;
    assert_eq!(report.description_rows_deleted, 0);
    let db = Database::open_readonly_with_config(&index, &base, &config)?;
    assert!(db.cache("description", &key)?.is_some());
    for (key, value) in contents {
        assert_eq!(
            db.description_content(&key)?.as_deref(),
            Some(value.as_str())
        );
    }
    Ok(())
}

#[test]
fn prune_never_confuses_a_registered_custom_index_with_a_shared_base_payload() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let base = temp.path().canonicalize()?;
    let store = base.join("global.sqlite");
    let config = json!({"artifactCachePath":store});
    let directory = base.join("global.sqlite.indexes").join(hash("contract"));
    std::fs::create_dir_all(&directory)?;
    let index = directory.join(format!("{}.usearch", hash("custom index")));
    let mut db = Database::open_with_config(&index, &base, &config, false)?;
    publish(&mut db, "pub fn custom() {}\n")?;
    drop(db);
    let report = prune(&config, false)?;
    assert_eq!(report.index_files_removed, 0);
    assert!(index.exists());
    let db = Database::open_readonly_with_config(&index, &base, &config)?;
    assert_eq!(db.files()?[0].source, "pub fn custom() {}\n");
    Ok(())
}
