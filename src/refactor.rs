//! Conservative, source-validated refactorings over the Tree-sitter index.
//!
//! Planning is read-only. Every input is checked again before publication;
//! writes are staged beside their targets and failures roll back completed
//! writes. This is not a crash-atomic multi-file filesystem transaction.

mod javascript;
mod native;
mod syntax;

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

use crate::parse::FileStructure;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

pub struct Plan {
    root: PathBuf,
    inputs: BTreeMap<String, String>,
    permissions: BTreeMap<String, fs::Permissions>,
    pub changes: Vec<FileChange>,
}

impl Plan {
    pub fn report(&self, dry_run: bool) -> Value {
        json!({"dryRun": dry_run, "changes": self.changes})
    }

    /// Render one unified hunk per changed file, retaining three context lines.
    pub fn print_diff(&self, out: &mut impl Write) -> Result<()> {
        for change in &self.changes {
            let before: Vec<_> = change
                .before
                .as_deref()
                .unwrap_or("")
                .split_inclusive('\n')
                .collect();
            let after: Vec<_> = change
                .after
                .as_deref()
                .unwrap_or("")
                .split_inclusive('\n')
                .collect();
            let common = before
                .iter()
                .zip(&after)
                .take_while(|(a, b)| a == b)
                .count();
            let suffix = before[common..]
                .iter()
                .rev()
                .zip(after[common..].iter().rev())
                .take_while(|(a, b)| a == b)
                .count();
            let start = common.saturating_sub(3);
            let old_end = (before.len() - suffix + 3).min(before.len());
            let new_end = (after.len() - suffix + 3).min(after.len());
            writeln!(
                out,
                "--- {}",
                if change.before.is_some() {
                    format!("a/{}", change.path)
                } else {
                    "/dev/null".into()
                }
            )?;
            writeln!(
                out,
                "+++ {}",
                if change.after.is_some() {
                    format!("b/{}", change.path)
                } else {
                    "/dev/null".into()
                }
            )?;
            writeln!(
                out,
                "@@ -{},{} +{},{} @@",
                if old_end == start { 0 } else { start + 1 },
                old_end - start,
                if new_end == start { 0 } else { start + 1 },
                new_end - start
            )?;
            for line in &before[start..common] {
                diff_line(out, ' ', line)?;
            }
            for line in &before[common..before.len() - suffix] {
                diff_line(out, '-', line)?;
            }
            for line in &after[common..after.len() - suffix] {
                diff_line(out, '+', line)?;
            }
            for line in &after[after.len() - suffix..new_end] {
                diff_line(out, ' ', line)?;
            }
        }
        Ok(())
    }

    pub fn apply(&self) -> Result<()> {
        // Validate the entire consulted snapshot, not just edited files: a new
        // reference in an unchanged input would also invalidate this plan.
        for (path, source) in &self.inputs {
            checked_path(&self.root, Path::new(path))?;
            ensure!(
                fs::read(self.root.join(path))? == source.as_bytes(),
                "Source changed while planning: {path}; retry refactor"
            );
        }
        for change in &self.changes {
            checked_path(&self.root, Path::new(&change.path))?;
            if change.before.is_none() {
                ensure!(
                    !self.root.join(&change.path).try_exists()?,
                    "Destination already exists: {}",
                    change.path
                );
            }
        }
        let mut staged = Vec::new();
        let mut directories = Vec::new();
        let result = (|| -> Result<()> {
            for change in &self.changes {
                if let Some(after) = &change.after {
                    let path = self.root.join(&change.path);
                    let parent = path.parent().context("Destination has no parent")?;
                    create_parents(parent, &mut directories)?;
                    let mut file = tempfile::NamedTempFile::new_in(parent)?;
                    file.write_all(after.as_bytes())?;
                    if let Some(permissions) = self.permissions.get(&change.path) {
                        file.as_file().set_permissions(permissions.clone())?;
                    }
                    file.as_file().sync_all()?;
                    staged.push((change, file));
                }
            }
            let mut published: Vec<&FileChange> = Vec::new();
            let publish = (|| -> Result<()> {
                for (change, file) in staged.drain(..) {
                    let path = self.root.join(&change.path);
                    if change.before.is_none() {
                        file.persist_noclobber(&path).map_err(|error| error.error)?;
                    } else {
                        file.persist(&path).map_err(|error| error.error)?;
                    }
                    published.push(change);
                }
                for change in self.changes.iter().filter(|change| change.after.is_none()) {
                    fs::remove_file(self.root.join(&change.path))?;
                    published.push(change);
                }
                Ok(())
            })();
            if let Err(error) = publish {
                let mut failures = Vec::new();
                for change in published.into_iter().rev() {
                    let path = self.root.join(&change.path);
                    let restored = (|| -> Result<()> {
                        match &change.after {
                            Some(after) => ensure!(
                                fs::read(&path)? == after.as_bytes(),
                                "Concurrent edit; refusing to overwrite it during rollback"
                            ),
                            None => ensure!(
                                !path.try_exists()?,
                                "Concurrent creation; refusing to overwrite it during rollback"
                            ),
                        }
                        match &change.before {
                            Some(before) => {
                                let mut original =
                                    tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
                                original.write_all(before.as_bytes())?;
                                if let Some(permissions) = self.permissions.get(&change.path) {
                                    original.as_file().set_permissions(permissions.clone())?;
                                }
                                original.persist(&path).map_err(|error| error.error)?;
                            }
                            None => fs::remove_file(&path)?,
                        }
                        Ok(())
                    })();
                    if let Err(error) = restored {
                        failures.push(format!("{}: {error}", change.path));
                    }
                }
                ensure!(
                    failures.is_empty(),
                    "Refactor failed: {error:#}; rollback also failed: {}",
                    failures.join(", ")
                );
                return Err(error).context("Refactor failed; completed writes were rolled back");
            }
            Ok(())
        })();
        if result.is_err() {
            drop(staged);
            for directory in directories.into_iter().rev() {
                let _ = fs::remove_dir(directory);
            }
        }
        result
    }
}

fn diff_line(out: &mut impl Write, prefix: char, line: &str) -> Result<()> {
    write!(out, "{prefix}{line}")?;
    if !line.ends_with('\n') {
        writeln!(out, "\n\\ No newline at end of file")?;
    }
    Ok(())
}

fn create_parents(path: &Path, created: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    create_parents(
        path.parent().context("No existing destination ancestor")?,
        created,
    )?;
    fs::create_dir(path)?;
    created.push(path.to_owned());
    Ok(())
}

pub(crate) fn supported(path: &str) -> bool {
    matches!(
        crate::parse::language_for_path(path),
        Some("javascript" | "jsx" | "typescript" | "tsx" | "rust" | "go" | "java" | "c")
    )
}

pub(crate) fn plan(
    root: &Path,
    cwd: &Path,
    files: BTreeMap<String, (String, FileStructure)>,
    source: &str,
    destination: &str,
) -> Result<Plan> {
    let root = root.canonicalize()?;
    let cwd = cwd.canonicalize()?;
    let (source_file, symbol) = selector(source)?;
    let source_path = argument_path(&root, &cwd, source_file)?;
    ensure!(
        files.contains_key(&source_path),
        "Source is not an indexed refactorable code file: {source_path}"
    );
    let (destination_path, new_name) = if let Some(symbol) = symbol {
        if identifier(destination) {
            (source_path.clone(), Some(destination.to_owned()))
        } else {
            let (path, name) = selector(destination)?;
            (
                argument_path(&root, &cwd, path)?,
                Some(
                    name.unwrap_or_else(|| symbol.rsplit('.').next().unwrap_or(symbol))
                        .to_owned(),
                ),
            )
        }
    } else {
        let (path, name) = selector(destination)?;
        ensure!(name.is_none(), "A file destination cannot include ::symbol");
        (argument_path(&root, &cwd, path)?, None)
    };
    ensure!(
        supported(&destination_path),
        "Destination must be a supported code file"
    );
    ensure!(
        same_family(&source_path, &destination_path),
        "Refactoring cannot cross source languages"
    );
    let files: BTreeMap<_, _> = files
        .into_iter()
        .filter(|(path, _)| same_family(&source_path, path))
        .collect();
    let mut inputs: BTreeMap<_, _> = files
        .iter()
        .map(|(path, (source, _))| (path.clone(), source.clone()))
        .collect();
    // Keep resolution policy in the same validated read set as source bytes.
    let javascript = matches!(
        crate::parse::language_for_path(&source_path),
        Some("javascript" | "jsx" | "typescript" | "tsx")
    );
    let policies = if javascript {
        resolution_policy(&root, &files)?
    } else {
        BTreeMap::new()
    };
    let source_inputs = inputs.clone();
    inputs.extend(policies.clone());
    let after = if !javascript {
        let mut workspace = native::Workspace::new(&root, files)?;
        workspace.refactor(&source_path, symbol, &destination_path, new_name.as_deref())?;
        inputs.extend(workspace.consulted());
        workspace.sources()
    } else {
        let mut workspace = javascript::Workspace::new(files)?;
        workspace.check_local_packages(&policies)?;
        if let Some(symbol) = symbol {
            let new_name = new_name.unwrap();
            ensure!(
                identifier(&new_name),
                "Invalid destination symbol name: {new_name}"
            );
            if symbol != new_name {
                workspace.rename(&source_path, symbol, &new_name)?;
            }
            if source_path != destination_path {
                workspace.move_symbol(&source_path, &new_name, &destination_path)?;
            }
            ensure!(
                symbol != new_name || source_path != destination_path,
                "Source and destination are identical"
            );
        } else {
            ensure!(
                source_path != destination_path,
                "Source and destination are identical"
            );
            ensure!(
                !root.join(&destination_path).try_exists()?,
                "Destination already exists: {destination_path}"
            );
            workspace.move_file(&source_path, &destination_path)?;
        }
        workspace.sources()
    };
    let mut permissions: BTreeMap<_, _> = files_permissions(&root, &inputs)?;
    if !permissions.contains_key(&destination_path) {
        permissions.insert(
            destination_path.clone(),
            fs::metadata(root.join(&source_path))?.permissions(),
        );
    }
    let mut result = Plan {
        root,
        inputs,
        permissions,
        changes: Vec::new(),
    };
    let paths: std::collections::BTreeSet<_> = source_inputs.keys().chain(after.keys()).collect();
    for path in paths {
        let before = source_inputs.get(path);
        let after = after.get(path);
        if before != after {
            checked_path(&result.root, Path::new(path))?;
            if before.is_none() {
                ensure!(
                    !result.root.join(path).try_exists()?,
                    "Destination already exists: {path}"
                );
                result
                    .permissions
                    .entry(path.clone())
                    .or_insert(fs::metadata(result.root.join(&source_path))?.permissions());
            }
            result.changes.push(FileChange {
                path: path.clone(),
                before: before.cloned(),
                after: after.cloned(),
            });
        }
    }
    Ok(result)
}

pub(crate) fn same_family(a: &str, b: &str) -> bool {
    let family = |path| match crate::parse::language_for_path(path) {
        Some("javascript" | "jsx" | "typescript" | "tsx") => Some("javascript"),
        language => language,
    };
    family(a) == family(b)
}

fn resolution_policy(
    root: &Path,
    files: &BTreeMap<String, (String, FileStructure)>,
) -> Result<BTreeMap<String, String>> {
    let mut paths = std::collections::BTreeSet::new();
    for path in files.keys() {
        let mut directory = Path::new(path).parent();
        while let Some(parent) = directory {
            for name in [
                "tsconfig.json",
                "tsconfig.jsonc",
                "jsconfig.json",
                "package.json",
            ] {
                paths.insert(parent.join(name));
            }
            directory = parent.parent();
        }
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("tsconfig") && (name.ends_with(".json") || name.ends_with(".jsonc")) {
            paths.insert(PathBuf::from(name.as_ref()));
        }
    }
    let custom = regex::Regex::new(
        r#""(?:paths|baseUrl|rootDirs|moduleSuffixes|extends|imports|exports|workspaces)"\s*:"#,
    )?;
    let mut result = BTreeMap::new();
    for path in paths {
        let absolute = root.join(&path);
        if !absolute.try_exists()? {
            continue;
        }
        checked_path(root, &path)?;
        let source = fs::read_to_string(&absolute)?;
        ensure!(
            !custom.is_match(&source),
            "Custom module resolution in {} is not yet supported by refactor (aliases, inherited configs, and package entrypoints/workspaces)",
            path.display()
        );
        result.insert(
            path.to_str()
                .context("Resolution policy path must be UTF-8")?
                .replace('\\', "/"),
            source,
        );
    }
    Ok(result)
}

fn files_permissions(
    root: &Path,
    files: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, fs::Permissions>> {
    files
        .keys()
        .map(|path| Ok((path.clone(), fs::metadata(root.join(path))?.permissions())))
        .collect()
}

fn selector(value: &str) -> Result<(&str, Option<&str>)> {
    if let Some((path, symbol)) = value.split_once("::") {
        ensure!(
            !path.is_empty() && !symbol.is_empty() && !symbol.contains("::"),
            "Expected file::symbol: {value}"
        );
        Ok((path, Some(symbol)))
    } else {
        ensure!(!value.is_empty(), "Expected a file path");
        Ok((value, None))
    }
}

fn identifier(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|c| c.is_alphabetic() || matches!(c, '_' | '$'))
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '$'))
}

fn argument_path(root: &Path, cwd: &Path, value: &str) -> Result<String> {
    // Inspect components before lexical `..` removal, otherwise alias/../file
    // could select a different file than normal filesystem traversal would.
    let value_path = Path::new(value);
    let mut probe = if value_path.is_absolute() {
        PathBuf::new()
    } else {
        cwd.to_owned()
    };
    let mut inside = probe.starts_with(root);
    for component in value_path.components() {
        probe.push(component);
        if inside && matches!(component, Component::Normal(_)) {
            match fs::symlink_metadata(&probe) {
                Ok(metadata) => ensure!(
                    !metadata.file_type().is_symlink(),
                    "Refactor does not follow symlinks: {}",
                    probe.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("Cannot inspect refactor path"),
            }
        }
        if probe.canonicalize().is_ok_and(|path| path == root) {
            inside = true;
        }
    }
    let path = lexical(&cwd.join(value));
    // Find the selected root by filesystem identity. This accepts ordinary and
    // verbatim Windows paths (and symlinked ancestors) without discarding any
    // symlinks *inside* the workspace before checked_path can reject them.
    let mut spelling = None;
    for ancestor in path.ancestors() {
        match ancestor.canonicalize() {
            Ok(identity) if identity == root => {
                spelling = Some(ancestor);
                break;
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error).context("Cannot resolve refactor path"),
        }
    }
    let relative =
        path.strip_prefix(spelling.context("Refactor paths must remain inside the repository")?)?;
    checked_path(root, relative)?;
    Ok(relative
        .to_str()
        .context("Refactor paths must be UTF-8")?
        .replace('\\', "/"))
}

fn checked_path(root: &Path, relative: &Path) -> Result<()> {
    let mut path = root.to_owned();
    for component in relative.components() {
        ensure!(
            matches!(component, Component::Normal(_)),
            "Invalid repository-relative refactor path"
        );
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "Refactor does not follow symlinks: {}",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot inspect {}", path.display()));
            }
        }
    }
    Ok(())
}

pub(super) fn lexical(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    output.push(component);
                }
            }
            _ => output.push(component),
        }
    }
    output
}

#[cfg(test)]
mod tests;
