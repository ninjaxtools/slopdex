use super::*;

#[test]
fn command_keys_are_canonical_and_administrative_commands_are_not_counted() {
    use crate::cli::test_support::parse;
    assert_eq!(
        command_name(&parse(&["search-md", "query"]).command),
        Some("search-docs")
    );
    assert_eq!(
        command_name(&parse(&["index", "errors"]).command),
        Some("index errors")
    );
    for args in [
        vec!["metrics"],
        vec!["metrics", "--reset"],
        vec!["help"],
        vec!["config"],
        vec!["cache", "prune"],
    ] {
        assert_eq!(command_name(&parse(&args).command), None);
    }
}

#[test]
fn lifetime_totals_survive_daily_retention_and_clock_changes() {
    let mut history = History::default();
    let counts = Counts {
        invocations: 1,
        succeeded: 1,
        results: 3,
        ..Counts::default()
    };
    history.add("map", &counts, 100 * 86400);
    history.add("map", &counts, 200 * 86400);
    history.add("lines", &counts, 199 * 86400);
    assert_eq!(history.totals().invocations, 3);
    assert_eq!(history.totals().results, 9);
    assert_eq!(history.days.len(), 2);
    assert_eq!(history.first_used_at_unix, Some(100 * 86400));
    assert_eq!(history.last_used_at_unix, Some(200 * 86400));
    history.prune_days(300 * 86400);
    assert!(history.days.is_empty());
    assert_eq!(history.totals().invocations, 3);
}

#[test]
fn usage_counts_answers_and_file_lines_without_retaining_content() {
    let mut usage = Usage::default();
    usage.faq(
        &serde_json::json!([
            {"description": "private answer"},
            {"description": "another private answer"}
        ]),
        1,
    );
    usage.line_files(&[(PathBuf::from("private.rs"), 12)]);
    assert_eq!(usage.results, Some(3));
    assert_eq!(usage.faq_reused_answers, 1);
    assert_eq!(usage.lines_counted, 12);
}

#[test]
fn corrupt_or_future_history_is_not_overwritten_and_reset_can_repair_it() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("usage.json");
    fs::write(&path, "not json")?;
    assert!(load(&path).is_err());
    assert_eq!(fs::read_to_string(&path)?, "not json");
    save(
        &path,
        &History {
            version: 2,
            ..History::default()
        },
    )?;
    assert!(
        load(&path)
            .unwrap_err()
            .to_string()
            .contains("Unsupported metrics version")
    );
    save(&path, &History::default())?;
    assert_eq!(load(&path)?.totals().invocations, 0);
    Ok(())
}

#[test]
fn concurrent_cold_and_warm_updates_preserve_every_increment() -> Result<()> {
    use std::sync::{Arc, Barrier, mpsc};
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("nested/usage.json");
    let barrier = Arc::new(Barrier::new(4));
    let (tx, rx) = mpsc::channel();
    let mut workers = Vec::new();
    for _ in 0..4 {
        let path = path.clone();
        let barrier = barrier.clone();
        let tx = tx.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            let result = (|| -> Result<()> {
                for _ in 0..5 {
                    let _lock = lock(&path)?;
                    let mut history = load(&path)?;
                    history.add(
                        "map",
                        &Counts {
                            invocations: 1,
                            succeeded: 1,
                            ..Counts::default()
                        },
                        86400,
                    );
                    save(&path, &history)?;
                }
                Ok(())
            })();
            let _ = tx.send(result);
        }));
    }
    drop(tx);
    let results: Vec<_> = (0..4)
        .map(|_| rx.recv_timeout(Duration::from_secs(10)))
        .collect();
    let joined: Vec<_> = workers.into_iter().map(|worker| worker.join()).collect();
    for worker in joined {
        assert!(worker.is_ok(), "metrics worker panicked");
    }
    for result in results {
        result??;
    }
    assert_eq!(load(&path)?.totals().invocations, 20);
    Ok(())
}

#[test]
fn metrics_wait_is_bounded_and_reads_do_not_need_writer_lock() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("usage.json");
    let _lock = lock(&path)?;
    save(&path, &History::default())?;
    let started = Instant::now();
    assert!(lock(&path).is_err());
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(load(&path)?.totals().invocations, 0);
    Ok(())
}
