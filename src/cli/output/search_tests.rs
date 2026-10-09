use super::print_search;
use crate::{
    cli::{
        args::{Detail, Format},
        output::{render::Presentation, test_support::map_config},
    },
    engine::Engine,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::fs;

#[test]
fn search_outputs_preserve_json_metadata_and_explain_summary_scores_and_content() {
    let rows = vec![
        json!({"type": "code", "function": {"path": "src/λ.rs", "name": "fallback",
            "qualifiedName": "Service.run", "description": "Purpose\nsecond line"},
            "similarity": 0.6, "rerankScore": 0.95, "codeSimilarity": 0.3,
            "descriptionSimilarity": 0.6, "fileDescriptionSimilarity": 0.9,
            "custom": {"escaped": "\"quoted\"\n"}}),
        json!({"type": "markdown", "similarity": 0.7, "chunk": {"path": "guide.md",
            "startLine": 12, "headingPath": ["Setup", "Credentials"], "content": "Use `KEY`.\nNext step."}}),
        json!({"type": "description", "similarity": 0.4, "descriptionSimilarity": 0.2,
            "fileDescriptionSimilarity": 0.6, "function": {"path": "other.rs", "name": "fallback"}}),
    ];
    let mut out = Vec::new();
    print_search(
        &mut out,
        &rows,
        Format::Json,
        Detail::Compact,
        false,
        &mut Presentation::empty(),
    )
    .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&out).unwrap(), json!(rows));
    assert!(out.ends_with(b"\n"));
    out.clear();
    print_search(
        &mut out,
        &rows,
        Format::Summary,
        Detail::Compact,
        false,
        &mut Presentation::empty(),
    )
    .unwrap();
    let summary = String::from_utf8(out).unwrap();
    assert_eq!(
        summary,
        concat!(
            "*** src/λ.rs\n@@ 1 @@\nService.run  // score=0.95 similarity=0.60\n",
            "\n*** guide.md\n@@ 12 @@\n# Setup\n  ## Credentials  <!-- score=0.70 -->\n",
            "\n*** other.rs\n@@ 1 @@\nfallback  // score=0.40\n"
        )
    );
    let mut out = Vec::new();
    print_search(
        &mut out,
        &rows,
        Format::Summary,
        Detail::Expanded,
        false,
        &mut Presentation::empty(),
    )
    .unwrap();
    let expanded = String::from_utf8(out).unwrap();
    assert!(
        expanded.contains("Service.run  // score=0.95"),
        "{expanded}"
    );
    assert!(expanded.contains(" | Purpose second line\n"), "{expanded}");
    assert!(expanded.contains("Use `KEY`.\nNext step."));
    assert!(summary.len() < expanded.len());
}

#[test]
fn search_uses_indexed_declaration_instead_of_callable_source() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(
        dir.path().join("api.rs"),
        "pub struct Api;\nimpl Api {\n  pub fn run(&self) { secret(); }\n}\n",
    )?;
    let index = dir.path().join("index.sqlite");
    let mut engine = Engine::open_map(dir.path(), &index, map_config(dir.path()))?;
    engine.refresh_structure()?;
    let rows = vec![json!({"type":"function", "similarity":0.9, "function":{
        "path":"api.rs", "qualifiedName":"Api.run", "name":"run", "startLine":3,
        "endLine":3, "source":"pub fn run(&self) { secret(); }"}})];
    let mut output = Vec::new();
    print_search(
        &mut output,
        &rows,
        Format::Summary,
        Detail::Compact,
        false,
        &mut Presentation::new(&engine),
    )?;
    let output = String::from_utf8(output)?;
    assert_eq!(
        output,
        "*** api.rs\n\n@@ 2-4 @@\nimpl Api\n  pub fn run(&self)  // score=0.90\n"
    );
    assert!(!output.contains("secret") && !output.contains('{'));
    Ok(())
}

#[test]
fn file_only_search_renders_header_scores_and_prose_without_callable_hunks() -> Result<()> {
    let rows = vec![json!({"type":"file", "file":{
        "path":"guide.md", "description":"Explains setup.\nLists credentials.",
        "sourceMode":"working-tree", "language":"markdown"
    }, "similarity":0.82, "fileDescriptionSimilarity":0.82, "rerankScore":0.93})];
    for detail in [Detail::Compact, Detail::Standard, Detail::Expanded] {
        for descriptions in [false, true] {
            let mut out = Vec::new();
            print_search(
                &mut out,
                &rows,
                Format::Json,
                detail,
                descriptions,
                &mut Presentation::empty(),
            )?;
            assert_eq!(serde_json::from_slice::<Value>(&out)?, json!(rows));
            out.clear();
            print_search(
                &mut out,
                &rows,
                Format::Summary,
                detail,
                descriptions,
                &mut Presentation::empty(),
            )?;
            let output = String::from_utf8(out)?;
            let scores = if detail == Detail::Expanded {
                " [file 0.82]"
            } else {
                ""
            };
            let prose = if detail != Detail::Compact || descriptions {
                "<!-- Explains setup. -->\n<!-- Lists credentials. -->\n"
            } else {
                ""
            };
            assert_eq!(
                output,
                format!("*** guide.md  <!-- score=0.93 similarity=0.82{scores} -->\n{prose}")
            );
            assert!(!output.contains("@@") && !output.contains("null"));
        }
    }
    Ok(())
}

#[test]
fn file_hits_group_with_callable_and_symbol_hits_and_preserve_json_with_calls() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(
        dir.path().join("api.rs"),
        "pub const TIMEOUT: u64 = 30;\npub fn run() { check(); }\nfn check() {}\n",
    )?;
    fs::write(dir.path().join("other.rs"), "pub fn other() {}\n")?;
    let index = dir.path().join("index.sqlite");
    let mut engine = Engine::open_map(dir.path(), &index, map_config(dir.path()))?;
    engine.refresh_structure()?;
    let structure = engine.presentation_structure("api.rs")?.unwrap();
    let mut symbol = serde_json::to_value(
        structure
            .nodes
            .iter()
            .find(|node| node.name == "run")
            .unwrap(),
    )?;
    symbol["path"] = json!("api.rs");
    let rows = vec![
        json!({"type":"function", "similarity":0.9, "function":{
            "path":"other.rs", "qualifiedName":"other", "name":"other", "startLine":1, "endLine":1}}),
        json!({"type":"symbol", "symbol":symbol, "similarity":0.6, "symbolSimilarity":0.6}),
        json!({"type":"file", "file":{
            "path":"api.rs", "description":"Handles requests.\nChecks permissions.",
            "sourceMode":"working-tree", "language":"rust"
        }, "similarity":0.98, "fileDescriptionSimilarity":0.98}),
        json!({"type":"function", "function":symbol, "similarity":0.5}),
    ];
    let calls = crate::cli::args::CallDepths {
        callees: 1,
        ..Default::default()
    };
    for detail in [Detail::Compact, Detail::Standard, Detail::Expanded] {
        let mut out = Vec::new();
        print_search(
            &mut out,
            &rows,
            Format::Json,
            detail,
            false,
            &mut Presentation::with_calls(&engine, calls, 0.9)?,
        )?;
        let enriched: Value = serde_json::from_slice(&out)?;
        assert_eq!(enriched[2], rows[2]);
        assert_eq!(enriched[1]["symbol"], rows[1]["symbol"]);
        assert_eq!(enriched[3]["function"], rows[3]["function"]);
        assert_eq!(
            enriched[1]["relatedCallables"],
            enriched[3]["relatedCallables"]
        );
        assert_eq!(enriched[1]["callees"], json!(["api.rs:3:check"]));
        out.clear();
        print_search(
            &mut out,
            &rows,
            Format::Summary,
            detail,
            false,
            &mut Presentation::with_calls(&engine, calls, 0.9)?,
        )?;
        let output = String::from_utf8(out)?;
        assert!(output.starts_with("*** api.rs  // score=0.98"), "{output}");
        assert_eq!(output.matches("*** api.rs").count(), 1, "{output}");
        assert_eq!(output.matches("pub fn run()").count(), 1, "{output}");
        assert!(
            output.contains("symbol score=0.60") && output.contains("score=0.50"),
            "{output}"
        );
        assert_eq!(
            output.contains("// Handles requests."),
            detail != Detail::Compact,
            "{output}"
        );
        assert_eq!(
            output.matches("Handles requests.").count(),
            usize::from(detail != Detail::Compact)
        );
        assert!(output.contains("callees: api.rs:3:check"), "{output}");
        assert!(!output.contains("pub const TIMEOUT"), "{output}");
    }
    let context = super::describe_search_context(&engine, &rows, 0, 0, 0, 0, 0.9)?;
    assert!(context.contains("*** api.rs  // score=0.98"));
    assert!(context.contains("// Handles requests.\n// Checks permissions."));
    Ok(())
}

#[test]
fn explicit_file_descriptions_share_one_header_with_unmatched_callable_hits() -> Result<()> {
    let rows = vec![
        json!({"type":"function", "function":{
            "path":"api.py", "name":"run", "startLine":4, "endLine":5,
            "description":"Handles a request."
        }, "similarity":0.7}),
        json!({"type":"file", "file":{
            "path":"api.py", "description":"Request routing.",
            "sourceMode":"working-tree", "language":"python"
        }, "similarity":0.8, "fileDescriptionSimilarity":0.8}),
    ];
    let mut out = Vec::new();
    print_search(
        &mut out,
        &rows,
        Format::Summary,
        Detail::Compact,
        true,
        &mut Presentation::empty(),
    )?;
    assert_eq!(
        String::from_utf8(out)?,
        "*** api.py  # score=0.80\n# Request routing.\n\n@@ 4-5 @@\nrun  # score=0.70 | Handles a request.\n"
    );
    Ok(())
}
