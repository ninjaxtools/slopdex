use super::{
    CrossOutput, Presentation, print_cross, print_errors, print_map, print_search,
    test_support::{edge, function, map_config},
};
use crate::{
    cli::args::{Detail, Format},
    engine::Engine,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Write},
};

#[test]
fn indexed_file_and_callable_descriptions_are_comments_at_the_requested_detail() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = "pub fn run() {}\n";
    fs::write(dir.path().join("api.rs"), source)?;
    let index = dir.path().join("index.sqlite");
    let mut engine = Engine::open_map(dir.path(), &index, map_config(dir.path()))?;
    engine.refresh_structure()?;
    drop(engine);
    let db = rusqlite::Connection::open(&index)?;
    let (identity, data): (String, String) = db.query_row(
        "SELECT identity,data FROM search_units WHERE path='api.rs' AND kind='function'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let data: Value = serde_json::from_str(&data)?;
    let global_path: String = db.query_row(
        "SELECT value FROM metadata WHERE key='global_path'",
        [],
        |row| row.get(0),
    )?;
    db.execute("ATTACH DATABASE ? AS global", [global_path])?;
    let file_description = "Handles API requests.\nIncludes validation.";
    let callable_description = "Runs work.\nReturns a result.";
    for text in [file_description, callable_description] {
        db.execute(
            "INSERT OR IGNORE INTO global.description_content(hash,content) VALUES(?,?)",
            rusqlite::params![crate::hash(text), text],
        )?;
    }
    db.execute("INSERT INTO descriptions(scope,path,identity,source_hash,content_hash,embedding_key) VALUES('file','api.rs','',?,?,NULL)",
        rusqlite::params![crate::hash(source), crate::hash(file_description)])?;
    db.execute("INSERT INTO descriptions(scope,path,identity,source_hash,content_hash,embedding_key) VALUES('callable','api.rs',?,?,?,NULL)",
        rusqlite::params![identity, data["sourceHash"].as_str(), crate::hash(callable_description)])?;
    drop(db);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let engine = loop {
        match Engine::open_map(dir.path(), &index, map_config(dir.path())) {
            Err(error)
                if error.to_string().starts_with("Index is in use")
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            result => break result?,
        }
    };
    let rows = engine.map(&json!({}))?;
    let mut compact = Vec::new();
    print_map(
        &mut compact,
        &rows,
        Format::Summary,
        Detail::Compact,
        Some(&engine),
        None,
    )?;
    assert!(!String::from_utf8(compact)?.contains("Handles API requests."));
    let mut expanded = Vec::new();
    print_map(
        &mut expanded,
        &rows,
        Format::Summary,
        Detail::Expanded,
        Some(&engine),
        None,
    )?;
    let expanded = String::from_utf8(expanded)?;
    assert!(
        expanded.starts_with("*** api.rs\n// Handles API requests.\n// Includes validation.\n\n@@"),
        "{expanded}"
    );
    assert!(
        expanded.contains("pub fn run()  // Runs work. Returns a result.\n"),
        "{expanded}"
    );

    let result = json!({"type":"function", "similarity":0.8, "function":{
        "path":"api.rs", "name":"run", "qualifiedName":"run", "startLine":1,"endLine":1,
        "description":"Runs work.\nReturns a result."}});
    let mut output = Vec::new();
    print_search(
        &mut output,
        std::slice::from_ref(&result),
        Format::Summary,
        Detail::Compact,
        true,
        &mut Presentation::new(&engine),
    )?;
    let output = String::from_utf8(output)?;
    assert!(
        output.starts_with("*** api.rs\n// Handles API requests.\n// Includes validation.\n\n@@"),
        "{output}"
    );
    assert!(
        output.contains("pub fn run()  // score=0.80 | Runs work. Returns a result.\n"),
        "{output}"
    );
    let mut output = Vec::new();
    print_search(
        &mut output,
        &[result],
        Format::Summary,
        Detail::Standard,
        false,
        &mut Presentation::new(&engine),
    )?;
    let output = String::from_utf8(output)?;
    assert!(
        !output.contains("Handles API requests.") && !output.contains("Runs work."),
        "{output}"
    );
    Ok(())
}

#[test]
fn empty_results_obey_array_jsonl_and_plain_output_contracts() {
    let mut out = Vec::new();
    print_search(
        &mut out,
        &[],
        Format::Json,
        Detail::Compact,
        false,
        &mut Presentation::empty(),
    )
    .unwrap();
    assert_eq!(out, b"[]\n");
    out.clear();
    print_errors(&mut out, &[], Format::Json).unwrap();
    assert_eq!(out, b"[]\n");
    out.clear();
    print_search(
        &mut out,
        &[],
        Format::Summary,
        Detail::Compact,
        false,
        &mut Presentation::empty(),
    )
    .unwrap();
    assert_eq!(out, b"No matches.\n");
    out.clear();
    print_errors(&mut out, &[], Format::Summary).unwrap();
    assert_eq!(out, b"No indexing errors.\n");
    for (format, expected) in [
        (Format::Json, ""),
        (Format::Summary, "No matches.\n"),
        (Format::Clusters, "No clusters.\n"),
    ] {
        out.clear();
        print_cross(
            &mut out,
            vec![json!({"source": function("a"), "matches": []})],
            CrossOutput::new(format, true, false, None, Detail::Compact),
            &mut Presentation::empty(),
            None,
        )
        .unwrap();
        assert_eq!(out, expected.as_bytes());
    }
}

#[test]
fn indexing_error_output_retains_diagnostics_and_uses_available_locations() {
    let errors = vec![
        json!({"path": "src/a.rs", "filePath": "old.rs", "startLine": 7, "startColumn": 3,
            "qualifiedName": "Service.run", "name": "run", "message": "unexpected token",
            "sourceMode": "working-tree", "extra": ["kept"]}),
        json!({"filePath": "src/b.rs", "startLine": 2, "name": "fallback", "message": "bad UTF-8"}),
        json!({"message": "read failed\ncaused by: missing file"}),
    ];
    let mut out = Vec::new();
    print_errors(&mut out, &errors, Format::Json).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&out).unwrap(),
        json!(errors)
    );
    out.clear();
    print_errors(&mut out, &errors, Format::Summary).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        concat!(
            "src/a.rs:7:3 :: Service.run  unexpected token\n",
            "src/b.rs:2 :: fallback  bad UTF-8\n",
            "(unknown file)  read failed\ncaused by: missing file\n"
        )
    );
}

#[test]
fn output_failures_are_returned_for_json_summary_and_cluster_writers() {
    struct BrokenPipe;
    impl Write for BrokenPipe {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let rows = vec![edge("a", "b", 0.9)];
    for format in [Format::Json, Format::Summary] {
        assert!(
            print_search(
                &mut BrokenPipe,
                &[json!({"function": function("a")})],
                format,
                Detail::Compact,
                false,
                &mut Presentation::empty()
            )
            .is_err()
        );
        assert!(print_errors(&mut BrokenPipe, &[json!({"message": "failure"})], format).is_err());
        assert!(
            print_cross(
                &mut BrokenPipe,
                rows.clone(),
                CrossOutput::new(format, true, false, None, Detail::Compact),
                &mut Presentation::empty(),
                None
            )
            .is_err()
        );
    }
    assert!(
        print_cross(
            &mut BrokenPipe,
            rows,
            CrossOutput::new(Format::Clusters, true, false, None, Detail::Compact),
            &mut Presentation::empty(),
            None
        )
        .is_err()
    );
}
