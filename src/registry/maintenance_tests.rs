use super::*;
use crate::cache;
use std::sync::{Arc, Barrier, mpsc};

struct Fixture {
    _dir: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    store: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let base = dir.path().canonicalize()?;
        let root = base.join("workspace");
        fs::create_dir(&root)?;
        Ok(Self {
            _dir: dir,
            store: base.join("global.sqlite"),
            base,
            root,
        })
    }

    fn db(&self, name: &str, root: &Path) -> Result<Database> {
        Database::open_with_config(
            &self.base.join(name),
            root,
            &json!({"artifactCachePath":self.store}),
            false,
        )
    }

    fn open_store(&self) -> Result<PathBuf> {
        drop(cache::open_store(&self.store, false)?);
        Ok(self.store.canonicalize()?)
    }
}

fn expect_busy(result: Result<ArtifactLock>) -> Result<()> {
    let error = result
        .err()
        .context("Maintenance unexpectedly acquired the store")?;
    ensure!(error.to_string().contains("store is busy"), "{error:#}");
    Ok(())
}

fn assert_no_legacy_artifacts(conn: &Connection, schema: &str) -> Result<()> {
    assert_eq!(
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM {schema}.sqlite_master WHERE type='table' AND name='legacy_artifacts'"
            ),
            [],
            |row| row.get::<_, i64>(0),
        )?,
        0
    );
    Ok(())
}

#[test]
fn shared_store_users_coexist_and_maintenance_excludes_both_modes() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.open_store()?;
    let timeout = Duration::from_millis(60);
    let first = store_usage(&store, false, timeout)?;
    let second = store_usage(&store, false, timeout)?;
    expect_busy(store_usage(&store, true, timeout))?;
    drop(first);
    expect_busy(store_usage(&store, true, Duration::ZERO))?;
    drop(second);
    let maintenance = store_usage(&store, true, timeout)?;
    expect_busy(store_usage(&store, false, timeout))?;
    expect_busy(store_usage(&store, true, Duration::ZERO))?;
    // Initialization and usage are separate locks: callers initialize first,
    // and a warm opener must reach its usage wait without taking a write lock.
    drop(lock_path(&store, "store", "initialize")?);
    drop(maintenance);
    drop(store_usage(&store, false, timeout)?);
    let maintenance_path = lock_directory(&store).join(lock_file_name("store", "maintenance")?);
    assert!(maintenance_path.exists());
    assert_eq!(prune_locks(&store)?, (0, 0));
    assert!(maintenance_path.exists());
    assert!(
        lock_directory(&store)
            .join(lock_file_name("store", "initialize")?)
            .exists()
    );
    Ok(())
}

#[test]
fn maintenance_waits_for_all_shared_users_then_proceeds() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.open_store()?;
    let first = store_usage(&store, false, Duration::ZERO)?;
    let second = store_usage(&store, false, Duration::ZERO)?;
    let barrier = Arc::new(Barrier::new(2));
    let (sender, receiver) = mpsc::channel();
    let worker = {
        let barrier = barrier.clone();
        thread::spawn(move || {
            barrier.wait();
            let result = store_usage(&store, true, Duration::from_secs(5)).map(drop);
            sender.send(result).expect("maintenance result receiver");
        })
    };
    barrier.wait();
    let waited_for_both = receiver.recv_timeout(Duration::from_millis(60));
    drop(first);
    let waited_for_second = receiver.recv_timeout(Duration::from_millis(60));
    drop(second);
    let finished = receiver.recv_timeout(Duration::from_secs(5));
    worker.join().expect("maintenance worker");
    assert!(matches!(
        waited_for_both,
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert!(matches!(
        waited_for_second,
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    finished??;
    Ok(())
}

#[test]
fn writable_and_readonly_databases_retain_shared_guards_until_dropped() -> Result<()> {
    let fixture = Fixture::new()?;
    let first = fixture.db("first.sqlite", &fixture.root)?;
    let second = fixture.db("second.sqlite", &fixture.root)?;
    let readonly = Database::open_readonly(&second.path, &fixture.root)?;
    let store = first.global_path().to_owned();
    expect_busy(store_usage(&store, true, Duration::ZERO))?;
    drop(first);
    expect_busy(store_usage(&store, true, Duration::ZERO))?;
    drop(second);
    expect_busy(store_usage(&store, true, Duration::ZERO))?;
    drop(readonly);
    drop(store_usage(&store, true, Duration::ZERO)?);
    Ok(())
}

#[test]
fn maintenance_blocks_writable_and_readonly_database_opens() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.db("index.sqlite", &fixture.root)?;
    let path = db.path.clone();
    let store = db.global_path().to_owned();
    drop(db);
    for readonly in [false, true] {
        let maintenance = store_usage(&store, true, Duration::ZERO)?;
        let barrier = Arc::new(Barrier::new(2));
        let (sender, receiver) = mpsc::channel();
        let worker = {
            let (barrier, path, root) = (barrier.clone(), path.clone(), fixture.root.clone());
            thread::spawn(move || {
                barrier.wait();
                let result = if readonly {
                    Database::open_readonly(&path, &root)
                } else {
                    Database::open(&path, &root, &json!({}), false)
                };
                sender
                    .send(result.map(drop))
                    .expect("database opener result receiver");
            })
        };
        barrier.wait();
        let waited = receiver.recv_timeout(Duration::from_millis(60));
        drop(maintenance);
        let finished = receiver.recv_timeout(Duration::from_secs(5));
        worker.join().expect("database opener");
        assert!(matches!(waited, Err(mpsc::RecvTimeoutError::Timeout)));
        finished??;
    }
    Ok(())
}

#[test]
fn writable_indexes_register_canonical_paths_without_git_discovery() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.db("custom-index.sqlite", &fixture.root)?;
    let registration: (String, String) =
        db.conn
            .query_row("SELECT index_path,root FROM global.workspaces", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
    assert_eq!(
        Path::new(&registration.0).canonicalize()?,
        db.path.canonicalize()?
    );
    assert_eq!(
        Path::new(&registration.1).canonicalize()?,
        fixture.root.canonicalize()?
    );
    assert!(Path::new(&registration.0).is_absolute());
    assert!(Path::new(&registration.1).is_absolute());
    assert!(db.meta("repository_identity")?.is_none());
    assert_eq!(
        db.conn.query_row(
            "SELECT COUNT(*) FROM global.sqlite_master WHERE name='repositories'",
            [],
            |row| row.get::<_, i64>(0),
        )?,
        0
    );
    assert_eq!(
        db.conn
            .query_row("PRAGMA global.user_version", [], |row| row.get::<_, i64>(0))?,
        1
    );
    let path = db.path.clone();
    drop(db);
    let root = fixture.base.join("not-created");
    let db = Database::open_with_config(
        &path,
        &root,
        &json!({"artifactCachePath":fixture.store}),
        true,
    )?;
    let registered_root: String = db.conn.query_row(
        "SELECT root FROM global.workspaces WHERE index_path=?",
        [path.canonicalize()?.to_str().unwrap()],
        |row| row.get(0),
    )?;
    assert_eq!(PathBuf::from(registered_root), std::path::absolute(root)?);
    assert_eq!(
        db.conn
            .query_row("SELECT COUNT(*) FROM global.workspaces", [], |row| row
                .get::<_, i64>(0))?,
        1
    );
    Ok(())
}

#[test]
fn pretracking_stores_gain_workspace_tracking_only_on_writable_open() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.db("index.sqlite", &fixture.root)?;
    let path = db.path.clone();
    assert_no_legacy_artifacts(&db.conn, "global")?;
    db.conn.execute_batch(
        "INSERT INTO global.sources VALUES('old-source','source');
         INSERT INTO global.cache VALUES('unit-content','old-unit','payload');
         INSERT INTO global.cache VALUES('explanation','old-answer','answer');
         INSERT INTO global.description_content VALUES('old-description','description');
         INSERT INTO global.embeddings VALUES('old-embedding',x'0000803f');
         DROP TABLE global.workspaces;",
    )?;
    drop(db);
    let readonly = Database::open_readonly(&path, &fixture.root)?;
    assert_eq!(
        readonly.conn.query_row(
            "SELECT COUNT(*) FROM global.sqlite_master WHERE name='workspaces'",
            [],
            |row| row.get::<_, i64>(0),
        )?,
        0
    );
    assert_eq!(readonly.conn.total_changes(), 0);
    assert_no_legacy_artifacts(&readonly.conn, "global")?;
    drop(readonly);
    let db = fixture.db("index.sqlite", &fixture.root)?;
    assert_eq!(
        db.conn
            .query_row("SELECT COUNT(*) FROM global.workspaces", [], |row| row
                .get::<_, i64>(0))?,
        1
    );
    assert_no_legacy_artifacts(&db.conn, "global")?;
    let payloads: (String, String, String, Vec<u8>) = db.conn.query_row(
        "SELECT (SELECT source FROM global.sources WHERE hash='old-source'),
                (SELECT value FROM global.cache WHERE kind='unit-content' AND key='old-unit'),
                (SELECT content FROM global.description_content WHERE hash='old-description'),
                (SELECT vector FROM global.embeddings WHERE key='old-embedding')",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    assert_eq!(
        payloads,
        (
            "source".into(),
            "payload".into(),
            "description".into(),
            vec![0, 0, 128, 63]
        )
    );
    db.conn.execute(
        "INSERT INTO global.sources VALUES('new-source','later source')",
        [],
    )?;
    db.conn.execute(
        "UPDATE global.workspaces SET root='unchanged-by-readonly'",
        [],
    )?;
    let readonly = Database::open_readonly(&path, &fixture.root)?;
    assert_eq!(
        readonly
            .conn
            .query_row("SELECT root FROM global.workspaces", [], |row| row
                .get::<_, String>(0))?,
        "unchanged-by-readonly"
    );
    drop((readonly, db));
    let db = fixture.db("index.sqlite", &fixture.root)?;
    assert_no_legacy_artifacts(&db.conn, "global")?;
    let root: String = db
        .conn
        .query_row("SELECT root FROM global.workspaces", [], |row| row.get(0))?;
    assert_eq!(
        Path::new(&root).canonicalize()?,
        fixture.root.canonicalize()?
    );
    assert_eq!(
        db.conn
            .query_row("PRAGMA global.user_version", [], |row| row.get::<_, i64>(0))?,
        1
    );
    Ok(())
}

#[test]
fn standalone_tracking_initialization_needs_no_artifact_tables_or_pins() -> Result<()> {
    let fixture = Fixture::new()?;
    let conn = Connection::open(&fixture.store)?;
    assert!(ensure_workspace_tracking(&conn, "not-a-schema").is_err());
    assert!(!workspace_tracking_exists(&conn, "main")?);
    ensure_workspace_tracking(&conn, "main")?;
    let tables = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    assert_eq!(tables, ["workspaces"]);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM workspaces", [], |row| row
            .get::<_, i64>(0))?,
        0
    );
    assert_no_legacy_artifacts(&conn, "main")?;
    // An initialized tracker is read-only setup even while another connection
    // owns the write lock; it must neither register nor acquire a transaction.
    conn.busy_timeout(Duration::from_millis(60))?;
    let writer = Connection::open(&fixture.store)?;
    let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate)?;
    tx.execute("INSERT INTO workspaces VALUES('pending','workspace')", [])?;
    let changes = conn.total_changes();
    ensure_workspace_tracking(&conn, "main")?;
    assert_eq!(conn.total_changes(), changes);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM workspaces", [], |row| row
            .get::<_, i64>(0))?,
        0
    );
    assert_no_legacy_artifacts(&conn, "main")?;
    Ok(())
}

#[test]
fn concurrent_pretracking_store_opens_register_every_index_without_pinning() -> Result<()> {
    let fixture = Fixture::new()?;
    let conn = cache::open_store(&fixture.store, false)?;
    conn.execute_batch(
        "DROP TABLE workspaces;
         INSERT INTO sources VALUES('old-source','source');",
    )?;
    drop(conn);
    let barrier = Arc::new(Barrier::new(6));
    let (sender, receiver) = mpsc::channel();
    let workers: Vec<_> = (0..6)
        .map(|worker| {
            let (barrier, sender) = (barrier.clone(), sender.clone());
            let path = fixture.base.join(format!("index-{worker}.sqlite"));
            let (root, store) = (fixture.root.clone(), fixture.store.clone());
            thread::spawn(move || {
                barrier.wait();
                let result = Database::open_with_config(
                    &path,
                    &root,
                    &json!({"artifactCachePath":store}),
                    false,
                );
                sender
                    .send(result)
                    .expect("concurrent opener result receiver");
            })
        })
        .collect();
    drop(sender);
    let results: Vec<_> = (0..workers.len())
        .map(|_| receiver.recv_timeout(Duration::from_secs(5)))
        .collect();
    let joined: Vec<_> = workers.into_iter().map(|worker| worker.join()).collect();
    for result in joined {
        result.expect("pretracking store opener");
    }
    let databases = results
        .into_iter()
        .map(|result| result.context("Pretracking store initialization did not finish")?)
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        databases[0]
            .conn
            .query_row("SELECT COUNT(*) FROM global.workspaces", [], |row| row
                .get::<_, i64>(0),)?,
        6
    );
    assert_no_legacy_artifacts(&databases[0].conn, "global")?;
    Ok(())
}

#[test]
fn unchanged_workspace_registration_never_waits_for_a_store_writer() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.db("index.sqlite", &fixture.root)?;
    let (path, root, store) = (
        db.path.clone(),
        fixture.root.clone(),
        db.global_path().to_owned(),
    );
    let writer = Connection::open(&store)?;
    let tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate)?;
    tx.execute("INSERT INTO metadata VALUES('pending','uncommitted')", [])?;
    let barrier = Arc::new(Barrier::new(2));
    let (sender, receiver) = mpsc::channel();
    let worker = {
        let barrier = barrier.clone();
        thread::spawn(move || {
            barrier.wait();
            let result = (|| -> Result<()> {
                for config in [json!({}), json!({"artifactCachePath":store})] {
                    let reopened = Database::open_with_config(&path, &root, &config, false)?;
                    ensure!(
                        reopened.conn.total_changes() == 0,
                        "A warm open wrote to the store"
                    );
                    let pending: bool = reopened.conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM global.metadata WHERE key='pending')",
                        [],
                        |row| row.get(0),
                    )?;
                    ensure!(!pending, "Uncommitted store rows became visible");
                    Database::open_readonly_with_config(&path, &root, &config)?;
                }
                Ok(())
            })();
            sender.send(result).expect("warm opener result receiver");
        })
    };
    barrier.wait();
    let result = receiver.recv_timeout(Duration::from_secs(5));
    drop(tx);
    worker.join().expect("warm database opener");
    result.context("A warm workspace open waited for the global writer")??;
    Ok(())
}

#[test]
#[cfg(unix)]
fn store_and_workspace_aliases_use_one_canonical_registration_and_lock() -> Result<()> {
    let fixture = Fixture::new()?;
    let alias = fixture.base.join("alias");
    std::os::unix::fs::symlink(&fixture.root, &alias)?;
    let db = Database::open_with_config(
        &alias.join("index.sqlite"),
        &alias,
        &json!({"artifactCachePath":fixture.store}),
        false,
    )?;
    let registered: (String, String) =
        db.conn
            .query_row("SELECT index_path,root FROM global.workspaces", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
    assert_eq!(
        Path::new(&registered.0).canonicalize()?,
        fixture.root.join("index.sqlite").canonicalize()?
    );
    assert_eq!(
        Path::new(&registered.1).canonicalize()?,
        fixture.root.canonicalize()?
    );
    assert_eq!(
        PathBuf::from(&registered.0),
        fixture.root.join("index.sqlite").canonicalize()?
    );
    assert_eq!(PathBuf::from(&registered.1), fixture.root.canonicalize()?);
    let store_alias = fixture.base.join("store-alias.sqlite");
    std::os::unix::fs::symlink(db.global_path(), &store_alias)?;
    expect_busy(store_usage(&store_alias, true, Duration::ZERO))?;
    let reopened = Database::open(
        &fixture.root.join("index.sqlite"),
        &alias,
        &json!({}),
        false,
    )?;
    assert_eq!(reopened.conn.total_changes(), 0);
    assert_eq!(
        reopened
            .conn
            .query_row("SELECT COUNT(*) FROM global.workspaces", [], |row| row
                .get::<_, i64>(0))?,
        1
    );
    Ok(())
}

#[test]
#[cfg(windows)]
fn windows_drive_verbatim_and_mixed_index_paths_share_one_registration() -> Result<()> {
    let fixture = Fixture::new()?;
    let db = fixture.db("index.sqlite", &fixture.root)?;
    let canonical = db.path.canonicalize()?;
    let verbatim = canonical
        .to_str()
        .context("Fixture index path is not UTF-8")?;
    let ordinary = if let Some(unc) = verbatim.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        verbatim
            .strip_prefix(r"\\?\")
            .unwrap_or(verbatim)
            .to_owned()
    };
    for path in [
        ordinary.clone(),
        verbatim.to_owned(),
        ordinary.replace('\\', "/"),
    ] {
        let reopened = Database::open(Path::new(&path), &fixture.root, &json!({}), false)?;
        assert_eq!(reopened.conn.total_changes(), 0);
        let registered: String =
            reopened
                .conn
                .query_row("SELECT index_path FROM global.workspaces", [], |row| {
                    row.get(0)
                })?;
        assert_eq!(Path::new(&registered).canonicalize()?, canonical);
        assert_eq!(
            reopened
                .conn
                .query_row("SELECT COUNT(*) FROM global.workspaces", [], |row| row
                    .get::<_, i64>(0),)?,
            1
        );
    }
    Ok(())
}
