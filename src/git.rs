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
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
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
