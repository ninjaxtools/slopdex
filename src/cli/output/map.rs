use super::{
    array,
    render::{markdown_map_bodies, source_code},
};
use crate::{
    cli::{
        args::{Detail, Format},
        io::print_json,
    },
    filter, map,
    parse::StructureNode,
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{collections::HashMap, io::Write};

pub(in crate::cli) fn print_map(
    out: &mut impl Write,
    rows: &[Value],
    format: Format,
    detail: Detail,
    source: Option<&dyn map::StructureSource>,
    selection: Option<&filter::Selection>,
) -> Result<()> {
    if format == Format::Json {
        return print_json(out, &rows);
    }
    for (index, row) in rows.iter().enumerate() {
        let path = row["path"].as_str().context("map result is missing path")?;
        let nodes: Vec<StructureNode> = serde_json::from_value(row["nodes"].clone())
            .with_context(|| format!("decode map nodes for {path}"))?;
        let mut extras: HashMap<usize, String> = array(&row["nodes"])
            .iter()
            .filter_map(|node| {
                let id = usize::try_from(node["id"].as_u64()?).ok()?;
                let calls = node["callees"].as_array()?;
                Some((
                    id,
                    calls
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|call| format!("{}\n", map::comment(path, call)))
                        .collect(),
                ))
            })
            .collect();
        if let Some(source) = source.and_then(|source| source.source(path)) {
            for (node, value) in nodes.iter().zip(array(&row["nodes"])) {
                if value["expandedCode"] == true
                    && let Some(code) = source_code(source, node)
                {
                    extras.entry(node.id).or_default().push_str(&code);
                }
            }
        }
        let full = source
            .map(|source| source.structure(path))
            .transpose()?
            .flatten();
        if detail == Detail::Expanded
            && let Some(source) = source.and_then(|source| source.source(path))
            && let Some(structure) = &full
        {
            let bodies = markdown_map_bodies(source, structure);
            for node in &nodes {
                if node.kind == "heading"
                    && selection.is_none_or(|selection| {
                        selection.kind_matches(&node.kind) && selection.symbol_matches(node)
                    })
                    && let Some(body) = bodies.get(&node.id)
                {
                    extras.entry(node.id).or_default().push_str(body);
                }
            }
        }
        let symbols = if detail == Detail::Expanded {
            source
                .map(|source| source.symbol_descriptions(path))
                .transpose()?
        } else {
            None
        };
        if index > 0 {
            writeln!(out)?;
        }
        write!(
            out,
            "{}",
            map::render_with_hits(
                &nodes,
                Some(path),
                detail.into(),
                source.and_then(|source| source.source(path)),
                full.as_ref(),
                map::Descriptions {
                    file: if detail == Detail::Expanded {
                        source.and_then(|source| source.file_description(path))
                    } else {
                        None
                    },
                    symbols: symbols.as_ref(),
                },
                map::HitDetails {
                    annotations: None,
                    extras: Some(&extras),
                },
            )
        )?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "map_tests.rs"]
mod tests;
