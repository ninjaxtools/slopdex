//! Reachability cleanup, run only while holding the store's exclusive maintenance lock.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;

use super::{DescriptionArtifact, PRUNE_BATCH_ROWS, PruneOptions, valid_hash};
use crate::{formats::FormatGroup, hash, registry};

#[derive(Default)]
pub(super) struct CleanupReport {
    pub worktrees_removed: u64,
    pub worktrees_busy: u64,
    pub artifact_rows_deleted: u64,
    pub vector_rows_deleted: u64,
    pub description_rows_deleted: u64,
    pub repository_rows_deleted: u64,
    pub index_files_removed: u64,
}

// Index and base locks predate the artifact-lock retirement protocol. Never unlink
// them: an opener may already be waiting on the same inode.
struct LockedFile(File);

impl Drop for LockedFile {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

fn try_lock(path: &Path) -> Result<Option<LockedFile>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    Ok(registry::lock_exclusive_until(&file, Instant::now())?.then_some(LockedFile(file)))
}

fn appended(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// Only positive absence is deletion permission. Permission and I/O errors abort.
fn missing(path: &Path) -> Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(!metadata.is_dir()),
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            Ok(true)
        }
        Err(error) => Err(error).with_context(|| format!("Inspect worktree {}", path.display())),
    }
}

fn missing_roots(identity: &Path, registered: Option<&PathBuf>) -> Result<bool> {
    Ok(missing(identity)?
        && registered
            .map(|path| missing(path))
            .transpose()?
            .unwrap_or(true))
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
        [name],
        |r| r.get(0),
    )?)
}

fn open_index(path: &Path, store: &Path) -> Result<Option<(Connection, PathBuf)>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    if !table_exists(&conn, "metadata")? {
        return Ok(None);
    }
    let binding: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key='global_path'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let Some(binding) = binding else {
        return Ok(None);
    };
    let binding = match fs::canonicalize(&binding) {
        Ok(binding) => binding,
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Ok(None);
        }
        Err(error) => return Err(error).context("Resolve index artifact-store binding"),
    };
    if binding != store {
        return Ok(None);
    }
    let identity: String =
        conn.query_row("SELECT value FROM metadata WHERE key='identity'", [], |r| {
            r.get(0)
        })?;
    let identity: Value = serde_json::from_str(&identity)?;
    ensure!(
        identity["schema"] == 4,
        "Unsupported index identity at {}",
        path.display()
    );
    let root = PathBuf::from(
        identity["root"]
            .as_str()
            .context("Index identity has no root")?,
    );
    ensure!(
        root.is_absolute(),
        "Index root is not absolute at {}",
        path.display()
    );
    Ok(Some((conn, root)))
}

fn candidates(conn: &Connection, store: &Path, worktrees: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut indexes = conn
        .prepare("SELECT index_path FROM workspaces")?
        .query_map([], |r| r.get::<_, String>(0).map(PathBuf::from))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    let entries = match fs::read_dir(worktrees) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(indexes),
        Err(error) => return Err(error).context("Read managed worktree indexes"),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || !entry.file_name().to_str().is_some_and(valid_hash) {
            continue;
        }
        let index = entry.path().join("index.sqlite");
        let metadata = match fs::symlink_metadata(&index) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            continue;
        }
        // Unrelated stores are inspected read-only, never locked or modified.
        if let Some((_db, root)) = open_index(&index, store)? {
            ensure!(
                entry.file_name() == hash(root.as_os_str().as_encoded_bytes()).as_str(),
                "Managed index root hash mismatch at {}",
                index.display()
            );
            indexes.insert(index.canonicalize()?);
        }
    }
    Ok(indexes)
}

fn mark(conn: &Connection, kind: &str, key: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO prune_keep VALUES(?,?)",
        params![kind, key],
    )?;
    Ok(())
}

fn mark_query(store: &Connection, index: &Connection, kind: &str, sql: &str) -> Result<()> {
    let mut stmt = index.prepare(sql)?;
    for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
        let key = row?;
        mark(store, kind, &key)?;
        if kind == "description-content" {
            store.execute(
                "INSERT OR IGNORE INTO prune_bound_descriptions VALUES(?)",
                [&key],
            )?;
        }
    }
    Ok(())
}

fn mark_workspace(store: &Connection, index: &Connection) -> Result<()> {
    let mut files = index.prepare("SELECT path,hash,language,parser_version FROM files")?;
    for row in files.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })? {
        let (path, source, language, parser) = row?;
        mark(store, "source", &source)?;
        store.execute(
            "INSERT OR IGNORE INTO prune_files VALUES(?,?)",
            params![path, source],
        )?;
        if let Some(parser) = parser {
            mark(
                store,
                "parse",
                &hash(serde_json::json!([parser, language, source]).to_string()),
            )?;
        }
    }
    mark_query(
        store,
        index,
        "unit-content",
        "SELECT DISTINCT content_hash FROM search_units",
    )?;
    mark_query(
        store,
        index,
        "description-content",
        "SELECT DISTINCT content_hash FROM descriptions",
    )?;
    mark_query(
        store,
        index,
        "description-content",
        "SELECT DISTINCT json_extract(metadata,'$.descriptionHash') FROM symbols WHERE json_extract(metadata,'$.descriptionHash') IS NOT NULL",
    )?;
    mark_query(
        store,
        index,
        "embedding",
        "SELECT embedding_key FROM unit_embeddings UNION SELECT embedding_key FROM descriptions WHERE embedding_key IS NOT NULL",
    )?;
    Ok(())
}

fn pointer_suffixes() -> Vec<String> {
    FormatGroup::ALL
        .into_iter()
        .map(FormatGroup::as_str)
        .chain(["markdown", "descriptions", "symbols"])
        .map(|role| format!(".{role}.shared.json"))
        .collect()
}

fn mark_bases(
    conn: &Connection,
    index: &Path,
    bases: &mut BTreeSet<(String, String)>,
) -> Result<()> {
    for suffix in pointer_suffixes() {
        let path = appended(index, &suffix);
        let text = match fs::read(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        // Corrupt pointers already cause a rebuild; they cannot root arbitrary paths.
        if let Ok(value) = serde_json::from_slice::<Value>(&text)
            && let (Some(contract), Some(base)) =
                (value["contract"].as_str(), value["base_id"].as_str())
            && valid_hash(contract)
            && valid_hash(base)
        {
            bases.insert((contract.to_owned(), base.to_owned()));
            // Symbol/name vectors are not in unit_embeddings. The saved pointer
            // is their durable live membership, including exact delta vectors.
            if let Some(membership) = value["membership"].as_object() {
                for key in membership.values().filter_map(Value::as_str) {
                    mark(conn, "embedding", key)?;
                }
            }
            if let Some(delta) = value["delta"].as_object() {
                for key in delta.keys() {
                    mark(conn, "embedding", key)?;
                }
            }
        }
    }
    Ok(())
}

fn remove_file(path: &Path) -> Result<u64> {
    match fs::symlink_metadata(path) {
        // Never follow or remove an unexpected directory or symlink.
        Ok(metadata) if metadata.is_file() => {
            fs::remove_file(path)?;
            Ok(1)
        }
        Ok(_) => Ok(0),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn remove_index(index: &Path, protected: &BTreeSet<PathBuf>) -> Result<u64> {
    let mut removed = 0;
    for suffix in pointer_suffixes()
        .into_iter()
        .chain(["-wal".into(), "-shm".into(), "".into()])
    {
        let path = appended(index, &suffix);
        if path != index && protected.contains(&path) {
            continue;
        }
        removed += remove_file(&path)?;
    }
    Ok(removed)
}

fn delete_batches(conn: &Connection, table: &str, predicate: &str) -> Result<u64> {
    let sql = format!(
        "DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE {predicate} LIMIT ?)"
    );
    let mut total = 0;
    loop {
        let count = conn.execute(&sql, [PRUNE_BATCH_ROWS])?;
        if count == 0 {
            return Ok(total);
        }
        total += count as u64;
    }
}

fn mark_descriptions(conn: &Connection, options: PruneOptions) -> Result<()> {
    let mut stmt = conn.prepare("SELECT key,value FROM cache WHERE kind='description'")?;
    let mut incomplete = false;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (key, value) = row?;
        let artifact = DescriptionArtifact::decode(&value);
        let live = if let Ok(artifact) = &artifact {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM prune_files WHERE path=? AND hash=?) OR EXISTS(SELECT 1 FROM prune_bound_descriptions WHERE hash=?)",
                params![artifact.generation.path, artifact.generation.file_hash, hash(&artifact.text)],
                |r| r.get::<_, bool>(0),
            )?
        } else {
            // An unrecognized generation format is not permission to delete a paid answer.
            true
        };
        if !options.prune_descriptions || live {
            mark(conn, "description", &key)?;
            if let Ok(artifact) = artifact {
                for content in artifact.generation.hashes() {
                    mark(conn, "description-content", content)?;
                }
                mark(conn, "description-content", &hash(&artifact.text))?;
            } else {
                incomplete = true;
            }
        }
    }
    if incomplete {
        conn.execute("INSERT OR IGNORE INTO prune_keep SELECT 'description-content',hash FROM description_content", [])?;
    }
    Ok(())
}

fn prune_repositories(conn: &Connection) -> Result<u64> {
    if !table_exists(conn, "repositories")? {
        return Ok(0);
    }
    let rows = conn
        .prepare("SELECT checkout,checkout_root FROM repositories")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut removed = 0;
    for (checkout, root) in rows {
        if missing(Path::new(&root))? {
            removed +=
                conn.execute("DELETE FROM repositories WHERE checkout=?", [checkout])? as u64;
        }
    }
    if table_exists(conn, "family_evidence")? {
        removed += delete_batches(
            conn,
            "family_evidence",
            "NOT EXISTS(SELECT 1 FROM repositories r WHERE r.family=family_evidence.family)",
        )?;
    }
    Ok(removed)
}

fn prune_bases(
    store: &Path,
    keep: &BTreeSet<(String, String)>,
    protected: &BTreeSet<PathBuf>,
) -> Result<u64> {
    let directory = appended(store, ".indexes");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if !metadata.is_dir() => return Ok(0),
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    }
    let contracts = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut removed = 0;
    for entry in contracts {
        let entry = entry?;
        let contract = entry.file_name();
        let Some(contract) = contract.to_str().filter(|name| valid_hash(name)) else {
            continue;
        };
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let mut bases = BTreeSet::new();
        for file in fs::read_dir(entry.path())? {
            let file = file?;
            if !file.file_type()?.is_file() {
                continue;
            }
            if let Some(name) = file.file_name().to_str()
                && let Some((base, _)) = name.split_once('.')
                && valid_hash(base)
                && [".base.json", ".usearch", ".usearch.manifest.json"]
                    .iter()
                    .any(|suffix| name == format!("{base}{suffix}"))
            {
                bases.insert(base.to_owned());
            }
        }
        for base in bases {
            if keep.contains(&(contract.to_owned(), base.clone())) {
                continue;
            }
            if [".base.json", ".usearch.manifest.json", ".usearch"]
                .iter()
                .any(|suffix| protected.contains(&entry.path().join(format!("{base}{suffix}"))))
            {
                continue;
            }
            let Some(_lock) = try_lock(&entry.path().join(format!("{base}.lock")))? else {
                continue;
            };
            // The publication marker goes first. Leave the persistent lock and
            // in-progress atomic-write temporaries untouched.
            for suffix in [".base.json", ".usearch.manifest.json", ".usearch"] {
                removed += remove_file(&entry.path().join(format!("{base}{suffix}")))?;
            }
        }
    }
    Ok(removed)
}

pub(super) fn cleanup(
    conn: &Connection,
    store: &Path,
    options: PruneOptions,
    worktrees: &Path,
) -> Result<CleanupReport> {
    cleanup_with(conn, store, options, worktrees, |_| Ok(()))
}

pub(super) fn cleanup_with(
    conn: &Connection,
    store: &Path,
    options: PruneOptions,
    worktrees: &Path,
    mut after_missing: impl FnMut(&Path) -> Result<()>,
) -> Result<CleanupReport> {
    registry::ensure_workspace_tracking(conn, "main")?;
    // Discard obsolete bookkeeping as well as its retention policy. Old,
    // undiscoverable custom indexes must be rebuilt rather than pinning data.
    conn.execute_batch(
        "DROP TABLE IF EXISTS legacy_artifacts;
        CREATE TEMP TABLE prune_keep(kind TEXT,key TEXT,PRIMARY KEY(kind,key));
        CREATE TEMP TABLE prune_files(path TEXT,hash TEXT,PRIMARY KEY(path,hash));
        CREATE TEMP TABLE prune_bound_descriptions(hash TEXT PRIMARY KEY);",
    )?;
    let mut report = CleanupReport::default();
    let mut bases = BTreeSet::new();
    let mut locks = Vec::new();
    let indexes = candidates(conn, store, worktrees)?;
    let registered = conn
        .prepare("SELECT index_path,root FROM workspaces")?
        .query_map([], |r| {
            Ok((
                PathBuf::from(r.get::<_, String>(0)?),
                PathBuf::from(r.get::<_, String>(1)?),
            ))
        })?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
    let coordination: BTreeSet<_> = indexes
        .iter()
        .map(|index| appended(index, ".lock"))
        .collect();
    let mut protected = indexes.clone();
    protected.extend(coordination.iter().cloned());
    protected.insert(store.to_owned());
    protected.insert(appended(store, "-wal"));
    protected.insert(appended(store, "-shm"));
    for index in indexes {
        ensure!(index.is_absolute(), "Registered index path is not absolute");
        if coordination.contains(&index)
            || index.starts_with(appended(store, ".locks"))
            || (index.starts_with(appended(store, ".indexes"))
                && index.extension().is_some_and(|ext| ext == "lock"))
        {
            // An unconventional custom index can also name a permanent lock.
            // Never unlink coordination paths, even as a candidate's main file.
            report.worktrees_busy += 1;
            continue;
        }
        match fs::symlink_metadata(&index) {
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
            {
                conn.execute(
                    "DELETE FROM workspaces WHERE index_path=?",
                    [index.to_str().context("Index path is not UTF-8")?],
                )?;
                continue;
            }
            Err(error) => return Err(error.into()),
            Ok(metadata) => ensure!(
                metadata.is_file(),
                "Registered index is not a regular file: {}",
                index.display()
            ),
        }
        let Some(lock) = try_lock(&appended(&index, ".lock"))? else {
            report.worktrees_busy += 1;
            continue;
        };
        let Some((db, root)) = open_index(&index, store)
            .with_context(|| format!("Inspect index {}", index.display()))?
        else {
            // A moved/replaced file no longer belongs to this store. Never delete it.
            conn.execute(
                "DELETE FROM workspaces WHERE index_path=?",
                [index.to_str().context("Index path is not UTF-8")?],
            )?;
            continue;
        };
        let registered_root = registered.get(&index);
        let was_missing = missing_roots(&root, registered_root)?;
        if was_missing {
            after_missing(&root)?;
        }
        if was_missing && missing_roots(&root, registered_root)? {
            drop(db);
            report.index_files_removed += remove_index(&index, &protected)?;
            report.worktrees_removed += 1;
            conn.execute(
                "DELETE FROM workspaces WHERE index_path=?",
                [index.to_str().context("Index path is not UTF-8")?],
            )?;
        } else {
            mark_workspace(conn, &db)
                .with_context(|| format!("Read index references {}", index.display()))?;
            mark_bases(conn, &index, &mut bases)?;
            let root = match root.canonicalize() {
                Ok(root) => root,
                Err(error)
                    if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
                {
                    registered_root.cloned().unwrap_or(root)
                }
                Err(error) => return Err(error.into()),
            };
            conn.execute("INSERT INTO workspaces VALUES(?,?) ON CONFLICT(index_path) DO UPDATE SET root=excluded.root", params![index.to_str().context("Index path is not UTF-8")?, root.to_str().context("Worktree path is not UTF-8")?])?;
        }
        locks.push(lock);
    }
    // An opener from before the maintenance barrier may already own an index
    // lock. Its snapshot was not inspected, so defer global sweeping entirely.
    if report.worktrees_busy != 0 {
        return Ok(report);
    }
    mark_descriptions(conn, options)?;
    if options.prune_vectors {
        report.vector_rows_deleted = delete_batches(
            conn,
            "embeddings",
            "NOT EXISTS(SELECT 1 FROM prune_keep k WHERE k.kind='embedding' AND k.key=embeddings.key)",
        )?;
    }
    if options.prune_descriptions {
        report.description_rows_deleted = delete_batches(
            conn,
            "cache",
            "kind='description' AND NOT EXISTS(SELECT 1 FROM prune_keep k WHERE k.kind='description' AND k.key=cache.key)",
        )?;
    }
    for (table, predicate) in [
        (
            "sources",
            "NOT EXISTS(SELECT 1 FROM prune_keep k WHERE k.kind='source' AND k.key=sources.hash)",
        ),
        (
            "description_content",
            "NOT EXISTS(SELECT 1 FROM prune_keep k WHERE k.kind='description-content' AND k.key=description_content.hash)",
        ),
        (
            "cache",
            "kind IN ('unit-content','parse','callgraph') AND NOT EXISTS(SELECT 1 FROM prune_keep k WHERE k.kind=cache.kind AND k.key=cache.key)",
        ),
        (
            "remote_uploads",
            "(kind='embedding' AND NOT EXISTS(SELECT 1 FROM embeddings e WHERE e.key=remote_uploads.key)) OR (kind='content' AND NOT EXISTS(SELECT 1 FROM description_content c WHERE c.hash=remote_uploads.key)) OR (kind IN ('description','explanation','rerank') AND NOT EXISTS(SELECT 1 FROM cache c WHERE c.kind=remote_uploads.kind AND c.key=remote_uploads.key))",
        ),
    ] {
        report.artifact_rows_deleted += delete_batches(conn, table, predicate)?;
    }
    report.repository_rows_deleted = prune_repositories(conn)?;
    report.index_files_removed += prune_bases(store, &bases, &protected)?;
    Ok(report)
}
