use super::{test_support::parse, *};

#[test]
fn symbol_search_requires_existing_index_even_without_refresh() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let index = dir.path().join("missing.sqlite");
    for command in [
        vec!["search-symbols", "query"],
        vec!["search", "query", "--symbols"],
    ] {
        for no_reindex in [false, true] {
            let mut argv = command.clone();
            argv.extend([
                "--root",
                dir.path().to_str().unwrap(),
                "--index",
                index.to_str().unwrap(),
            ]);
            if no_reindex {
                argv.push("--no-reindex");
            }
            let mut out = Vec::new();
            let error = run_cli(&parse(&argv), &mut out).unwrap_err();
            assert!(
                error.to_string().contains("run `slopdex update` first"),
                "{error}"
            );
            assert!(out.is_empty());
            assert!(!index.exists());
        }
    }
    Ok(())
}
