use super::print_search;
use crate::{
    cli::{
        args::{Detail, Format},
        output::render::Presentation,
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
    let mut engine = Engine::open_map(dir.path(), &index, json!({}))?;
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
