use super::{Presentation, print_markdown, rank, score_details};
use crate::{
    cli::{
        args::{CallDepths, Detail, Format},
        output::{array, search::print_search},
    },
    engine::Engine,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::fs;

#[test]
fn symbol_search_renders_saved_nodes_heading_bodies_and_independent_scores() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(
        dir.path().join("api.rs"),
        "pub const TIMEOUT: u64 = 30;\npub type Session = String;\npub fn validate_session() { check(); }\nfn check() {}\n",
    )?;
    fs::write(
        dir.path().join("guide.md"),
        "# Guide\nParent body.\n## Setup\nSaved setup body.\n### Empty\n## Other\nOther body.\n",
    )?;
    let index = dir.path().join("index.sqlite");
    let mut engine = Engine::open_map(dir.path(), &index, json!({}))?;
    engine.refresh_structure()?;
    let symbol = |path: &str, name: &str| -> Result<Value> {
        let structure = engine.presentation_structure(path)?.unwrap();
        let node = structure
            .nodes
            .iter()
            .find(|node| node.name == name)
            .unwrap();
        let mut value = serde_json::to_value(node)?;
        value["path"] = json!(path);
        value["sourceMode"] = json!("working-tree");
        Ok(value)
    };
    let callable = symbol("api.rs", "validate_session")?;
    let rows = vec![
        json!({"type":"symbol", "symbol":symbol("guide.md", "Setup")?, "similarity":0.95, "symbolSimilarity":0.95}),
        json!({"type":"symbol", "symbol":symbol("guide.md", "Empty")?, "similarity":0.8, "symbolSimilarity":0.8}),
        json!({"type":"symbol", "symbol":symbol("api.rs", "TIMEOUT")?, "similarity":0.7, "symbolSimilarity":0.7}),
        json!({"type":"symbol", "symbol":symbol("api.rs", "Session")?, "similarity":0.6, "symbolSimilarity":0.6}),
        json!({"type":"symbol", "symbol":callable, "similarity":0.85, "symbolSimilarity":0.85}),
        json!({"type":"function", "function":callable, "similarity":0.65}),
    ];
    // Presentation must use the saved snapshot, even if the working tree changes.
    fs::write(dir.path().join("guide.md"), "# Replaced\nNew body.\n")?;
    let mut json_out = Vec::new();
    print_search(
        &mut json_out,
        &rows,
        Format::Json,
        Detail::Expanded,
        false,
        &mut Presentation::new(&engine),
    )?;
    assert_eq!(serde_json::from_slice::<Value>(&json_out)?, json!(rows));
    for detail in [Detail::Compact, Detail::Expanded] {
        let mut output = Vec::new();
        print_search(
            &mut output,
            &rows,
            Format::Summary,
            detail,
            false,
            &mut Presentation::new(&engine),
        )?;
        let output = String::from_utf8(output)?;
        assert!(
            output.find("*** guide.md").unwrap() < output.find("*** api.rs").unwrap(),
            "{output}"
        );
        assert_eq!(output.matches("# Guide").count(), 1, "{output}");
        assert!(
            output.contains("## Setup  <!-- symbol score=0.95"),
            "{output}"
        );
        assert!(
            output.contains("### Empty  <!-- symbol score=0.80"),
            "{output}"
        );
        assert!(output.contains("pub const TIMEOUT"), "{output}");
        assert!(output.contains("pub type Session"), "{output}");
        assert_eq!(
            output.matches("pub fn validate_session()").count(),
            1,
            "{output}"
        );
        assert!(
            output.contains("symbol score=0.85") && output.contains(" | score=0.65"),
            "{output}"
        );
        assert_eq!(
            output.contains("Saved setup body."),
            detail == Detail::Expanded,
            "{output}"
        );
        assert_eq!(
            output.contains("[symbol 0.95]"),
            detail == Detail::Expanded,
            "{output}"
        );
        for omitted in [
            "Parent body.",
            "Other body.",
            "## Other",
            "New body.",
            "@ code:",
        ] {
            assert!(!output.contains(omitted), "{omitted}: {output}");
        }
    }
    let mut presentation = Presentation::with_calls(
        &engine,
        CallDepths {
            callees: 1,
            ..CallDepths::default()
        },
        0.9,
    )?;
    for row in &rows[..4] {
        assert!(presentation.key(&row["symbol"]).is_none());
    }
    presentation.prepare(
        &rows[..4]
            .iter()
            .map(|row| &row["symbol"])
            .collect::<Vec<_>>(),
    );
    assert!(presentation.expanded.as_ref().unwrap().depths.is_empty());
    let mut output = Vec::new();
    print_search(
        &mut output,
        &rows,
        Format::Json,
        Detail::Expanded,
        false,
        &mut presentation,
    )?;
    let enriched: Value = serde_json::from_slice(&output)?;
    for row in &array(&enriched)[..4] {
        assert!(row.get("relatedCallables").is_none());
    }
    assert_eq!(enriched[4]["symbol"], rows[4]["symbol"]);
    assert_eq!(
        enriched[4]["relatedCallables"],
        enriched[5]["relatedCallables"]
    );
    assert_eq!(enriched[4]["callees"], enriched[5]["callees"]);
    assert!(!array(&enriched[4]["relatedCallables"]).is_empty());
    assert_eq!(
        score_details(&json!({"symbolSimilarity":0.4639})),
        "  [symbol 0.46]"
    );
    Ok(())
}

#[test]
fn displayed_scores_round_to_two_places() {
    let row = json!({
        "similarity": 0.4639,
        "rerankScore": 0.5321,
        "codeSimilarity": 0.416,
        "descriptionSimilarity": 0.3925,
        "fileDescriptionSimilarity": 0.971
    });
    assert_eq!(rank(&row), "score=0.53 similarity=0.46");
    assert_eq!(
        score_details(&row),
        "  [combined thirds; code 0.42, description 0.39, file 0.97]"
    );
    assert_eq!(rank(&json!({"similarity": 0.996})), "score=1.00");
}

#[test]
fn markdown_search_uses_last_heading_of_chunk() {
    let row = json!({"type":"markdown", "similarity":0.8, "chunk":{
        "path":"guide.md", "startLine":8, "endLine":14,
        "headingPath":["Guide", "Setup"], "content":"# Guide\n\n## Setup\n\nDetails."}});
    let mut output = Vec::new();
    print_markdown(
        &mut output,
        &row,
        Detail::Compact,
        false,
        &mut Presentation::empty(),
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "*** guide.md\n@@ 8-14 @@\n# Guide\n  ## Setup  <!-- score=0.80 -->\n"
    );
}

#[test]
fn standard_search_detail_previews_markdown_and_explicit_descriptions() -> Result<()> {
    let description = json!({"type":"function", "similarity":0.8,
        "function":{"path":"api.rs", "name":"run", "description":"Handles requests.\nChecks permissions."}});
    let mut output = Vec::new();
    print_search(
        &mut output,
        std::slice::from_ref(&description),
        Format::Summary,
        Detail::Standard,
        true,
        &mut Presentation::empty(),
    )?;
    assert!(
        String::from_utf8(output)?
            .contains("run  // score=0.80 | Handles requests. Checks permissions.\n")
    );
    let mut output = Vec::new();
    print_search(
        &mut output,
        &[description],
        Format::Summary,
        Detail::Standard,
        false,
        &mut Presentation::empty(),
    )?;
    assert!(!String::from_utf8(output)?.contains("// Handles requests."));

    let body = format!("{} extra material", "λ".repeat(170));
    let row = json!({"type":"markdown", "similarity":0.8, "chunk":{
        "path":"guide.md", "startLine":1, "endLine":8, "headingPath":["Guide"],
        "content":format!("# Guide\n\n{body}")}});
    let mut output = Vec::new();
    print_markdown(
        &mut output,
        &row,
        Detail::Standard,
        false,
        &mut Presentation::empty(),
    )?;
    let output = String::from_utf8(output)?;
    let preview = output
        .lines()
        .find_map(|line| line.strip_prefix("@ preview: "))
        .unwrap();
    assert_eq!(preview.chars().count(), 160);
    assert!(preview.ends_with('…') && !preview.contains("extra material"));
    let mut expanded = Vec::new();
    print_markdown(
        &mut expanded,
        &row,
        Detail::Expanded,
        false,
        &mut Presentation::empty(),
    )?;
    assert!(String::from_utf8(expanded)?.contains(&body));
    Ok(())
}

#[test]
fn ranked_hits_group_files_and_emit_ancestors_once_in_source_order() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(
        dir.path().join("api.rs"),
        "impl Api {\n  fn write(&self) { first(); }\n  fn flush(&self) { second(); }\n}\n",
    )?;
    fs::write(
        dir.path().join("guide.md"),
        "# Guide\n\n## Setup\nFirst.\n\n### Details\nSecond.\n",
    )?;
    let index = dir.path().join("index.sqlite");
    let mut engine = Engine::open_map(dir.path(), &index, json!({}))?;
    engine.refresh_structure()?;
    let rows = vec![
        json!({"type":"function", "similarity":0.9, "function":{"path":"api.rs", "qualifiedName":"Api.flush", "name":"flush", "startLine":3, "endLine":3}}),
        json!({"type":"markdown", "similarity":0.8, "chunk":{"path":"guide.md", "startLine":6, "endLine":7, "headingPath":["Guide", "Setup", "Details"], "content":"# Guide\n## Setup\n### Details\n\nSecond."}}),
        json!({"type":"function", "similarity":0.7, "function":{"path":"api.rs", "qualifiedName":"Api.write", "name":"write", "startLine":2, "endLine":2}}),
        json!({"type":"markdown", "similarity":0.6, "chunk":{"path":"guide.md", "startLine":3, "endLine":4, "headingPath":["Guide", "Setup"], "content":"# Guide\n## Setup\n\nFirst."}}),
    ];
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
    assert_eq!(output.matches("*** api.rs").count(), 1);
    assert_eq!(output.matches("*** guide.md").count(), 1);
    assert!(output.find("*** api.rs").unwrap() < output.find("*** guide.md").unwrap());
    assert!(output.find("fn write(&self)").unwrap() < output.find("fn flush(&self)").unwrap());
    assert!(output.find("## Setup").unwrap() < output.find("### Details").unwrap());
    assert!(output.contains("## Setup  <!-- score=0.60 -->"), "{output}");
    assert!(
        output.contains("### Details  <!-- score=0.80 -->"),
        "{output}"
    );
    assert!(
        output.contains("@@ 1-4 @@\nimpl Api\n  fn write(&self)  // score=0.70\n  fn flush(&self)  // score=0.90\n"),
        "{output}"
    );
    assert_eq!(output.matches("impl Api").count(), 1);
    assert_eq!(output.matches("# Guide").count(), 1);
    assert!(!output.contains("first()") && !output.contains("second()"));
    Ok(())
}

#[test]
fn search_orders_files_by_best_rerank_score_and_keeps_individual_scores() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(
        dir.path().join("a.rs"),
        "fn first() {}\n\n\n\n\nfn second() {}\n",
    )?;
    fs::write(dir.path().join("b.rs"), "fn other() {}\n")?;
    let index = dir.path().join("index.sqlite");
    let mut engine = Engine::open_map(dir.path(), &index, json!({}))?;
    engine.refresh_structure()?;
    let rows = vec![
        json!({"type":"function", "similarity":0.8, "rerankScore":0.9, "function":{"path":"a.rs", "name":"second", "qualifiedName":"second", "startLine":6, "endLine":6}}),
        json!({"type":"function", "similarity":0.99, "rerankScore":0.7, "function":{"path":"b.rs", "name":"other", "qualifiedName":"other", "startLine":1, "endLine":1}}),
        json!({"type":"function", "similarity":0.4, "rerankScore":0.5, "function":{"path":"a.rs", "name":"first", "qualifiedName":"first", "startLine":1, "endLine":1}}),
    ];
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
    assert!(
        output.find("*** a.rs").unwrap() < output.find("*** b.rs").unwrap(),
        "{output}"
    );
    assert!(
        output.find("fn first()").unwrap() < output.find("fn second()").unwrap(),
        "{output}"
    );
    assert!(
        output.contains("@@ 1-6 @@\nfn first()  // score=0.50 similarity=0.40\nfn second()  // score=0.90 similarity=0.80\n"),
        "{output}"
    );
    assert_eq!(output.matches("*** a.rs").count(), 1);
    assert!(output.contains("score=0.50 similarity=0.40"), "{output}");
    assert!(output.contains("score=0.90 similarity=0.80"), "{output}");
    Ok(())
}
