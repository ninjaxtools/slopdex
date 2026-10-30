use super::discovery::*;
use super::*;
#[cfg(unix)]
use std::os::unix::fs::symlink;

fn git_command(root: &Path, args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn git_fixture() -> Result<(tempfile::TempDir, Engine)> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("repo");
    fs::create_dir(&root)?;
    git_command(&root, &["init", "-q"])?;
    fs::write(root.join("source.rs"), "pub fn first() {}\n")?;
    git_command(&root, &["add", "."])?;
    git_command(&root, &["commit", "-qm", "initial"])?;
    let engine = Engine::open_map(
        &root,
        &temp.path().join("index.sqlite"),
        json!({"artifactCachePath":temp.path().join("global.sqlite")}),
    )?;
    Ok((temp, engine))
}

#[test]
fn lock_file_patterns_cover_dependency_ecosystems_without_hiding_manifests() -> Result<()> {
    let locks = globs(&json!(EXCLUDED_LOCK_FILES))?;
    for name in [
        "package-lock.json",
        "npm-shrinkwrap.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "pnpm-lock.yml",
        "shrinkwrap.yaml",
        "bun.lock",
        "bun.lockb",
        "deno.lock",
        "lazy-lock.json",
        "esy.lock/index.json",
        "Cargo.lock",
        "Pipfile.lock",
        "poetry.lock",
        "pdm.lock",
        "uv.lock",
        "pylock.toml",
        "pylock.dev.toml",
        "requirements.lock",
        "Gemfile.lock",
        "gems.locked",
        "composer.lock",
        "go.sum",
        "go.work.sum",
        "Gopkg.lock",
        "glide.lock",
        "mix.lock",
        "pubspec.lock",
        "packages.lock.json",
        "project.lock.json",
        "paket.lock",
        "packages-lock.json",
        "Package.resolved",
        "Podfile.lock",
        "Cartfile.resolved",
        "gradle.lockfile",
        "compileClasspath.lockfile",
        ".terraform.lock.hcl",
        "flake.lock",
        "conan.lock",
        "vcpkg-lock.json",
        "manifest.toml",
        "Manifest.toml",
        "Manifest-v1.11.toml",
        "JuliaManifest.toml",
        "JuliaManifest-v1.11.toml",
        "renv.lock",
        "packrat.lock",
        "cpanfile.snapshot",
        "cabal.project.freeze",
        "cabal.project.local.freeze",
        "stack.yaml.lock",
        "example.opam.locked",
        "dub.selections.json",
        "Berksfile.lock",
        "Puppetfile.lock",
        "custom.lock.yaml",
        "custom.lock.yml",
        ".meteor/versions",
    ] {
        for prefix in ["", "nested/workspace/"] {
            let path = format!("{prefix}{name}");
            assert!(locks.is_match(&path), "lock file was not excluded: {path}");
        }
    }
    for path in [
        "package.json",
        "nested/Cargo.toml",
        "pyproject.toml",
        "go.mod",
        "Project.toml",
        "build.zig.zon",
        "src/lock.rs",
        "src/lock.json",
        "package-lock.json.md",
        "lockfile.py",
        "Manifest.toml.md",
    ] {
        assert!(!locks.is_match(path), "non-lock file was excluded: {path}");
    }
    Ok(())
}

#[test]
fn discovery_excludes_lock_files_even_with_explicit_includes_and_candidates() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("repo");
    let mut candidates = HashSet::new();
    for prefix in ["", "nested/workspace/"] {
        for name in [
            "package-lock.json",
            "npm-shrinkwrap.json",
            "lazy-lock.json",
            "pnpm-lock.yaml",
            "packages.lock.json",
            "pylock.toml",
            "pylock.dev.toml",
            ".terraform.lock.hcl",
            "Manifest.toml",
            "manifest.toml",
            "Manifest-v1.11.toml",
            "dub.selections.json",
            "package.json",
            "Cargo.toml",
            "pyproject.toml",
            "lock.rs",
        ] {
            let relative = format!("{prefix}{name}");
            let path = root.join(&relative);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, "")?;
            candidates.insert(relative);
        }
    }
    // Only files are excluded; a directory with a lock-file name is still walked.
    fs::create_dir_all(root.join("workspace/package-lock.json"))?;
    fs::write(root.join("workspace/package-lock.json/source.rs"), "")?;
    candidates.insert("workspace/package-lock.json/source.rs".to_owned());
    let expected = [
        "Cargo.toml",
        "lock.rs",
        "nested/workspace/Cargo.toml",
        "nested/workspace/lock.rs",
        "nested/workspace/package.json",
        "nested/workspace/pyproject.toml",
        "package.json",
        "pyproject.toml",
        "workspace/package-lock.json/source.rs",
    ];
    for include in [json!([]), json!(["**"])] {
        let config = json!({
            "include":include,
            "artifactCachePath":temp.path().join("global.sqlite"),
        });
        for subset in [None, Some(&candidates)] {
            assert_eq!(
                source_paths_subset(
                    &root,
                    &temp.path().join("index.sqlite"),
                    &config,
                    subset,
                    None,
                    false,
                )?,
                expected
            );
        }
    }
    Ok(())
}

#[test]
fn lock_file_policy_change_removes_legacy_entries_from_clean_git_index() -> Result<()> {
    let (_temp, mut engine) = git_fixture()?;
    let path = "package-lock.json";
    let source = "{\"lockfileVersion\":3}";
    fs::write(engine.root.join(path), source)?;
    git_command(&engine.root, &["add", "."])?;
    git_command(&engine.root, &["commit", "-qm", "lock file"])?;
    engine.refresh_structure()?;
    assert_eq!(engine.files.len(), 1);

    // Seed the entry and policies an older version would have published.
    let mut file = engine.files["source.rs"].clone();
    file.path = path.to_owned();
    file.source = source.to_owned();
    file.hash = hash(source);
    file.language = "json".to_owned();
    engine.db.apply_structure(
        &[(file, parse::parse(path, source)?)],
        &[],
        git::head(&engine.root).as_deref(),
    )?;
    let mut legacy: Value = serde_json::from_str(&selection_contract(&engine.config))?;
    assert_eq!(
        legacy.as_array_mut().unwrap().pop(),
        Some(json!(EXCLUDED_LOCK_FILES))
    );
    let legacy = legacy.to_string();
    engine.db.set_meta("selection_policy", &legacy)?;
    let mut policy: Value = serde_json::from_str(&engine.db.meta("discovery_policy")?.unwrap())?;
    policy[0] = json!(legacy);
    engine
        .db
        .set_meta("discovery_policy", &policy.to_string())?;
    engine.load()?;
    assert!(engine.files.contains_key(path));
    assert!(git::dirty_paths(&engine.root)?.is_empty());

    assert_eq!(engine.refresh_structure()?["filesDeleted"], 1);
    assert_eq!(engine.db.paths()?, ["source.rs"]);
    assert!(!engine.files.contains_key(path));
    assert_eq!(engine.refresh_structure()?["filesDeleted"], 0);
    Ok(())
}

#[test]
fn source_paths_subset_normalizes_nested_git_candidates() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("repo");
    for path in [
        "nested/new.rs",
        "nested/deeper/source.rs",
        "nested/unselected.rs",
        "other/unselected.rs",
    ] {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, "fn example() {}")?;
    }
    let candidates = HashSet::from([
        "nested/new.rs".to_owned(),
        "nested/deeper/source.rs".to_owned(),
    ]);
    let paths = source_paths_subset(
        &root,
        &temp.path().join("index.sqlite"),
        &json!({"artifactCachePath":temp.path().join("global.sqlite")}),
        Some(&candidates),
        None,
        false,
    )?;
    assert_eq!(paths, ["nested/deeper/source.rs", "nested/new.rs"]);
    Ok(())
}

#[test]
fn refresh_rechecks_dirty_provenance_before_publication_and_restore() -> Result<()> {
    let (_temp, mut engine) = git_fixture()?;
    engine.refresh_structure()?;
    let old_checkpoint = engine.db.meta("checkpoint")?;
    let path = engine.root.join("source.rs");
    let committed = "pub fn second() {}\n";
    fs::write(&path, committed)?;
    git_command(&engine.root, &["commit", "-qam", "second"])?;
    let error = engine
        .refresh_structure_with(|| {
            fs::write(&path, "pub fn concurrent_edit() {}\n")?;
            Ok(())
        })
        .unwrap_err();
    assert!(
        error.to_string().contains("working tree changed"),
        "{error}"
    );
    assert_eq!(engine.db.meta("checkpoint")?, old_checkpoint);
    assert_eq!(engine.source("source.rs")?, "pub fn first() {}\n");
    fs::write(&path, committed)?;
    engine.refresh_structure()?;
    assert_eq!(engine.source("source.rs")?, committed);
    assert!(git::dirty_paths(&engine.root)?.is_empty());
    assert_eq!(engine.refresh_structure()?["filesUpdated"], 0);
    Ok(())
}

#[test]
fn ignored_nested_policy_files_invalidate_clean_git_snapshots() -> Result<()> {
    let (_temp, mut engine) = git_fixture()?;
    fs::create_dir(engine.root.join("nested"))?;
    fs::write(engine.root.join(".gitignore"), "*.ignore\n")?;
    fs::write(engine.root.join("nested/.ignore"), "")?;
    fs::write(engine.root.join("nested/kept.rs"), "pub fn kept() {}\n")?;
    git_command(&engine.root, &["add", "."])?;
    git_command(&engine.root, &["commit", "-qm", "nested"])?;
    engine.refresh_structure()?;
    assert!(engine.files.contains_key("nested/kept.rs"));
    for policy in ["*.rs\n", ""] {
        fs::write(engine.root.join("nested/.ignore"), policy)?;
        assert!(git::dirty_paths(&engine.root)?.is_empty());
        engine.refresh_structure()?;
        assert_eq!(
            engine.files.contains_key("nested/kept.rs"),
            policy.is_empty()
        );
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 0);
    }
    Ok(())
}

#[test]
fn in_tree_artifact_writes_do_not_invalidate_git_discovery() -> Result<()> {
    let (_temp, engine) = git_fixture()?;
    let root = engine.root.clone();
    drop(engine);
    let mut engine = Engine::open_map(
        &root,
        &root.join("index.sqlite"),
        json!({"artifactCachePath":root.join("global.sqlite")}),
    )?;
    engine.refresh_structure()?;
    assert_eq!(engine.files.len(), 1);
    assert!(
        !git::dirty_paths(&root)?.is_empty(),
        "positive control: generated files are untracked"
    );
    let before = engine.db.meta("snapshot")?;
    assert_eq!(engine.refresh_structure()?["filesUpdated"], 0);
    assert_eq!(engine.db.meta("snapshot")?, before);
    let dirty: HashSet<String> = serde_json::from_str(&engine.db.meta("dirty_paths")?.unwrap())?;
    assert!(dirty.is_empty());
    Ok(())
}

#[test]
fn writable_engines_share_reads_and_scope_exclusivity_to_writes() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("repo");
    fs::create_dir(&root)?;
    fs::write(root.join("source.rs"), "pub fn indexed() {}\n")?;
    let index = temp.path().join("index.sqlite");
    let config = json!({"artifactCachePath":temp.path().join("global.sqlite")});
    let mut first = Engine::open_map(&root, &index, config.clone())?;
    first.refresh_structure()?;
    let second = Engine::open_map(&root, &index, config)?;
    assert_eq!(first.status()?, second.status()?);

    let probe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(temp.path().join("index.sqlite.lock"))?;
    FileExt::try_lock_shared(&probe)?;
    FileExt::unlock(&probe)?;
    assert!(FileExt::try_lock_exclusive(&probe).is_err());
    first
        .try_write_index(|| anyhow::bail!("optional write must be skipped with another reader"))?;
    drop(second);

    first.write_index(|engine| {
        assert!(FileExt::try_lock_shared(&probe).is_err());
        assert!(FileExt::try_lock_exclusive(&probe).is_err());
        engine.db.set_meta("write-test", "published")
    })?;
    FileExt::try_lock_shared(&probe)?;
    FileExt::unlock(&probe)?;
    assert!(FileExt::try_lock_exclusive(&probe).is_err());

    let stale = first.workspace_stamp()?;
    first.write_index(|engine| engine.db.set_meta("write-test", "newer publication"))?;
    let guard = first.lock.write()?;
    let rejected = first.finish_index_write::<()>(stale, guard, || {
        anyhow::bail!("stale prepared work must not run")
    });
    assert!(
        rejected
            .unwrap_err()
            .to_string()
            .contains("Index changed while waiting to write")
    );

    let failed = first.write_index::<()>(|_| anyhow::bail!("write failed"));
    assert_eq!(failed.unwrap_err().to_string(), "write failed");
    FileExt::try_lock_shared(&probe)?;
    FileExt::unlock(&probe)?;
    assert!(FileExt::try_lock_exclusive(&probe).is_err());
    drop(first);
    FileExt::try_lock_exclusive(&probe)?;
    FileExt::unlock(&probe)?;
    Ok(())
}

#[test]
fn concurrent_cold_engine_opens_do_not_wait_for_initialized_readers() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("repo");
    fs::create_dir(&root)?;
    let index = temp.path().join("index.sqlite");
    let config = json!({"artifactCachePath":temp.path().join("global.sqlite")});
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let (tx, rx) = std::sync::mpsc::channel();
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let (root, index, config) = (root.clone(), index.clone(), config.clone());
            let (barrier, tx) = (barrier.clone(), tx.clone());
            std::thread::spawn(move || {
                barrier.wait();
                let result = Engine::open_map(&root, &index, config);
                tx.send(result).expect("engine opener receiver");
            })
        })
        .collect();
    drop(tx);
    // Keep every successful engine open while its peers finish initialization.
    let results: Vec<_> = (0..4)
        .map(|_| rx.recv_timeout(Duration::from_secs(15)))
        .collect();
    for worker in workers {
        worker.join().expect("engine opener");
    }
    let engines = results
        .into_iter()
        .map(|result| result.context("cold engine open stalled")?)
        .collect::<Result<Vec<_>>>()?;
    for engine in &engines {
        assert_eq!(
            engine.db.meta("incarnation")?,
            engines[0].db.meta("incarnation")?
        );
    }
    Ok(())
}

#[test]
fn contended_writer_times_out_but_readers_can_share() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("index.lock");
    let first = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)?;
    let second = OpenOptions::new().read(true).write(true).open(&path)?;
    let writer = OpenOptions::new().read(true).write(true).open(&path)?;
    lock_index(&first, true)?;
    lock_index(&second, true)?;
    let error = lock_index_with_timeout(&writer, false, Duration::from_millis(60))
        .expect_err("writer must wait for other readers");
    assert!(error.to_string().contains("Index is in use"));
    drop(first);
    drop(second);
    lock_index(&writer, false)?;
    let reader = OpenOptions::new().read(true).write(true).open(&path)?;
    let error = lock_index_with_timeout(&reader, true, Duration::from_millis(60))
        .expect_err("reader must wait for a writer");
    assert!(error.to_string().contains("Index is in use"));
    drop(writer);
    lock_index(&reader, true)?;
    Ok(())
}

#[test]
#[cfg(unix)]
fn normalize_absolute_source_paths_through_symlinked_root() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("repo");
    let alias = temp.path().join("alias");
    let outside = temp.path().join("outside");
    fs::create_dir_all(root.join("src"))?;
    fs::write(root.join("src/source.rs"), "fn source() {}")?;
    fs::create_dir(&outside)?;
    symlink(&root, &alias)?;
    symlink(&outside, root.join("escape"))?;
    let engine = Engine::open(
        &alias,
        &temp.path().join("index.sqlite"),
        json!({"artifactCachePath":temp.path().join("global.sqlite")}),
    )?;

    // Both spellings must match the canonical root stored by Engine::open,
    // including when a system temp directory itself is a symlink on macOS.
    for base in [alias, root.canonicalize()?] {
        for suffix in ["", "src", "src/source.rs"] {
            assert_eq!(
                engine.normalize_path(base.join(suffix).to_str().unwrap())?,
                suffix
            );
        }
    }
    for path in [outside, root.join("escape"), root.join("../outside")] {
        let error = engine.normalize_path(path.to_str().unwrap()).unwrap_err();
        assert_eq!(error.to_string(), "Path is outside repository");
    }
    assert_eq!(engine.normalize_path("./src/source.rs")?, "src/source.rs");
    assert!(engine.normalize_path("../outside").is_err());
    Ok(())
}
#[test]
fn all_grouped_index_pointers_are_excluded_from_discovery() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let index = temp.path().join("index.sqlite");
    fs::write(temp.path().join("visible.json"), "{}")?;
    for role in FormatGroup::ALL
        .into_iter()
        .map(FormatGroup::as_str)
        .chain(["markdown", "descriptions", "symbols"])
    {
        let pointer = temp.path().join(format!("index.sqlite.{role}.shared.json"));
        fs::write(&pointer, "{}")?;
        assert!(index_artifact(&pointer, &index));
    }
    assert_eq!(
        source_paths(temp.path(), &index, &json!({"include":["**"]}))?,
        ["visible.json"]
    );
    Ok(())
}

fn source_reads() -> usize {
    SOURCE_READS.with(|reads| reads.replace(0))
}

fn assert_send<T: Send>() {}

#[test]
fn engine_stays_send_with_derived_caches() {
    assert_send::<Engine>();
}

#[test]
fn non_git_refresh_reuses_fingerprints_and_respects_size_policy() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("plain");
    fs::create_dir(&root)?;
    fs::write(root.join("first.rs"), "pub fn first() {}\n")?;
    fs::write(root.join("second.rs"), "pub fn second() {}\n")?;
    let index = temp.path().join("index.sqlite");
    let config = json!({"artifactCachePath":temp.path().join("global.sqlite")});
    let mut engine = Engine::open_map(&root, &index, config.clone())?;
    assert_eq!(engine.refresh_structure()?["filesUpdated"], 2);
    assert!(engine.db.meta("checkpoint")?.is_none());
    let generation = engine.db.generation()?;
    source_reads();
    for _ in 0..2 {
        let refreshed = engine.refresh_structure()?;
        assert_eq!(refreshed["filesUpdated"], 0);
        assert_eq!(engine.db.generation()?, generation);
    }
    assert_eq!(source_reads(), 0, "unchanged non-Git files were re-read");

    // A different length always changes the fingerprint, whatever the clock.
    fs::write(root.join("second.rs"), "pub fn second_edited() {}\n")?;
    assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
    assert_eq!(source_reads(), 1, "only the edited file is read");
    drop(engine);

    // Discovery policy changes rescan without re-reading unchanged files.
    let mut excluded = config.clone();
    excluded["exclude"] = json!(["first.rs"]);
    let mut engine = Engine::open_map(&root, &index, excluded)?;
    let refreshed = engine.refresh_structure()?;
    assert_eq!(
        (&refreshed["filesUpdated"], &refreshed["filesDeleted"]),
        (&json!(0), &json!(1))
    );
    assert_eq!(source_reads(), 0);
    drop(engine);

    // The size limit is policy that changes the result for unchanged metadata.
    let mut small = config.clone();
    small["maxFileSize"] = json!(20);
    let mut engine = Engine::open_map(&root, &index, small)?;
    engine.refresh_structure()?;
    let errors = engine.errors()?;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0]["path"], "second.rs");
    assert_eq!(errors[0]["code"], "read-error");
    drop(engine);
    let mut engine = Engine::open_map(&root, &index, config)?;
    engine.refresh_structure()?;
    assert!(engine.errors()?.is_empty());
    assert_eq!(engine.db.paths()?, ["first.rs", "second.rs"]);
    Ok(())
}

#[test]
fn ineligible_dirty_paths_do_not_defeat_the_unchanged_fast_path() -> Result<()> {
    let (_temp, mut engine) = git_fixture()?;
    engine.refresh_structure()?;
    let root = engine.root.clone();
    // Untracked files discovery never selects: an unsupported type, and a
    // supported type below an always-excluded directory.
    fs::write(root.join("scratch.bin"), "binary")?;
    fs::create_dir(root.join("target"))?;
    fs::write(root.join("target/built.rs"), "pub fn built() {}\n")?;
    let generation = engine.db.generation()?;
    engine.refresh_structure()?;
    assert_eq!(engine.db.meta("dirty_paths")?.as_deref(), Some("[]"));
    assert_eq!(engine.db.generation()?, generation);
    assert_eq!(engine.db.paths()?, ["source.rs"]);

    // Selectable changes and ignore-policy files remain triggers.
    fs::write(root.join("added.rs"), "pub fn added() {}\n")?;
    assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
    assert_eq!(
        engine.db.meta("dirty_paths")?.as_deref(),
        Some(r#"["added.rs"]"#)
    );
    fs::write(root.join(".gitignore"), "added.rs\n")?;
    assert_eq!(engine.refresh_structure()?["filesDeleted"], 1);
    assert_eq!(
        engine.db.meta("dirty_paths")?.as_deref(),
        Some(r#"[".gitignore"]"#)
    );
    assert_eq!(engine.db.paths()?, ["source.rs"]);
    // Indexed paths stay dirty even when configuration would not select them.
    fs::write(root.join("source.rs"), "pub fn edited() {}\n")?;
    let eligibility = Eligibility::new(&json!({"exclude":["source.rs"]}))?;
    let mut dirty = HashSet::from(["source.rs".to_owned(), "scratch.bin".to_owned()]);
    engine.retain_relevant_dirty(&mut dirty, &eligibility);
    assert_eq!(dirty, HashSet::from(["source.rs".to_owned()]));
    Ok(())
}

#[test]
fn deleted_directories_are_forgotten_by_candidate_refreshes() -> Result<()> {
    let (_temp, mut engine) = git_fixture()?;
    let root = engine.root.clone();
    fs::create_dir_all(root.join("gone/deeper"))?;
    fs::write(root.join("gone/deeper/old.rs"), "pub fn old() {}\n")?;
    git_command(&root, &["add", "."])?;
    git_command(&root, &["commit", "-qm", "nested"])?;
    engine.refresh_structure()?;
    let directories = |engine: &Engine| -> Result<BTreeSet<String>> {
        Ok(serde_json::from_str(
            &engine.db.meta("source_directories")?.unwrap(),
        )?)
    };
    assert!(directories(&engine)?.contains("gone/deeper"));
    // Establish a checkpoint so the next refresh is a candidate walk.
    fs::remove_dir_all(root.join("gone"))?;
    git_command(&root, &["add", "--all"])?;
    git_command(&root, &["commit", "-qm", "removed"])?;
    assert_eq!(engine.refresh_structure()?["filesDeleted"], 1);
    assert_eq!(directories(&engine)?, BTreeSet::from([String::new()]));
    Ok(())
}

#[cfg(unix)]
#[test]
fn relative_utf8_skips_undecodable_paths_without_filesystem_access() -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let root = Path::new("repo");
    assert_eq!(
        relative_utf8(root, &root.join("kept.rs"))?,
        Some("kept.rs".into())
    );
    for name in [&b"na\xefve.rs"[..], b"caf\xe9/inner.rs", b"data\xff.bin"] {
        let path = root.join(std::ffi::OsStr::from_bytes(name));
        assert_eq!(relative_utf8(root, &path)?, None);
    }
    assert!(relative_utf8(root, Path::new("outside.rs")).is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
fn discovery_skips_undecodable_names() -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("plain");
    fs::create_dir(&root)?;
    if !crate::test_support::supports_non_utf8_names(&root)? {
        return Ok(());
    }
    fs::create_dir_all(root.join(std::ffi::OsStr::from_bytes(b"caf\xe9")))?;
    fs::write(root.join("kept.rs"), "pub fn kept() {}\n")?;
    // Latin-1 names: a supported source file, a nested one, and other data.
    for name in [&b"na\xefve.rs"[..], b"caf\xe9/inner.rs", b"data\xff.bin"] {
        fs::write(
            root.join(std::ffi::OsStr::from_bytes(name)),
            "pub fn x() {}\n",
        )?;
    }
    let index = temp.path().join("index.sqlite");
    let config = json!({"artifactCachePath":temp.path().join("global.sqlite")});
    // This is exactly what an unindexed map walks.
    assert_eq!(source_paths(&root, &index, &config)?, ["kept.rs"]);
    let mut engine = Engine::open_map(&root, &index, config)?;
    assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
    assert_eq!(engine.refresh_structure()?["filesUpdated"], 0);
    assert_eq!(engine.db.paths()?, ["kept.rs"]);
    Ok(())
}

#[cfg(unix)]
#[test]
fn discovery_skips_unreadable_directories() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("plain");
    fs::create_dir_all(root.join("locked"))?;
    fs::write(root.join("kept.rs"), "pub fn kept() {}\n")?;
    fs::write(root.join("locked/hidden.rs"), "pub fn hidden() {}\n")?;
    let index = temp.path().join("index.sqlite");
    let config = json!({"artifactCachePath":temp.path().join("global.sqlite")});
    assert_eq!(
        source_paths(&root, &index, &config)?,
        ["kept.rs", "locked/hidden.rs"]
    );

    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000))?;
    let restore = || fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755));
    if fs::read_dir(root.join("locked")).is_ok() {
        // Privileged users bypass directory permissions.
        return Ok(restore()?);
    }
    let walked = source_paths(&root, &index, &config);
    let indexed = Engine::open_map(&root, &index, config).and_then(|mut engine| {
        let first = engine.refresh_structure()?;
        // The remembered directory's policy probes are unreadable as well.
        let second = engine.refresh_structure()?;
        Ok((first, second, engine.db.paths()?))
    });
    restore()?;
    assert_eq!(walked?, ["kept.rs"]);
    let (first, second, paths) = indexed?;
    assert_eq!(first["filesUpdated"], 1);
    assert_eq!(second["filesUpdated"], 0);
    assert_eq!(paths, ["kept.rs"]);

    // The selected root itself must still be walkable.
    fs::set_permissions(&root, fs::Permissions::from_mode(0o000))?;
    let walked = source_paths(
        &root,
        &index,
        &json!({"artifactCachePath":temp.path().join("global.sqlite")}),
    );
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
    assert!(walked.is_err());
    Ok(())
}

/// Offline OpenAI-compatible provider: unit vectors, fixed prose, and
/// identity rankings. Records how many documents each rerank request held.
fn provider() -> Result<(String, std::sync::Arc<std::sync::Mutex<Vec<usize>>>)> {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    let reranked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = reranked.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let respond = |stream: &mut std::net::TcpStream| -> Result<()> {
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut reader = BufReader::new(&mut *stream);
                let mut line = String::new();
                reader.read_line(&mut line)?;
                let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                let mut length = 0;
                loop {
                    line.clear();
                    ensure!(reader.read_line(&mut line)? > 0, "truncated headers");
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse()?;
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body)?;
                let body: Value = serde_json::from_slice(&body)?;
                let response = if path.ends_with("/embeddings") {
                    let dimensions = body["dimensions"].as_u64().unwrap_or(2) as usize;
                    let mut vector = vec![0.0; dimensions];
                    vector[0] = 1.0;
                    let data: Vec<_> = (0..body["input"].as_array().map_or(0, Vec::len))
                        .map(|index| json!({"index":index,"embedding":vector}))
                        .collect();
                    json!({"data":data})
                } else if body["instructions"]
                    .as_str()
                    .is_some_and(|s| s.contains("Rank candidate documents"))
                {
                    let prompt: Value =
                        serde_json::from_str(body["messages"][0]["content"].as_str().unwrap())?;
                    let count = prompt["resultCount"].as_u64().unwrap() as usize;
                    seen.lock().unwrap().push(count);
                    let ranking: Vec<_> = (0..count)
                        .map(|index| json!({"index":index,"score":1.0 - index as f64 / 1000.0}))
                        .collect();
                    json!({"output_text":json!({"ranking":ranking}).to_string()})
                } else {
                    json!({"output_text":"Generated summary."})
                };
                let bytes = if path.ends_with("/harness") {
                    response["output_text"]
                        .as_str()
                        .unwrap()
                        .as_bytes()
                        .to_vec()
                } else {
                    serde_json::to_vec(&response)?
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                )?;
                stream.write_all(&bytes)?;
                Ok(())
            };
            let _ = respond(&mut stream);
        }
    });
    Ok((base, reranked))
}

use crate::test_agent as agent;

fn provider_config(temp: &tempfile::TempDir, base: &str) -> Value {
    json!({
        "embeddingProvider":"openai", "embeddingModel":"unit-embedding",
        "embeddingDimensions":2, "symbolDimensions":2,
        "descriptionCommand":agent::http(base, "unit"), "descriptionModel":"unit-description",
        "rerankerProvider":"command", "rerankerModel":"unit-reranker",
        "rerankerCommand":agent::http(base, "unit"),
        "embeddingBaseUrl":base,
        "embeddingApiKey":"unit",
        "providerMaxRetries":0, "retryDelayMs":0, "providerTimeoutMs":5000,
        "artifactCachePath":temp.path().join("global.sqlite")
    })
}

#[test]
fn reranker_candidate_cap_reports_omitted_results() -> Result<()> {
    let (base, reranked) = provider()?;
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("plain");
    fs::create_dir(&root)?;
    let source: String = (0..12)
        .map(|index| format!("pub fn candidate_{index}() -> usize {{\n    {index}\n}}\n\n"))
        .collect();
    fs::write(root.join("many.rs"), source)?;
    let mut config = provider_config(&temp, &base);
    config["rerankingEnabled"] = json!(true);
    let mut engine = Engine::open(&root, &temp.path().join("index.sqlite"), config)?;
    engine.refresh()?;
    assert_eq!(
        engine
            .items
            .iter()
            .filter(|item| item.kind == "function")
            .count(),
        12
    );
    // The default candidate count equals this limit, so the row count alone
    // cannot show that two more functions matched.
    for _ in 0..2 {
        let limited = engine.search_limited("candidate", "search-code", &json!({"limit":10}))?;
        assert_eq!(limited.rows.len(), 10);
        assert!(
            limited.omitted,
            "candidates beyond the reranker cap were not reported"
        );
        assert!(limited.rows.iter().all(|row| row["type"] == "function"));
        assert!(
            limited
                .rows
                .iter()
                .all(|row| row["rerankScore"].is_number())
        );
    }
    // The probe is never sent to the reranker, and the answer was cached.
    assert_eq!(*reranked.lock().unwrap(), [10]);
    assert_eq!(
        engine
            .search("candidate", "search-code", &json!({"limit":10}))?
            .len(),
        10
    );
    let exact = engine.search_limited("candidate", "search-code", &json!({"limit":12}))?;
    assert_eq!(exact.rows.len(), 12);
    assert!(!exact.omitted);
    let merged = engine.search_queries_limited(
        &["candidate".to_owned(), "other".to_owned()],
        "search-code",
        &json!({"limit":10}),
    )?;
    assert_eq!(merged.rows.len(), 10);
    assert!(merged.omitted);
    assert!(merged.rows.iter().all(|row| row["type"] == "function"));
    Ok(())
}

#[test]
fn descriptions_cover_healthy_callables_beside_parse_errors() -> Result<()> {
    let (base, _) = provider()?;
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("plain");
    fs::create_dir(&root)?;
    fs::write(
        root.join("damaged.rs"),
        "pub fn healthy() -> usize {\n    1\n}\n\npub fn broken( {\n",
    )?;
    fs::write(root.join("binary.rs"), [0xff, 0xfe, 0xfd])?;
    let config = provider_config(&temp, &base);
    let mut engine = Engine::open(&root, &temp.path().join("index.sqlite"), config)?;
    engine.refresh()?;
    let codes: BTreeSet<_> = engine
        .errors()?
        .iter()
        .map(|error| (error["path"].to_string(), error["code"].to_string()))
        .collect();
    assert!(codes.contains(&(r#""damaged.rs""#.into(), r#""parse-error""#.into())));
    assert!(codes.contains(&(r#""binary.rs""#.into(), r#""read-error""#.into())));
    let diagnostics = engine.errors()?.len();
    engine.generate_descriptions()?;
    let healthy = engine
        .items
        .iter()
        .find(|item| item.kind == "function" && item.data["name"] == "healthy")
        .context("healthy callable is indexed")?;
    assert_eq!(healthy.data["description"], "Generated summary.");
    assert_eq!(
        engine.files["damaged.rs"].description.as_deref(),
        Some("Generated summary.")
    );
    assert!(engine.files["binary.rs"].description.is_none());
    // The diagnostics survive description publication.
    assert_eq!(engine.errors()?.len(), diagnostics);
    Ok(())
}

/// Pins the dialect differences documented in docs/reference.md.
#[test]
fn config_globs_and_selector_globs_keep_their_documented_dialects() -> Result<()> {
    let config = |pattern: &str, path: &str| -> Result<bool> {
        Ok(globs(&json!([pattern]))?.is_match(path))
    };
    let selector = |pattern: &str, path: &str| -> Result<bool> {
        Ok(Selection::compile(&json!({"glob":[pattern]}))?.path_matches(path))
    };
    for (pattern, path, in_config, in_selector) in [
        // `*` crosses directories only in config globs.
        ("src/*.rs", "src/a.rs", true, true),
        ("src/*.rs", "src/a/b.rs", true, false),
        ("*.rs", "a/b.rs", true, true),
        ("src/?.rs", "src//.rs", true, false),
        // A slash-free pattern is a basename pattern only for selectors.
        ("test.rs", "test.rs", true, true),
        ("test.rs", "a/test.rs", false, true),
        // Anchors.
        ("/a.rs", "a.rs", false, true),
        ("/a.rs", "b/a.rs", false, false),
        // Directory patterns.
        ("src/", "src/a.rs", false, false),
        ("src", "src/a.rs", false, false),
        ("src/**", "src/a/b.rs", true, true),
        ("**/fixtures/**", "a/fixtures/b/c.rs", true, true),
        ("**/fixtures/**", "fixtures/c.rs", true, true),
        ("a/**/b.rs", "a/b.rs", true, true),
        // Alternation and classes exist in both.
        ("*.{rs,go}", "a.go", true, true),
        ("[ab].rs", "b.rs", true, true),
        // Matching is case-sensitive in both.
        ("*.RS", "a.rs", false, false),
    ] {
        assert_eq!(
            config(pattern, path)?,
            in_config,
            "config {pattern} on {path}"
        );
        assert_eq!(
            selector(pattern, path)?,
            in_selector,
            "selector {pattern} on {path}"
        );
    }
    // Negation exists only for selectors; config treats `!` literally.
    assert!(!config("!a.rs", "b.rs")?);
    assert!(config("!a.rs", "!a.rs")?);
    assert!(selector("!a.rs", "b.rs")?);
    assert!(!selector("!a.rs", "a.rs")?);
    Ok(())
}
