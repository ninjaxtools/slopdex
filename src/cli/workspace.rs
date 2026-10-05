//! Workspace paths and index identity and migration.

use crate::cache;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

pub(super) fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    // Canonicalize when comparing identities, not when interpreting repo-relative source filters.
    Ok(path)
}

pub(super) fn config_path(root: &Path, explicit: Option<&Path>) -> Result<PathBuf> {
    absolute(
        &explicit
            .map(Path::to_owned)
            .unwrap_or_else(|| root.join(".slopdex/config.json")),
    )
}

pub(super) fn index_path(root: &Path, explicit: Option<&Path>, config: &Value) -> Result<PathBuf> {
    if let Some(path) = explicit.or_else(|| config["indexPath"].as_str().map(Path::new)) {
        return absolute(path);
    }
    let root = root
        .canonicalize()
        .context("Repository root does not exist")?;
    Ok(cache::directory()?
        .join("workspaces")
        .join(crate::hash(root.as_os_str().as_encoded_bytes()))
        .join("index.sqlite"))
}

pub(super) fn require_index(index: &Path) -> Result<()> {
    ensure!(
        index.is_file(),
        "No index found at {}; run `slopdex update` first",
        index.display()
    );
    Ok(())
}

/// Migrate a compatible snapshot with SQLite backup so committed WAL data is included.
pub(super) fn migrate_legacy_index(root: &Path, index: &Path) -> Result<()> {
    let old = root.join(".slopdex/index.sqlite");
    if index.exists() || !old.exists() || same_path(&old, index)? {
        return Ok(());
    }
    let parent = index.parent().context("Index path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut lock_path = index.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(PathBuf::from(lock_path))?;
    lock.lock_exclusive()?;
    if index.exists() {
        return Ok(());
    }
    let source = Connection::open_with_flags(&old, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let identity: String = match source.query_row(
        "SELECT value FROM metadata WHERE key='identity'",
        [],
        |row| row.get(0),
    ) {
        Ok(value) => value,
        Err(_) => return Ok(()),
    };
    if serde_json::from_str::<Value>(&identity)? != json!({"schema":3,"root":root.canonicalize()?})
    {
        return Ok(());
    }
    let temporary = parent.join(format!(".index.sqlite.migrate-{}", std::process::id()));
    let copied = (|| -> Result<()> {
        let mut target = Connection::open(&temporary)?;
        let backup = rusqlite::backup::Backup::new(&source, &mut target)?;
        backup.run_to_completion(256, std::time::Duration::from_millis(20), None)?;
        drop(backup);
        drop(target);
        fs::rename(&temporary, index)?;
        Ok(())
    })();
    if copied.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    copied
}

fn canonical_identity(path: &Path) -> Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let path = absolute(path)?;
            let mut resolved = PathBuf::new();
            for component in path.components() {
                match component {
                    Component::CurDir => {}
                    Component::ParentDir => {
                        resolved.pop();
                    }
                    part => resolved.push(part.as_os_str()),
                }
                match fs::canonicalize(&resolved) {
                    Ok(canonical) => resolved = canonical,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("resolve {}", resolved.display()));
                    }
                }
            }
            Ok(resolved)
        }
        Err(error) => Err(error).with_context(|| format!("resolve {}", path.display())),
    }
}

pub(super) fn same_path(left: &Path, right: &Path) -> Result<bool> {
    if canonical_identity(left)? == canonical_identity(right)? {
        return Ok(true);
    }
    // Hard links also identify the same database and must not acquire a second exclusive lock.
    #[cfg(unix)]
    if let (Ok(left), Ok(right)) = (fs::metadata(left), fs::metadata(right)) {
        use std::os::unix::fs::MetadataExt;
        return Ok(left.dev() == right.dev() && left.ino() == right.ino());
    }
    Ok(false)
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
