use super::{
    number,
    render::{Presentation, RankedHit, print_ranked_files, rank, score_details},
};
use crate::{
    callgraph::Refs,
    cli::{
        args::{CallDepths, Detail, Format},
        io::print_json,
    },
    engine::Engine,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::io::Write;

pub(in crate::cli) fn print_search(
    out: &mut impl Write,
    rows: &[Value],
    format: Format,
    detail: Detail,
    descriptions: bool,
    presentation: &mut Presentation<'_>,
) -> Result<()> {
    if format == Format::Json {
        if !presentation.calls_enabled() {
            return print_json(out, &rows);
        }
        let mut rows = rows.to_vec();
        for row in &mut rows {
            let item = if row["type"] == "function" {
                Some(&row["function"])
            } else if row["type"] == "symbol" && presentation.key(&row["symbol"]).is_some() {
                Some(&row["symbol"])
            } else {
                None
            };
            if let Some(item) = item {
                let related = presentation.related_json(item);
                let outgoing = presentation.outgoing_json(item);
                row["relatedCallables"] = json!(related);
                row["references"] = json!(outgoing);
            }
        }
        return print_json(out, &rows);
    }
    if rows.is_empty() {
        writeln!(out, "No matches.")?;
    }
    print_ranked_files(
        out,
        rows.iter()
            .map(|row| {
                let markdown = row["type"] == "markdown" || row["type"] == "document";
                RankedHit {
                    item: if markdown {
                        &row["chunk"]
                    } else if row["type"] == "symbol" {
                        &row["symbol"]
                    } else if row["type"] == "file" {
                        &row["file"]
                    } else {
                        &row["function"]
                    },
                    row,
                    annotation: format!(
                        "{}{}{}",
                        if row["type"] == "symbol" {
                            "symbol "
                        } else {
                            ""
                        },
                        rank(row),
                        if detail == Detail::Expanded {
                            score_details(row)
                        } else {
                            String::new()
                        }
                    ),
                    score: row["rerankScore"]
                        .as_f64()
                        .unwrap_or_else(|| number(row, "similarity")),
                    markdown,
                    description: descriptions,
                    target: false,
                }
            })
            .collect(),
        detail,
        presentation,
        None,
    )
}

pub(crate) use super::render::RenderedSymbol;

/// FAQ uses expanded search presentation; dependencies include traversal and
/// reference targets, and rendered symbols are what the LLM may later cite.
pub(crate) fn faq_search_context(
    engine: &Engine,
    rows: &[Value],
    depths: (usize, usize, usize, usize),
    refs: Refs,
    expand_code_threshold: f64,
) -> Result<(
    String,
    std::collections::BTreeSet<String>,
    Vec<RenderedSymbol>,
)> {
    let (callers, callees, expand_callers, expand_callees) = depths;
    let mut out = Vec::new();
    let mut presentation = Presentation::with_calls(
        engine,
        CallDepths {
            callers,
            callees,
            refs,
            expand_callers,
            expand_callees,
        },
        expand_code_threshold,
    )?;
    presentation.record_symbols = true;
    print_search(
        &mut out,
        rows,
        Format::Summary,
        Detail::Expanded,
        false,
        &mut presentation,
    )?;
    let mut symbols: Vec<_> = presentation
        .rendered_symbols
        .into_inner()
        .into_iter()
        .collect();
    symbols.sort_by(|a, b| {
        (&a.path, a.start_line, &a.qualified_name).cmp(&(&b.path, b.start_line, &b.qualified_name))
    });
    Ok((
        String::from_utf8(out)?,
        presentation.dependencies.into_inner(),
        symbols,
    ))
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
