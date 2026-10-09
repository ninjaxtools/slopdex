use super::{CrossOutput, print_cross};
use crate::cli::{
    args::{Detail, Format},
    output::{
        render::Presentation,
        test_support::{edge, function, map_config},
    },
};
use crate::engine::Engine;
use anyhow::Result;
use serde_json::{Value, json};
use std::fs;

#[test]
fn cohesion_is_distance_first_and_json_is_one_object_per_source() {
    let rows = vec![
        json!({"source": function("a"), "matches": [
            {"function": function("b"), "similarity": 0.99, "physicalDistance": 1},
            {"function": function("c"), "similarity": 0.91, "physicalDistance": 4},
            {"function": function("d"), "similarity": 0.95, "physicalDistance": 4}
        ]}),
        edge("x", "y", 0.9),
    ];
    let mut out = Vec::new();
    print_cross(
        &mut out,
        rows,
        CrossOutput::new(Format::Json, true, true, Some(1), Detail::Compact),
        &mut Presentation::empty(),
        None,
    )
    .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert_eq!(output.lines().count(), 1);
    let row: Value = serde_json::from_str(output.trim()).unwrap();
    assert_eq!(row["matches"][0]["function"]["id"], "d");
    assert_eq!(row["matches"][1]["function"]["id"], "c");
}

#[test]
fn cross_summary_groups_sources_and_matches_from_the_same_file() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(
        dir.path().join("api.rs"),
        "impl Api {\n  fn first(&self) {}\n  fn second(&self) {}\n}\n",
    )?;
    let index = dir.path().join("index.sqlite");
    let mut engine = Engine::open_map(dir.path(), &index, map_config(dir.path()))?;
    engine.refresh_structure()?;
    let first = json!({"path":"api.rs", "qualifiedName":"Api.first", "name":"first", "startLine":2,"endLine":2});
    let second = json!({"path":"api.rs", "qualifiedName":"Api.second", "name":"second", "startLine":3,"endLine":3});
    let rows = vec![json!({"source":second, "matches":[{"function":first, "similarity":0.91}]})];
    let mut output = Vec::new();
    print_cross(
        &mut output,
        rows,
        CrossOutput::new(Format::Summary, true, false, None, Detail::Compact),
        &mut Presentation::new(&engine),
        None,
    )?;
    let output = String::from_utf8(output)?;
    assert_eq!(output.matches("*** api.rs").count(), 1, "{output}");
    assert_eq!(output.matches("impl Api").count(), 1, "{output}");
    assert!(
        output.find("fn first(&self)").unwrap() < output.find("fn second(&self)").unwrap(),
        "{output}"
    );
    assert!(
        output.contains("fn first(&self)  // score=0.91"),
        "{output}"
    );
    assert!(output.contains("fn second(&self)  // source"), "{output}");
    Ok(())
}

#[test]
fn cross_jsonl_limits_matched_sources_without_truncating_edges_or_metadata() {
    let mut first = edge("a", "b", 0.8);
    first["source"]["description"] = json!("line one\n\"line two\" λ");
    first["scoring"] = json!({"mode": "combined", "weights": [1, 1, 1]});
    first["matches"]
        .as_array_mut()
        .unwrap()
        .push(json!({"function": function("c"),
        "similarity": 0.9, "descriptionSimilarity": 0.8, "fileDescriptionSimilarity": 1.0}));
    let rows = vec![
        json!({"source": function("empty"), "matches": []}),
        first.clone(),
        edge("x", "y", 0.7),
    ];
    let mut out = Vec::new();
    print_cross(
        &mut out,
        rows.clone(),
        CrossOutput::new(Format::Json, false, false, Some(1), Detail::Compact),
        &mut Presentation::empty(),
        None,
    )
    .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert_eq!(output.lines().count(), 1);
    assert!(output.ends_with('\n'));
    assert_eq!(serde_json::from_str::<Value>(&output).unwrap(), first);
    let mut out = Vec::new();
    print_cross(
        &mut out,
        rows,
        CrossOutput::new(Format::Summary, false, false, Some(1), Detail::Compact),
        &mut Presentation::empty(),
        None,
    )
    .unwrap();
    let output = String::from_utf8(out).unwrap();
    assert!(
        output.starts_with("*** src/a.rs\n@@ 1 @@\na  // source\n"),
        "{output}"
    );
    assert!(
        output.contains("*** src/a.rs\n@@ 1 @@\na  // source\n"),
        "{output}"
    );
    assert!(output.contains("*** src/b.rs\n@@ 1 @@\nb  // target score=0.80"));
    assert!(output.contains("*** src/c.rs\n@@ 1 @@\nc  // target score=0.90"));
    assert!(!output.contains("src/x.rs"));
}
