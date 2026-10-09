use super::*;
use serde_json::json;

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
