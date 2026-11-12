use super::*;

fn fixture() -> Result<Repo> {
    let mut repo = Repo::new()?;
    repo.record_metrics = true;
    Ok(repo)
}

#[test]
fn metrics_unused_project_is_offline_readonly_and_does_not_count_itself() -> Result<()> {
    let repo = fixture()?;
    // Metrics must work even if provider settings cannot be parsed.
    repo.write(".slopdex/config.json", "not json")?;
    let before = filesystem_snapshot(&repo.home)?;
    let first = repo.cli_json(&["metrics"])?;
    assert_eq!(first["totals"]["invocations"], 0);
    assert!(first["commands"].as_object().unwrap().is_empty());
    assert_eq!(first, repo.cli_json(&["metrics"])?);
    assert_eq!(before, filesystem_snapshot(&repo.home)?);
    assert!(!repo.index.exists());
    let text = repo.cli_format(&["metrics"], "text")?;
    assert!(text.status.success());
    assert!(String::from_utf8(text.stdout)?.contains("No usage recorded yet"));
    Ok(())
}

#[test]
fn metrics_tracks_commands_results_failures_and_zero_results_without_queries() -> Result<()> {
    let repo = fixture()?;
    repo.write("code.rs", "pub fn private_marker() {}\n")?;
    repo.cli_json(&["map"])?;
    repo.cli_json(&["map", "-e", "secret_query_no_match"])?;
    repo.cli_json(&["lines"])?;
    let failed = repo.cli(&["search-code", "secret_failed_query"])?;
    assert!(!failed.status.success());
    repo.cli_json(&["map", "missing.rs"])?;
    let report = repo.cli_json(&["metrics"])?;
    assert_eq!(report["totals"]["invocations"], 5);
    assert_eq!(report["totals"]["succeeded"], 4);
    assert_eq!(report["totals"]["failed"], 1);
    assert_eq!(report["commands"]["map"]["invocations"], 3);
    assert_eq!(report["commands"]["map"]["results"], 1);
    assert_eq!(report["commands"]["map"]["emptyResultInvocations"], 2);
    assert_eq!(report["commands"]["lines"]["results"], 1);
    assert_eq!(report["totals"]["linesCounted"], 1);
    assert_eq!(report["recentActiveDays"], 1);
    assert!(report["firstUsedAtUnix"].as_u64().is_some());
    assert_eq!(report, repo.cli_json(&["metrics"])?);
    let stored = fs::read_to_string(report["metricsPath"].as_str().unwrap())?;
    for secret in [
        "private_marker",
        "secret_query_no_match",
        "secret_failed_query",
        "code.rs",
    ] {
        assert!(!stored.contains(secret), "metrics retained {secret}");
    }
    assert!(!repo.index.exists());
    let text = repo.cli_format(&["metrics"], "text")?;
    assert!(text.status.success());
    assert!(String::from_utf8(text.stdout)?.contains("5 invocations"));
    Ok(())
}

#[test]
fn metrics_is_project_scoped_survives_index_rebuild_and_can_be_disabled_and_reset() -> Result<()> {
    let repo = fixture()?;
    let other = Repo::new()?;
    repo.write("code.rs", "pub fn example() {}\n")?;
    repo.cli_json(&["map"])?;
    // Share the cache, not the project root.
    let output = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .arg("--root")
        .arg(&other.root)
        .args(["metrics", "--output", "json"])
        .output()?;
    ensure!(output.status.success());
    let independent: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(independent["totals"]["invocations"], 0);
    {
        let mut engine = repo.open_map(&json!({}))?;
        engine.refresh_structure()?;
    }
    {
        let mut engine = repo.open_map(&json!({"forceReindex": true}))?;
        engine.refresh_structure()?;
    }
    let output = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .arg("--index")
        .arg(&repo.index)
        .args(["map", "--no-reindex", "--output", "json"])
        .env("SLOPDEX_NO_METRICS", "1")
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(repo.cli_json(&["metrics"])?["totals"]["invocations"], 1);
    repo.cli_json(&["map", "--no-reindex"])?;
    assert_eq!(
        repo.cli_json(&["metrics"])?["totals"]["savedSnapshotInvocations"],
        1
    );
    assert_eq!(
        repo.cli_json(&["metrics", "--reset"])?["totals"]["invocations"],
        0
    );
    repo.cli_json(&["map"])?;
    assert_eq!(repo.cli_json(&["metrics"])?["totals"]["invocations"], 1);
    Ok(())
}

#[test]
fn metrics_corruption_never_fails_a_command_and_explicit_reset_repairs_it() -> Result<()> {
    let repo = fixture()?;
    repo.write("code.rs", "pub fn example() {}\n")?;
    repo.cli_json(&["map"])?;
    let report = repo.cli_json(&["metrics"])?;
    let path = Path::new(report["metricsPath"].as_str().unwrap());
    fs::write(path, "corrupt history")?;
    let output = repo.cli(&["map"])?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not record project metrics"));
    assert_eq!(fs::read_to_string(path)?, "corrupt history");
    assert!(!repo.cli(&["metrics"])?.status.success());
    assert_eq!(
        repo.cli_json(&["metrics", "--reset"])?["totals"]["invocations"],
        0
    );
    Ok(())
}

#[test]
fn metrics_counts_faq_reuse_and_successful_search_results() -> Result<()> {
    let mock = Mock::start()?;
    let repo = fixture()?;
    repo.write(".slopdex/config.json", &mock.config().to_string())?;
    repo.write("code.rs", &function("selected", "VECTOR_EAST"))?;
    let original = repo.cli_json(&["faq", "east original question", "--threshold", "-1"])?;
    let reused = repo.cli_json(&["faq", "east original question", "--threshold", "-1"])?;
    assert_eq!(original["description"], reused["description"]);
    repo.cli_json(&["search-code", "east", "--threshold", "-1", "--limit", "1"])?;
    let calls = mock.count();
    let report = repo.cli_json(&["metrics"])?;
    assert_eq!(report["commands"]["faq"]["invocations"], 2);
    assert_eq!(report["commands"]["faq"]["results"], 2);
    assert_eq!(report["commands"]["faq"]["faqReusedAnswers"], 1);
    assert_eq!(report["commands"]["search-code"]["results"], 1);
    assert_eq!(mock.count(), calls, "metrics called a provider");
    Ok(())
}

#[test]
fn metrics_refactor_counts_applied_files_but_not_dry_run_proposals() -> Result<()> {
    let repo = fixture()?;
    repo.write("code.ts", "export function original() { return 42; }\n")?;
    repo.cli_json(&["refactor", "code.ts::original", "renamed", "--dry-run"])?;
    assert_eq!(repo.cli_json(&["metrics"])?["totals"]["filesChanged"], 0);
    repo.cli_json(&["refactor", "code.ts::original", "renamed"])?;
    let report = repo.cli_json(&["metrics"])?;
    assert_eq!(report["commands"]["refactor"]["invocations"], 2);
    assert_eq!(report["totals"]["dryRuns"], 1);
    assert_eq!(report["totals"]["filesChanged"], 1);
    assert!(fs::read_to_string(repo.root.join("code.ts"))?.contains("renamed"));
    Ok(())
}

#[test]
fn metrics_external_workspaces_are_attributed_once_to_the_inspected_project() -> Result<()> {
    let invoker = fixture()?;
    let external = fixture()?;
    external.init_git()?;
    external.write("one/a.rs", "pub fn first() {}\n")?;
    external.write("two/b.rs", "pub fn second() {}\n")?;
    let one = external.root.join("one");
    let two = external.root.join("two");
    invoker.cli_json(&["map", one.to_str().unwrap(), two.to_str().unwrap()])?;
    assert_eq!(invoker.cli_json(&["metrics"])?["totals"]["invocations"], 0);
    let output = invoker
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .arg("--root")
        .arg(&external.root)
        .args(["metrics", "--output", "json"])
        .output()?;
    ensure!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["commands"]["map"]["invocations"], 1);
    assert_eq!(report["commands"]["map"]["results"], 2);
    Ok(())
}

#[cfg(unix)]
#[test]
fn metrics_symlinked_cwd_uses_the_same_project_identity() -> Result<()> {
    let repo = fixture()?;
    repo.init_git()?;
    repo.write("code.rs", "pub fn example() {}\n")?;
    let alias = repo._temp.path().join("alias");
    std::os::unix::fs::symlink(&repo.root, &alias)?;
    let output = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .current_dir(&alias)
        .args(["map", "--output", "json"])
        .output()?;
    ensure!(output.status.success());
    let report = repo.cli_json(&["metrics"])?;
    assert_eq!(report["totals"]["invocations"], 1);
    assert_eq!(
        Path::new(report["project"].as_str().unwrap()).canonicalize()?,
        repo.root.canonicalize()?
    );
    Ok(())
}
