use super::{
    array,
    render::{markdown_map_bodies, source_code},
};
#[cfg(test)]
use crate::cli::{args::Format, io::print_json};
use crate::{cli::args::Detail, filter, map, parse::StructureNode};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{collections::HashMap, io::Write};

#[cfg(test)]
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
        print_map_row(out, row, index > 0, detail, source, None, selection)?;
    }
    Ok(())
}

/// Print one map file's row. `full` supplies the file's complete structure
/// when the caller already holds it (streamed map output), avoiding a
/// second load; otherwise it is fetched from `source`.
pub(in crate::cli) fn print_map_row(
    out: &mut impl Write,
    row: &Value,
    separator: bool,
    detail: Detail,
    source: Option<&dyn map::StructureSource>,
    full: Option<&crate::parse::FileStructure>,
    selection: Option<&filter::Selection>,
) -> Result<()> {
    {
        let path = row["path"].as_str().context("map result is missing path")?;
        let nodes: Vec<StructureNode> = serde_json::from_value(row["nodes"].clone())
            .with_context(|| format!("decode map nodes for {path}"))?;
        let references: HashMap<usize, Vec<String>> = array(&row["nodes"])
            .iter()
            .filter_map(|node| {
                let id = usize::try_from(node["id"].as_u64()?).ok()?;
                let references = node["references"].as_array()?;
                Some((
                    id,
                    references
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                ))
            })
            .collect();
        let mut extras = HashMap::<usize, String>::new();
        if let Some(source) = source.and_then(|source| source.source(path)) {
            for (node, value) in nodes.iter().zip(array(&row["nodes"])) {
                if value["expandedCode"] == true
                    && let Some(code) = source_code(source, node)
                {
                    extras.entry(node.id).or_default().push_str(&code);
                }
            }
        }
        let fetched = if full.is_none() {
            source
                .map(|source| source.structure(path))
                .transpose()?
                .flatten()
        } else {
            None
        };
        let full = full.or(fetched.as_deref());
        if detail == Detail::Expanded
            && let Some(source) = source.and_then(|source| source.source(path))
            && let Some(structure) = full
        {
            let bodies = markdown_map_bodies(source, structure);
            for node in &nodes {
                if node.kind == "heading"
                    && selection.is_none_or(|selection| {
                        selection.kind_matches(&node.kind)
                            && selection.symbol_matches_at(path, node)
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
        if separator {
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
                full,
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
                    references: Some(&references),
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
