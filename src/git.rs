use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{parse, storage::Item};

/// Checkout identity, independent of repository-relatedness evidence.
/// A linked worktree has its own checkout/git directories but shares a common directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub checkout_root: PathBuf,
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
    /// Selected root relative to the canonical checkout root (empty for the whole checkout).
    pub scope: PathBuf,
    pub object_format: String,
    pub head: Option<String>,
    pub tree: Option<String>,
    pub shallow: bool,
    /// Optional persisted initialization evidence; cheap discovery leaves this unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage: Option<Lineage>,
}

/// Initialization-only hints for finding related independent clones.
/// Neither shared roots nor remotes establish that an indexed snapshot is valid.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lineage {
    pub object_format: String,
    pub head: Option<String>,
    pub shallow: bool,
    /// None for shallow/unborn repositories; shallow boundary commits are not history roots.
    pub history_roots: Option<Vec<String>>,
    /// Sorted, deduplicated hints without URL credentials, query strings, or fragments.
    pub remotes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changes {
    pub head: Option<String>,
    /// Current staged, unstaged, and untracked paths, including both ends of renames.
    pub dirty: HashSet<String>,
    /// Candidate invalidations: dirty paths plus the checkpoint-to-HEAD tree diff.
    pub changed: HashSet<String>,
    /// Git-visible ignore-file changes under the scope or in its checkout ancestors.
    /// Engine policy fingerprints still own external/global ignore inputs.
    pub policy_changed: bool,
}

fn output(root: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("Run Git")
}

fn text(root: &Path, args: &[&str]) -> Option<String> {
    let output = output(root, args).ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    // Strip exactly Git's line terminator, preserving newlines in directory names.
    Some(text.strip_suffix('\n').unwrap_or(&text).to_owned())
}

fn bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = output(root, args)?;
    ensure!(
        output.status.success(),
        "Git failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

pub fn head(root: &Path) -> Option<String> {
    resolve_commit(root, "HEAD")
}

pub(crate) fn excludes_file(root: &Path) -> Option<PathBuf> {
    text(root, &["config", "--path", "--get", "core.excludesFile"]).map(|path| {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    })
}

fn resolve_commit(root: &Path, reference: &str) -> Option<String> {
    text(
        root,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
    )
}

fn checkout(root: &Path) -> Result<Option<(PathBuf, PathBuf)>> {
    let Some(top) = text(
        root,
        &["rev-parse", "--path-format=absolute", "--show-toplevel"],
    ) else {
        return Ok(None);
    };
    let checkout = Path::new(&top).canonicalize()?;
    let selected = root.canonicalize()?;
    let scope = selected
        .strip_prefix(&checkout)
        .context("Selected root is outside the Git checkout")?
        .to_owned();
    Ok(Some((checkout, scope)))
}

/// Discover identity without walking history, listing files, or reading remotes.
/// Non-Git directories (including bare repositories) return None; unborn checkouts have no HEAD.
pub fn identity(root: &Path) -> Result<Option<Identity>> {
    let Some((checkout_root, scope)) = checkout(root)? else {
        return Ok(None);
    };
    let git_path = |flag| -> Result<PathBuf> {
        let path = text(root, &["rev-parse", "--path-format=absolute", flag])
            .context("Discover Git directory")?;
        Ok(Path::new(&path).canonicalize()?)
    };
    let head = head(root);
    let tree = head.as_deref().and_then(|head| {
        text(
            root,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{head}^{{tree}}"),
            ],
        )
    });
    Ok(Some(Identity {
        git_dir: git_path("--absolute-git-dir")?,
        common_dir: git_path("--git-common-dir")?,
        checkout_root,
        scope,
        object_format: text(root, &["rev-parse", "--show-object-format=storage"])
            .context("Discover Git object format")?,
        head,
        tree,
        shallow: text(root, &["rev-parse", "--is-shallow-repository"])
            .context("Discover shallow Git history")?
            == "true",
        lineage: None,
    }))
}

/// Compute relatedness evidence once at initialization; callers persist/cache the result.
/// HEAD is shared-commit evidence and complete-history roots can link diverged clones.
pub fn lineage(root: &Path) -> Result<Option<Lineage>> {
    let Some(identity) = identity(root)? else {
        return Ok(None);
    };
    let history_roots = if !identity.shallow {
        identity
            .head
            .as_deref()
            .map(|head| -> Result<Vec<String>> {
                let data = bytes(root, &["rev-list", "--max-parents=0", head, "--"])?;
                let mut roots: Vec<_> = String::from_utf8(data)?
                    .lines()
                    .map(str::to_owned)
                    .collect();
                roots.sort();
                roots.dedup();
                Ok(roots)
            })
            .transpose()?
    } else {
        None
    };
    let config = output(
        root,
        &["config", "--null", "--get-regexp", "^remote\\..*\\.url$"],
    )?;
    ensure!(
        config.status.success() || config.status.code() == Some(1),
        "Read Git remotes: {}",
        String::from_utf8_lossy(&config.stderr).trim()
    );
    let mut remotes = Vec::new();
    for record in config.stdout.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let record = std::str::from_utf8(record)?;
        if let Some((_, value)) = record.split_once('\n')
            && let Some(remote) = normalize_remote(&identity.checkout_root, value)
        {
            remotes.push(remote);
        }
    }
    remotes.sort();
    remotes.dedup();
    Ok(Some(Lineage {
        object_format: identity.object_format,
        head: identity.head,
        shallow: identity.shallow,
        history_roots,
        remotes,
    }))
}

fn normalize_remote(root: &Path, value: &str) -> Option<String> {
    let clean_path = |path: &str| {
        path.trim_end_matches('/')
            .trim_end_matches(".git")
            .to_owned()
    };
    if value.contains("://") {
        let url = reqwest::Url::parse(value).ok()?;
        if url.scheme() == "file" {
            let path = url.to_file_path().ok()?;
            return Some(clean_path(&path.to_string_lossy()));
        }
        let host = url.host_str()?.to_ascii_lowercase();
        let port = match url.port() {
            Some(22 | 80 | 443 | 9418) | None => String::new(),
            Some(port) => format!(":{port}"),
        };
        return Some(format!(
            "{host}{port}/{}",
            clean_path(url.path().trim_start_matches('/'))
        ));
    }
    if value.contains("::") {
        // Opaque remote helpers cannot be reliably sanitized.
        return None;
    }
    // SCP-style SSH URLs: drop userinfo and transport so SSH/HTTPS hints match.
    // Strip userinfo before splitting the host/path, including password-like colons.
    let scp = value
        .rsplit_once('@')
        .filter(|(userinfo, _)| !userinfo.contains(['/', '\\']))
        .map_or(value, |(_, remote)| remote);
    if let Some((authority, path)) = scp.split_once(':')
        && authority.len() > 1
        && !authority.contains(['/', '\\'])
    {
        let host = authority.rsplit('@').next()?.to_ascii_lowercase();
        let path = path.split(['?', '#']).next()?;
        return Some(format!(
            "{host}/{}",
            clean_path(path.trim_start_matches('/'))
        ));
    }
    if scp != value {
        return None;
    }
    let path = root.join(value);
    let path = path.canonicalize().unwrap_or(path);
    Some(clean_path(&path.to_string_lossy()))
}

fn insert_scoped(paths: &mut HashSet<String>, scope: &Path, bytes: &[u8]) -> Result<()> {
    let path = std::str::from_utf8(bytes).context("Git path is not UTF-8")?;
    if let Ok(relative) = Path::new(path).strip_prefix(scope)
        && !relative.as_os_str().is_empty()
        && relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        paths.insert(
            relative
                .to_str()
                .context("Git path is not UTF-8")?
                .to_owned(),
        );
    }
    Ok(())
}

fn is_policy_path(scope: &Path, bytes: &[u8]) -> Result<bool> {
    let path = Path::new(std::str::from_utf8(bytes).context("Git path is not UTF-8")?);
    let name = path.file_name().and_then(|name| name.to_str());
    let parent = path.parent().unwrap_or(Path::new(""));
    Ok(matches!(name, Some(".gitignore" | ".ignore" | ".rgignore"))
        && (parent.starts_with(scope) || scope.starts_with(parent)))
}

fn status_paths(checkout: &Path, scope: &Path) -> Result<(HashSet<String>, bool)> {
    let data = bytes(
        checkout,
        &[
            "-c",
            "status.relativePaths=false",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "--",
        ],
    )?;
    let mut records = data.split(|b| *b == 0).filter(|b| !b.is_empty());
    let mut paths = HashSet::new();
    let mut policy_changed = false;
    while let Some(record) = records.next() {
        ensure!(
            record.len() >= 4 && record[2] == b' ',
            "Invalid Git status record"
        );
        insert_scoped(&mut paths, scope, &record[3..])?;
        policy_changed |= is_policy_path(scope, &record[3..])?;
        if record[..2].iter().any(|b| matches!(b, b'R' | b'C')) {
            let original = records.next().context("Missing Git rename source")?;
            insert_scoped(&mut paths, scope, original)?;
            policy_changed |= is_policy_path(scope, original)?;
        }
    }
    Ok((paths, policy_changed))
}

/// Git's tracked/untracked candidates, relative to the selected root; apply engine policies too.
/// Unlike changes(), this supports initial scans of unborn checkouts.
pub fn tracked_untracked_paths(root: &Path) -> Result<Option<HashSet<String>>> {
    let Some((checkout, scope)) = checkout(root)? else {
        return Ok(None);
    };
    let data = bytes(
        &checkout,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )?;
    let mut paths = HashSet::new();
    for path in data.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        insert_scoped(&mut paths, &scope, path)?;
    }
    Ok(Some(paths))
}

/// None requests a full scan: non-Git/unborn checkout, no checkpoint, or missing prior commit.
/// No ancestry relationship is required (branch switches and unrelated histories are supported).
/// Callers must also revisit previously dirty paths so restoring a file clears cached edits.
pub fn changes(root: &Path, checkpoint: Option<&str>) -> Result<Option<Changes>> {
    let Some((checkout, scope)) = checkout(root)? else {
        return Ok(None);
    };
    let Some(current) = head(root) else {
        return Ok(None);
    };
    let Some(previous) = checkpoint.and_then(|reference| resolve_commit(root, reference)) else {
        return Ok(None);
    };
    let (dirty, mut policy_changed) = status_paths(&checkout, &scope)?;
    let mut changed = dirty.clone();
    if previous != current {
        let data = bytes(
            &checkout,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--name-only",
                "-z",
                &previous,
                &current,
                "--",
            ],
        )?;
        for path in data.split(|b| *b == 0).filter(|b| !b.is_empty()) {
            insert_scoped(&mut changed, &scope, path)?;
            policy_changed |= is_policy_path(&scope, path)?;
        }
    }
    Ok(Some(Changes {
        head: Some(current),
        dirty,
        changed,
        policy_changed,
    }))
}

pub fn dirty_paths(root: &Path) -> Result<HashSet<String>> {
    if head(root).is_none() {
        // Preserve the existing engine's unborn/non-Git provenance behavior.
        return Ok(HashSet::new());
    }
    let Some((checkout, scope)) = checkout(root)? else {
        return Ok(HashSet::new());
    };
    Ok(status_paths(&checkout, &scope)?.0)
}

pub fn resolve_base(root: &Path, reference: &str, checkpoint: Option<&str>) -> Result<String> {
    let resolved =
        resolve_commit(root, reference).context("Cannot resolve --changed-since commit")?;
    let checkpoint = checkpoint.context("--changed-since requires a Git checkpoint")?;
    let checkpoint = resolve_commit(root, checkpoint).context("Cannot resolve Git checkpoint")?;
    bytes(
        root,
        &["merge-base", "--is-ancestor", &resolved, &checkpoint],
    )
    .context("--changed-since must be an ancestor of the indexed commit")?;
    Ok(resolved)
}

pub fn changed_identities(root: &Path, resolved: &str, items: &[Item]) -> Result<HashSet<u64>> {
    let prefix = text(root, &["rev-parse", "--show-prefix"]).unwrap_or_default();
    let mut base = HashMap::<String, HashSet<(String, String)>>::new();
    let mut changed = HashSet::new();
    for item in items.iter().filter(|i| i.kind == "function") {
        if !base.contains_key(&item.path) {
            let source = bytes(
                root,
                &[
                    "show",
                    "--end-of-options",
                    &format!("{resolved}:{prefix}{}", item.path),
                ],
            )
            .ok()
            .and_then(|b| String::from_utf8(b).ok());
            let symbols = source
                .and_then(|s| parse::parse(&item.path, &s).ok())
                .map(|p| {
                    p.callables
                        .into_iter()
                        .map(|c| (c.qualified_name, c.source_hash))
                        .collect()
                })
                .unwrap_or_default();
            base.insert(item.path.clone(), symbols);
        }
        if !base[&item.path].contains(&(
            item.data["qualifiedName"].as_str().unwrap_or("").into(),
            item.data["sourceHash"].as_str().unwrap_or("").into(),
        )) {
            changed.insert(item.id);
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    struct Repo(tempfile::TempDir);

    impl Repo {
        fn new() -> Result<Self> {
            let repo = Self(tempfile::tempdir()?);
            repo.git(&["init", "--quiet", "--template=", "--object-format=sha1"])?;
            repo.git(&["config", "diff.renames", "true"])?;
            Ok(repo)
        }

        fn root(&self) -> &Path {
            self.0.path()
        }

        fn git(&self, args: &[&str]) -> Result<String> {
            let output = Command::new("git")
                .arg("-C")
                .arg(self.root())
                .args([
                    "-c",
                    "user.name=Storage Git Tests",
                    "-c",
                    "user.email=tests@example.invalid",
                    "-c",
                    "commit.gpgSign=false",
                    "-c",
                    "tag.gpgSign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .output()?;
            ensure!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(String::from_utf8(output.stdout)?.trim().to_owned())
        }

        fn write(&self, path: &str, source: &str) -> Result<()> {
            let path = self.root().join(path);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, source)?;
            Ok(())
        }

        fn commit(&self) -> Result<String> {
            self.git(&["add", "--all"])?;
            self.git(&["commit", "--quiet", "--allow-empty", "-m", "fixture"])?;
            Ok(head(self.root()).expect("committed HEAD"))
        }
    }

    fn item(id: u64, path: &str, source: &str) -> Result<Item> {
        let parsed = parse::parse(path, source)?;
        assert_eq!(parsed.callables.len(), 1);
        Ok(Item {
            id,
            path: path.into(),
            identity: format!("{path}:{id}"),
            kind: "function".into(),
            data: serde_json::to_value(&parsed.callables[0])?,
            embedding: "unused".into(),
            description_embedding: None,
        })
    }

    #[test]
    fn refs_peel_tags_reject_noncommits_and_require_checkpoint() -> Result<()> {
        let repo = Repo::new()?;
        repo.write("code.rs", "fn example() {}")?;
        let base = repo.commit()?;
        repo.git(&["tag", "lightweight"])?;
        repo.git(&["tag", "-a", "annotated", "-m", "tag"])?;
        let checkpoint = repo.commit()?;
        repo.git(&["checkout", "--quiet", "--detach", &base])?;
        assert_eq!(head(repo.root()).as_deref(), Some(base.as_str()));
        for reference in ["lightweight", "annotated", base.as_str()] {
            assert_eq!(
                resolve_base(repo.root(), reference, Some(&checkpoint))?,
                base
            );
        }
        // Indexed ancestry is authoritative even when live HEAD is older.
        assert_eq!(
            resolve_base(repo.root(), &checkpoint, Some(&checkpoint))?,
            checkpoint
        );
        for reference in ["HEAD:code.rs", "HEAD^{tree}", "--help", "missing"] {
            assert!(
                resolve_base(repo.root(), reference, Some(&checkpoint))
                    .unwrap_err()
                    .to_string()
                    .contains("Cannot resolve"),
                "{reference}"
            );
        }
        assert!(
            resolve_base(repo.root(), "annotated", None)
                .unwrap_err()
                .to_string()
                .contains("requires a Git checkpoint")
        );
        Ok(())
    }

    #[test]
    fn nongit_and_unborn_roots_have_no_checkpoint() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("code.rs"), "fn example() {}")?;
        assert!(head(dir.path()).is_none());
        assert!(identity(dir.path())?.is_none());
        assert!(lineage(dir.path())?.is_none());
        assert!(changes(dir.path(), None)?.is_none());
        assert!(tracked_untracked_paths(dir.path())?.is_none());
        assert!(dirty_paths(dir.path())?.is_empty());
        assert!(resolve_base(dir.path(), "HEAD", None).is_err());
        let repo = Repo::new()?;
        repo.write("staged.rs", "fn staged() {}")?;
        repo.git(&["add", "staged.rs"])?;
        repo.write("untracked.rs", "fn untracked() {}")?;
        // Engine assigns working-tree provenance to every file without HEAD.
        assert!(head(repo.root()).is_none());
        let identity = identity(repo.root())?.unwrap();
        assert!(identity.head.is_none());
        assert!(identity.tree.is_none());
        assert!(identity.lineage.is_none());
        assert!(lineage(repo.root())?.unwrap().history_roots.is_none());
        assert!(changes(repo.root(), Some("HEAD"))?.is_none());
        assert_eq!(
            tracked_untracked_paths(repo.root())?.unwrap(),
            HashSet::from(["staged.rs".into(), "untracked.rs".into()])
        );
        assert!(dirty_paths(repo.root())?.is_empty());
        assert!(resolve_base(repo.root(), "HEAD", None).is_err());
        repo.commit()?;
        assert!(head(repo.root()).is_some());
        assert!(dirty_paths(repo.root())?.is_empty());
        Ok(())
    }

    #[test]
    fn dirty_paths_preserve_nul_delimited_names_and_respect_ignores() -> Result<()> {
        let repo = Repo::new()?;
        let paths = [
            "space name.rs",
            #[cfg(unix)]
            "tab\tname.rs",
            #[cfg(unix)]
            "line\nbreak.rs",
            "é.rs",
            "-option.rs",
        ];
        for path in paths {
            repo.write(path, "fn before() {}")?;
        }
        repo.write(".gitignore", "ignored/\n")?;
        repo.commit()?;
        for path in paths {
            repo.write(path, "fn after() {}")?;
        }
        repo.git(&["add", "--", "space name.rs", "-option.rs"])?;
        fs::remove_file(repo.root().join("é.rs"))?;
        repo.write("new file.rs", "fn new() {}")?;
        repo.write("ignored/skip.rs", "fn ignored() {}")?;
        let expected = paths
            .into_iter()
            .chain(["new file.rs"])
            .map(str::to_owned)
            .collect();
        assert_eq!(dirty_paths(repo.root())?, expected);
        Ok(())
    }

    #[test]
    fn renames_across_subdirectory_boundary_keep_relative_dirty_paths() -> Result<()> {
        let repo = Repo::new()?;
        for (path, source) in [
            ("pkg/inside.rs", "fn inside() {}"),
            ("pkg/leaving.rs", "fn leaving() {}"),
            ("arriving.rs", "fn arriving() {}"),
            ("outside.rs", "fn outside() {}"),
        ] {
            repo.write(path, source)?;
        }
        repo.commit()?;
        repo.git(&["mv", "pkg/inside.rs", "pkg/renamed.rs"])?;
        repo.git(&["mv", "pkg/leaving.rs", "left.rs"])?;
        repo.git(&["mv", "arriving.rs", "pkg/arrived.rs"])?;
        repo.write("outside.rs", "fn changed_outside() {}")?;
        assert_eq!(
            dirty_paths(&repo.root().join("pkg"))?,
            ["inside.rs", "renamed.rs", "leaving.rs", "arrived.rs"]
                .map(str::to_owned)
                .into_iter()
                .collect()
        );
        repo.commit()?;
        assert!(dirty_paths(&repo.root().join("pkg"))?.is_empty());
        Ok(())
    }

    #[test]
    fn changed_identities_use_stored_content_and_path_not_live_checkout() -> Result<()> {
        let repo = Repo::new()?;
        let original = "fn same() -> i32 { 1 }";
        let edited = "fn same() -> i32 { 2 }";
        repo.write("a.rs", original)?;
        repo.write("b.rs", edited)?;
        let base = repo.commit()?;
        // Current checkout disagrees with both the base and indexed records.
        repo.write("a.rs", edited)?;
        fs::remove_file(repo.root().join("b.rs"))?;
        let mut markdown = item(5, "a.rs", edited)?;
        markdown.kind = "markdown".into();
        markdown.data = json!({"content": "not a callable"});
        let items = [
            item(1, "a.rs", &format!("\n\n{original}"))?,
            item(2, "a.rs", edited)?,
            item(3, "b.rs", edited)?,
            item(4, "renamed.rs", original)?,
            markdown,
        ];
        assert_eq!(
            changed_identities(repo.root(), &base, &items)?,
            HashSet::from([2, 4])
        );
        // Moving HEAD must not change comparison against the resolved base.
        repo.commit()?;
        assert_eq!(
            changed_identities(repo.root(), &base, &items)?,
            HashSet::from([2, 4])
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn changed_identities_preserve_whitespace_in_subdirectory_prefix() -> Result<()> {
        let repo = Repo::new()?;
        let source = "fn example() {}";
        repo.write(" \tpackage\n/code.rs", source)?;
        // A trimmed prefix would either read this decoy or a nonexistent path.
        repo.write("package\n/code.rs", "fn decoy() {}")?;
        let base = repo.commit()?;
        let root = repo.root().join(" \tpackage\n");
        let items = [item(1, "code.rs", source)?];
        assert!(changed_identities(&root, &base, &items)?.is_empty());
        assert!(dirty_paths(&root)?.is_empty());
        repo.write(" \tpackage\n/code.rs", "fn edited() {}")?;
        assert_eq!(dirty_paths(&root)?, HashSet::from(["code.rs".into()]));
        assert!(changed_identities(&root, &base, &items)?.is_empty());
        Ok(())
    }

    #[test]
    fn linked_worktrees_have_distinct_checkout_identity_and_shared_lineage() -> Result<()> {
        let repo = Repo::new()?;
        repo.write("pkg/code.rs", "fn example() {}")?;
        repo.commit()?;
        let temp = tempfile::tempdir()?;
        let linked = temp.path().join("linked checkout");
        repo.git(&[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ])?;
        let original = identity(repo.root())?.unwrap();
        let worktree = identity(&linked)?.unwrap();
        assert_ne!(original.checkout_root, worktree.checkout_root);
        assert_ne!(original.git_dir, worktree.git_dir);
        assert_eq!(original.common_dir, worktree.common_dir);
        assert_eq!(original.head, worktree.head);
        assert_eq!(original.tree, worktree.tree);
        assert_eq!(original.object_format, "sha1");
        assert!(!original.shallow);
        assert_eq!(lineage(repo.root())?, lineage(&linked)?);
        let nested = identity(&linked.join("pkg"))?.unwrap();
        assert_eq!(nested.scope, Path::new("pkg"));
        assert_eq!(nested.checkout_root, worktree.checkout_root);
        assert_eq!(nested.git_dir, worktree.git_dir);
        assert_eq!(
            serde_json::from_value::<Identity>(serde_json::to_value(&nested)?)?,
            nested
        );
        #[cfg(unix)]
        {
            let alias = temp.path().join("alias");
            std::os::unix::fs::symlink(&linked, &alias)?;
            assert_eq!(identity(&alias.join("pkg"))?, Some(nested));
        }
        Ok(())
    }

    #[test]
    fn clone_lineage_distinguishes_complete_roots_from_shallow_boundaries() -> Result<()> {
        let repo = Repo::new()?;
        repo.write("code.rs", "fn first() {}")?;
        let first = repo.commit()?;
        repo.write("code.rs", "fn second() {}")?;
        let second = repo.commit()?;
        let temp = tempfile::tempdir()?;
        let full = temp.path().join("full");
        let shallow = temp.path().join("shallow");
        let source = reqwest::Url::from_directory_path(repo.root())
            .unwrap()
            .to_string();
        repo.git(&["clone", "--quiet", &source, full.to_str().unwrap()])?;
        repo.git(&[
            "clone",
            "--quiet",
            "--depth=1",
            &source,
            shallow.to_str().unwrap(),
        ])?;
        let original = lineage(repo.root())?.unwrap();
        let complete = lineage(&full)?.unwrap();
        let partial = lineage(&shallow)?.unwrap();
        assert_eq!(original.history_roots, Some(vec![first.clone()]));
        assert_eq!(complete.history_roots, original.history_roots);
        assert_eq!(complete.head, Some(second.clone()));
        assert_eq!(partial.head, complete.head);
        assert_eq!(partial.object_format, complete.object_format);
        assert!(partial.shallow);
        assert!(partial.history_roots.is_none());
        assert_eq!(partial.remotes, complete.remotes);
        assert_ne!(
            identity(repo.root())?.unwrap().common_dir,
            identity(&full)?.unwrap().common_dir
        );
        assert!(changes(&shallow, Some(&first))?.is_none());
        assert!(changes(&shallow, Some(&second))?.is_some());
        // Removing the shallow boundary makes roots available only when evidence is recomputed.
        let result = output(&shallow, &["fetch", "--quiet", "--unshallow"])?;
        ensure!(result.status.success(), "fetch failed");
        assert_eq!(
            lineage(&shallow)?.unwrap().history_roots,
            Some(vec![first.clone()])
        );
        // Independent divergent clones share relatedness hints, not snapshot identity.
        fs::write(full.join("code.rs"), "fn divergent() {}")?;
        let full_path = full.to_str().unwrap();
        repo.git(&["-C", full_path, "add", "--all"])?;
        repo.git(&[
            "-C",
            full_path,
            "commit",
            "--quiet",
            "-m",
            "divergent clone",
        ])?;
        let divergent = lineage(&full)?.unwrap();
        assert_ne!(divergent.head, original.head);
        assert_eq!(divergent.history_roots, Some(vec![first]));
        Ok(())
    }

    #[test]
    fn lineage_keeps_all_complete_history_roots() -> Result<()> {
        let repo = Repo::new()?;
        repo.write("first.rs", "first")?;
        let first = repo.commit()?;
        repo.git(&["checkout", "--quiet", "--orphan", "second-root"])?;
        repo.git(&["rm", "--quiet", "-rf", "."])?;
        repo.write("second.rs", "second")?;
        let second = repo.commit()?;
        repo.git(&[
            "merge",
            "--quiet",
            "--allow-unrelated-histories",
            "--no-edit",
            &first,
        ])?;
        let mut roots = vec![first, second];
        roots.sort();
        assert_eq!(lineage(repo.root())?.unwrap().history_roots, Some(roots));
        Ok(())
    }

    #[test]
    fn identity_supports_sha256_object_format() -> Result<()> {
        let repo = Repo(tempfile::tempdir()?);
        repo.git(&["init", "--quiet", "--template=", "--object-format=sha256"])?;
        repo.write("code.rs", "fn example() {}")?;
        let checkpoint = repo.commit()?;
        let identity = identity(repo.root())?.unwrap();
        assert_eq!(identity.object_format, "sha256");
        assert_eq!(identity.head.as_ref().unwrap().len(), 64);
        assert_eq!(identity.tree.as_ref().unwrap().len(), 64);
        assert_eq!(
            lineage(repo.root())?.unwrap().history_roots,
            Some(vec![checkpoint.clone()])
        );
        repo.write("code.rs", "fn edited() {}")?;
        assert_eq!(
            changes(repo.root(), Some(&checkpoint))?.unwrap().changed,
            HashSet::from(["code.rs".into()])
        );
        Ok(())
    }

    #[test]
    fn lineage_sanitizes_and_normalizes_remote_hints() -> Result<()> {
        let repo = Repo::new()?;
        repo.commit()?;
        repo.git(&[
            "remote",
            "add",
            "https",
            "https://user:secret@EXAMPLE.com/org/repo.git?token=private#secret",
        ])?;
        repo.git(&["remote", "add", "ssh", "git@example.com:org/repo.git"])?;
        repo.git(&[
            "remote",
            "add",
            "scp-credentials",
            "user:password@example.com:org/repo.git",
        ])?;
        repo.git(&[
            "remote",
            "add",
            "ssh-url",
            "ssh://user:password@example.com:22/org/repo.git",
        ])?;
        repo.git(&[
            "remote",
            "add",
            "custom-port",
            "https://user:secret@example.com:8443/org/other.git",
        ])?;
        repo.git(&[
            "remote",
            "add",
            "helper",
            "helper::user:secret@example.com/repo",
        ])?;
        let evidence = lineage(repo.root())?.unwrap();
        assert_eq!(
            evidence.remotes,
            vec!["example.com/org/repo", "example.com:8443/org/other"]
        );
        let serialized = serde_json::to_string(&evidence)?;
        for secret in ["secret", "private", "password", "user:"] {
            assert!(!serialized.contains(secret));
        }
        let mut identity = identity(repo.root())?.unwrap();
        identity.lineage = Some(evidence);
        assert_eq!(
            serde_json::from_str::<Identity>(&serde_json::to_string(&identity)?)?,
            identity
        );
        Ok(())
    }

    #[test]
    fn changes_include_index_worktree_untracked_deletions_and_restore() -> Result<()> {
        let repo = Repo::new()?;
        for name in [
            "staged.rs",
            "unstaged.rs",
            "both.rs",
            "deleted.rs",
            "renamed.rs",
        ] {
            repo.write(name, &format!("fn {}() {{}}", name.trim_end_matches(".rs")))?;
        }
        repo.write(".gitignore", "ignored/\n")?;
        let checkpoint = repo.commit()?;
        assert!(
            changes(repo.root(), Some(&checkpoint))?
                .unwrap()
                .changed
                .is_empty()
        );
        repo.write("staged.rs", "fn staged_changed() {}")?;
        repo.write("both.rs", "fn staged_both() {}")?;
        repo.git(&["add", "staged.rs", "both.rs"])?;
        repo.write("both.rs", "fn unstaged_both() {}")?;
        repo.write("unstaged.rs", "fn unstaged_changed() {}")?;
        repo.git(&["rm", "--quiet", "deleted.rs"])?;
        repo.git(&["mv", "renamed.rs", "new name.rs"])?;
        repo.write("new.rs", "fn untracked() {}")?;
        repo.write("ignored/skip.rs", "fn ignored() {}")?;
        let result = changes(repo.root(), Some(&checkpoint))?.unwrap();
        assert_eq!(result.head.as_deref(), Some(checkpoint.as_str()));
        assert_eq!(
            result.dirty,
            HashSet::from(
                [
                    "staged.rs",
                    "unstaged.rs",
                    "both.rs",
                    "deleted.rs",
                    "renamed.rs",
                    "new name.rs",
                    "new.rs"
                ]
                .map(str::to_owned)
            )
        );
        assert_eq!(result.changed, result.dirty);
        repo.git(&["reset", "--quiet", "--hard", &checkpoint])?;
        fs::remove_file(repo.root().join("new.rs"))?;
        let restored = changes(repo.root(), Some(&checkpoint))?.unwrap();
        assert!(restored.dirty.is_empty());
        assert!(restored.changed.is_empty());
        Ok(())
    }

    #[test]
    fn changes_diff_branch_switches_without_requiring_ancestry() -> Result<()> {
        let repo = Repo::new()?;
        repo.write("pkg/stable.rs", "fn stable() {}")?;
        let base = repo.commit()?;
        repo.write("pkg/left.rs", "fn left() {}")?;
        let left = repo.commit()?;
        repo.git(&["checkout", "--quiet", "--detach", &base])?;
        repo.write("pkg/right.rs", "fn right() {}")?;
        repo.write("outside.rs", "fn outside() {}")?;
        let right = repo.commit()?;
        let result = changes(&repo.root().join("pkg"), Some(&left))?.unwrap();
        assert_eq!(result.head, Some(right.clone()));
        assert!(result.dirty.is_empty());
        assert_eq!(
            result.changed,
            HashSet::from(["left.rs".into(), "right.rs".into()])
        );
        assert!(changes(repo.root(), None)?.is_none());
        for missing in ["missing", "--help", "HEAD^{tree}", "HEAD:outside.rs"] {
            assert!(changes(repo.root(), Some(missing))?.is_none(), "{missing}");
        }
        repo.git(&["checkout", "--quiet", "--orphan", "unrelated"])?;
        repo.git(&["rm", "--quiet", "-rf", "."])?;
        repo.write("pkg/unrelated.rs", "fn unrelated() {}")?;
        let unrelated = repo.commit()?;
        let result = changes(&repo.root().join("pkg"), Some(&right))?.unwrap();
        assert_eq!(result.head, Some(unrelated));
        assert_eq!(
            result.changed,
            HashSet::from(["stable.rs".into(), "right.rs".into(), "unrelated.rs".into()])
        );
        Ok(())
    }

    #[test]
    fn changes_scope_both_ends_of_dirty_and_committed_renames() -> Result<()> {
        let repo = Repo::new()?;
        for path in [
            "pkg/inside.rs",
            "pkg/leaving.rs",
            "arriving.rs",
            "outside.rs",
        ] {
            repo.write(path, &format!("// {path}"))?;
        }
        let checkpoint = repo.commit()?;
        repo.git(&["mv", "pkg/inside.rs", "pkg/renamed.rs"])?;
        repo.git(&["mv", "pkg/leaving.rs", "left.rs"])?;
        repo.git(&["mv", "arriving.rs", "pkg/arrived.rs"])?;
        repo.write("outside.rs", "changed")?;
        repo.write("pkg/new.rs", "new")?;
        let root = repo.root().join("pkg");
        let expected = HashSet::from(
            [
                "inside.rs",
                "renamed.rs",
                "leaving.rs",
                "arrived.rs",
                "new.rs",
            ]
            .map(str::to_owned),
        );
        let dirty = changes(&root, Some(&checkpoint))?.unwrap();
        assert_eq!(dirty.dirty, expected);
        assert_eq!(dirty.changed, expected);
        repo.commit()?;
        let committed = changes(&root, Some(&checkpoint))?.unwrap();
        assert!(committed.dirty.is_empty());
        assert_eq!(committed.changed, expected);
        assert!(
            committed
                .changed
                .iter()
                .all(|path| !path.starts_with("../"))
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn changes_preserve_weird_names_in_status_diff_and_selected_root() -> Result<()> {
        let repo = Repo::new()?;
        let scope = " \tpackage\n";
        let names = [
            "space name.rs",
            "tab\tname.rs",
            "line\nbreak.rs",
            "é.rs",
            "-option.rs",
            "quote\"slash\\.rs",
        ];
        for name in names {
            repo.write(&format!("{scope}/{name}"), "original")?;
        }
        let checkpoint = repo.commit()?;
        for name in names {
            repo.write(&format!("{scope}/{name}"), "edited")?;
        }
        let old = format!("{scope}/line\nbreak.rs");
        let new = format!("{scope}/renamed\n\t\".rs");
        repo.git(&["mv", "--", &old, &new])?;
        repo.write(&format!("{scope}/new\nfile.rs"), "untracked")?;
        let root = repo.root().join(scope);
        let expected: HashSet<_> = names
            .into_iter()
            .chain(["renamed\n\t\".rs", "new\nfile.rs"])
            .map(str::to_owned)
            .collect();
        let dirty = changes(&root, Some(&checkpoint))?.unwrap();
        assert_eq!(dirty.dirty, expected);
        assert_eq!(dirty.changed, expected);
        assert_eq!(identity(&root)?.unwrap().scope, Path::new(scope));
        repo.commit()?;
        let committed = changes(&root, Some(&checkpoint))?.unwrap();
        assert!(committed.dirty.is_empty());
        assert_eq!(committed.changed, expected);
        Ok(())
    }

    #[test]
    fn initial_candidates_respect_git_ignores_and_selected_scope() -> Result<()> {
        let repo = Repo::new()?;
        repo.write(".gitignore", "ignored/\n")?;
        repo.write("pkg/tracked.rs", "tracked")?;
        repo.write("outside.rs", "outside")?;
        repo.commit()?;
        fs::remove_file(repo.root().join("pkg/tracked.rs"))?;
        repo.write("pkg/untracked.rs", "untracked")?;
        repo.write("pkg/ignored/skip.rs", "ignored")?;
        assert_eq!(
            tracked_untracked_paths(&repo.root().join("pkg"))?.unwrap(),
            HashSet::from(["tracked.rs".into(), "untracked.rs".into()])
        );
        Ok(())
    }

    #[test]
    fn changes_report_scoped_and_ancestor_ignore_policy_changes() -> Result<()> {
        let repo = Repo::new()?;
        repo.write("pkg/code.rs", "code")?;
        repo.write(".gitignore", "ignored/\n")?;
        repo.write("other/.gitignore", "ignored/\n")?;
        let checkpoint = repo.commit()?;
        let root = repo.root().join("pkg");
        repo.write("other/.gitignore", "different/\n")?;
        assert!(!changes(&root, Some(&checkpoint))?.unwrap().policy_changed);
        repo.write(".gitignore", "different/\n")?;
        let ancestor = changes(&root, Some(&checkpoint))?.unwrap();
        assert!(ancestor.policy_changed);
        assert!(ancestor.changed.is_empty());
        assert!(ancestor.dirty.is_empty());
        repo.commit()?;
        let committed = changes(&root, Some(&checkpoint))?.unwrap();
        assert!(committed.policy_changed);
        assert!(committed.changed.is_empty());
        let latest = head(repo.root()).unwrap();
        repo.write("pkg/.ignore", "*.rs\n")?;
        let scoped = changes(&root, Some(&latest))?.unwrap();
        assert!(scoped.policy_changed);
        assert_eq!(scoped.changed, HashSet::from([".ignore".into()]));
        Ok(())
    }
}
