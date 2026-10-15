//! Workspace tracking, repository relatedness, and artifact/store coordination.
//! Family membership is discovery evidence, never proof that a snapshot is reusable.

use std::{
    collections::BTreeSet,
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::{git, hash, storage::Database};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS global.repositories(
 checkout TEXT PRIMARY KEY,checkout_root TEXT NOT NULL,git_dir TEXT,common_dir TEXT,
 object_format TEXT,family TEXT NOT NULL,fingerprint TEXT NOT NULL,last_seen INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS global.repositories_family ON repositories(family);
CREATE TABLE IF NOT EXISTS global.family_evidence(evidence TEXT PRIMARY KEY,family TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS global.family_evidence_family ON family_evidence(family);
";
pub(crate) const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
/// Kinds whose lock file is a fixed, never-retired path. Store initialization
/// and maintenance must always synchronize through the same named file.
const PERSISTENT_KINDS: [&str; 1] = ["store"];
const PERSISTENT_STORE_KEYS: [&str; 2] = ["initialize", "maintenance"];

/// Ensure current workspace tracking exists. `schema` is `main` for a standalone
/// store connection or `global` for an attached store. Warm opens are read-only setup.
pub(crate) fn ensure_workspace_tracking(conn: &Connection, schema: &str) -> Result<()> {
    ensure!(matches!(schema, "main" | "global"), "Invalid store schema");
    if workspace_tracking_exists(conn, schema)? {
        return Ok(());
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    initialize_workspace_tracking(&tx, schema)?;
    tx.commit()?;
    Ok(())
}

fn workspace_tracking_exists(conn: &Connection, schema: &str) -> Result<bool> {
    Ok(conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM {schema}.sqlite_master WHERE type='table' AND name='workspaces')"
        ),
        [],
        |row| row.get(0),
    )?)
}

/// Called under an immediate transaction. Recheck after taking the write lock:
/// a concurrent initializer may already have created the tracking table.
fn initialize_workspace_tracking(conn: &Connection, schema: &str) -> Result<()> {
    if workspace_tracking_exists(conn, schema)? {
        return Ok(());
    }
    conn.execute_batch(&format!(
        "CREATE TABLE {schema}.workspaces(index_path TEXT PRIMARY KEY,root TEXT NOT NULL);"
    ))?;
    Ok(())
}

/// Track every writable workspace index, independently of Git registration.
/// Existing stores gain this optional table lazily without a schema-version bump.
/// A warm open with unchanged registration must not take a write transaction:
/// another connection may already be writing either attached database.
pub(crate) fn register_workspace(db: &Database, root: &Path) -> Result<()> {
    let index = db.path.canonicalize()?;
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            std::path::absolute(root)?
        }
        Err(error) => return Err(error.into()),
    };
    let index = index
        .to_str()
        .context("Workspace index path is not UTF-8")?;
    let root = root.to_str().context("Workspace root path is not UTF-8")?;
    let exists = workspace_tracking_exists(&db.conn, "global")?;
    if exists
        && db
            .conn
            .query_row(
                "SELECT root FROM global.workspaces WHERE index_path=?",
                [index],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .as_deref()
            == Some(root)
    {
        return Ok(());
    }
    ensure_writable(db)?;
    let tx = Transaction::new_unchecked(&db.conn, TransactionBehavior::Immediate)?;
    initialize_workspace_tracking(&tx, "global")?;
    tx.execute(
        "INSERT INTO global.workspaces(index_path,root) VALUES(?,?)
         ON CONFLICT(index_path) DO UPDATE SET root=excluded.root WHERE root<>excluded.root",
        params![index, root],
    )?;
    tx.commit()?;
    Ok(())
}

/// Register once per checkout incarnation. Cached calls use filesystem identity
/// and one indexed family lookup, without invoking Git or traversing history.
/// Read-only databases may return an existing registration but cannot initialize it.
pub(crate) fn register(db: &Database, root: &Path) -> Result<Value> {
    let root = root.canonicalize()?;
    if let Some(cached) = db.meta("repository_identity")? {
        let mut value: Value = serde_json::from_str(&cached)?;
        if value["registry_version"] == 1
            && value["root"] == json!(root)
            && value["fingerprint"] == fingerprint(&root, &value)?
            && let Some(checkout) = value["checkout"].as_str()
            && let Some(family) = db
                .conn
                .query_row(
                    "SELECT family FROM global.repositories WHERE checkout=?",
                    [checkout],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
        {
            // Another checkout can have linked previously distinct families.
            value["family"] = json!(family);
            return Ok(value);
        }
    }
    ensure_writable(db)?;
    let mut value = if let Some(mut identity) = git::identity(&root)? {
        identity.lineage = git::lineage(&root)?;
        serde_json::to_value(identity)?
    } else {
        json!({"checkout_root":root,"git_dir":null,"common_dir":null,
            "scope":"","object_format":null,"head":null,"tree":null,"lineage":null})
    };
    value["registry_version"] = json!(1);
    value["root"] = json!(root);
    value["workspace_scope"] = value["common_dir"]
        .as_str()
        .map_or_else(|| json!(root), |path| json!(path));
    value["checkout"] = json!(hash(serde_json::to_vec(&json!([
        stamp(Path::new(
            value["checkout_root"]
                .as_str()
                .context("Missing checkout root")?
        ))?,
        optional_stamp(&value["git_dir"])?,
        optional_stamp(&value["common_dir"])?
    ]))?));
    value["fingerprint"] = json!(fingerprint(&root, &value)?);

    let mut evidence = Vec::new();
    if let Some(common) = value["common_dir"].as_str() {
        evidence.push(format!(
            "common:{}",
            hash(serde_json::to_vec(&json!([
                value["object_format"],
                stamp(Path::new(common))?
            ]))?)
        ));
    }
    if let Some(head) = value["head"].as_str() {
        evidence.push(commit_evidence(&value, head)?);
    }
    if let Some(roots) = value["lineage"]["history_roots"].as_array() {
        for root in roots {
            evidence.push(commit_evidence(
                &value,
                root.as_str().context("Invalid history root")?,
            )?);
        }
    }
    // Remotes remain sanitized supporting hints in lineage, never merge keys.
    publish(db, &mut value, &evidence)?;
    Ok(value)
}

/// Observe the actual indexed Git commit on each refresh. Commit IDs are full
/// storage-format OIDs, not ref names. None represents an unborn/non-Git checkpoint.
/// Returns the current family, including any merge caused by this observation.
pub(crate) fn observe_checkpoint(db: &Database, root: &Path, head: Option<&str>) -> Result<Value> {
    ensure_writable(db)?;
    let mut value = register(db, root)?;
    let evidence = head
        .map(|head| commit_evidence(&value, head))
        .transpose()?
        .into_iter()
        .collect::<Vec<_>>();
    if value["head"] != json!(head) {
        // The old tree belongs to the old HEAD; no extra Git discovery is needed.
        value["tree"] = Value::Null;
    }
    value["head"] = json!(head);
    publish(db, &mut value, &evidence)?;
    Ok(value)
}

fn ensure_writable(db: &Database) -> Result<()> {
    let query_only: bool = db.conn.query_row("PRAGMA query_only", [], |r| r.get(0))?;
    ensure!(
        !query_only && !db.conn.is_readonly("main")? && !db.conn.is_readonly("global")?,
        "Repository registration requires a writable database"
    );
    Ok(())
}

fn publish(db: &Database, value: &mut Value, evidence: &[String]) -> Result<()> {
    let tx = Transaction::new_unchecked(&db.conn, TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA)?;
    let checkout = value["checkout"].as_str().context("Missing checkout key")?;
    let mut families = BTreeSet::new();
    if let Some(family) = tx
        .query_row(
            "SELECT family FROM global.repositories WHERE checkout=?",
            [checkout],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        families.insert(family);
    }
    for key in evidence {
        if let Some(family) = tx
            .query_row(
                "SELECT family FROM global.family_evidence WHERE evidence=?",
                [key],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            families.insert(family);
        }
    }
    let family = families
        .first()
        .cloned()
        .unwrap_or_else(|| hash(format!("repository-family:{checkout}")));
    for previous in families.iter().filter(|previous| **previous != family) {
        tx.execute(
            "UPDATE global.repositories SET family=? WHERE family=?",
            params![family, previous],
        )?;
        tx.execute(
            "UPDATE global.family_evidence SET family=? WHERE family=?",
            params![family, previous],
        )?;
    }
    tx.execute(
        "INSERT INTO global.repositories VALUES(?,?,?,?,?,?,?,unixepoch())
         ON CONFLICT(checkout) DO UPDATE SET family=excluded.family,
          fingerprint=excluded.fingerprint,last_seen=excluded.last_seen",
        params![
            checkout,
            value["checkout_root"].as_str(),
            value["git_dir"].as_str(),
            value["common_dir"].as_str(),
            value["object_format"].as_str(),
            family,
            value["fingerprint"].as_str(),
        ],
    )?;
    for key in evidence {
        tx.execute(
            "INSERT INTO global.family_evidence VALUES(?,?)
             ON CONFLICT(evidence) DO UPDATE SET family=excluded.family",
            params![key, family],
        )?;
    }
    value["family"] = json!(family);
    tx.execute(
        "INSERT INTO metadata VALUES('repository_identity',?)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [value.to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

fn commit_evidence(identity: &Value, head: &str) -> Result<String> {
    let format = identity["object_format"]
        .as_str()
        .context("Git checkpoint requires a Git identity")?;
    let length = match format {
        "sha1" => 40,
        "sha256" => 64,
        _ => anyhow::bail!("Unsupported Git object format: {format}"),
    };
    ensure!(
        head.len() == length && head.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Checkpoint must be a full {format} commit ID"
    );
    Ok(format!("commit:{format}:{}", head.to_ascii_lowercase()))
}

// Do not use directory mtime/ctime: normal Git and workspace writes change them.
// Inode/device and creation time identify replacement even at unchanged paths.
fn stamp(path: &Path) -> Result<Value> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Ok(Value::Null);
        }
        Err(error) => return Err(error.into()),
    };
    let created = metadata
        .created()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().to_string());
    #[cfg(unix)]
    let id = {
        use std::os::unix::fs::MetadataExt;
        json!([metadata.dev(), metadata.ino()])
    };
    #[cfg(not(unix))]
    let id = Value::Null;
    Ok(json!([path, path.canonicalize()?, id, created]))
}

fn optional_stamp(path: &Value) -> Result<Value> {
    path.as_str()
        .map(|path| stamp(Path::new(path)))
        .transpose()
        .map(|value| value.unwrap_or(Value::Null))
}

fn fingerprint(root: &Path, identity: &Value) -> Result<String> {
    let mut marker = Value::Null;
    for ancestor in root.ancestors() {
        let path = ancestor.join(".git");
        match fs::metadata(&path) {
            Ok(metadata) => {
                marker = json!([
                    stamp(&path)?,
                    if metadata.is_file() {
                        Some(hash(fs::read(&path)?))
                    } else {
                        None
                    }
                ]);
                break;
            }
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(hash(serde_json::to_vec(&json!([
        stamp(root)?,
        marker,
        optional_stamp(&identity["checkout_root"])?,
        optional_stamp(&identity["git_dir"])?,
        optional_stamp(&identity["common_dir"])?
    ]))?))
}

/// A held artifact or store lock. Dropping it releases the lock; per-artifact
/// files are retired, while store coordination files remain permanently named.
pub(crate) struct ArtifactLock {
    file: fs::File,
    path: PathBuf,
    persistent: bool,
}

impl Drop for ArtifactLock {
    fn drop(&mut self) {
        if !self.persistent {
            retire(&self.file, &self.path);
        }
        release(&self.file);
    }
}

/// Unlink a lock file while its lock is held, then mark the orphaned file so a
/// waiter that already opened it retries on a fresh file instead of sharing a
/// name with nobody. Unlinking only ever happens under the file's own lock.
fn retire(file: &fs::File, path: &Path) -> bool {
    if fs::remove_file(path).is_err() {
        // Still reachable and unmarked: it simply remains a usable lock file.
        return false;
    }
    let mut file = file;
    let _ = file.write_all(b"retired");
    true
}

/// Unlock explicitly rather than by closing. A child process forked by another
/// thread briefly inherits every open descriptor, and a lock that is only
/// released by the last close would outlive its guard until that child execs.
fn release(file: &fs::File) {
    let _ = FileExt::unlock(file);
}

/// Whether a locked file is still the one its path names. A retired file is
/// nonempty; on Unix the identity check also covers a holder that died between
/// unlinking and marking, and files removed by anything else.
fn live(file: &fs::File, path: &Path) -> bool {
    let Ok(held) = file.metadata() else {
        return false;
    };
    if held.len() != 0 {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path)
            .is_ok_and(|current| current.dev() == held.dev() && current.ino() == held.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

/// Poll for an exclusive advisory lock until `deadline`. `Ok(false)` reports a
/// timeout; the caller decides whether that is an error or a fallback.
pub(crate) fn lock_exclusive_until(file: &fs::File, deadline: Instant) -> std::io::Result<bool> {
    lock_until(file, true, deadline)
}

fn lock_until(file: &fs::File, exclusive: bool, deadline: Instant) -> std::io::Result<bool> {
    loop {
        let attempt = if exclusive {
            FileExt::try_lock_exclusive(file)
        } else {
            FileExt::try_lock_shared(file)
        };
        match attempt {
            Ok(()) => return Ok(true),
            // Windows reports ERROR_LOCK_VIOLATION, which is not necessarily
            // mapped to WouldBlock. Compare fs2's native contention code too.
            Err(error)
                if error.kind() == ErrorKind::WouldBlock
                    || error.raw_os_error().is_some_and(|code| {
                        Some(code) == fs2::lock_contended_error().raw_os_error()
                    }) =>
            {
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn lock_directory(global_path: &Path) -> PathBuf {
    let mut directory = global_path.as_os_str().to_owned();
    directory.push(".locks");
    PathBuf::from(directory)
}

fn lock_file_name(kind: &str, key: &str) -> Result<String> {
    Ok(format!("{}.lock", hash(serde_json::to_vec(&[kind, key])?)))
}

fn open_lock_file(path: &Path, create: bool) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(path)
}

/// Coordinate maintenance against every live database/artifact-store user.
/// Initialize/open the store first, then acquire this guard before attaching or
/// using it. Shared users retain their guard for their entire lifetime; pruning
/// retains an exclusive guard across its complete global mark/sweep operation.
/// Unlike per-artifact locks, this shared/exclusive lock must never be unlinked.
pub(crate) fn store_usage(
    global_path: &Path,
    exclusive: bool,
    timeout: Duration,
) -> Result<ArtifactLock> {
    let global_path = global_path
        .canonicalize()
        .with_context(|| format!("Resolve artifact store {}", global_path.display()))?;
    let directory = lock_directory(&global_path);
    fs::create_dir_all(&directory)?;
    let path = directory.join(lock_file_name("store", "maintenance")?);
    let file = open_lock_file(&path, true)
        .with_context(|| format!("Open store maintenance lock {}", path.display()))?;
    ensure!(
        lock_until(&file, exclusive, Instant::now() + timeout)
            .context("Acquire store maintenance lock")?,
        "Artifact store is busy: {}",
        global_path.display()
    );
    Ok(ArtifactLock {
        file,
        path,
        persistent: true,
    })
}

/// Acquire only around the provider request/artifact computation. After acquiring,
/// recheck the artifact cache: another process may have filled it while we waited.
/// Dropping the returned guard releases the lock and retires its file.
pub(crate) fn artifact_lock(db: &Database, kind: &str, key: &str) -> Result<ArtifactLock> {
    lock_path(db.global_path(), kind, key)
}

/// Database-independent accessor for parallel workers: capture the global path,
/// not a borrowed SQLite connection. It must be the store's canonical path.
///
/// Locks are per exact `(kind, key)`, so unrelated artifacts never contend. A
/// thread that needs several keys of one kind must use [`lock_paths`]: taking
/// them one by one in caller order can deadlock against another batch.
pub(crate) fn lock_path(global_path: &Path, kind: &str, key: &str) -> Result<ArtifactLock> {
    ensure!(
        !kind.is_empty()
            && kind.len() <= 64
            && kind
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "Invalid artifact kind"
    );
    ensure!(
        !key.is_empty() && key.len() <= 4096 && !key.chars().any(char::is_control),
        "Invalid artifact key"
    );
    let directory = lock_directory(global_path);
    fs::create_dir_all(&directory)?;
    ensure!(directory.is_dir(), "Artifact lock path is not a directory");
    let path = directory.join(lock_file_name(kind, key)?);
    let persistent = PERSISTENT_KINDS.contains(&kind);
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        let file = match open_lock_file(&path, true) {
            Ok(file) => file,
            // Windows keeps a retired file's name reserved until its last
            // handle closes; creating the replacement is refused meanwhile.
            Err(error)
                if cfg!(windows)
                    && error.kind() == ErrorKind::PermissionDenied
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Open artifact lock {}", path.display()));
            }
        };
        ensure!(
            lock_exclusive_until(&file, deadline).context("Acquire artifact lock")?,
            "Timed out waiting for artifact lock {kind}:{key}"
        );
        if persistent || live(&file, &path) {
            return Ok(ArtifactLock {
                file,
                path,
                persistent,
            });
        }
        // The previous holder retired this file while we waited on it. The
        // lock we just took protects nothing; retry on the current file.
        ensure!(
            Instant::now() < deadline,
            "Timed out waiting for artifact lock {kind}:{key}"
        );
    }
}

/// Acquire several keys of one kind for a single batch. Keys are deduplicated,
/// because locking one file through two descriptors of the same process would
/// wait on itself, and taken in sorted key order, so overlapping batches in any
/// thread or process cannot deadlock each other.
pub(crate) fn lock_paths<'a>(
    global_path: &Path,
    kind: &str,
    keys: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<ArtifactLock>> {
    keys.into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|key| lock_path(global_path, kind, key))
        .collect()
}

/// Remove lock files nobody holds, including those left by binaries that never
/// retired them. Returns `(removed, busy)`. A file is removed only under its own
/// exclusive lock and with the same retirement marking as a normal release, so
/// concurrent lockers of this version stay correct. Store initialization and
/// maintenance locks are permanent and are never removed.
pub(crate) fn prune_locks(global_path: &Path) -> Result<(u64, u64)> {
    let directory = lock_directory(global_path);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => {
            return Err(error).with_context(|| format!("Read {}", directory.display()));
        }
    };
    let mut persistent = Vec::new();
    for kind in PERSISTENT_KINDS {
        for key in PERSISTENT_STORE_KEYS {
            persistent.push(lock_file_name(kind, key)?);
        }
    }
    let (mut removed, mut busy) = (0, 0);
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(".lock") else {
            continue;
        };
        if stem.len() != 64
            || !stem.bytes().all(|byte| byte.is_ascii_hexdigit())
            || persistent.iter().any(|keep| keep == name)
            || !entry.file_type()?.is_file()
        {
            continue;
        }
        let path = entry.path();
        // Never create: a file retired since the listing must stay gone.
        let Ok(file) = open_lock_file(&path, false) else {
            continue;
        };
        if !lock_exclusive_until(&file, Instant::now()).unwrap_or(false) {
            busy += 1;
            continue;
        }
        if live(&file, &path) && retire(&file, &path) {
            removed += 1;
        }
        release(&file);
    }
    Ok((removed, busy))
}

#[cfg(test)]
mod maintenance_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, process::Command, sync::mpsc};

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            Ok(Self {
                dir: tempfile::tempdir()?,
            })
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn git(&self, root: &Path, args: &[&str]) -> Result<()> {
            let output = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", self.path("missing-config"))
                .env("GIT_AUTHOR_NAME", "Registry fixture")
                .env("GIT_AUTHOR_EMAIL", "registry@example.test")
                .env("GIT_COMMITTER_NAME", "Registry fixture")
                .env("GIT_COMMITTER_EMAIL", "registry@example.test")
                .output()?;
            ensure!(
                output.status.success(),
                "Git fixture failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(())
        }

        fn repo(&self, name: &str) -> Result<PathBuf> {
            let root = self.path(name);
            fs::create_dir(&root)?;
            self.git(&root, &["init", "--quiet", "--object-format=sha1"])?;
            fs::write(root.join("source.txt"), name)?;
            self.git(&root, &["add", "."])?;
            self.git(&root, &["commit", "--quiet", "-m", name])?;
            Ok(root)
        }

        fn db(&self, name: &str, root: &Path) -> Result<Database> {
            Database::open_with_config(
                &self.path(&format!("{name}.sqlite")),
                root,
                &json!({"artifactCachePath":self.path("global.sqlite")}),
                false,
            )
        }
    }

    #[test]
    fn worktrees_and_scopes_share_a_family_and_workspace() -> Result<()> {
        let fixture = Fixture::new()?;
        let root = fixture.repo("main")?;
        let linked = fixture.path("linked");
        fixture.git(
            &root,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                linked.to_str().unwrap(),
            ],
        )?;
        let first = register(&fixture.db("main", &root)?, &root)?;
        let second = register(&fixture.db("linked", &linked)?, &linked)?;
        assert_eq!(first["family"], second["family"]);
        assert_eq!(first["workspace_scope"], second["workspace_scope"]);
        assert_ne!(first["checkout"], second["checkout"]);
        fs::create_dir(root.join("pkg"))?;
        let scoped = root.join("pkg");
        let scoped = register(&fixture.db("scoped", &scoped)?, &scoped)?;
        assert_eq!(scoped["checkout"], first["checkout"]);
        assert_eq!(scoped["scope"], "pkg");
        assert_eq!(scoped["workspace_scope"], first["common_dir"]);
        Ok(())
    }

    #[test]
    fn diverged_clones_share_roots_but_remotes_alone_do_not_merge() -> Result<()> {
        let fixture = Fixture::new()?;
        let root = fixture.repo("original")?;
        let clone = fixture.path("clone");
        fixture.git(
            fixture.dir.path(),
            &[
                "clone",
                "--quiet",
                root.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        )?;
        fixture.git(&root, &["commit", "--quiet", "--allow-empty", "-m", "left"])?;
        fixture.git(
            &clone,
            &["commit", "--quiet", "--allow-empty", "-m", "right"],
        )?;
        let unrelated = fixture.repo("unrelated")?;
        for path in [&root, &clone, &unrelated] {
            fixture.git(path, &["remote", "remove", "origin"]).ok();
            fixture.git(
                path,
                &["remote", "add", "origin", "https://example.test/shared.git"],
            )?;
        }
        let first = register(&fixture.db("original", &root)?, &root)?;
        let second = register(&fixture.db("clone", &clone)?, &clone)?;
        let third = register(&fixture.db("unrelated", &unrelated)?, &unrelated)?;
        assert_ne!(first["head"], second["head"]);
        assert_eq!(first["family"], second["family"]);
        assert_ne!(first["family"], third["family"]);
        assert_eq!(first["lineage"]["remotes"], third["lineage"]["remotes"]);
        Ok(())
    }

    #[test]
    fn checkpoints_merge_existing_families_and_cached_reads_follow_merge() -> Result<()> {
        let fixture = Fixture::new()?;
        let root = fixture.repo("left")?;
        let other = fixture.repo("right")?;
        let left = fixture.db("left", &root)?;
        let right = fixture.db("right", &other)?;
        let first = register(&left, &root)?;
        let second = register(&right, &other)?;
        assert_ne!(first["family"], second["family"]);
        fixture.git(
            &other,
            &["fetch", "--quiet", root.to_str().unwrap(), "HEAD"],
        )?;
        fixture.git(&other, &["checkout", "--quiet", "--detach", "FETCH_HEAD"])?;
        let merged = observe_checkpoint(&right, &other, git::head(&other).as_deref())?;
        assert_eq!(register(&left, &root)?["family"], merged["family"]);
        assert_eq!(register(&right, &other)?["family"], merged["family"]);
        let families: i64 = left.conn.query_row(
            "SELECT COUNT(DISTINCT family) FROM global.family_evidence",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(families, 1);
        assert!(observe_checkpoint(&right, &other, Some("HEAD")).is_err());
        Ok(())
    }

    #[test]
    fn replacement_at_same_path_reinitializes_but_normal_writes_do_not() -> Result<()> {
        let fixture = Fixture::new()?;
        let root = fixture.repo("current")?;
        let db = fixture.db("index", &root)?;
        let first = register(&db, &root)?;
        fixture.git(&root, &["commit", "--quiet", "--allow-empty", "-m", "next"])?;
        // Cached registration deliberately leaves HEAD to checkpoint observation.
        assert_eq!(register(&db, &root)?, first);
        fs::rename(&root, fixture.path("old"))?;
        let replacement = fixture.repo("replacement")?;
        fs::rename(replacement, &root)?;
        let second = register(&db, &root)?;
        assert_ne!(first["checkout"], second["checkout"]);
        assert_ne!(first["family"], second["family"]);
        assert_ne!(first["fingerprint"], second["fingerprint"]);
        Ok(())
    }

    #[test]
    fn optional_stamps_allow_disappearing_and_replaced_git_directories() -> Result<()> {
        let fixture = Fixture::new()?;
        let root = fixture.repo("current")?;
        let db = fixture.db("index", &root)?;
        let first = register(&db, &root)?;
        assert_eq!(optional_stamp(&Value::Null)?, Value::Null);
        assert_eq!(
            optional_stamp(&json!(fixture.path("missing")))?,
            Value::Null
        );
        let not_directory = fixture.path("file");
        fs::write(&not_directory, "not a directory")?;
        assert_eq!(
            optional_stamp(&json!(not_directory.join("missing")))?,
            Value::Null
        );
        let old_git = fixture.path("old-git");
        fs::rename(root.join(".git"), &old_git)?;
        // A stale optional Git path must invalidate the cache, not abort before
        // discovery can register the now non-Git workspace.
        let nongit = register(&db, &root)?;
        assert!(nongit["object_format"].is_null());
        assert_ne!(first["checkout"], nongit["checkout"]);
        fs::rename(&old_git, root.join(".git"))?;
        assert_eq!(register(&db, &root)?, first);
        fs::rename(root.join(".git"), &old_git)?;
        let replacement = fixture.repo("replacement")?;
        fs::rename(replacement.join(".git"), root.join(".git"))?;
        let second = register(&db, &root)?;
        assert_ne!(first["checkout"], second["checkout"]);
        assert_ne!(first["family"], second["family"]);
        assert_ne!(first["fingerprint"], second["fingerprint"]);
        Ok(())
    }

    #[test]
    fn readonly_registration_never_initializes_schema() -> Result<()> {
        let fixture = Fixture::new()?;
        let root = fixture.repo("repo")?;
        let writable = fixture.db("index", &root)?;
        let readonly = Database::open_readonly(&writable.path, &root)?;
        assert!(register(&readonly, &root).is_err());
        let tables: i64 = writable.conn.query_row(
            "SELECT COUNT(*) FROM global.sqlite_master WHERE name IN ('repositories','family_evidence')",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(tables, 0);
        let expected = register(&writable, &root)?;
        assert_eq!(register(&readonly, &root)?, expected);
        assert!(observe_checkpoint(&readonly, &root, None).is_err());
        fs::rename(root.join(".git"), fixture.path("old-git"))?;
        assert!(register(&readonly, &root).is_err());
        assert_eq!(
            readonly.meta("repository_identity")?,
            Some(expected.to_string())
        );
        Ok(())
    }

    #[test]
    fn nongit_and_unborn_registration_gain_checkpoint_evidence() -> Result<()> {
        let fixture = Fixture::new()?;
        let root = fixture.path("workspace");
        fs::create_dir(&root)?;
        let db = fixture.db("index", &root)?;
        let nongit = register(&db, &root)?;
        assert!(nongit["object_format"].is_null());
        fixture.git(&root, &["init", "--quiet", "--object-format=sha1"])?;
        let unborn = register(&db, &root)?;
        assert_ne!(unborn["checkout"], nongit["checkout"]);
        assert!(unborn["head"].is_null());
        fixture.git(&root, &["commit", "--quiet", "--allow-empty", "-m", "born"])?;
        let head = git::head(&root).unwrap();
        let observed = observe_checkpoint(&db, &root, Some(&head))?;
        assert_eq!(observed["family"], unborn["family"]);
        let family: String = db.conn.query_row(
            "SELECT family FROM global.family_evidence WHERE evidence=?",
            [format!("commit:sha1:{head}")],
            |r| r.get(0),
        )?;
        assert_eq!(json!(family), observed["family"]);
        assert_ne!(
            commit_evidence(&json!({"object_format":"sha1"}), &"a".repeat(40))?,
            commit_evidence(&json!({"object_format":"sha256"}), &"a".repeat(64))?
        );
        Ok(())
    }

    #[test]
    fn artifact_waiters_recheck_cache_after_release_and_other_keys_proceed() -> Result<()> {
        let fixture = Fixture::new()?;
        let root = fixture.repo("repo")?;
        let db = fixture.db("first", &root)?;
        let waiter = fixture.db("second", &root)?;
        let held = artifact_lock(&db, "description", "key")?;
        let independent = artifact_lock(&db, "description", "different")?;
        let different_kind = artifact_lock(&db, "embedding", "key")?;
        drop((independent, different_kind));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || -> Result<()> {
            ready_tx.send(())?;
            let _lock = artifact_lock(&waiter, "description", "key")?;
            done_tx.send(waiter.cache("description", "key")?)?;
            Ok(())
        });
        ready_rx.recv_timeout(Duration::from_secs(5))?;
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        db.cache_put("description", "key", "computed once")?;
        drop(held);
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(5))?,
            Some("computed once".to_owned())
        );
        worker.join().unwrap()?;
        assert!(artifact_lock(&db, "../bad", "key").is_err());
        assert!(artifact_lock(&db, "description", "").is_err());
        assert!(artifact_lock(&db, "description", "bad\0key").is_err());
        Ok(())
    }

    #[test]
    fn lock_path_can_be_shared_by_workers_without_a_database() -> Result<()> {
        let fixture = Fixture::new()?;
        let global = fixture.path("global.sqlite");
        let held = lock_path(&global, "embedding", "key")?;
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        thread::scope(|scope| -> Result<()> {
            let worker = scope.spawn(|| -> Result<()> {
                ready_tx.send(())?;
                let _lock = lock_path(&global, "embedding", "key")?;
                done_tx.send(())?;
                Ok(())
            });
            ready_rx.recv_timeout(Duration::from_secs(5))?;
            let waited = done_rx.recv_timeout(Duration::from_millis(100));
            drop(held);
            // Release and join before judging, so a regression cannot strand
            // the worker for the full lock timeout.
            let finished = done_rx.recv_timeout(Duration::from_secs(5));
            worker.join().unwrap()?;
            assert!(matches!(waited, Err(mpsc::RecvTimeoutError::Timeout)));
            finished?;
            Ok(())
        })
    }

    fn lock_files(global: &Path) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(lock_directory(global))? {
            names.push(entry?.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        Ok(names)
    }

    #[test]
    fn released_locks_leave_no_files_however_many_distinct_keys_are_used() -> Result<()> {
        let fixture = Fixture::new()?;
        let global = fixture.path("global.sqlite");
        for round in 0..400 {
            let key = hash(format!("key-{round}"));
            let single = lock_path(&global, "embedding", &key)?;
            assert_eq!(lock_files(&global)?.len(), 1);
            drop(single);
        }
        let keys: Vec<_> = (0..300).map(|key| hash(format!("batch-{key}"))).collect();
        let batch = lock_paths(&global, "embedding", keys.iter().map(String::as_str))?;
        assert_eq!(lock_files(&global)?.len(), keys.len());
        drop(batch);
        assert!(lock_files(&global)?.is_empty());
        // The permanent initialization lock is the only file that may remain.
        drop(lock_path(&global, "store", "initialize")?);
        drop(lock_path(&global, "store", "initialize")?);
        assert_eq!(
            lock_files(&global)?,
            [lock_file_name("store", "initialize")?]
        );
        Ok(())
    }

    #[test]
    fn a_batch_with_repeated_keys_does_not_wait_on_itself() -> Result<()> {
        let fixture = Fixture::new()?;
        let global = fixture.path("global.sqlite");
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = lock_paths(&global, "embedding", ["b", "a", "b", "a", "c", "b"])
                .map(|locks| locks.len());
            done_tx.send(result).expect("batch result receiver");
        });
        let result = done_rx.recv_timeout(Duration::from_secs(10));
        // On a regression the worker self-blocks until its own lock timeout;
        // it is still joined before the failure is reported.
        worker.join().unwrap();
        assert_eq!(
            result.context("A batch waited on its own repeated key")??,
            3
        );
        Ok(())
    }

    #[test]
    fn overlapping_batches_and_single_keys_exclude_without_deadlock() -> Result<()> {
        let fixture = Fixture::new()?;
        let global = fixture.path("global.sqlite");
        let keys = ["a", "b", "c", "d", "e"];
        let held: Vec<_> = keys
            .iter()
            .map(|_| std::sync::atomic::AtomicBool::new(false))
            .collect();
        let barrier = std::sync::Barrier::new(8);
        let results: Vec<Result<()>> = thread::scope(|scope| {
            let workers: Vec<_> = (0..8usize)
                .map(|worker| {
                    let (global, held, barrier) = (&global, &held, &barrier);
                    scope.spawn(move || -> Result<()> {
                        barrier.wait();
                        for round in 0..60usize {
                            // Batches request overlapping keys in conflicting
                            // caller orders; every third request is one key.
                            let mut wanted: Vec<usize> = if (worker + round) % 3 == 0 {
                                vec![(worker + round) % keys.len()]
                            } else {
                                (0..3).map(|i| (worker + round + i) % keys.len()).collect()
                            };
                            if worker % 2 == 0 {
                                wanted.reverse();
                            }
                            let locks = if wanted.len() == 1 {
                                vec![lock_path(global, "embedding", keys[wanted[0]])?]
                            } else {
                                lock_paths(global, "embedding", wanted.iter().map(|&i| keys[i]))?
                            };
                            for &index in &wanted {
                                ensure!(
                                    !held[index].swap(true, std::sync::atomic::Ordering::SeqCst),
                                    "Two holders of key {}",
                                    keys[index]
                                );
                            }
                            thread::yield_now();
                            for &index in &wanted {
                                held[index].store(false, std::sync::atomic::Ordering::SeqCst);
                            }
                            drop(locks);
                        }
                        Ok(())
                    })
                })
                .collect();
            // Join every worker before propagating any failure.
            workers
                .into_iter()
                .map(|worker| worker.join().expect("lock worker"))
                .collect()
        });
        results.into_iter().collect::<Result<Vec<_>>>()?;
        assert!(lock_files(&global)?.is_empty());
        Ok(())
    }

    #[test]
    fn pruning_removes_only_unheld_legacy_lock_files() -> Result<()> {
        let fixture = Fixture::new()?;
        let global = fixture.path("global.sqlite");
        assert_eq!(prune_locks(&global)?, (0, 0));
        let directory = lock_directory(&global);
        fs::create_dir_all(&directory)?;
        // Files as older binaries left them: empty, never unlinked.
        let legacy: Vec<_> = (0..20)
            .map(|index| directory.join(format!("{}.lock", hash(format!("legacy-{index}")))))
            .collect();
        for path in &legacy {
            fs::write(path, "")?;
        }
        let unrelated = [
            directory.join("notes.txt"),
            directory.join("short.lock"),
            directory.join(format!("{}.lock", "z".repeat(64))),
        ];
        for path in &unrelated {
            fs::write(path, "")?;
        }
        fs::create_dir(directory.join(format!("{}.lock", hash("directory"))))?;
        drop(lock_path(&global, "store", "initialize")?);
        // An old binary holds one legacy file; a current one holds a live key.
        let old_holder = open_lock_file(&legacy[0], false)?;
        assert!(lock_exclusive_until(&old_holder, Instant::now())?);
        let current = lock_path(&global, "embedding", "live")?;
        assert_eq!(prune_locks(&global)?, (19, 2));
        assert!(legacy[0].exists());
        assert!(legacy[1..].iter().all(|path| !path.exists()));
        assert!(unrelated.iter().all(|path| path.exists()));
        assert!(
            directory
                .join(lock_file_name("store", "initialize")?)
                .exists()
        );
        // The surviving holders still exclude new lockers of the same file.
        assert!(!lock_exclusive_until(
            &open_lock_file(&legacy[0], false)?,
            Instant::now()
        )?);
        // An explicit unlock is immediate even if a child forked by a parallel
        // test still holds an inherited copy of the descriptor.
        release(&old_holder);
        drop((old_holder, current));
        assert_eq!(prune_locks(&global)?, (1, 0));
        assert_eq!(prune_locks(&global)?, (0, 0));
        Ok(())
    }

    #[test]
    fn a_waiter_on_a_retired_file_moves_to_the_current_lock() -> Result<()> {
        let fixture = Fixture::new()?;
        let global = fixture.path("global.sqlite");
        let held = lock_path(&global, "description", "key")?;
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        thread::scope(|scope| -> Result<()> {
            let worker = scope.spawn(|| -> Result<()> {
                ready_tx.send(())?;
                // Opens the file `held` is about to retire, then waits on it.
                let lock = lock_path(&global, "description", "key")?;
                done_tx.send(())?;
                drop(lock);
                Ok(())
            });
            ready_rx.recv_timeout(Duration::from_secs(5))?;
            thread::sleep(Duration::from_millis(100));
            drop(held);
            let finished = done_rx.recv_timeout(Duration::from_secs(5));
            worker.join().unwrap()?;
            finished?;
            Ok(())
        })?;
        assert!(lock_files(&global)?.is_empty());
        Ok(())
    }
}
