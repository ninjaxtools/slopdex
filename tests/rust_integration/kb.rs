use super::*;

fn source() -> &'static str {
    "pub fn example() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n    let e = 5;\n    let f = 6;\n    let g = 7;\n    let h = 8;\n}\n"
}

pub(super) fn fixture(mock: &Mock) -> Result<Repo> {
    let repo = Repo::new()?;
    repo.init_git()?;
    repo.git(&["config", "--local", "user.name", "KB Fixture"])?;
    repo.git(&["config", "--local", "user.email", "kb@example.invalid"])?;
    repo.git(&["config", "--local", "commit.gpgsign", "false"])?;
    fs::create_dir(repo.root.join(".git/empty-hooks"))?;
    repo.git(&[
        "config",
        "--local",
        "core.hooksPath",
        repo.root.join(".git/empty-hooks").to_str().unwrap(),
    ])?;
    repo.write(".slopdex/config.json", &mock.config().to_string())?;
    repo.write("source.rs", source())?;
    repo.git(&["add", "--all"])?;
    repo.git(&["commit", "--quiet", "-m", "source baseline"])?;
    Ok(repo)
}

#[test]
fn init_query_list_and_refresh_use_markdown_and_git_without_a_faq_index() -> Result<()> {
    let mock = Mock::start()?;
    let repo = fixture(&mock)?;
    let result = repo.cli_json(&["kb", "init"])?;
    assert_eq!(mock.requests("/responses").len(), 2);
    assert!(
        !repo.index.exists(),
        "init must not need an embedding index"
    );
    assert_eq!(result["articles"], json!(["docs/kb/example-topic.md"]));
    assert_eq!(
        repo.git(&["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"])?,
        "docs/kb/example-topic.md"
    );
    assert_eq!(repo.git(&["status", "--porcelain"])?, "");
    let markdown = fs::read_to_string(repo.root.join("docs/kb/example-topic.md"))?;
    assert!(markdown.contains("**Markdown**"));
    assert!(markdown.contains("[source.rs](/source.rs#L1-L10)"));
    let calls = mock.count();
    let listed = repo.cli_json(&["kb", "list"])?;
    assert_eq!(listed[0]["freshness"], 1.0);
    assert_eq!(listed[0]["commit"], result["commit"]);
    assert_eq!(mock.count(), calls);
    repo.cli_json(&["update"])?;
    let agent_calls = mock.requests("/responses").len();
    let queried = repo.cli_json(&["kb", "query", "VECTOR_EAST"])?;
    assert_eq!(queried.as_array().unwrap().len(), 1, "{queried}");
    assert_eq!(queried[0]["description"], markdown);
    assert_eq!(queried[0]["freshness"], 1.0);
    assert_eq!(repo.cli_json(&["kb", "query", "VECTOR_EAST"])?, queried);
    assert!(
        repo.cli_json(&["kb", "query", "VECTOR_WEST"])?
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(mock.requests("/responses").len(), agent_calls);
    let channels = repo.cli_json(&["index", "list"])?;
    assert!(
        !channels
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "faq")
    );
    assert!(
        !repo
            .cli_json(&["index", "list", "docs"])?
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        repo.db()?.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='faq_entries'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    assert!(!repo.index.with_extension("sqlite.faq.shared.json").exists());

    // One changed line is exactly the refresh boundary: it is not refreshed.
    repo.write("source.rs", &source().replace("let c = 3", "let c = 9"))?;
    repo.git(&["add", "source.rs"])?;
    repo.git(&["commit", "--quiet", "-m", "change one line"])?;
    assert_eq!(repo.cli_json(&["kb", "list"])?[0]["freshness"], 0.9);
    assert_eq!(repo.cli_json(&["kb", "refresh"])?["articles"], json!([]));
    // Two changed lines are still queryable at 0.8, but require refresh at 0.9.
    repo.write(
        "source.rs",
        &source()
            .replace("let c = 3", "let c = 9")
            .replace("let d = 4", "let d = 9"),
    )?;
    repo.git(&["add", "source.rs"])?;
    repo.git(&["commit", "--quiet", "-m", "change two lines"])?;
    assert_eq!(repo.cli_json(&["kb", "list"])?[0]["freshness"], 0.8);
    assert_eq!(
        repo.cli_json(&["kb", "query", "VECTOR_EAST"])?[0]["freshness"],
        0.8
    );
    assert!(
        repo.cli_json(&["kb", "query", "VECTOR_EAST", "--freshness", "0.9"])?
            .as_array()
            .unwrap()
            .is_empty()
    );
    let refreshed = repo.cli_json(&["kb", "refresh"])?;
    assert_eq!(refreshed["articles"], result["articles"]);
    assert_eq!(repo.cli_json(&["kb", "list"])?[0]["freshness"], 1.0);
    assert_eq!(
        repo.cli_json(&["kb", "list"])?[0]["commit"],
        refreshed["commit"]
    );
    assert_eq!(repo.git(&["status", "--porcelain"])?, "");
    assert_eq!(
        repo.git(&["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"])?,
        "docs/kb/example-topic.md"
    );
    Ok(())
}

#[test]
fn generation_rejects_dirty_repositories_and_invalid_agent_outputs_without_publication()
-> Result<()> {
    let mock = Mock::start()?;
    let repo = fixture(&mock)?;
    repo.write("pending.txt", "pending")?;
    let calls = mock.count();
    let output = repo.cli(&["kb", "init"])?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Commit all"));
    assert_eq!(mock.count(), calls);
    repo.git(&["add", "pending.txt"])?;
    assert!(!repo.cli(&["kb", "init"])?.status.success());
    assert_eq!(
        repo.git(&["diff", "--cached", "--name-only"])?,
        "pending.txt"
    );
    repo.git(&["commit", "--quiet", "-m", "pending"])?;
    mock.state.lock().unwrap().kb_article =
        Some(json!({"description":"Cannot cite this.","references":[{"path":"missing.rs"}]}));
    let head = repo.git(&["rev-parse", "HEAD"])?;
    assert!(!repo.cli(&["kb", "init"])?.status.success());
    assert!(!repo.root.join("docs/kb").exists());
    assert_eq!(repo.git(&["rev-parse", "HEAD"])?, head);
    Ok(())
}

#[test]
fn source_changes_during_generation_are_not_committed_or_overwritten() -> Result<()> {
    let mock = Mock::start()?;
    let repo = fixture(&mock)?;
    let root = repo.root.clone();
    let head = repo.git(&["rev-parse", "HEAD"])?;
    mock.on_next_request(move || {
        fs::write(root.join("source.rs"), "pub fn editor_change() {}\n")?;
        Ok(())
    });
    let output = repo.cli(&["kb", "init"])?;
    assert!(!output.status.success());
    assert_eq!(repo.git(&["rev-parse", "HEAD"])?, head);
    assert_eq!(
        fs::read_to_string(repo.root.join("source.rs"))?,
        "pub fn editor_change() {}\n"
    );
    assert!(!repo.root.join("docs/kb").exists());
    Ok(())
}

#[test]
fn article_agents_default_to_four_workers_and_support_overrides() -> Result<()> {
    for workers in [4, 2] {
        let temp = tempfile::tempdir()?;
        let concurrent = ConcurrentRequests::new(
            ConcurrentWork::Callables,
            workers,
            temp.path().join("unused.sqlite"),
            false,
        );
        let mock = Mock::start_with(Some(concurrent.clone()))?;
        let repo = fixture(&mock)?;
        mock.state.lock().unwrap().kb_topics = Some(json!(
            (0..8)
                .map(|i| json!({"title":format!("Topic {i}"),"description":"Explain this topic."}))
                .collect::<Vec<_>>()
        ));
        let args = if workers == 4 {
            vec!["kb", "init"]
        } else {
            vec!["kb", "init", "--parallelism", "2"]
        };
        assert_eq!(
            repo.cli_json(&args)?["articles"].as_array().unwrap().len(),
            8
        );
        assert_eq!(concurrent.progress.lock().unwrap().maximum, workers);
        assert_eq!(mock.requests("/responses").len(), 9);
    }
    Ok(())
}

#[test]
fn custom_kb_path_and_saved_snapshot_queries_are_safe() -> Result<()> {
    let mock = Mock::start()?;
    let repo = fixture(&mock)?;
    let mut config = mock.config();
    config["kbPath"] = json!("notes/knowledge");
    config["kbQueryFreshness"] = json!(0.95);
    repo.write(".slopdex/config.json", &config.to_string())?;
    repo.git(&["add", "--all"])?;
    repo.git(&["commit", "--quiet", "-m", "configure kb"])?;
    let result = repo.cli_json(&["kb", "init"])?;
    assert_eq!(result["articles"][0], "notes/knowledge/example-topic.md");
    repo.cli_json(&["update"])?;
    let mut engine = repo.open(&config)?;
    assert_eq!(engine.kb_query("VECTOR_EAST", &json!({}))?.len(), 1);
    repo.write(
        "notes/knowledge/example-topic.md",
        "# Edited\n\nUncommitted article.\n",
    )?;
    assert!(
        engine
            .kb_query("VECTOR_EAST", &json!({"freshness":0}))?
            .is_empty()
    );
    engine.refresh()?;
    // An untrusted article cannot inherit freshness from the old commit.
    assert!(engine.kb_query("VECTOR_EAST", &json!({}))?.is_empty());
    Ok(())
}

#[test]
fn kb_freshness_respects_git_checkout_conversion_and_inherited_eol_settings() -> Result<()> {
    for settings in ["autocrlf = true", "autocrlf = false\n\teol = crlf"] {
        let mock = Mock::start()?;
        let repo = fixture(&mock)?;
        repo.write(".gitattributes", "source.rs text\n*.md text\n")?;
        repo.git(&["add", "--all"])?;
        repo.git(&["commit", "--quiet", "-m", "text attributes"])?;
        // This test intentionally uses checkout conversion, not the ordinary
        // LF fixture policy. Only child processes see the temporary global file.
        repo.git(&["config", "--local", "--unset", "core.autocrlf"])?;
        repo.git(&["config", "--local", "--unset", "core.eol"])?;
        fs::write(
            repo.home.join("gitconfig"),
            format!("[core]\n\t{settings}\n[fixture]\n\tmarker = inherited\n"),
        )?;
        let git = |args: &[&str]| -> Result<String> {
            let output = repo
                .child("git")
                .env("GIT_CONFIG_GLOBAL", "../home/gitconfig")
                .args(args)
                .output()?;
            ensure!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(String::from_utf8(output.stdout)?.trim().to_owned())
        };
        assert_eq!(git(&["config", "--get", "fixture.marker"])?, "inherited");
        fs::remove_file(repo.root.join("source.rs"))?;
        git(&["checkout-index", "-u", "--force", "--", "source.rs"])?;
        assert_eq!(
            fs::read_to_string(repo.root.join("source.rs"))?,
            source().replace('\n', "\r\n")
        );
        assert_eq!(
            git(&["status", "--porcelain"])?,
            "",
            "{settings}: {}",
            git(&["diff", "--", "source.rs"])?
        );
        let cli = |args: &[&str]| -> Result<Value> {
            let output = repo
                .child(env!("CARGO_BIN_EXE_slopdex"))
                .env("GIT_CONFIG_GLOBAL", "../home/gitconfig")
                .args(["--output", "json", "--trust-repo-commands"])
                .args(args)
                .output()?;
            ensure!(
                output.status.success(),
                "{}; Git status: {}",
                String::from_utf8_lossy(&output.stderr),
                git(&["status", "--porcelain"])?
            );
            Ok(serde_json::from_slice(&output.stdout)?)
        };
        cli(&["kb", "init"])?;
        assert_eq!(cli(&["kb", "list"])?[0]["freshness"], 1.0, "{settings}");
        repo.write(
            "source.rs",
            &source()
                .replace("let c = 3", "let c = 9")
                .replace('\n', "\r\n"),
        )?;
        assert_eq!(cli(&["kb", "list"])?[0]["freshness"], 0.9, "{settings}");
    }
    Ok(())
}

#[test]
fn article_commit_failure_leaves_files_and_never_runs_commit_hooks() -> Result<()> {
    let mock = Mock::start()?;
    let repo = fixture(&mock)?;
    repo.git(&["config", "--local", "user.name", ""])?;
    let head = repo.git(&["rev-parse", "HEAD"])?;
    let output = repo.cli(&["kb", "init"])?;
    assert!(!output.status.success());
    assert_eq!(repo.git(&["rev-parse", "HEAD"])?, head);
    assert!(repo.root.join("docs/kb/example-topic.md").exists());
    assert_eq!(repo.git(&["diff", "--cached", "--name-only"])?, "");
    assert_eq!(fs::read_to_string(repo.root.join("source.rs"))?, source());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let repo = fixture(&mock)?;
        let hook = repo.root.join(".git/empty-hooks/pre-commit");
        fs::write(
            &hook,
            "#!/bin/sh\nprintf unexpected > ../home/hook-ran\nexit 77\n",
        )?;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
        repo.cli_json(&["kb", "init"])?;
        assert!(!repo.home.join("hook-ran").exists());
        assert_eq!(repo.git(&["status", "--porcelain"])?, "");
    }
    Ok(())
}

#[test]
fn a_moved_head_or_duplicate_topics_cannot_publish_articles() -> Result<()> {
    let mock = Mock::start()?;
    let repo = fixture(&mock)?;
    let root = repo.root.clone();
    mock.on_next_request(move || {
        let output = Command::new("git")
            .current_dir(root)
            .args([
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "concurrent commit",
            ])
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    });
    assert!(!repo.cli(&["kb", "init"])?.status.success());
    assert!(!repo.root.join("docs/kb").exists());
    assert_eq!(
        repo.git(&["log", "-1", "--format=%s"])?,
        "concurrent commit"
    );
    mock.state.lock().unwrap().kb_topics = Some(json!([
        {"title":"Same topic!", "description":"One"},
        {"title":"Same topic?", "description":"Two"}
    ]));
    let before = mock.requests("/responses").len();
    assert!(!repo.cli(&["kb", "init"])?.status.success());
    assert_eq!(mock.requests("/responses").len(), before + 1);
    assert!(!repo.root.join("docs/kb").exists());
    Ok(())
}

#[test]
fn kb_query_limits_articles_after_freshness_and_does_not_run_rerankers() -> Result<()> {
    let mock = Mock::start()?;
    let repo = fixture(&mock)?;
    let mut config = mock.config();
    config["rerankingEnabled"] = json!(true);
    config["rerankerProvider"] = json!("command");
    // If query invoked a command reranker, it would fail this fixture.
    config["rerankerCommand"] = json!([agent::executable(), "fail"]);
    config["defaultLimit"] = json!(1);
    repo.write(".slopdex/config.json", &config.to_string())?;
    repo.write("other.rs", source())?;
    repo.write(
        "docs/kb/a-stale.md",
        "# VECTOR_EAST stale\n\nVECTOR_EAST\n\n## References\n- [source.rs](/source.rs#L1-L10)\n",
    )?;
    repo.write("docs/kb/b-fresh.md", "# VECTOR_EAST fresh\n\nVECTOR_EAST\n\n## Details\nVECTOR_EAST more detail\n\n## References\n- [other.rs](/other.rs#L1-L10)\n")?;
    repo.write(
        "docs/kb/c-fresh.md",
        "# VECTOR_EAST peer\n\nVECTOR_EAST\n\n## References\n- [other.rs](/other.rs#L1-L10)\n",
    )?;
    repo.git(&["add", "--all"])?;
    repo.git(&["commit", "--quiet", "-m", "articles"])?;
    repo.write("source.rs", "pub fn totally_changed() {}\n")?;
    repo.cli_json(&["update"])?;
    let calls = mock.requests("/responses").len();
    let best = repo.cli_json(&["kb", "query", "VECTOR_EAST"])?;
    assert_eq!(best.as_array().unwrap().len(), 1);
    assert_eq!(best[0]["path"], "docs/kb/b-fresh.md");
    let all = repo.cli_json(&["kb", "query", "VECTOR_EAST", "--limit", "none"])?;
    assert_eq!(
        all.as_array().unwrap().len(),
        2,
        "section hits must be deduplicated: {all}"
    );
    assert!(
        repo.cli_json(&["kb", "query", "VECTOR_EAST", "--threshold", "1"])?
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(mock.requests("/responses").len(), calls);
    Ok(())
}
