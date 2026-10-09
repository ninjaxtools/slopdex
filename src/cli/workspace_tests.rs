use super::*;
use serde_json::json;

#[test]
fn source_paths_select_external_directories_without_changing_cwd() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("current");
    let other = temp.path().join("other");
    fs::create_dir(&root)?;
    fs::create_dir(&other)?;
    fs::write(root.join("local.rs"), "pub fn local() {}")?;
    fs::write(other.join("external.rs"), "pub fn external() {}")?;
    let canonical_root = root.canonicalize()?;
    let canonical_other = other.canonicalize()?;
    let cwd = std::env::current_dir()?;

    let local = source_workspace(&root, Path::new("../current/local.rs"))?;
    assert_eq!(local.root, root);
    assert_eq!(local.paths, [canonical_root.join("local.rs")]);
    assert!(local.directory.is_none());

    for path in [
        PathBuf::from("../other/external.rs"),
        other.join("external.rs"),
        other.clone(),
    ] {
        let external = source_workspace(&root, &path)?;
        assert_eq!(external.root, canonical_other);
        assert_eq!(
            external.directory.as_deref(),
            Some(canonical_other.as_path())
        );
        assert!(external.paths[0].starts_with(&canonical_other));
    }
    assert_eq!(std::env::current_dir()?, cwd);
    let groups = map_workspaces(
        &root,
        &[
            PathBuf::from("local.rs"),
            other.join("external.rs"),
            other.clone(),
        ],
    )?;
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].paths.len(), 1);
    assert_eq!(groups[1].paths.len(), 2);
    let missing = map_workspaces(&root, &[other.join("missing.rs")])?;
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].root, root);
    Ok(())
}

#[cfg(unix)]
#[test]
fn external_source_symlinks_select_the_destination_workspace() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("current");
    let other = temp.path().join("other");
    fs::create_dir(&root)?;
    fs::create_dir(&other)?;
    fs::write(other.join("external.rs"), "pub fn external() {}")?;
    std::os::unix::fs::symlink(&other, root.join("alias"))?;
    let external = source_workspace(&root, Path::new("alias/external.rs"))?;
    let other = other.canonicalize()?;
    assert_eq!(external.root, other);
    assert_eq!(external.paths, [other.join("external.rs")]);
    Ok(())
}

#[test]
fn default_index_uses_fresh_canonical_root_namespace() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("repo");
    fs::create_dir(&root)?;
    let canonical = root.canonicalize()?;
    let expected = cache::directory()?
        .join("worktrees-v1")
        .join(crate::hash(canonical.as_os_str().as_encoded_bytes()))
        .join("index.sqlite");
    assert_eq!(index_path(&root, None, &json!({}))?, expected);
    assert_eq!(index_path(&root.join("."), None, &json!({}))?, expected);
    #[cfg(unix)]
    {
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias)?;
        assert_eq!(index_path(&alias, None, &json!({}))?, expected);
    }
    let other = temp.path().join("other");
    fs::create_dir(&other)?;
    assert_ne!(index_path(&other, None, &json!({}))?, expected);
    Ok(())
}

#[test]
fn explicit_index_paths_keep_cwd_resolution_and_precedence() -> Result<()> {
    let temp = tempfile::tempdir()?;
    // Explicit paths do not require the selected root to exist.
    let root = temp.path().join("missing");
    let config = json!({"indexPath": "saved/index.sqlite"});
    let cwd = std::env::current_dir()?;
    assert_eq!(
        index_path(&root, None, &config)?,
        cwd.join("saved/index.sqlite")
    );
    assert_eq!(
        index_path(&root, Some(Path::new("explicit.sqlite")), &config)?,
        cwd.join("explicit.sqlite")
    );
    let explicit = temp.path().join("absolute.sqlite");
    assert_eq!(index_path(&root, Some(&explicit), &config)?, explicit);
    Ok(())
}

#[test]
fn identical_target_detection_handles_missing_paths_and_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    assert!(
        same_path(
            &temp.path().join(".slopdex/index.sqlite"),
            &temp.path().join("other/../.slopdex/index.sqlite")
        )
        .unwrap()
    );
    #[cfg(unix)]
    {
        let actual = temp.path().join("actual");
        fs::create_dir(&actual).unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        assert!(same_path(&actual.join("index.sqlite"), &alias.join("index.sqlite")).unwrap());
    }
}

#[cfg(unix)]
#[test]
fn path_identity_handles_hard_links_symlink_parents_and_resolution_failures() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("index.sqlite");
    let hard_link = temp.path().join("hard.sqlite");
    let different = temp.path().join("different.sqlite");
    fs::write(&original, b"same bytes").unwrap();
    fs::hard_link(&original, &hard_link).unwrap();
    fs::write(&different, b"same bytes").unwrap();
    assert!(same_path(&original, &hard_link).unwrap());
    assert!(!same_path(&original, &different).unwrap());
    let nested = temp.path().join("actual/nested");
    fs::create_dir_all(&nested).unwrap();
    let alias = temp.path().join("alias");
    symlink(&nested, &alias).unwrap();
    let via_parent = alias.join("../missing/index.sqlite");
    assert!(
        same_path(
            &via_parent,
            &temp.path().join("actual/missing/index.sqlite")
        )
        .unwrap()
    );
    assert!(!same_path(&via_parent, &temp.path().join("missing/index.sqlite")).unwrap());
    assert!(same_path(&original.join("child"), &different).is_err());
    let cycle = temp.path().join("cycle");
    symlink("cycle", &cycle).unwrap();
    assert!(same_path(&cycle, &original).is_err());
}
