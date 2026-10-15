use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    base: PathBuf,
    home: PathBuf,
    cache: PathBuf,
    store: PathBuf,
    config: PathBuf,
    invoker: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        // Child cwd resolves symlinked ancestors, including macOS /var.
        let base = temp.path().canonicalize()?;
        let home = base.join("home");
        let cache = home.join("cache");
        let invoker = base.join("invoker");
        fs::create_dir_all(&cache)?;
        fs::create_dir(&invoker)?;
        let store = base.join("artifacts.sqlite");
        let config = base.join("config.json");
        fs::write(&config, json!({"artifactCachePath":store}).to_string())?;
        Ok(Self {
            _temp: temp,
            base,
            home,
            cache,
            store,
            config,
            invoker,
        })
    }

    fn workspace(&self, name: &str) -> Result<PathBuf> {
        let root = self.base.join(name);
        fs::create_dir(&root)?;
        // Deliberately non-Git: a missing .git is not a missing workspace.
        fs::write(
            root.join("api.rs"),
            format!("pub fn {name}() -> i32 {{ 42 }}\n"),
        )?;
        Ok(root)
    }

    fn command(&self, root: &Path, index: Option<&Path>) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_slopdex"));
        command
            .current_dir(&self.invoker)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_CACHE_HOME", &self.cache)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.base.join("absent-gitconfig"))
            .env("NO_PROXY", "*")
            .arg("--root")
            .arg(root)
            .arg("--config")
            .arg(&self.config)
            .args(["--output", "json"]);
        #[cfg(windows)]
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        if let Some(index) = index {
            command.arg("--index").arg(index);
        }
        command
    }

    fn run(&self, root: &Path, index: Option<&Path>, args: &[&str]) -> Result<Value> {
        cli_json(self.command(root, index).args(args))
    }

    fn managed_index(&self, root: &Path) -> Result<PathBuf> {
        Ok(self
            .cache
            .join("slopdex/worktrees-v1")
            .join(slopdex::hash(
                root.canonicalize()?.as_os_str().as_encoded_bytes(),
            ))
            .join("index.sqlite"))
    }

    fn index(&self, root: &Path, custom: Option<&Path>) -> Result<PathBuf> {
        let index = match custom {
            Some(index) => index.to_owned(),
            None => self.managed_index(root)?,
        };
        // A missing index uses the ephemeral map path. An empty SQLite file plus
        // a forced structural map initializes real bindings without any provider.
        fs::create_dir_all(index.parent().context("Index has no parent")?)?;
        fs::write(&index, "")?;
        self.run(
            root,
            custom,
            &["--force-reindex", "--yes-really-rebuild-the-index", "map"],
        )?;
        ensure!(index.is_file(), "CLI did not create {}", index.display());
        Ok(index)
    }

    fn prune(&self, flags: &[&str]) -> Result<Value> {
        let mut args = vec!["cache", "prune"];
        args.extend_from_slice(flags);
        self.run(&self.invoker, None, &args)
    }
}

fn cli_json(command: &mut Command) -> Result<Value> {
    let output = command.output()?;
    ensure!(
        output.status.success(),
        "{command:?} failed: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout).context("CLI stdout was not JSON")
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_os_string();
    path.push(suffix);
    path.into()
}

fn count(conn: &Connection, sql: &str) -> Result<u64> {
    let count: i64 = conn.query_row(sql, [], |row| row.get(0))?;
    Ok(count.try_into()?)
}

fn rebuildable_rows(conn: &Connection) -> Result<u64> {
    count(
        conn,
        "SELECT (SELECT count(*) FROM sources)
         +(SELECT count(*) FROM description_content)
         +(SELECT count(*) FROM cache WHERE kind NOT IN ('description','explanation','rerank'))",
    )
}

fn assert_idle(report: &Value) {
    for field in [
        "worktreesRemoved",
        "artifactRowsDeleted",
        "vectorRowsDeleted",
        "descriptionRowsDeleted",
        "snapshotRowsDeleted",
    ] {
        assert_eq!(report[field], 0, "{field}: {report}");
    }
}

fn seed_vector(conn: &Connection, label: &str) -> Result<String> {
    let key = slopdex::hash(label);
    let vector: Vec<_> = [1.0_f32, 0.0_f32]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect();
    conn.execute("INSERT INTO embeddings VALUES(?,?)", params![key, vector])?;
    Ok(key)
}

struct Description {
    key: String,
    value: String,
    contents: Vec<String>,
}

fn seed_description(
    conn: &Connection,
    label: &str,
    path: &str,
    file_hash: &str,
    source_hash: &str,
) -> Result<Description> {
    let values = [
        json!({"provider":"mock","model":"description-v1"}).to_string(),
        json!({"temperature":0}).to_string(),
        format!("Describe {label}"),
        format!("Describe callable {label} in {path}"),
        format!("File context for {label}"),
        format!("Earlier assistant answer for {label}"),
    ];
    let contents: Vec<_> = values.iter().map(slopdex::hash).collect();
    for (hash, content) in contents.iter().zip(&values) {
        conn.execute(
            "INSERT OR IGNORE INTO description_content VALUES(?,?)",
            params![hash, content],
        )?;
    }
    let value = json!({
        "text":format!("Saved description for {label}"),
        "generation":{
            "scope":"callable", "path":path, "symbol":label,
            "source_hash":source_hash, "file_hash":file_hash,
            "profile_hash":contents[0], "settings_hash":contents[1],
            "system_hash":contents[2], "prompt_hash":contents[3],
            "file_description_hash":contents[4],
            "messages":[{"role":"assistant","content_hash":contents[5]}],
            "regenerate":false
        }
    })
    .to_string();
    let key = slopdex::hash(format!("description:{label}"));
    conn.execute(
        "INSERT INTO cache VALUES('description',?,?)",
        params![key, value],
    )?;
    Ok(Description {
        key,
        value,
        contents,
    })
}

fn assert_description(conn: &Connection, description: &Description) -> Result<()> {
    let value: String = conn.query_row(
        "SELECT value FROM cache WHERE kind='description' AND key=?",
        [&description.key],
        |row| row.get(0),
    )?;
    assert_eq!(value, description.value);
    for key in &description.contents {
        let content: String = conn.query_row(
            "SELECT content FROM description_content WHERE hash=?",
            [key],
            |row| row.get(0),
        )?;
        assert_eq!(slopdex::hash(content), *key);
    }
    Ok(())
}

#[test]
fn default_prune_removes_deleted_managed_worktrees_but_preserves_provider_results() -> Result<()> {
    let fixture = Fixture::new()?;
    let live = fixture.workspace("live")?;
    let gone = fixture.workspace("gone")?;
    let live_index = fixture.index(&live, None)?;
    let gone_index = fixture.index(&gone, None)?;
    let mapped = fixture.run(&live, None, &["--no-reindex", "map"])?;
    assert!(!live.join(".git").exists());

    let conn = Connection::open(&fixture.store)?;
    let code_vector = seed_vector(&conn, "paid-code-embedding")?;
    let query_vector = seed_vector(&conn, "paid-query-embedding")?;
    let file_hash = slopdex::hash(fs::read(gone.join("api.rs"))?);
    let description = seed_description(&conn, "gone", "api.rs", &file_hash, &file_hash)?;
    for kind in ["explanation", "rerank"] {
        conn.execute(
            "INSERT INTO cache VALUES(?,?,?)",
            params![kind, slopdex::hash(kind), format!("Paid {kind} result")],
        )?;
    }
    let orphan_source = slopdex::hash("orphan source");
    conn.execute(
        "INSERT INTO sources VALUES(?,?)",
        params![orphan_source, "orphan source"],
    )?;
    let payload = json!({"source":"unused", "embeddingInput":"unused"}).to_string();
    let orphan_unit = slopdex::hash(&payload);
    conn.execute(
        "INSERT INTO cache VALUES('unit-content',?,?)",
        params![orphan_unit, payload],
    )?;
    conn.execute(
        "INSERT INTO snapshots VALUES(?,?)",
        params![slopdex::hash("obsolete snapshot"), "{}"],
    )?;
    let before = rebuildable_rows(&conn)?;
    drop(conn);
    fs::remove_dir_all(&gone)?;

    let report = fixture.prune(&[])?;
    assert_eq!(report["worktreesRemoved"], 1);
    assert_eq!(report["vectorRowsDeleted"], 0);
    assert_eq!(report["descriptionRowsDeleted"], 0);
    assert_eq!(report["snapshotRowsDeleted"], 1);
    assert_eq!(
        Path::new(report["store"].as_str().unwrap()).canonicalize()?,
        fixture.store.canonicalize()?
    );
    assert!(!gone_index.exists());
    // The separately named index.sqlite.lock may intentionally remain persistent.
    assert!(live_index.is_file());
    let conn = Connection::open(&fixture.store)?;
    let deleted = before - rebuildable_rows(&conn)?;
    assert!(deleted >= 2);
    assert_eq!(report["artifactRowsDeleted"], deleted);
    assert_eq!(count(&conn, "SELECT count(*) FROM workspaces")?, 1);
    for key in [code_vector, query_vector] {
        let vector: Vec<u8> =
            conn.query_row("SELECT vector FROM embeddings WHERE key=?", [key], |row| {
                row.get(0)
            })?;
        assert_eq!(
            vector,
            [1.0_f32.to_le_bytes(), 0.0_f32.to_le_bytes()].concat()
        );
    }
    assert_description(&conn, &description)?;
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM cache WHERE kind IN ('explanation','rerank')"
        )?,
        2
    );
    assert!(!conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sources WHERE hash=?)",
        [&orphan_source],
        |row| row.get::<_, bool>(0),
    )?);
    assert!(!conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM cache WHERE kind='unit-content' AND key=?)",
        [&orphan_unit],
        |row| row.get::<_, bool>(0),
    )?);
    drop(conn);
    assert_eq!(fixture.run(&live, None, &["--no-reindex", "map"])?, mapped);
    assert_idle(&fixture.prune(&[])?);
    Ok(())
}

#[test]
fn prune_removes_only_a_registered_custom_index_and_recognized_sidecars() -> Result<()> {
    let fixture = Fixture::new()?;
    let root = fixture.workspace("custom")?;
    let directory = fixture.base.join("indexes");
    let index = directory.join("custom.sqlite");
    fixture.index(&root, Some(&index))?;
    let sidecars: Vec<_> = [
        "-wal",
        "-shm",
        ".code.shared.json",
        ".descriptions.shared.json",
        ".symbols.shared.json",
    ]
    .into_iter()
    .map(|suffix| with_suffix(&index, suffix))
    .collect();
    for sidecar in &sidecars {
        fs::write(sidecar, "")?;
    }
    let neighbors = [
        with_suffix(&index, ".backup"),
        with_suffix(&index, ".code.shared.json.backup"),
        directory.join("neighbor.sqlite"),
    ];
    for neighbor in &neighbors {
        fs::write(neighbor, "unrelated user data")?;
    }
    fs::remove_dir_all(&root)?;

    let report = fixture.prune(&[])?;
    assert_eq!(report["worktreesRemoved"], 1);
    assert_eq!(report["vectorRowsDeleted"], 0);
    assert_eq!(report["descriptionRowsDeleted"], 0);
    assert!(!index.exists());
    for sidecar in &sidecars {
        assert!(!sidecar.exists(), "left sidecar {}", sidecar.display());
    }
    for neighbor in &neighbors {
        assert_eq!(fs::read_to_string(neighbor)?, "unrelated user data");
    }
    assert!(directory.is_dir());
    assert_idle(&fixture.prune(&[])?);
    Ok(())
}

#[test]
fn provider_prune_flags_are_independent_and_preserve_live_workspace_bindings() -> Result<()> {
    let fixture = Fixture::new()?;
    let root = fixture.workspace("live")?;
    let index = fixture.index(&root, None)?;
    let conn = Connection::open(&fixture.store)?;
    let live_code = seed_vector(&conn, "live-code")?;
    let live_description = seed_vector(&conn, "live-description")?;
    seed_vector(&conn, "orphan-query")?;
    seed_vector(&conn, "orphan-document")?;
    seed_vector(&conn, "historical-unbound-code")?;
    let db = Connection::open(&index)?;
    let (unit, path, identity, input_hash, data): (i64, String, String, String, String) = db
        .query_row(
            "SELECT id,path,identity,embedding_input_hash,data FROM search_units WHERE kind='function' LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )?;
    let data: Value = serde_json::from_str(&data)?;
    let source_hash = data["sourceHash"].as_str().context("missing sourceHash")?;
    let file_hash: String =
        db.query_row("SELECT hash FROM files WHERE path=?", [&path], |row| {
            row.get(0)
        })?;
    let description = seed_description(&conn, "live", &path, &file_hash, source_hash)?;
    let obsolete = seed_description(
        &conn,
        "obsolete-context",
        &path,
        &slopdex::hash("old file context"),
        source_hash,
    )?;
    let missing = seed_description(
        &conn,
        "missing-file",
        "missing.rs",
        &slopdex::hash("missing file"),
        &slopdex::hash("missing callable"),
    )?;
    let text = "The live callable returns forty-two.";
    let content_hash = slopdex::hash(text);
    conn.execute(
        "INSERT INTO description_content VALUES(?,?)",
        params![content_hash, text],
    )?;
    for kind in ["explanation", "rerank"] {
        conn.execute(
            "INSERT INTO cache VALUES(?,?,?)",
            params![kind, slopdex::hash(kind), "paid result"],
        )?;
    }
    db.execute(
        "INSERT INTO unit_embeddings VALUES(?,'code',?,?,?)",
        params![unit, slopdex::hash("mock profile"), input_hash, live_code],
    )?;
    db.execute(
        "INSERT INTO descriptions VALUES('callable',?,?,?,?,?)",
        params![path, identity, source_hash, content_hash, live_description],
    )?;
    drop((db, conn));
    let mapped = fixture.run(&root, None, &["--no-reindex", "map"])?;

    let report = fixture.prune(&[])?;
    assert_eq!(report["worktreesRemoved"], 0);
    assert_eq!(report["vectorRowsDeleted"], 0);
    assert_eq!(report["descriptionRowsDeleted"], 0);
    let conn = Connection::open(&fixture.store)?;
    assert_eq!(count(&conn, "SELECT count(*) FROM embeddings")?, 5);
    assert_description(&conn, &description)?;
    assert_description(&conn, &obsolete)?;
    assert_description(&conn, &missing)?;
    drop(conn);

    let report = fixture.prune(&["--prune-vectors"])?;
    assert_eq!(report["worktreesRemoved"], 0);
    assert_eq!(report["vectorRowsDeleted"], 3);
    assert_eq!(report["descriptionRowsDeleted"], 0);
    let conn = Connection::open(&fixture.store)?;
    assert_eq!(count(&conn, "SELECT count(*) FROM embeddings")?, 2);
    for key in [&live_code, &live_description] {
        assert!(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM embeddings WHERE key=?)",
            [key],
            |row| row.get::<_, bool>(0),
        )?);
    }
    assert_description(&conn, &obsolete)?;
    assert_description(&conn, &missing)?;
    drop(conn);
    assert_idle(&fixture.prune(&["--prune-vectors"])?);

    let report = fixture.prune(&["--prune-descriptions"])?;
    assert_eq!(report["worktreesRemoved"], 0);
    assert_eq!(report["vectorRowsDeleted"], 0);
    assert_eq!(report["descriptionRowsDeleted"], 2);
    let conn = Connection::open(&fixture.store)?;
    assert_eq!(count(&conn, "SELECT count(*) FROM embeddings")?, 2);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM cache WHERE kind='description'")?,
        1
    );
    assert_description(&conn, &description)?;
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM cache WHERE kind IN ('explanation','rerank')"
        )?,
        2
    );
    assert_eq!(
        conn.query_row(
            "SELECT content FROM description_content WHERE hash=?",
            [&content_hash],
            |row| row.get::<_, String>(0),
        )?,
        text
    );
    drop(conn);
    assert_eq!(fixture.run(&root, None, &["--no-reindex", "map"])?, mapped);
    assert_idle(&fixture.prune(&["--prune-vectors", "--prune-descriptions"])?);
    Ok(())
}

#[test]
fn prune_without_workspaces_does_not_create_an_index() -> Result<()> {
    let fixture = Fixture::new()?;
    drop(slopdex::cache::Artifacts::open(
        &json!({"artifactCachePath":fixture.store}),
    )?);
    let conn = Connection::open(&fixture.store)?;
    assert_eq!(count(&conn, "SELECT count(*) FROM workspaces")?, 0);
    seed_vector(&conn, "unbound paid vector")?;
    let description = seed_description(
        &conn,
        "unbound",
        "missing.rs",
        &slopdex::hash("missing file"),
        &slopdex::hash("missing callable"),
    )?;
    conn.execute(
        "INSERT INTO sources VALUES(?,?)",
        params![slopdex::hash("unbound source"), "unbound source"],
    )?;
    drop(conn);
    let report = fixture.prune(&[])?;
    assert_eq!(report["worktreesRemoved"], 0);
    assert_eq!(report["artifactRowsDeleted"], 1);
    assert_eq!(report["vectorRowsDeleted"], 0);
    assert_eq!(report["descriptionRowsDeleted"], 0);
    let conn = Connection::open(&fixture.store)?;
    assert_description(&conn, &description)?;
    assert_eq!(count(&conn, "SELECT count(*) FROM embeddings")?, 1);
    drop(conn);
    assert!(!fixture.managed_index(&fixture.invoker)?.exists());
    assert_eq!(fs::read_dir(&fixture.invoker)?.count(), 0);
    assert_idle(&fixture.prune(&[])?);
    Ok(())
}

#[test]
#[cfg(unix)]
fn prune_uses_canonical_workspace_identity_through_a_symlinked_cwd() -> Result<()> {
    let fixture = Fixture::new()?;
    let root = fixture.workspace("aliased")?;
    let alias = fixture.base.join("alias");
    match std::os::unix::fs::symlink(&fixture.base, &alias) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    // All relative CLI/config paths resolve from a physical cwd, despite the
    // deliberately symlinked spelling passed to Command::current_dir.
    fs::write(
        &fixture.config,
        json!({"artifactCachePath":"../artifacts.sqlite"}).to_string(),
    )?;
    let index = fixture.base.join("aliased.sqlite");
    fs::write(&index, "")?;
    cli_json(
        fixture
            .command(
                Path::new("../aliased"),
                Some(Path::new("../aliased.sqlite")),
            )
            .current_dir(alias.join("invoker"))
            .args(["--force-reindex", "--yes-really-rebuild-the-index", "map"]),
    )?;
    let conn = Connection::open(&fixture.store)?;
    let (registered_index, registered_root): (String, String) =
        conn.query_row("SELECT index_path,root FROM workspaces", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    assert_eq!(
        Path::new(&registered_index).canonicalize()?,
        index.canonicalize()?
    );
    assert_eq!(
        Path::new(&registered_root).canonicalize()?,
        root.canonicalize()?
    );
    drop(conn);
    let db = Connection::open(&index)?;
    let binding: String = db.query_row(
        "SELECT value FROM metadata WHERE key='global_path'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(
        Path::new(&binding).canonicalize()?,
        fixture.store.canonicalize()?
    );
    drop(db);
    assert_idle(&fixture.prune(&[])?);
    assert!(index.is_file());
    fs::remove_dir_all(&root)?;
    assert_eq!(fixture.prune(&[])?["worktreesRemoved"], 1);
    assert!(!index.exists());
    assert_idle(&fixture.prune(&[])?);
    Ok(())
}
