use super::{
    number,
    render::{Presentation, RankedHit, print_ranked_files, rank, score_details},
};
use crate::cli::{
    args::{Detail, Format},
    io::print_json,
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

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
