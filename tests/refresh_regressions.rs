//! Refresh regressions: saved diagnostics, undecodable names, and unreadable
//! directories must neither defeat the unchanged fast path nor abort indexing.
//! Structural engines make no provider calls.
use anyhow::{Result, ensure};
use rusqlite::Connection;
use serde_json::{Value, json};
use slopdex::engine::Engine;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

#[cfg(unix)]
#[path = "../src/test_support.rs"]
mod test_support;

struct Repo {
    _temp: TempDir,
    root: PathBuf,
    index: PathBuf,
    config: Value,
}

impl Repo {
    /// Every store lives in this fixture's own temporary directory.
    fn new(git: bool) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let base = temp.path().canonicalize()?;
        let root = base.join("repo");
        fs::create_dir(&root)?;
        let repo = Self {
            root,
            index: base.join("index.sqlite"),
            config: json!({"artifactCachePath": base.join("artifacts.sqlite")}),
            _temp: temp,
        };
        if git {
            repo.git(&["init", "--quiet"])?;
        }
        Ok(repo)
    }

    fn git(&self, args: &[&str]) -> Result<()> {
        let output = Command::new("git")
            .current_dir(&self.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .args([
                "-c",
                "user.name=Refresh Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
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
        Ok(())
    }

    fn commit(&self) -> Result<()> {
        self.git(&["add", "--all"])?;
        self.git(&["commit", "--quiet", "-m", "fixture"])
    }

    fn write(&self, path: &str, text: impl AsRef<[u8]>) -> Result<()> {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap())?;
        Ok(fs::write(path, text)?)
    }

    fn open(&self) -> Result<Engine> {
        self.open_at(&self.root)
    }

    fn open_at(&self, root: &Path) -> Result<Engine> {
        Engine::open_map(root, &self.index, self.config.clone())
    }

    fn reader(&self) -> Result<Engine> {
        Engine::open_map_readonly(&self.root, &self.index, self.config.clone())
    }

    /// Stand-in for a cached query answer: publication deletes the whole cache.
    fn seed_search_cache(&self) -> Result<()> {
        Connection::open(&self.index)?.execute(
            "INSERT INTO search_cache(key,value) VALUES('regression-probe','[]')",
            [],
        )?;
        Ok(())
    }

    fn search_cache_rows(&self) -> Result<i64> {
        Ok(Connection::open(&self.index)?.query_row(
            "SELECT count(*) FROM search_cache WHERE key='regression-probe'",
            [],
            |row| row.get(0),
        )?)
    }
}

fn error_codes(engine: &Engine) -> Result<Vec<(String, String)>> {
    let mut codes: Vec<_> = engine
        .errors()?
        .iter()
        .map(|error| {
            (
                error["path"].as_str().unwrap().to_owned(),
                error["code"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    codes.sort();
    codes.dedup();
    Ok(codes)
}

#[cfg(unix)]
fn paths(engine: &Engine) -> Result<Vec<String>> {
    let mut paths: Vec<_> = engine
        .map(&json!({"limit": "none"}))?
        .iter()
        .filter_map(|row| row["path"].as_str().map(str::to_owned))
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

const BROKEN: &str = "pub fn healthy() -> usize {\n    1\n}\n\npub fn broken( {\n";

/// Repeated refreshes of an unchanged tree with saved diagnostics publish
/// nothing, keep cached answers, and let read-only commands stay read-only.
fn assert_saved_diagnostics_are_stable(repo: &Repo, expected: &[(&str, &str)]) -> Result<()> {
    let expected: Vec<_> = expected
        .iter()
        .map(|(path, code)| ((*path).to_owned(), (*code).to_owned()))
        .collect();
    let mut engine = repo.open()?;
    let first = engine.refresh_structure()?;
    assert_eq!(error_codes(&engine)?, expected);
    repo.seed_search_cache()?;
    for _ in 0..3 {
        let refreshed = engine.refresh_structure()?;
        assert_eq!(refreshed["filesUpdated"], 0, "{refreshed}");
        assert_eq!(refreshed["filesDeleted"], 0, "{refreshed}");
        assert_eq!(refreshed["generation"], first["generation"]);
        assert_eq!(repo.search_cache_rows()?, 1);
    }
    drop(engine);
    // A fresh process sees the same thing, including behind a shared lock: an
    // error here would be the request to reopen with the exclusive lock.
    let mut reader = repo.reader()?;
    let refreshed = reader.refresh_structure()?;
    assert_eq!(refreshed["filesUpdated"], 0);
    assert_eq!(refreshed["generation"], first["generation"]);
    assert_eq!(error_codes(&reader)?, expected);
    drop(reader);
    let mut engine = repo.open()?;
    assert_eq!(
        engine.refresh_structure()?["generation"],
        first["generation"]
    );
    assert_eq!(repo.search_cache_rows()?, 1);
    Ok(())
}

#[test]
fn parse_error_files_keep_the_unchanged_fast_path() -> Result<()> {
    for git in [true, false] {
        let repo = Repo::new(git)?;
        repo.write("broken.rs", BROKEN)?;
        repo.write("fine.rs", "pub fn fine() {}\n")?;
        if git {
            repo.commit()?;
        }
        assert_saved_diagnostics_are_stable(&repo, &[("broken.rs", "parse-error")])?;
        // An uncommitted file with diagnostics is stable as well.
        repo.write("draft.rs", BROKEN)?;
        assert_saved_diagnostics_are_stable(
            &repo,
            &[("broken.rs", "parse-error"), ("draft.rs", "parse-error")],
        )?;
    }
    Ok(())
}

#[test]
fn unreadable_content_keeps_the_unchanged_fast_path() -> Result<()> {
    for git in [true, false] {
        let mut repo = Repo::new(git)?;
        repo.config["maxFileSize"] = json!(64);
        repo.write(
            "large.rs",
            format!("pub fn large() {{}}\n// {}\n", "x".repeat(80)),
        )?;
        repo.write("latin1.rs", b"pub fn caf\xe9() {}\n")?;
        repo.write("fine.rs", "pub fn fine() {}\n")?;
        if git {
            repo.commit()?;
        }
        assert_saved_diagnostics_are_stable(
            &repo,
            &[("large.rs", "read-error"), ("latin1.rs", "read-error")],
        )?;
    }
    Ok(())
}

#[test]
fn diagnostics_follow_content_and_policy_changes() -> Result<()> {
    for git in [true, false] {
        let mut repo = Repo::new(git)?;
        repo.config["maxFileSize"] = json!(64);
        repo.write("code.rs", "pub fn code() {}\n")?;
        repo.write(
            "large.rs",
            format!("pub fn large() {{}}\n// {}\n", "x".repeat(80)),
        )?;
        if git {
            repo.commit()?;
        }
        let mut engine = repo.open()?;
        engine.refresh_structure()?;
        assert_eq!(
            error_codes(&engine)?,
            [("large.rs".to_owned(), "read-error".to_owned())]
        );

        // Healthy to damaged, and back.
        repo.write("code.rs", BROKEN)?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
        assert!(error_codes(&engine)?.contains(&("code.rs".to_owned(), "parse-error".to_owned())));
        repo.write("code.rs", "pub fn repaired() -> usize {\n    2\n}\n")?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
        assert!(
            !error_codes(&engine)?
                .iter()
                .any(|(path, _)| path == "code.rs")
        );

        // A failed read is retried once its metadata changes...
        repo.write("large.rs", "pub fn small() {}\n")?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
        assert!(error_codes(&engine)?.is_empty());
        // ...including an empty file, whose hash equals the failure placeholder.
        repo.write("large.rs", format!("// {}\n", "x".repeat(80)))?;
        engine.refresh_structure()?;
        assert_eq!(error_codes(&engine)?.len(), 1);
        repo.write("large.rs", "")?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
        assert!(error_codes(&engine)?.is_empty());

        // ...and when the size limit changes while the file does not.
        repo.write(
            "large.rs",
            format!("pub fn large() {{}}\n// {}\n", "x".repeat(80)),
        )?;
        engine.refresh_structure()?;
        assert_eq!(error_codes(&engine)?.len(), 1);
        drop(engine);
        repo.config["maxFileSize"] = json!(4096);
        let mut engine = repo.open()?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
        assert!(error_codes(&engine)?.is_empty());
        drop(engine);
        repo.config["maxFileSize"] = json!(64);
        let mut engine = repo.open()?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
        assert_eq!(
            error_codes(&engine)?,
            [("large.rs".to_owned(), "read-error".to_owned())]
        );
    }
    Ok(())
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::{
        ffi::OsStr,
        os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    };

    fn latin1(root: &Path, name: &[u8]) -> PathBuf {
        root.join(OsStr::from_bytes(name))
    }

    #[test]
    fn untracked_undecodable_name_outside_the_selected_root() -> Result<()> {
        let repo = Repo::new(true)?;
        if !test_support::supports_non_utf8_names(&repo.root)? {
            return Ok(());
        }
        repo.write("pkg/code.rs", "pub fn code() {}\n")?;
        repo.commit()?;
        fs::write(latin1(&repo.root, b"caf\xe9.rs"), "pub fn outside() {}\n")?;
        let mut engine = repo.open_at(&repo.root.join("pkg"))?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 0);
        assert_eq!(paths(&engine)?, ["code.rs"]);
        Ok(())
    }

    #[test]
    fn untracked_undecodable_name_inside_the_selected_root() -> Result<()> {
        let repo = Repo::new(true)?;
        if !test_support::supports_non_utf8_names(&repo.root)? {
            return Ok(());
        }
        repo.write("pkg/code.rs", "pub fn code() {}\n")?;
        repo.commit()?;
        let mut engine = repo.open()?;
        engine.refresh_structure()?;
        // Appears after the checkpoint, so both status and discovery report it.
        fs::write(
            latin1(&repo.root, b"pkg/na\xefve.rs"),
            "pub fn inside() {}\n",
        )?;
        repo.write("pkg/added.rs", "pub fn added() {}\n")?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
        assert_eq!(paths(&engine)?, ["pkg/added.rs", "pkg/code.rs"]);
        Ok(())
    }

    #[test]
    fn tracked_undecodable_name_inside_the_selected_root() -> Result<()> {
        let repo = Repo::new(true)?;
        if !test_support::supports_non_utf8_names(&repo.root)? {
            return Ok(());
        }
        repo.write("pkg/code.rs", "pub fn code() {}\n")?;
        fs::write(
            latin1(&repo.root, b"pkg/tracked\xff.rs"),
            "pub fn tracked() {}\n",
        )?;
        fs::create_dir(latin1(&repo.root, b"d\xe9p"))?;
        fs::write(
            latin1(&repo.root, b"d\xe9p/nested.rs"),
            "pub fn nested() {}\n",
        )?;
        repo.commit()?;
        let mut engine = repo.open()?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
        // Modify and commit it: the checkpoint diff names the path too.
        fs::write(
            latin1(&repo.root, b"pkg/tracked\xff.rs"),
            "pub fn edited() {}\n",
        )?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 0);
        repo.write("pkg/code.rs", "pub fn changed() {}\n")?;
        repo.commit()?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
        assert_eq!(paths(&engine)?, ["pkg/code.rs"]);
        Ok(())
    }

    #[test]
    fn unreadable_directory_does_not_abort_refresh() -> Result<()> {
        for git in [true, false] {
            let repo = Repo::new(git)?;
            repo.write("kept.rs", "pub fn kept() {}\n")?;
            repo.write("locked/hidden.rs", "pub fn hidden() {}\n")?;
            if git {
                repo.commit()?;
            }
            let locked = repo.root.join("locked");
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
            if fs::read_dir(&locked).is_ok() {
                // Privileged users bypass directory permissions.
                fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))?;
                return Ok(());
            }
            let result = repo.open().and_then(|mut engine| {
                let first = engine.refresh_structure()?;
                let second = engine.refresh_structure()?;
                Ok((first, second, paths(&engine)?))
            });
            // Restore access before asserting so the fixture can be removed.
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))?;
            let (first, second, indexed) = result?;
            assert_eq!(first["filesUpdated"], 1, "git={git}");
            assert_eq!(second["filesUpdated"], 0, "git={git}");
            assert_eq!(indexed, ["kept.rs"]);
            if !git {
                // Regained access is noticed by the next full scan.
                let mut engine = repo.open()?;
                assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
                assert_eq!(paths(&engine)?, ["kept.rs", "locked/hidden.rs"]);
            }
        }
        Ok(())
    }
}
