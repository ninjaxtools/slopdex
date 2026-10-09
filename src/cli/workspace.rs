//! Workspace paths and index identity.

use crate::cache;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
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
        .join("worktrees-v1")
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
