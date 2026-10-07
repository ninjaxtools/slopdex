use super::{array, number, text};
use crate::{
    callgraph::{CallGraph, Expansion, Key},
    cli::args::{CallDepths, Detail},
    engine::Engine,
    map,
    parse::{FileStructure, StructureNode},
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::Write,
};

pub(super) fn source_code(source: &str, node: &StructureNode) -> Option<String> {
    let code = source.get(node.start_byte..node.end_byte)?;
    (!code.trim().is_empty()).then(|| format!("@ code:\n{code}\n"))
}

/// Only the body immediately following each heading: child sections are rendered
/// under their own headings, and omitted headings still delimit filtered bodies.
pub(super) fn markdown_map_bodies(
    source: &str,
    structure: &FileStructure,
) -> HashMap<usize, String> {
    let lines: Vec<_> = source.lines().collect();
    let mut headings: Vec<_> = structure
        .nodes
        .iter()
        .filter(|node| node.kind == "heading")
        .collect();
    headings.sort_by_key(|node| node.start_line);
    headings
        .iter()
        .enumerate()
        .filter_map(|(index, node)| {
            let start = node.start_line - 1 + node.signature.lines().count();
            let end = headings
                .get(index + 1)
                .map_or(lines.len(), |next| next.start_line - 1);
            let body = lines.get(start..end)?.join("\n");
            (!body.trim().is_empty()).then(|| (node.id, format!("{}\n", body.trim_matches('\n'))))
        })
        .collect()
}

pub(super) fn rank(row: &Value) -> String {
    let similarity = number(row, "similarity");
    if let Some(rerank) = row["rerankScore"].as_f64() {
        format!("score={rerank:.2} similarity={similarity:.2}")
    } else {
        format!("score={similarity:.2}")
    }
}

pub(super) fn score_details(row: &Value) -> String {
    if let Some(symbol) = row["symbolSimilarity"].as_f64() {
        return format!("  [symbol {symbol:.2}]");
    }
    match (
        row["descriptionSimilarity"].as_f64(),
        row["fileDescriptionSimilarity"].as_f64(),
    ) {
        (Some(description), Some(file)) => {
            if let Some(code) = row["codeSimilarity"].as_f64() {
                format!(
                    "  [combined thirds; code {code:.2}, description {description:.2}, file {file:.2}]"
                )
            } else {
                format!("  [combined 50/50; description {description:.2}, file {file:.2}]")
            }
        }
        _ => String::new(),
    }
}

pub(in crate::cli) struct Presentation<'a> {
    engine: Option<&'a Engine>,
    structures: HashMap<String, Option<FileStructure>>,
    calls: CallDepths,
    expand_code_threshold: f64,
    graph: Option<CallGraph>,
    expanded: Option<Expansion>,
    force_code: BTreeSet<Key>,
}

impl<'a> Presentation<'a> {
    pub(in crate::cli) fn new(engine: &'a Engine) -> Self {
        Self {
            engine: Some(engine),
            structures: HashMap::new(),
            calls: CallDepths::default(),
            expand_code_threshold: 0.9,
            graph: None,
            expanded: None,
            force_code: BTreeSet::new(),
        }
    }

    pub(in crate::cli) fn with_calls(
        engine: &'a Engine,
        calls: CallDepths,
        expand_code_threshold: f64,
    ) -> Result<Self> {
        let mut presentation = Self::new(engine);
        presentation.calls = calls;
        presentation.expand_code_threshold = expand_code_threshold;
        if calls.enabled() {
            presentation.graph = Some(engine.call_graph()?);
        }
        Ok(presentation)
    }

    pub(super) fn calls_enabled(&self) -> bool {
        self.calls.enabled()
    }

    pub(super) fn key(&self, function: &Value) -> Option<Key> {
        let path = function["path"].as_str()?;
        let structure = self.graph.as_ref()?.files.get(path)?;
        let node = map::matching_node(structure, function)?;
        if !matches!(
            node.kind.as_str(),
            "function" | "method" | "constructor" | "generator"
        ) {
            return None;
        }
        Some(Key {
            path: path.to_owned(),
            id: node.id,
        })
    }

    fn prepare(&mut self, functions: &[&Value]) {
        if let Some(graph) = &self.graph {
            let expanded = graph.expand(
                functions.iter().filter_map(|f| self.key(f)),
                self.calls.callers,
                self.calls.callees,
            );
            self.force_code = graph.code_keys(
                &expanded,
                self.calls.expand_callers,
                self.calls.expand_callees,
            );
            self.expanded = Some(expanded);
        }
    }

    fn call_list(&self, path: &str, id: usize, depth: usize) -> Option<String> {
        let key = Key {
            path: path.to_owned(),
            id,
        };
        let calls = self.expanded.as_ref()?.comments.get(&key)?;
        map::render_callers(path, calls.iter().map(String::as_str), depth)
    }

    fn code(
        &self,
        path: &str,
        node: &StructureNode,
        similarity: Option<f64>,
        detail: Detail,
    ) -> Option<String> {
        if node.kind == "heading" {
            return None;
        }
        let forced = self.force_code.contains(&Key {
            path: path.to_owned(),
            id: node.id,
        });
        if !forced
            && (detail != Detail::Expanded
                || !similarity.is_some_and(|s| s > self.expand_code_threshold))
        {
            return None;
        }
        let source = self.engine?.presentation_source(path)?;
        source_code(source, node)
    }

    pub(in crate::cli) fn related_json(&self, function: &Value) -> Vec<Value> {
        let (Some(graph), Some(key)) = (&self.graph, self.key(function)) else {
            return Vec::new();
        };
        let expanded = graph.expand([key.clone()], self.calls.callers, self.calls.callees);
        let code_keys = graph.code_keys(
            &expanded,
            self.calls.expand_callers,
            self.calls.expand_callees,
        );
        expanded.depths.iter().filter(|(other, _)| **other != key).filter_map(|(other, depth)| {
            let node = graph.node(other)?;
            let mut value = serde_json::to_value(node).ok()?;
            value.as_object_mut()?.remove("calls");
            if code_keys.contains(other) {
                value["expandedCode"] = json!(true);
                if let Some(code) = self.engine.and_then(|engine| engine.presentation_source(&other.path))
                    .and_then(|source| source.get(node.start_byte..node.end_byte)) {
                    value["source"] = json!(code);
                }
            }
            Some(json!({"path":other.path,"node":value,"callDepth":depth,"callees":expanded.comments.get(other).cloned().unwrap_or_default()}))
        }).collect()
    }

    pub(in crate::cli) fn outgoing_json(&self, function: &Value) -> Vec<String> {
        let (Some(graph), Some(key)) = (&self.graph, self.key(function)) else {
            return Vec::new();
        };
        graph
            .expand([key.clone()], self.calls.callers, self.calls.callees)
            .comments
            .remove(&key)
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(super) fn empty() -> Self {
        Self {
            engine: None,
            structures: HashMap::new(),
            calls: CallDepths::default(),
            expand_code_threshold: 0.9,
            graph: None,
            expanded: None,
            force_code: BTreeSet::new(),
        }
    }

    fn structure(&mut self, path: &str) -> Result<Option<&FileStructure>> {
        if !self.structures.contains_key(path) {
            self.structures.insert(
                path.to_owned(),
                self.engine
                    .map(|engine| engine.presentation_structure(path))
                    .transpose()?
                    .flatten(),
            );
        }
        Ok(self.structures[path].as_ref())
    }

    fn context(&mut self, function: &Value) -> Result<Vec<StructureNode>> {
        Ok(self
            .structure(text(function, "path"))?
            .and_then(|structure| {
                map::matching_node(structure, function).map(|node| map::ancestors(structure, node))
            })
            .unwrap_or_default())
    }

    fn markdown_context(&mut self, chunk: &Value) -> Result<Vec<StructureNode>> {
        Ok(self
            .structure(text(chunk, "path"))?
            .map(|structure| map::heading_context(structure, chunk))
            .unwrap_or_default())
    }
}

#[derive(Clone, Copy)]
pub(super) struct FunctionDisplay {
    pub(super) description: bool,
    pub(super) similarity: Option<f64>,
}

pub(super) fn print_function(
    out: &mut impl Write,
    function: &Value,
    annotation: &str,
    detail: Detail,
    display: FunctionDisplay,
    presentation: &mut Presentation<'_>,
) -> Result<()> {
    print_function_with_header(
        out,
        function,
        annotation,
        detail,
        display,
        presentation,
        true,
    )
}

fn print_function_with_header(
    out: &mut impl Write,
    function: &Value,
    annotation: &str,
    detail: Detail,
    display: FunctionDisplay,
    presentation: &mut Presentation<'_>,
    file_header: bool,
) -> Result<()> {
    if file_header {
        write!(out, "{}", map::file_header(text(function, "path")))?;
        if (detail == Detail::Expanded || display.description)
            && let Some(description) = presentation
                .engine
                .and_then(|engine| engine.presentation_file_description(text(function, "path")))
                .filter(|text| !text.trim().is_empty())
        {
            writeln!(
                out,
                "{}",
                map::comment_block(text(function, "path"), description)
            )?;
        }
    }
    let nodes = presentation.context(function)?;
    let qualified = nodes
        .last()
        .filter(|node| nodes.len() == 1 && node.parent_id.is_some())
        .map(|node| node.qualified_name.as_str());
    let annotation = [Some(annotation), qualified]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let description = if detail == Detail::Expanded || display.description {
        function["description"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
    } else {
        None
    };
    if !nodes.is_empty() {
        write!(
            out,
            "{}",
            map::render_context_with_description(
                &nodes,
                Some(&annotation),
                detail.into(),
                description
            )
        )?;
    } else {
        let start = function["startLine"].as_u64().unwrap_or(1) as usize;
        let end = function["endLine"].as_u64().unwrap_or(start as u64) as usize;
        let name = function["qualifiedName"]
            .as_str()
            .unwrap_or_else(|| text(function, "name"));
        let suffix = map::inline_note(text(function, "path"), Some(&annotation), description);
        writeln!(out, "{}{name}{suffix}", map::hunk(start, end, None))?;
    }
    if let Some(key) = presentation.key(function)
        && let Some(calls) =
            presentation.call_list(&key.path, key.id, nodes.len().saturating_sub(1))
    {
        write!(out, "{calls}")?;
    }
    if let Some(node) = nodes.last()
        && let Some(code) =
            presentation.code(text(function, "path"), node, display.similarity, detail)
    {
        write!(out, "{code}")?;
    }
    Ok(())
}

#[cfg(test)]
fn print_markdown(
    out: &mut impl Write,
    row: &Value,
    detail: Detail,
    show_description: bool,
    presentation: &mut Presentation<'_>,
) -> Result<()> {
    print_markdown_with_header(out, row, detail, show_description, presentation, true)
}

fn print_markdown_with_header(
    out: &mut impl Write,
    row: &Value,
    detail: Detail,
    show_description: bool,
    presentation: &mut Presentation<'_>,
    header: bool,
) -> Result<()> {
    let chunk = &row["chunk"];
    if header {
        write!(out, "{}", map::file_header(text(chunk, "path")))?;
    }
    if header
        && (detail == Detail::Expanded || show_description)
        && let Some(description) = presentation
            .engine
            .and_then(|engine| engine.presentation_file_description(text(chunk, "path")))
            .filter(|text| !text.trim().is_empty())
    {
        writeln!(
            out,
            "{}",
            map::comment_block(text(chunk, "path"), description)
        )?;
    }
    let start = chunk["startLine"].as_u64().unwrap_or(1) as usize;
    let end = chunk["endLine"].as_u64().unwrap_or(start as u64) as usize;
    write!(out, "{}", map::hunk(start, end, None))?;
    let nodes = presentation.markdown_context(chunk)?;
    let score = rank(row);
    let last_heading = if let Some(node) = nodes.last() {
        write!(
            out,
            "{}",
            map::render_declarations_with_annotation(&nodes, detail.into(), Some(&score))
        )?;
        Some(node.signature.clone())
    } else {
        let content = text(chunk, "content");
        let mut last = None;
        let headings: Vec<_> = array(&chunk["headingPath"])
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for (depth, name) in headings.iter().enumerate() {
            let heading = content
                .lines()
                .find(|line| line.starts_with('#') && line.trim_start_matches('#').trim() == *name)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{} {name}", "#".repeat(depth + 1)));
            let suffix = if depth + 1 == headings.len() {
                map::inline_note(text(chunk, "path"), Some(&score), None)
            } else {
                String::new()
            };
            writeln!(out, "{}{heading}{suffix}", "  ".repeat(depth))?;
            last = Some(heading);
        }
        if last.is_none() {
            writeln!(
                out,
                "{}{}",
                content
                    .lines()
                    .find(|line| !line.is_empty())
                    .unwrap_or("(untitled)"),
                map::inline_note(text(chunk, "path"), Some(&score), None)
            )?;
        }
        last
    };
    write!(
        out,
        "{}",
        markdown_extra(chunk, last_heading.as_deref(), detail)
    )?;
    Ok(())
}

fn markdown_extra(chunk: &Value, last_heading: Option<&str>, detail: Detail) -> String {
    let mut output = String::new();
    if detail != Detail::Compact {
        let content = text(chunk, "content");
        let body = if let Some(heading) = last_heading.filter(|heading| content.contains(*heading))
        {
            content
                .split_once(heading)
                .unwrap()
                .1
                .trim_start_matches('\n')
        } else if last_heading.is_none() {
            // Without a heading, compact output already displays the first line.
            content.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
        } else {
            content
        };
        if detail == Detail::Standard {
            let summary = body.split_whitespace().collect::<Vec<_>>().join(" ");
            if !summary.is_empty() {
                let preview = if summary.chars().count() > 160 {
                    format!("{}…", summary.chars().take(159).collect::<String>())
                } else {
                    summary
                };
                output.push_str(&format!("@ preview: {preview}\n"));
            }
        } else if !body.trim().is_empty() {
            output.push_str(body);
            output.push('\n');
        }
    }
    output
}

pub(super) struct RankedHit<'a> {
    pub(super) item: &'a Value,
    pub(super) row: &'a Value,
    pub(super) annotation: String,
    pub(super) score: f64,
    pub(super) markdown: bool,
    pub(super) description: bool,
    pub(super) target: bool,
}

pub(super) fn print_ranked_files(
    out: &mut impl Write,
    hits: Vec<RankedHit<'_>>,
    detail: Detail,
    source: &mut Presentation<'_>,
    mut target: Option<&mut Presentation<'_>>,
) -> Result<()> {
    source.prepare(
        &hits
            .iter()
            .filter(|hit| !hit.target && !hit.markdown)
            .map(|hit| hit.item)
            .collect::<Vec<_>>(),
    );
    if let Some(target) = target.as_deref_mut() {
        target.prepare(
            &hits
                .iter()
                .filter(|hit| hit.target && !hit.markdown)
                .map(|hit| hit.item)
                .collect::<Vec<_>>(),
        );
    }
    let mut files: BTreeMap<(bool, String), Vec<RankedHit<'_>>> = BTreeMap::new();
    for hit in hits {
        files
            .entry((hit.target, text(hit.item, "path").to_owned()))
            .or_default()
            .push(hit);
    }
    for path in source
        .expanded
        .as_ref()
        .into_iter()
        .flat_map(|e| e.depths.keys().map(|key| key.path.clone()))
    {
        files.entry((false, path)).or_default();
    }
    if let Some(target) = target.as_deref() {
        for path in target
            .expanded
            .as_ref()
            .into_iter()
            .flat_map(|e| e.depths.keys().map(|key| key.path.clone()))
        {
            files.entry((true, path)).or_default();
        }
    }
    let mut files: Vec<_> = files.into_iter().collect();
    files.sort_by(|a, b| {
        let maximum = |hits: &Vec<RankedHit<'_>>| {
            hits.iter()
                .map(|hit| hit.score)
                .fold(f64::NEG_INFINITY, f64::max)
        };
        maximum(&b.1)
            .total_cmp(&maximum(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    for (index, ((is_target, path), hits)) in files.into_iter().enumerate() {
        if index > 0 {
            writeln!(out)?;
        }
        if is_target && target.is_some() {
            print_ranked_file(out, &path, &hits, detail, target.as_deref_mut().unwrap())?;
        } else {
            print_ranked_file(out, &path, &hits, detail, source)?;
        }
    }
    Ok(())
}

fn print_ranked_file(
    out: &mut impl Write,
    path: &str,
    hits: &[RankedHit<'_>],
    detail: Detail,
    presentation: &mut Presentation<'_>,
) -> Result<()> {
    let structure = presentation.structure(path)?.cloned();
    let mut selected = HashMap::<usize, StructureNode>::new();
    let mut annotations: HashMap<usize, String> = HashMap::new();
    let mut callees = HashMap::new();
    let mut extras = HashMap::new();
    let mut descriptions = HashMap::new();
    let mut unmatched = Vec::new();
    let heading_bodies = if detail == Detail::Expanded
        && hits
            .iter()
            .any(|hit| hit.row["type"] == "symbol" && hit.item["kind"] == "heading")
    {
        structure
            .as_ref()
            .and_then(|structure| {
                presentation
                    .engine?
                    .presentation_source(path)
                    .map(|source| markdown_map_bodies(source, structure))
            })
            .unwrap_or_default()
    } else {
        HashMap::new()
    };
    for hit in hits {
        let chain = structure
            .as_ref()
            .map(|structure| {
                if hit.markdown {
                    map::heading_context(structure, hit.item)
                } else {
                    map::matching_node(structure, hit.item)
                        .map(|node| map::ancestors(structure, node))
                        .unwrap_or_default()
                }
            })
            .unwrap_or_else(|| {
                if hit.row["type"] == "symbol" {
                    serde_json::from_value::<StructureNode>(hit.item.clone())
                        .map(|node| vec![node])
                        .unwrap_or_default()
                } else {
                    Vec::new()
                }
            });
        if let Some(matched) = chain.last() {
            if let Some(previous) = annotations.get_mut(&matched.id) {
                if previous != &hit.annotation {
                    previous.push_str(" | ");
                    previous.push_str(&hit.annotation);
                }
            } else {
                annotations.insert(matched.id, hit.annotation.clone());
            }
            if hit.markdown {
                extras
                    .entry(matched.id)
                    .or_insert_with(String::new)
                    .push_str(&markdown_extra(hit.item, Some(&matched.signature), detail));
            } else if hit.row["type"] == "symbol" && matched.kind == "heading" {
                if let Some(body) = heading_bodies.get(&matched.id) {
                    let extra = extras.entry(matched.id).or_insert_with(String::new);
                    if !extra.contains(body) {
                        extra.push_str(body);
                    }
                }
            } else if (detail == Detail::Expanded || hit.description)
                && let Some(description) = hit.item["description"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
            {
                descriptions.insert(matched.id, description.to_owned());
            }
            if !hit.markdown
                && let Some(code) =
                    presentation.code(path, matched, hit.row["similarity"].as_f64(), detail)
            {
                let extra = extras.entry(matched.id).or_insert_with(String::new);
                if !extra.contains("@ code:\n") {
                    extra.push_str(&code);
                }
            }
            for node in chain {
                selected.insert(node.id, node);
            }
        } else {
            unmatched.push(hit);
        }
    }
    if let (Some(graph), Some(expanded)) = (&presentation.graph, &presentation.expanded)
        && let Some(structure) = graph.files.get(path)
    {
        for key in expanded.depths.keys().filter(|key| key.path == path) {
            if let Some(node) = graph.node(key) {
                for ancestor in map::ancestors(structure, node) {
                    selected.entry(ancestor.id).or_insert(ancestor);
                }
                if let Some(calls) = expanded.comments.get(key) {
                    callees.insert(key.id, calls.clone());
                }
                if let Some(code) = presentation.code(path, node, None, detail) {
                    let extra = extras.entry(key.id).or_insert_with(String::new);
                    if !extra.contains("@ code:\n") {
                        extra.push_str(&code);
                    }
                }
            }
        }
    }
    let mut nodes: Vec<_> = selected.into_values().collect();
    nodes.sort_by_key(|node| (node.start_byte, node.id));
    unmatched.sort_by_key(|hit| {
        (
            hit.item["startLine"].as_u64().unwrap_or(1),
            hit.item["startColumn"].as_u64().unwrap_or(1),
        )
    });
    if !nodes.is_empty() {
        let file_description = (detail == Detail::Expanded
            || hits.iter().any(|hit| hit.description))
        .then(|| {
            presentation
                .engine
                .and_then(|engine| engine.presentation_file_description(path))
        })
        .flatten();
        write!(
            out,
            "{}",
            map::render_with_hits(
                &nodes,
                Some(path),
                detail.into(),
                presentation
                    .engine
                    .and_then(|engine| engine.presentation_source(path)),
                structure.as_ref(),
                map::Descriptions {
                    file: file_description,
                    symbols: Some(&descriptions)
                },
                map::HitDetails {
                    annotations: Some(&annotations),
                    callees: Some(&callees),
                    extras: Some(&extras)
                },
            )
        )?;
    }
    for (fallback_index, hit) in unmatched.iter().enumerate() {
        if !nodes.is_empty() || fallback_index > 0 {
            writeln!(out)?;
        }
        if hit.markdown {
            print_markdown_with_header(
                out,
                hit.row,
                detail,
                hit.description,
                presentation,
                nodes.is_empty() && fallback_index == 0,
            )?;
        } else {
            print_function_with_header(
                out,
                hit.item,
                &hit.annotation,
                detail,
                FunctionDisplay {
                    description: hit.description,
                    similarity: hit.row["similarity"].as_f64(),
                },
                presentation,
                nodes.is_empty() && fallback_index == 0,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
