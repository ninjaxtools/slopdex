use std::{
    collections::{HashMap, HashSet},
    path::Path,
    process::Command,
};

use anyhow::{Context, Result, ensure};

use crate::{parse, storage::Item};

fn text(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    output.status.success().then(|| {
        // Git adds a line terminator; other whitespace can belong to a path.
        String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .to_owned()
    })
}

fn bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "Git failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

pub fn head(root: &Path) -> Option<String> {
    text(root, &["rev-parse", "--verify", "HEAD"])
}

pub fn dirty_paths(root: &Path) -> Result<HashSet<String>> {
    if head(root).is_none() {
        return Ok(HashSet::new());
    }
    let mut paths = bytes(
        root,
        &["diff", "--relative", "--name-only", "-z", "HEAD", "--", "."],
    )?;
    paths.extend(bytes(
        root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?);
    paths
        .split(|b| *b == 0)
        .filter(|b| !b.is_empty())
        .map(|b| Ok(String::from_utf8(b.to_vec())?))
        .collect()
}

pub fn resolve_base(root: &Path, reference: &str, checkpoint: Option<&str>) -> Result<String> {
    let resolved = text(
        root,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
    )
    .context("Cannot resolve --changed-since commit")?;
    let head = checkpoint.context("--changed-since requires a Git checkpoint")?;
    bytes(root, &["merge-base", "--is-ancestor", &resolved, head])
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
                &["show", &format!("{resolved}:{prefix}{}", item.path)],
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
            repo.git(&["init", "--quiet", "--template="])?;
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
        assert!(dirty_paths(dir.path())?.is_empty());
        assert!(resolve_base(dir.path(), "HEAD", None).is_err());
        let repo = Repo::new()?;
        repo.write("staged.rs", "fn staged() {}")?;
        repo.git(&["add", "staged.rs"])?;
        repo.write("untracked.rs", "fn untracked() {}")?;
        // Engine assigns working-tree provenance to every file without HEAD.
        assert!(head(repo.root()).is_none());
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
            ["renamed.rs", "leaving.rs", "arrived.rs"]
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
}
