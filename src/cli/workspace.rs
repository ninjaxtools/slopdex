//! Workspace paths and index identity.

use crate::cache;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

pub(super) struct SourceWorkspace {
    pub(super) root: PathBuf,
    pub(super) paths: Vec<PathBuf>,
    /// Effective cwd for relative paths saved in an external source's config.
    pub(super) directory: Option<PathBuf>,
}

/// Interpret source arguments against the selected workspace, then discover an
/// external source's own checkout (or directory for non-Git sources).
pub(super) fn source_workspace(root: &Path, path: &Path) -> Result<SourceWorkspace> {
    let selected = canonical_identity(root)?;
    let source = canonical_identity(&root.join(path))?;
    let (root, directory) = if source.starts_with(&selected) {
        (root.to_owned(), None)
    } else {
        let mut directory = source.as_path();
        while !directory.is_dir() {
            directory = directory.parent().context("Source path has no directory")?;
        }
        (
            crate::git::checkout_root(directory)?.unwrap_or_else(|| directory.to_owned()),
            Some(directory.to_owned()),
        )
    };
    Ok(SourceWorkspace {
        root,
        paths: vec![source],
        directory,
    })
}

pub(super) fn map_workspaces(root: &Path, paths: &[PathBuf]) -> Result<Vec<SourceWorkspace>> {
    if paths.is_empty() {
        return Ok(vec![SourceWorkspace {
            root: root.to_owned(),
            paths: Vec::new(),
            directory: None,
        }]);
    }
    let mut workspaces: Vec<SourceWorkspace> = Vec::new();
    let mut missing = Vec::new();
    for path in paths {
        if !root
            .join(path)
            .try_exists()
            .with_context(|| format!("Cannot inspect map path {}", path.display()))?
        {
            missing.push(path);
            continue;
        }
        let workspace = source_workspace(root, path)?;
        let mut existing = None;
        for (index, group) in workspaces.iter().enumerate() {
            if same_path(&group.root, &workspace.root)? && group.directory == workspace.directory {
                existing = Some(index);
                break;
            }
        }
        if let Some(index) = existing {
            workspaces[index].paths.extend(workspace.paths);
        } else {
            workspaces.push(workspace);
        }
    }
    if workspaces.is_empty() {
        // Keep the normal empty-map lifecycle (including the missing-index -q
        // warning). existing_options will warn about and discard these paths.
        return Ok(vec![SourceWorkspace {
            root: root.to_owned(),
            paths: paths.to_vec(),
            directory: None,
        }]);
    }
    for path in missing {
        crate::ui::warning(format!(
            "slopdex: warning: map path does not exist; ignoring: {}",
            path.display()
        ));
    }
    Ok(workspaces)
}

pub(super) fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    // Canonicalize when comparing identities, not when interpreting repo-relative source filters.
    Ok(path)
}

pub(super) fn root_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return absolute(path);
    }
    let cwd = std::env::current_dir()?;
    Ok(crate::git::checkout_root(&cwd)?.unwrap_or(cwd))
}

pub(super) fn config_path(root: &Path, explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return absolute(path);
    }
    let local = root.join(".slopdex/config.json");
    if local
        .try_exists()
        .with_context(|| format!("Cannot inspect config {}", local.display()))?
    {
        return absolute(&local);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
        .context("Cannot locate the user's config directory")?;
    Ok(base.join("slopdex/config.json"))
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
