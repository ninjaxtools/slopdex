//! Source discovery, exclusion policy, and refresh fingerprints.

use super::*;

pub(super) const EXCLUDED: &[&str] = &[
    ".git",
    ".slopdex",
    "node_modules",
    "dist",
    "build",
    "coverage",
    "vendor",
    "generated",
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    "target",
];

// Generated dependency resolutions, including ecosystems without a .lock suffix.
// Match at any depth, but do not exclude dependency manifests or source names.
pub(super) const EXCLUDED_LOCK_FILES: &[&str] = &[
    "**/*.lock",
    "**/*.lockb",
    "**/*.lockfile",
    "**/*.locked",
    "**/*.lock.json",
    "**/*.lock.yaml",
    "**/*.lock.yml",
    "**/*-lock.json",
    "**/*-lock.yaml",
    "**/*-lock.yml",
    "**/npm-shrinkwrap.json",
    "**/shrinkwrap.yaml",
    "**/esy.lock/**",
    "**/pylock.toml",
    "**/pylock.*.toml",
    "**/go.sum",
    "**/go.work.sum",
    "**/Package.resolved",
    "**/Cartfile.resolved",
    "**/.terraform.lock.hcl",
    "**/manifest.toml",
    "**/Manifest.toml",
    "**/Manifest-v*.toml",
    "**/JuliaManifest.toml",
    "**/JuliaManifest-v*.toml",
    "**/cpanfile.snapshot",
    "**/cabal.project.freeze",
    "**/cabal.project.*.freeze",
    "**/dub.selections.json",
    "**/.meteor/versions",
];

pub(crate) fn source_paths(root: &Path, index: &Path, config: &Value) -> Result<Vec<String>> {
    source_paths_subset(root, index, config, None, None, true)
}

/// File-level selection shared by discovery and Git status filtering.
pub(super) struct Eligibility {
    include: GlobSet,
    exclude: GlobSet,
    lock_files: GlobSet,
}

impl Eligibility {
    pub(super) fn new(config: &Value) -> Result<Self> {
        Ok(Self {
            include: globs(&config["include"])?,
            exclude: globs(&config["exclude"])?,
            lock_files: globs(&json!(EXCLUDED_LOCK_FILES))?,
        })
    }

    pub(super) fn selects_file(&self, path: &str) -> bool {
        parse::language_for_path(path).is_some()
            && !self.lock_files.is_match(path)
            && !self.exclude.is_match(path)
            && (self.include.is_empty() || self.include.is_match(path))
    }

    /// Whether a root-relative path reported by Git could be walked and
    /// selected. Ignore files are not consulted: this may keep an ignored path,
    /// never drop a selectable one.
    pub(super) fn selects_git_path(&self, path: &str) -> bool {
        self.selects_file(path)
            && !path
                .rsplit('/')
                .skip(1)
                .any(|directory| EXCLUDED.contains(&directory))
    }
}

/// A vanished or unreadable entry below the root is skipped; the root itself
/// must be walkable.
pub(super) fn skippable_walk_error(error: &ignore::Error) -> bool {
    error.depth().is_some_and(|depth| depth > 0)
        && error.io_error().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            )
        })
}

pub(super) fn source_paths_subset(
    root: &Path,
    index: &Path,
    config: &Value,
    candidates: Option<&HashSet<String>>,
    mut directories: Option<&mut HashSet<String>>,
    warn: bool,
) -> Result<Vec<String>> {
    let eligibility = Eligibility::new(config)?;
    ensure!(
        config["maxFileSize"].as_u64().unwrap_or(1_048_576) > 0,
        "maxFileSize must be positive"
    );
    let mut walker = ignore::WalkBuilder::new(root);
    let filter_root = root.to_owned();
    let candidates = candidates.cloned();
    let global_path = crate::cache::store_path(config)?;
    walker
        .hidden(false)
        .require_git(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            if global_artifact(entry.path(), &global_path) {
                return false;
            }
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                if EXCLUDED.contains(&entry.file_name().to_string_lossy().as_ref())
                    || entry.path().join(".git").exists()
                {
                    return false;
                }
                return candidates.as_ref().is_none_or(|paths| {
                    paths
                        .iter()
                        .any(|path| filter_root.join(path).starts_with(entry.path()))
                });
            }
            candidates.as_ref().is_none_or(|paths| {
                relative(&filter_root, entry.path())
                    .ok()
                    .is_some_and(|path| paths.contains(&path))
            })
        });
    let mut paths = Vec::new();
    let mut unreadable = 0usize;
    let mut undecodable = 0usize;
    for entry in walker.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if skippable_walk_error(&error) => {
                unreadable += 1;
                continue;
            }
            Err(error) => return Err(error).context("Cannot walk repository"),
        };
        // Indexed paths are UTF-8 strings. Other names cannot be source paths.
        let path = relative_utf8(root, entry.path())?;
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            if let (Some(directories), Some(path)) = (&mut directories, path) {
                directories.insert(path);
            }
            continue;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) || index_artifact(entry.path(), index) {
            continue;
        }
        let Some(path) = path else {
            undecodable += usize::from(eligibility.selects_file(&entry.path().to_string_lossy()));
            continue;
        };
        if eligibility.selects_file(&path) {
            paths.push(path);
        }
    }
    if warn && unreadable > 0 {
        ui::warning(format_args!(
            "slopdex: skipped {unreadable} unreadable or vanished path(s) while scanning {}",
            root.display()
        ));
    }
    if warn && undecodable > 0 {
        ui::warning(format_args!(
            "slopdex: skipped {undecodable} source file(s) whose path is not valid UTF-8"
        ));
    }
    paths.sort();
    Ok(paths)
}

pub(super) fn global_artifact(path: &Path, global: &Path) -> bool {
    if path == global {
        return true;
    }
    path.parent() == global.parent()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix(global.file_name()?.to_str()?))
            .is_some_and(|suffix| matches!(suffix, "-wal" | "-shm" | ".locks" | ".indexes"))
}

pub(super) fn policy_path(path: &str) -> bool {
    matches!(
        Path::new(path).file_name().and_then(|name| name.to_str()),
        Some(".gitignore" | ".ignore" | ".rgignore")
    )
}

pub(super) fn has_read_error(file: &File) -> bool {
    file.errors
        .iter()
        .any(|error| error["code"] == "read-error")
}

/// Ignore-policy probe for a remembered directory. An unreadable directory
/// cannot contribute rules, but regaining access must be noticed as a change.
pub(super) fn policy_fingerprint(path: &Path) -> Result<Value> {
    match fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            Ok(json!("unreadable"))
        }
        _ => file_fingerprint(path),
    }
}

pub(super) fn file_fingerprint(path: &Path) -> Result<Value> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        // A parent replaced by a file is as absent as a deleted file.
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(Value::Null);
        }
        Err(error) => return Err(error.into()),
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|time| time.as_nanos().to_string());
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        json!([
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec(),
            metadata.mode()
        ])
    };
    #[cfg(not(unix))]
    let identity = json!(
        metadata
            .created()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|time| time.as_nanos().to_string())
    );
    Ok(json!([metadata.len(), modified, identity]))
}

pub(super) fn selection_contract(config: &Value) -> String {
    json!([
        STRUCTURE_PARSER_VERSION,
        config["include"],
        config["exclude"],
        config["maxFileSize"].as_u64().unwrap_or(1_048_576),
        EXCLUDED,
        EXCLUDED_LOCK_FILES
    ])
    .to_string()
}

pub(super) fn indexing_policy(
    root: &Path,
    config: &Value,
    identity: Option<&str>,
    directories: &HashSet<String>,
) -> Result<String> {
    // Validate even on the unchanged fast path.
    globs(&config["include"])?;
    globs(&config["exclude"])?;
    ensure!(
        config["maxFileSize"].as_u64().unwrap_or(1_048_576) > 0,
        "maxFileSize must be positive"
    );
    let mut external = BTreeMap::new();
    // Effective ignore inputs can themselves be Git-ignored. Remember walked
    // directories and stat only their policy files, not every source file.
    for directory in directories {
        for name in [".gitignore", ".ignore", ".rgignore"] {
            let path = root.join(directory).join(name);
            external.insert(path.clone(), policy_fingerprint(&path)?);
        }
    }
    for parent in root.ancestors() {
        for name in [".gitignore", ".ignore", ".rgignore"] {
            let path = parent.join(name);
            external.insert(path.clone(), file_fingerprint(&path)?);
        }
    }
    if let Some(identity) = identity {
        let identity: Value = serde_json::from_str(identity)?;
        for name in [
            "info/exclude",
            "info/sparse-checkout",
            "config",
            "config.worktree",
        ] {
            for directory in [
                identity["common_dir"].as_str(),
                identity["git_dir"].as_str(),
            ]
            .into_iter()
            .flatten()
            {
                let path = Path::new(directory).join(name);
                external.insert(path.clone(), file_fingerprint(&path)?);
            }
        }
    }
    let global_ignore = git::excludes_file(root).or_else(|| {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
            .map(|directory| directory.join("git/ignore"))
    });
    if let Some(path) = global_ignore {
        external.insert(path.clone(), file_fingerprint(&path)?);
    }
    external.retain(|_, value| !value.is_null());
    Ok(json!([selection_contract(config), external]).to_string())
}

#[cfg(test)]
thread_local! {
    /// Source reads attempted on this thread, to prove refreshes skip them.
    pub(super) static SOURCE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn read_source(root: &Path, path: &str, max_size: u64) -> Result<String> {
    #[cfg(test)]
    SOURCE_READS.with(|reads| reads.set(reads.get() + 1));
    let path = root.join(path);
    ensure!(
        fs::metadata(&path)?.len() <= max_size,
        "File exceeds maxFileSize ({max_size} bytes)"
    );
    fs::read_to_string(&path).context("Cannot read UTF-8 source")
}

pub(crate) fn normalize_path(root: &Path, path: &str) -> Result<String> {
    let path = Path::new(path);
    if path.is_absolute() {
        // Both indexed and direct maps use a canonical root; resolve aliases
        // before checking that an absolute source path is inside it.
        let path = path.canonicalize().context("Cannot resolve source path")?;
        return relative(root, &path);
    }
    ensure!(
        !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "Source path must remain inside the repository"
    );
    Ok(path
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect::<PathBuf>()
        .to_string_lossy()
        .replace('\\', "/"))
}

/// The index may live inside the scanned root; its vector manifests are JSON.
pub(super) fn index_artifact(path: &Path, index: &Path) -> bool {
    if path == index {
        return true;
    }
    if path.parent() != index.parent() {
        return false;
    }
    let Some(suffix) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(index.file_name()?.to_str()?))
    else {
        return false;
    };
    matches!(suffix, ".lock" | "-wal" | "-shm")
        || FormatGroup::ALL
            .into_iter()
            .map(FormatGroup::as_str)
            .chain(["markdown", "descriptions", "symbols"])
            .any(|role| suffix == format!(".{role}.shared.json"))
}

pub(crate) fn under(path: &str, parent: &str) -> bool {
    parent.is_empty()
        || path == parent
        || path
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('/'))
}
pub(super) fn relative(root: &Path, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(root)
        .context("Path is outside repository")?
        .to_str()
        .context("Non-UTF-8 source path")?
        .replace('\\', "/"))
}
/// None for a path that cannot be represented as an indexed (UTF-8) path.
pub(super) fn relative_utf8(root: &Path, path: &Path) -> Result<Option<String>> {
    Ok(path
        .strip_prefix(root)
        .context("Path is outside repository")?
        .to_str()
        .map(|path| path.replace('\\', "/")))
}
/// Config `include`/`exclude` patterns. These are plain globset globs matched
/// against the whole root-relative path: `*` and `?` also match `/`, a pattern
/// without a slash is not a basename pattern, and there is no negation. `-g`
/// selectors use gitignore semantics instead (see `filter::Selection`).
pub(super) fn globs(value: &Value) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    if let Some(patterns) = value.as_array() {
        for pattern in patterns {
            builder.add(Glob::new(
                pattern.as_str().context("Glob must be a string")?,
            )?);
        }
    }
    Ok(builder.build()?)
}
