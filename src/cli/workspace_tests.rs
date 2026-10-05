use super::*;

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
