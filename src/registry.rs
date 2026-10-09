//! Repository relatedness and per-artifact single-flight coordination.
//! Family membership is discovery evidence, never proof that a snapshot is reusable.

use std::{
    collections::BTreeSet,
    fs,
    io::ErrorKind,
    path::Path,
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
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
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Acquire only around the provider request/artifact computation. After acquiring,
/// recheck the artifact cache: another process may have filled it while we waited.
/// Dropping the returned file releases the lock. Lock files must not be unlinked
/// during GC, since waiters could otherwise lock a different inode for the same key.
pub(crate) fn artifact_lock(db: &Database, kind: &str, key: &str) -> Result<fs::File> {
    lock_path(db.global_path(), kind, key)
}

/// Database-independent accessor for parallel workers: capture the global path,
/// not a borrowed SQLite connection. It must be the store's canonical path.
pub(crate) fn lock_path(global_path: &Path, kind: &str, key: &str) -> Result<fs::File> {
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
    let mut directory = global_path.as_os_str().to_owned();
    directory.push(".locks");
    let directory = std::path::PathBuf::from(directory);
    fs::create_dir_all(&directory)?;
    ensure!(directory.is_dir(), "Artifact lock path is not a directory");
    let path = directory.join(format!("{}.lock", hash(serde_json::to_vec(&[kind, key])?)));
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("Open artifact lock {}", path.display()))?;
    let started = Instant::now();
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            // Windows reports ERROR_LOCK_VIOLATION, which is not necessarily
            // mapped to WouldBlock. Compare fs2's native contention code too.
            Err(error)
                if error.kind() == ErrorKind::WouldBlock
                    || error.raw_os_error().is_some_and(|code| {
                        Some(code) == fs2::lock_contended_error().raw_os_error()
                    }) =>
            {
                ensure!(
                    started.elapsed() < LOCK_TIMEOUT,
                    "Timed out waiting for artifact lock {kind}:{key}"
                );
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("Acquire artifact lock"),
        }
    }
}

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
            assert!(matches!(
                done_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            drop(held);
            done_rx.recv_timeout(Duration::from_secs(5))?;
            worker.join().unwrap()?;
            Ok(())
        })
    }
}
