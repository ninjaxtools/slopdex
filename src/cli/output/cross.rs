use super::{
    array,
    clusters::print_clusters,
    number,
    render::{
        FunctionDisplay, Presentation, RankedHit, print_function, print_ranked_files, rank,
        score_details,
    },
};
use crate::cli::args::{Detail, Format};
use anyhow::Result;
use serde_json::{Value, json};
use std::io::Write;

#[derive(Clone, Copy)]
pub(in crate::cli) struct CrossOutput {
    format: Format,
    same_index: bool,
    cohesion: bool,
    limit: Option<usize>,
    detail: Detail,
}

impl CrossOutput {
    pub(in crate::cli) fn new(
        format: Format,
        same_index: bool,
        cohesion: bool,
        limit: Option<usize>,
        detail: Detail,
    ) -> Self {
        Self {
            format,
            same_index,
            cohesion,
            limit,
            detail,
        }
    }
}

pub(in crate::cli) fn print_cross(
    out: &mut impl Write,
    mut rows: Vec<Value>,
    options: CrossOutput,
    source_presentation: &mut Presentation<'_>,
    mut target_presentation: Option<&mut Presentation<'_>>,
) -> Result<bool> {
    let CrossOutput {
        format,
        same_index,
        cohesion,
        limit,
        detail,
    } = options;
    rows.retain(|row| !array(&row["matches"]).is_empty());
    let omitted = limit.is_some_and(|limit| rows.len() > limit);
    if format == Format::Json && source_presentation.calls_enabled() {
        for row in &mut rows {
            row["relatedCallables"] = json!(source_presentation.related_json(&row["source"]));
            row["callees"] = json!(source_presentation.outgoing_json(&row["source"]));
            for item in row["matches"].as_array_mut().into_iter().flatten() {
                let view = target_presentation
                    .as_deref()
                    .unwrap_or(source_presentation);
                item["relatedCallables"] = json!(view.related_json(&item["function"]));
                item["callees"] = json!(view.outgoing_json(&item["function"]));
            }
        }
    }
    if format == Format::Clusters {
        return print_clusters(out, &rows, same_index, limit);
    }
    if cohesion {
        for row in &mut rows {
            if let Some(matches) = row["matches"].as_array_mut() {
                matches.sort_by(|a, b| {
                    number(b, "physicalDistance")
                        .total_cmp(&number(a, "physicalDistance"))
                        .then_with(|| number(b, "similarity").total_cmp(&number(a, "similarity")))
                });
            }
        }
    }
    if rows.is_empty() && format == Format::Summary {
        writeln!(out, "No matches.")?;
    }
    if format == Format::Summary && !rows.is_empty() {
        let mut hits = Vec::new();
        for row in rows.iter().take(limit.unwrap_or(usize::MAX)) {
            let matches = array(&row["matches"]);
            hits.push(RankedHit {
                item: &row["source"],
                row,
                annotation: "source".to_owned(),
                score: matches
                    .iter()
                    .map(|item| number(item, "similarity"))
                    .fold(f64::NEG_INFINITY, f64::max),
                markdown: false,
                description: false,
                target: false,
            });
            for item in matches {
                let distance = item["physicalDistance"]
                    .as_f64()
                    .map(|d| format!(" distance={d}"))
                    .unwrap_or_default();
                hits.push(RankedHit {
                    item: &item["function"],
                    row: item,
                    annotation: format!(
                        "{}{}{distance}{}",
                        if same_index { "" } else { "target " },
                        rank(item),
                        if detail == Detail::Expanded {
                            score_details(item)
                        } else {
                            String::new()
                        }
                    ),
                    score: number(item, "similarity"),
                    markdown: false,
                    description: false,
                    target: !same_index,
                });
            }
        }
        print_ranked_files(out, hits, detail, source_presentation, target_presentation)?;
        return Ok(omitted);
    }
    for (index, row) in rows.iter().take(limit.unwrap_or(usize::MAX)).enumerate() {
        if format == Format::Json {
            serde_json::to_writer(&mut *out, row)?;
            writeln!(out)?;
        } else {
            if index > 0 {
                writeln!(out)?;
            }
            writeln!(out, "Source")?;
            let source_score = array(&row["matches"])
                .iter()
                .filter_map(|item| item["similarity"].as_f64())
                .reduce(f64::max);
            print_function(
                out,
                &row["source"],
                "",
                detail,
                FunctionDisplay {
                    description: false,
                    similarity: source_score,
                },
                source_presentation,
            )?;
            for item in array(&row["matches"]) {
                let distance = item["physicalDistance"]
                    .as_f64()
                    .map(|d| format!(" distance={d}"))
                    .unwrap_or_default();
                writeln!(out)?;
                let annotation = format!(
                    "{}{}{distance}{}",
                    if same_index { "" } else { "target " },
                    rank(item),
                    if detail == Detail::Expanded {
                        score_details(item)
                    } else {
                        String::new()
                    }
                );
                if let Some(target) = target_presentation.as_deref_mut() {
                    print_function(
                        out,
                        &item["function"],
                        &annotation,
                        detail,
                        FunctionDisplay {
                            description: false,
                            similarity: item["similarity"].as_f64(),
                        },
                        target,
                    )?;
                } else {
                    print_function(
                        out,
                        &item["function"],
                        &annotation,
                        detail,
                        FunctionDisplay {
                            description: false,
                            similarity: item["similarity"].as_f64(),
                        },
                        source_presentation,
                    )?;
                }
            }
        }
    }
    Ok(omitted)
}

#[cfg(test)]
#[path = "cross_tests.rs"]
mod tests;
