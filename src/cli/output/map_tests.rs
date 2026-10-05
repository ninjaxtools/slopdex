use super::print_map;
use crate::{
    cli::args::{Detail, Format},
    filter, map,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{collections::HashSet, fs};

#[test]
fn semantic_heading_selection_expands_only_direct_match_bodies() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(
        dir.path().join("guide.md"),
        "# Guide\nParent body.\n## Setup\nSelected body.\n### Linux\nChild body.\n## Other\nOther body.\n",
    )?;
    let options = json!({"symbolQuery": ["setup"], "regexp": "^Guide\\.Setup$"});
    let source = map::Unindexed::parse(
        dir.path(),
        &dir.path().join("index.sqlite"),
        &json!({}),
        &options,
    )?;
    let selection =
        filter::Selection::compile(&options)?.with_symbol_names(HashSet::from(["setup".into()]));
    let structure = map::StructureSource::structure(&source, "guide.md")?.unwrap();
    let rows = vec![json!({"path": "guide.md", "nodes": selection.select_structure(&structure)})];
    let mut output = Vec::new();
    print_map(
        &mut output,
        &rows,
        Format::Summary,
        Detail::Expanded,
        Some(&source),
        Some(&selection),
    )?;
    let output = String::from_utf8(output)?;
    assert!(
        output.contains("# Guide") && output.contains("## Setup"),
        "{output}"
    );
    assert!(output.contains("Selected body."), "{output}");
    for omitted in [
        "Parent body.",
        "Child body.",
        "Other body.",
        "### Linux",
        "## Other",
    ] {
        assert!(!output.contains(omitted), "{output}");
    }
    Ok(())
}

#[test]
fn map_json_is_a_full_array_and_summary_uses_structure_renderer() {
    let structure = crate::parse::parse(
        "example.rs",
        "pub struct Service { pub count: usize }\npub fn run() {}\n",
    )
    .unwrap()
    .structure;
    let rows = vec![json!({"path": "example.rs", "nodes": structure.nodes})];
    let mut json_output = Vec::new();
    print_map(
        &mut json_output,
        &rows,
        Format::Json,
        Detail::Compact,
        None,
        None,
    )
    .unwrap();
    let decoded: Value = serde_json::from_slice(&json_output).unwrap();
    assert_eq!(decoded, json!(rows));
    assert!(decoded[0]["nodes"][0].get("startByte").is_some());
    let mut summary = Vec::new();
    print_map(
        &mut summary,
        &rows,
        Format::Summary,
        Detail::Compact,
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(summary).unwrap(),
        map::render_nodes(&structure.nodes, Some("example.rs"))
    );
    let mut empty = Vec::new();
    print_map(&mut empty, &[], Format::Json, Detail::Compact, None, None).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&empty).unwrap(), json!([]));
}
