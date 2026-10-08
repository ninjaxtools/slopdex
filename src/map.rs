//! Source-ordered declaration excerpts shared by map and semantic results.
//! Ranges refer to the original file; signatures deliberately omit bodies.

use crate::{
    callgraph::{CallGraph, Key},
    engine,
    filter::Selection,
    parse::{self, FileStructure, ParsedFile, StructureNode},
    ui,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
};

/// The same selection and rendering paths serve indexed and directly parsed files.
pub(crate) trait StructureSource {
    fn paths(&self) -> Result<Vec<String>>;
    fn structure(&self, path: &str) -> Result<Option<FileStructure>>;
    fn source(&self, path: &str) -> Option<&str>;
    fn call_graph(&self) -> Result<CallGraph>;

    fn selection(&self, options: &Value) -> Result<Selection> {
        Selection::compile(options)
    }

    fn file_description(&self, _path: &str) -> Option<&str> {
        None
    }

    fn symbol_descriptions(&self, _path: &str) -> Result<HashMap<usize, String>> {
        Ok(HashMap::new())
    }
}

#[derive(Default)]
pub(crate) struct Unindexed {
    files: BTreeMap<String, (String, ParsedFile)>,
}

impl Unindexed {
    pub(crate) fn parse(
        root: &Path,
        index: &Path,
        config: &Value,
        options: &Value,
    ) -> Result<Self> {
        let selection = Selection::compile(options)?;
        let paths = selected_paths(root, options)?;
        let (callers, callees) = call_depths(options);
        let max_size = config["maxFileSize"].as_u64().unwrap_or(1_048_576);
        let mut result = Self::default();
        for path in engine::source_paths(root, index, config)? {
            // Call resolution needs the whole workspace; otherwise parse only
            // files that can contribute to the requested map.
            if callers == 0 && callees == 0 && !path_matches(&selection, &paths, &path) {
                continue;
            }
            let source = match engine::read_source(root, &path, max_size) {
                Ok(source) => source,
                Err(error) => {
                    if config["ignoreErrors"] != true {
                        ui::warning(format!("slopdex: {path}: {error:#}"));
                    }
                    continue;
                }
            };
            let parsed = parse::parse(&path, &source)?;
            if config["ignoreErrors"] != true {
                for error in &parsed.errors {
                    ui::warning(format!(
                        "slopdex: {path}:{}: {}",
                        error.start_line, error.message
                    ));
                }
            }
            result.files.insert(path, (source, parsed));
        }
        Ok(result)
    }
}

impl StructureSource for Unindexed {
    fn paths(&self) -> Result<Vec<String>> {
        Ok(self.files.keys().cloned().collect())
    }

    fn structure(&self, path: &str) -> Result<Option<FileStructure>> {
        Ok(self
            .files
            .get(path)
            .map(|(_, parsed)| parsed.structure.clone()))
    }

    fn source(&self, path: &str) -> Option<&str> {
        self.files.get(path).map(|(source, _)| source.as_str())
    }

    fn call_graph(&self) -> Result<CallGraph> {
        CallGraph::from_parsed(
            self.files
                .iter()
                .map(|(path, (_, parsed))| (path.as_str(), parsed)),
        )
    }
}

fn selected_paths(root: &Path, options: &Value) -> Result<Vec<String>> {
    options["paths"]
        .as_array()
        .map(|paths| {
            paths
                .iter()
                .map(|path| {
                    engine::normalize_path(
                        root,
                        path.as_str().context("Map paths must be strings")?,
                    )
                })
                .collect()
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn path_matches(selection: &Selection, paths: &[String], path: &str) -> bool {
    selection.path_matches(path)
        && (paths.is_empty() || paths.iter().any(|p| engine::under(path, p)))
}

fn call_depths(options: &Value) -> (usize, usize) {
    let depth = |key, expanded| {
        options[key]
            .as_u64()
            .unwrap_or(0)
            .max(options[expanded].as_u64().unwrap_or(0)) as usize
    };
    (
        depth("callers", "expandCallers"),
        depth("callees", "expandCallees"),
    )
}

pub(crate) fn query(
    source: &dyn StructureSource,
    root: &Path,
    options: &Value,
) -> Result<Vec<Value>> {
    let selection = source.selection(options)?;
    let paths = selected_paths(root, options)?;
    let (callers, callees) = call_depths(options);
    let mut selected = BTreeMap::<String, Vec<StructureNode>>::new();
    for path in source.paths()? {
        if !path_matches(&selection, &paths, &path) {
            continue;
        }
        if let Some(structure) = source.structure(&path)? {
            let nodes = selection.select_structure(&structure);
            if !nodes.is_empty() {
                selected.insert(path, nodes);
            }
        }
    }
    let expansion = if callers > 0 || callees > 0 {
        let graph = source.call_graph()?;
        let expand_callers = options["expandCallers"].as_u64().unwrap_or(0) as usize;
        let expand_callees = options["expandCallees"].as_u64().unwrap_or(0) as usize;
        let seeds = selected.iter().flat_map(|(path, nodes)| {
            nodes
                .iter()
                .filter(|node| {
                    matches!(
                        node.kind.as_str(),
                        "function" | "method" | "constructor" | "generator"
                    )
                })
                .map(|node| Key {
                    path: path.clone(),
                    id: node.id,
                })
        });
        let expanded = graph.expand(seeds, callers, callees);
        let code_keys = graph.code_keys(&expanded, expand_callers, expand_callees);
        for key in expanded.depths.keys() {
            if let Some(structure) = graph.files.get(&key.path)
                && let Some(node) = graph.node(key)
            {
                let nodes = selected.entry(key.path.clone()).or_default();
                for ancestor in ancestors(structure, node) {
                    if !nodes.iter().any(|existing| existing.id == ancestor.id) {
                        nodes.push(ancestor);
                    }
                }
            }
        }
        Some((expanded, code_keys))
    } else {
        None
    };
    let mut result = Vec::new();
    for (path, mut nodes) in selected {
        nodes.sort_by_key(|node| (node.start_byte, node.id));
        let mut values = Vec::new();
        for node in nodes {
            let key = Key {
                path: path.clone(),
                id: node.id,
            };
            let mut value = serde_json::to_value(&node)?;
            value.as_object_mut().unwrap().remove("calls");
            if let Some((expansion, code_keys)) = &expansion {
                if let Some(depth) = expansion.depths.get(&key) {
                    value["callDepth"] = json!(depth);
                }
                if let Some(calls) = expansion.comments.get(&key) {
                    value["callees"] = json!(calls);
                }
                if code_keys.contains(&key) {
                    value["expandedCode"] = json!(true);
                    if let Some(code) = source
                        .source(&path)
                        .and_then(|source| source.get(node.start_byte..node.end_byte))
                    {
                        value["source"] = json!(code);
                    }
                }
            }
            values.push(value);
        }
        result.push(json!({"path":path,"nodes":values}));
    }
    Ok(result)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detail {
    Compact,
    Standard,
    Expanded,
}

#[derive(Default)]
pub struct Descriptions<'a> {
    pub file: Option<&'a str>,
    pub symbols: Option<&'a HashMap<usize, String>>,
}

#[derive(Default)]
pub struct HitDetails<'a> {
    pub annotations: Option<&'a HashMap<usize, String>>,
    pub callees: Option<&'a HashMap<usize, Vec<String>>>,
    pub extras: Option<&'a HashMap<usize, String>>,
}

pub(crate) fn symbol_location(path: &str, start: usize, end: usize, name: &str) -> String {
    let range = if end > start {
        format!("{start}-{end}")
    } else {
        start.to_string()
    };
    format!("{path}:{range}:{name}")
}

pub(crate) fn group_symbol_locations<'a>(
    locations: impl IntoIterator<Item = &'a str>,
) -> BTreeMap<&'a str, Vec<&'a str>> {
    let mut files = BTreeMap::<_, Vec<_>>::new();
    for location in locations {
        // The numeric range separates the path from the symbol; either can
        // otherwise contain colons, including Rust's qualified names.
        let parts = location.match_indices(':').find_map(|(index, _)| {
            let symbol = &location[index + 1..];
            let range = symbol.split_once(':')?.0;
            let (start, end) = range.split_once('-').unwrap_or((range, range));
            let start = start.parse::<usize>().ok()?;
            end.parse::<usize>().ok()?;
            Some((&location[..index], start, symbol))
        });
        if let Some((path, start, symbol)) = parts {
            files.entry(path).or_default().push((start, symbol));
        }
    }
    files
        .into_iter()
        .map(|(path, mut symbols)| {
            symbols.sort_by_key(|(start, _)| *start);
            (
                path,
                symbols.into_iter().map(|(_, symbol)| symbol).collect(),
            )
        })
        .collect()
}

pub(crate) fn render_callees<'a>(
    language: &str,
    callees: impl IntoIterator<Item = &'a str>,
    depth: usize,
) -> Option<String> {
    let files = group_symbol_locations(callees);
    if files.is_empty() {
        return None;
    }
    let indent = "  ".repeat(depth + 1);
    if files.len() == 1
        && let Some((path, symbols)) = files.first_key_value()
        && let [symbol] = symbols.as_slice()
    {
        return Some(format!(
            "{indent}{}\n",
            comment(language, &format!("callees: {path}:{symbol}"))
        ));
    }
    let mut output = format!("{indent}{}\n", comment(language, "callees:"));
    for (path, symbols) in files {
        if let [symbol] = symbols.as_slice() {
            output.push_str(&format!(
                "{indent}{}\n",
                comment_with_indent(language, &format!("{path}:{symbol}"), "  ")
            ));
            continue;
        }
        output.push_str(&format!(
            "{indent}{}\n",
            comment_with_indent(language, &format!("{path}:"), "  ")
        ));
        for symbol in symbols {
            output.push_str(&format!(
                "{indent}{}\n",
                comment_with_indent(language, symbol, "    ")
            ));
        }
    }
    Some(output)
}

/// Generated text is rendered as a language-appropriate comment, not source.
pub fn comment(language: &str, description: &str) -> String {
    comment_with_indent(language, description, "")
}

fn comment_with_indent(language: &str, description: &str, indent: &str) -> String {
    let description = description.trim();
    match crate::parse::language_for_path(language).unwrap_or(language) {
        "python" | "bash" => format!("# {indent}{description}"),
        "markdown" => {
            // `--` is invalid inside HTML comments, including generated descriptions.
            format!("<!-- {indent}{} -->", description.replace("--", "- -"))
        }
        "c" | "css" => format!("/* {indent}{description} */"),
        "terraform" | "yaml" | "toml" => format!("# {indent}{description}"),
        "xml" | "html" => format!("<!-- {indent}{} -->", description.replace("--", "- -")),
        _ => format!("// {indent}{description}"),
    }
}

pub fn comment_block(language: &str, description: &str) -> String {
    description
        .lines()
        .map(|line| format!("{}\n", comment(language, line)))
        .collect()
}

pub fn inline_note(language: &str, annotation: Option<&str>, description: Option<&str>) -> String {
    let notes: Vec<_> = annotation
        .filter(|note| !note.is_empty())
        .into_iter()
        .chain(description.filter(|note| !note.trim().is_empty()))
        .map(|note| note.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    if notes.is_empty() {
        String::new()
    } else {
        format!("  {}", comment(language, &notes.join(" | ")))
    }
}

pub fn file_header(path: &str) -> String {
    format!("*** {path}\n")
}

pub fn render(structure: &FileStructure, path: Option<&str>) -> String {
    render_nodes(&structure.nodes, path)
}

pub fn render_nodes(nodes: &[StructureNode], path: Option<&str>) -> String {
    render_with_detail(nodes, path, Detail::Compact)
}

pub fn render_with_detail(nodes: &[StructureNode], path: Option<&str>, detail: Detail) -> String {
    render_with_source(nodes, path, detail, None)
}

/// Source is the indexed snapshot. It lets Markdown headings separated only by
/// blank lines share one hunk without folding across omitted prose.
pub fn render_with_source(
    nodes: &[StructureNode],
    path: Option<&str>,
    detail: Detail,
    source: Option<&str>,
) -> String {
    render_with_structure(nodes, path, detail, source, None)
}

/// `full_structure` is the unfiltered indexed tree. It prevents visibility/kind
/// filters from making an incomplete container or sibling run look complete.
pub fn render_with_structure(
    nodes: &[StructureNode],
    path: Option<&str>,
    detail: Detail,
    source: Option<&str>,
    full_structure: Option<&FileStructure>,
) -> String {
    render_with_descriptions(
        nodes,
        path,
        detail,
        source,
        full_structure,
        Descriptions::default(),
    )
}

pub fn render_with_descriptions(
    nodes: &[StructureNode],
    path: Option<&str>,
    detail: Detail,
    source: Option<&str>,
    full_structure: Option<&FileStructure>,
    descriptions: Descriptions<'_>,
) -> String {
    render_with_hits(
        nodes,
        path,
        detail,
        source,
        full_structure,
        descriptions,
        HitDetails::default(),
    )
}

pub fn render_with_hits(
    nodes: &[StructureNode],
    path: Option<&str>,
    detail: Detail,
    source: Option<&str>,
    full_structure: Option<&FileStructure>,
    descriptions: Descriptions<'_>,
    hits: HitDetails<'_>,
) -> String {
    let by_id: HashMap<_, _> = nodes.iter().map(|node| (node.id, node)).collect();
    let mut ordered: Vec<_> = nodes.iter().collect();
    ordered.sort_by_key(|node| (node.start_byte, node.id));
    let mut full_children: HashMap<Option<usize>, Vec<&StructureNode>> = HashMap::new();
    if let Some(structure) = full_structure {
        let mut full: Vec<_> = structure.nodes.iter().collect();
        full.sort_by_key(|node| (node.start_byte, node.id));
        for node in full {
            full_children.entry(node.parent_id).or_default().push(node);
        }
    }
    let full_next: HashMap<_, _> = full_children
        .values()
        .flat_map(|siblings| siblings.windows(2).map(|pair| (pair[0].id, pair[1].id)))
        .collect();
    let full_heading_next: HashMap<_, _> = full_structure
        .into_iter()
        .flat_map(|structure| structure.nodes.iter().filter(|node| node.kind == "heading"))
        .collect::<Vec<_>>()
        .windows(2)
        .map(|pair| (pair[0].id, pair[1].id))
        .collect();
    let source_lines = source.map(|source| source.lines().collect::<Vec<_>>());
    let mut output = path.map(file_header).unwrap_or_default();
    if let (Some(path), Some(description)) = (
        path,
        descriptions
            .file
            .filter(|description| !description.trim().is_empty()),
    ) {
        output.push_str(&comment_block(path, description));
    }
    let mut group: Vec<&StructureNode> = Vec::new();
    for node in ordered {
        let same_run = group.last().is_some_and(|previous| {
            let parent_present = node.parent_id.is_none_or(|id| by_id.contains_key(&id));
            let previous_parent_present =
                previous.parent_id.is_none_or(|id| by_id.contains_key(&id));
            let child = node.parent_id == Some(previous.id);
            // Fold headings only across gaps after their declarations, not
            // across the section bodies covered by their displayed ranges.
            let end = if child || previous.kind == "heading" {
                declaration_line(previous)
            } else {
                display_end(previous)
            };
            let nearby = (child && end == node.start_line)
                || blank_gap(end, node.start_line, source_lines.as_deref());
            parent_present
                && previous_parent_present
                && nearby
                && if child {
                    full_structure.is_none_or(|_| {
                        full_children
                            .get(&Some(previous.id))
                            .and_then(|children| children.first())
                            .is_some_and(|first| first.id == node.id)
                    })
                } else if previous.kind == "heading" && node.kind == "heading" {
                    full_structure
                        .is_none_or(|_| full_heading_next.get(&previous.id) == Some(&node.id))
                } else {
                    previous.parent_id == node.parent_id
                        && full_structure
                            .is_none_or(|_| full_next.get(&previous.id) == Some(&node.id))
                }
        });
        if !same_run && !group.is_empty() {
            append_group(
                &mut output,
                &group,
                &by_id,
                detail,
                descriptions.symbols,
                &hits,
            );
            group.clear();
        }
        group.push(node);
    }
    if !group.is_empty() {
        append_group(
            &mut output,
            &group,
            &by_id,
            detail,
            descriptions.symbols,
            &hits,
        );
    }
    output
}

fn append_group(
    output: &mut String,
    group: &[&StructureNode],
    by_id: &HashMap<usize, &StructureNode>,
    detail: Detail,
    descriptions: Option<&HashMap<usize, String>>,
    hits: &HitDetails<'_>,
) {
    if !output.is_empty() {
        output.push('\n');
    }
    let first = group[0];
    let last = group[group.len() - 1];
    let first_depth = depth(first, by_id);
    let qualified =
        (first_depth == 0 && first.parent_id.is_some() && first.qualified_name != first.name)
            .then_some(first.qualified_name.as_str());
    output.push_str(&hunk(
        first.start_line,
        group
            .iter()
            .map(|node| display_end(node))
            .max()
            .unwrap_or(display_end(last)),
        qualified,
    ));
    for node in group {
        let depth = depth(node, by_id);
        output.push_str(&declaration(
            node,
            depth,
            detail,
            descriptions.and_then(|descriptions| descriptions.get(&node.id).map(String::as_str)),
            hits.annotations
                .and_then(|scores| scores.get(&node.id).map(String::as_str)),
        ));
        if let Some(callees) = hits.callees.and_then(|callees| callees.get(&node.id))
            && let Some(calls) =
                render_callees(&node.language, callees.iter().map(String::as_str), depth)
        {
            output.push_str(&calls);
        }
        if let Some(extra) = hits.extras.and_then(|extras| extras.get(&node.id)) {
            output.push_str(extra);
        }
    }
}

/// Compare against a declaration rather than its enclosing body: the last line
/// of a Markdown heading, or the first declaration line after Rust attributes.
fn declaration_line(node: &StructureNode) -> usize {
    if node.kind == "heading" {
        node.start_line + node.signature.lines().count().saturating_sub(1)
    } else if node.language == "rust" {
        node.start_line
            + node
                .attributes
                .iter()
                .map(|attr| attr.lines().count())
                .sum::<usize>()
    } else {
        node.start_line
    }
}

fn blank_gap(end: usize, start: usize, source_lines: Option<&[&str]>) -> bool {
    end.checked_add(1) == Some(start)
        || (matches!(start.checked_sub(end), Some(2..=5))
            && source_lines.is_some_and(|lines| {
                lines
                    .get(end..start - 1)
                    .is_some_and(|gap| gap.iter().all(|line| line.trim().is_empty()))
            }))
}

fn depth(node: &StructureNode, by_id: &HashMap<usize, &StructureNode>) -> usize {
    let mut depth = 0;
    let mut parent = node.parent_id;
    let mut visited = HashSet::new();
    while let Some(id) = parent.filter(|id| visited.insert(*id)) {
        let Some(ancestor) = by_id.get(&id) else {
            break;
        };
        depth += 1;
        parent = ancestor.parent_id;
    }
    depth
}

fn display_end(node: &StructureNode) -> usize {
    inclusive_end(node.end_line, node.end_column, node.start_line)
}

pub fn render_node(
    node: &StructureNode,
    depth: usize,
    annotation: Option<&str>,
    detail: Detail,
) -> String {
    let end = display_end(node);
    let mut output = hunk(node.start_line, end, None);
    output.push_str(&declaration(node, depth, detail, None, annotation));
    output
}

/// A ranked hit is independent of neighboring hits: render its ancestors on
/// every occurrence, but keep the hunk range anchored to the matching symbol.
pub fn render_context(nodes: &[StructureNode], annotation: Option<&str>, detail: Detail) -> String {
    render_context_with_description(nodes, annotation, detail, None)
}

pub fn render_context_with_description(
    nodes: &[StructureNode],
    annotation: Option<&str>,
    detail: Detail,
    description: Option<&str>,
) -> String {
    let Some(matched) = nodes.last() else {
        return String::new();
    };
    let mut output = hunk(matched.start_line, display_end(matched), None);
    for (depth, node) in nodes.iter().enumerate() {
        output.push_str(&declaration(
            node,
            depth,
            detail,
            if depth + 1 == nodes.len() {
                description
            } else {
                None
            },
            annotation.filter(|_| depth + 1 == nodes.len()),
        ));
    }
    output
}

pub fn render_declarations(nodes: &[StructureNode], detail: Detail) -> String {
    render_declarations_with_annotation(nodes, detail, None)
}

pub fn render_declarations_with_annotation(
    nodes: &[StructureNode],
    detail: Detail,
    annotation: Option<&str>,
) -> String {
    let mut output = String::new();
    for (depth, node) in nodes.iter().enumerate() {
        output.push_str(&declaration(
            node,
            depth,
            detail,
            None,
            annotation.filter(|_| depth + 1 == nodes.len()),
        ));
    }
    output
}

pub fn ancestors(structure: &FileStructure, node: &StructureNode) -> Vec<StructureNode> {
    let by_id: HashMap<_, _> = structure.nodes.iter().map(|node| (node.id, node)).collect();
    let mut nodes = vec![node.clone()];
    let mut parent = node.parent_id;
    let mut visited = HashSet::new();
    while let Some(id) = parent.filter(|id| visited.insert(*id)) {
        let Some(node) = by_id.get(&id) else {
            break;
        };
        nodes.push((*node).clone());
        parent = node.parent_id;
    }
    nodes.reverse();
    nodes
}

/// Markdown chunks may start at a heading or later in its section. Resolve the
/// deepest enclosing heading with exactly the indexed heading path.
pub fn heading_context(structure: &FileStructure, chunk: &Value) -> Vec<StructureNode> {
    let Some(start) = chunk["startLine"].as_u64().map(|line| line as usize) else {
        return Vec::new();
    };
    let path: Vec<_> = chunk["headingPath"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if path.is_empty() {
        return Vec::new();
    }
    structure
        .nodes
        .iter()
        .filter(|node| {
            node.kind == "heading"
                && node.start_line <= start
                && (start < node.end_line || start == node.end_line && node.end_column > 1)
        })
        .filter_map(|node| {
            let chain = ancestors(structure, node);
            (chain.iter().map(|n| n.name.as_str()).collect::<Vec<_>>() == path).then_some(chain)
        })
        .max_by_key(|chain| (chain.len(), chain.last().unwrap().start_line))
        .unwrap_or_default()
}

fn declaration(
    node: &StructureNode,
    depth: usize,
    detail: Detail,
    description: Option<&str>,
    annotation: Option<&str>,
) -> String {
    let mut output = String::new();
    let indent = "  ".repeat(depth);
    if detail != Detail::Compact {
        for attr in &node.attributes {
            // JavaScript export modifiers belong on the declaration line.
            if is_js(node) && matches!(attr.as_str(), "export" | "default" | "declare") {
                continue;
            }
            for line in attr.lines() {
                output.push_str(&format!("{indent}{line}\n"));
            }
        }
    }
    let signature = signature(node, detail);
    let line_count = signature.lines().count();
    for (index, line) in signature.lines().enumerate() {
        output.push_str(&indent);
        output.push_str(line);
        if index + 1 == line_count {
            output.push_str(&inline_note(&node.language, annotation, description));
        }
        output.push('\n');
    }
    output
}

fn is_js(node: &StructureNode) -> bool {
    matches!(
        node.language.as_str(),
        "javascript" | "jsx" | "typescript" | "tsx"
    )
}

fn signature(node: &StructureNode, detail: Detail) -> String {
    let signature = if node.signature.is_empty() {
        &node.name
    } else {
        &node.signature
    };
    if node.kind == "heading" {
        return signature.to_owned();
    }
    let mut signature = signature.replace('\r', "\\r").replace('\n', "\\n");
    if is_js(node) {
        let modifiers: Vec<_> = node
            .attributes
            .iter()
            .filter(|attr| matches!(attr.as_str(), "export" | "default" | "declare"))
            .filter(|attr| {
                !signature
                    .split_whitespace()
                    .take(3)
                    .any(|word| word == attr.as_str())
            })
            .map(String::as_str)
            .collect();
        if !modifiers.is_empty() {
            signature = format!("{} {signature}", modifiers.join(" "));
        }
    }
    if detail == Detail::Expanded {
        signature
    } else {
        truncate(&signature, 180)
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let mut result: String = value.chars().take(max - 1).collect();
    result.push('…');
    result
}

fn inclusive_end(end: usize, column: usize, start: usize) -> usize {
    if column == 1 && end > start {
        end - 1
    } else {
        end.max(start)
    }
}

pub fn hunk(start: usize, end: usize, annotation: Option<&str>) -> String {
    let range = if end <= start {
        start.to_string()
    } else {
        format!("{start}-{end}")
    };
    match annotation.filter(|s| !s.is_empty()) {
        Some(annotation) => format!("@@ {range} @@ {annotation}\n"),
        None => format!("@@ {range} @@\n"),
    }
}

/// Resolve a search hit to its structural declaration. Repeated names and
/// overloads use source coordinates rather than just a qualified name.
pub fn matching_node<'a>(structure: &'a FileStructure, unit: &Value) -> Option<&'a StructureNode> {
    let start = unit["startLine"].as_u64()? as usize;
    let end = unit["endLine"].as_u64().unwrap_or(start as u64) as usize;
    let qualified = unit["qualifiedName"].as_str();
    let name = unit["name"].as_str();
    structure
        .nodes
        .iter()
        .filter(|node| {
            (unit["startByte"].is_u64()
                || matches!(
                    node.kind.as_str(),
                    "function" | "method" | "constructor" | "generator"
                ))
                && (qualified == Some(node.qualified_name.as_str())
                    || name == Some(node.name.as_str())
                        && node.start_line <= end
                        && node.end_line >= start)
        })
        .min_by_key(|node| {
            (
                qualified != Some(node.qualified_name.as_str()),
                node.start_line.abs_diff(start) + node.end_line.abs_diff(end),
                node.start_column
                    .abs_diff(unit["startColumn"].as_u64().unwrap_or(1) as usize),
                node.id,
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_uses_source_resolved_semantic_selection() -> Result<()> {
        struct Resolved(FileStructure);
        impl StructureSource for Resolved {
            fn paths(&self) -> Result<Vec<String>> {
                Ok(vec!["guide.md".into()])
            }
            fn structure(&self, _path: &str) -> Result<Option<FileStructure>> {
                Ok(Some(self.0.clone()))
            }
            fn source(&self, _path: &str) -> Option<&str> {
                None
            }
            fn call_graph(&self) -> Result<CallGraph> {
                anyhow::bail!("call graph is not requested")
            }
            fn selection(&self, options: &Value) -> Result<Selection> {
                Ok(Selection::compile(options)?.with_symbol_names(HashSet::from(["setup".into()])))
            }
        }
        let source = Resolved(
            parse::parse("guide.md", "# Guide\n## Setup\n### Linux\n## Other\n")?.structure,
        );
        let options = json!({"symbolQuery": ["setup"]});
        let rows = query(&source, Path::new("."), &options)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|node| node["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["Guide", "Setup"]
        );
        let unresolved = Unindexed {
            files: BTreeMap::from([(
                "guide.md".into(),
                (
                    String::new(),
                    parse::parse("guide.md", "# Guide\n## Setup\n")?,
                ),
            )]),
        };
        assert!(query(&unresolved, Path::new("."), &options)?.is_empty());
        Ok(())
    }

    #[test]
    fn source_order_and_ranges_without_bodies_or_kind_sections() {
        let source = "use std::io;\n#[derive(Clone)]\npub struct S { pub x: i32 }\nimpl S { pub fn go(&self) { secret(); } }";
        let structure = crate::parse::parse("x.rs", source).unwrap().structure;
        let output = render_nodes(&structure.nodes, Some("x.rs"));
        assert!(
            output.starts_with("*** x.rs\n\n@@ 1-3 @@\nuse std::io\npub struct S\n  pub x: i32\n"),
            "{output}"
        );
        assert!(
            output.contains("@@ 4 @@\nimpl S\n  pub fn go(&self)\n"),
            "{output}"
        );
        assert!(!output.contains("secret") && !output.contains("{") && !output.contains("[1]"));
        let expanded = render_with_detail(&structure.nodes, None, Detail::Standard);
        assert!(expanded.contains("#[derive(Clone)]"));
        let selected: Vec<_> = structure
            .nodes
            .into_iter()
            .filter(|n| n.kind == "method")
            .collect();
        let filtered = render_nodes(&selected, None);
        assert!(
            filtered.contains("@@ 4 @@ S.go\npub fn go(&self)"),
            "{filtered}"
        );
    }

    #[test]
    fn search_matches_overloads_by_range() {
        let structure = crate::parse::parse("x.ts", "export function f(x: string): string;\nexport function f(x: number): number;\nexport function f(x: number) { return x; }").unwrap().structure;
        let node = matching_node(
            &structure,
            &serde_json::json!({"qualifiedName":"f", "startLine":3, "endLine":3}),
        )
        .unwrap();
        assert_eq!(node.start_line, 3);
        assert!(!render_node(node, 0, None, Detail::Compact).contains("return x"));
    }

    #[test]
    fn exports_literals_and_attributes_survive_declaration_rendering() {
        let source = "export default class Api { run() { hidden(); } }\nexport type Label = 'two  words' | `a  b`;";
        let structure = crate::parse::parse("x.ts", source).unwrap().structure;
        let compact = render_nodes(&structure.nodes, Some("x.ts"));
        assert!(compact.contains("export default class Api"), "{compact}");
        assert!(compact.contains("'two  words' | `a  b`"), "{compact}");
        assert!(!compact.contains("hidden") && !compact.contains("{ run"));
        let standard = render_with_detail(&structure.nodes, None, Detail::Standard);
        assert!(
            !standard
                .lines()
                .any(|line| matches!(line, "export" | "default"))
        );
    }

    #[test]
    fn typescript_union_branches_render_with_their_members() {
        let source = "export type Result =\n  | { compacted: false; events: Event[] }\n  | { compacted: true; summary: string };";
        let structure = crate::parse::parse("result.ts", source).unwrap().structure;
        let output = render_nodes(&structure.nodes, Some("result.ts"));
        assert_eq!(
            output,
            "*** result.ts\n\n@@ 1-3 @@\nexport type Result = | { compacted: false; events: Event[] } | { compacted: true; summary: string }\n"
        );
    }

    #[test]
    fn parent_and_adjacent_methods_share_hunk_but_unverified_gaps_do_not() {
        let source = "impl Writer {\n    fn write(&mut self) {\n        do_write();\n    }\n    fn flush(&mut self) {\n        do_flush();\n    }\n\n    fn close(&mut self) {\n        do_close();\n    }\n}\n";
        let structure = crate::parse::parse("writer.rs", source).unwrap().structure;
        let output = render_nodes(&structure.nodes, Some("writer.rs"));
        assert!(
            output.contains(
                "@@ 1-12 @@\nimpl Writer\n  fn write(&mut self)\n  fn flush(&mut self)\n"
            ),
            "{output}"
        );
        assert!(
            output.contains("@@ 9-11 @@\n  fn close(&mut self)\n"),
            "{output}"
        );
        assert!(!output.contains("do_write") && !output.contains("do_flush"));
        assert_eq!(output.matches("@@ 1-12 @@").count(), 1);
    }

    #[test]
    fn up_to_four_blank_lines_join_siblings_but_five_or_a_comment_do_not() {
        let source = "fn first() {}\n  \nfn second() {}\n\n\nfn third() {}\n\n\n  \n\nfn fourth() {}\n\n\n\n\n\nfn fifth() {}\n// separator\nfn sixth() {}\n";
        let structure = crate::parse::parse("api.rs", source).unwrap().structure;
        let output = render_with_structure(
            &structure.nodes,
            Some("api.rs"),
            Detail::Compact,
            Some(source),
            Some(&structure),
        );
        assert!(
            output.contains("@@ 1-11 @@\nfn first()\nfn second()\nfn third()\nfn fourth()\n"),
            "{output}"
        );
        assert!(output.contains("@@ 17 @@\nfn fifth()\n"), "{output}");
        assert!(output.contains("@@ 19 @@\nfn sixth()\n"), "{output}");
        assert_eq!(output.matches("@@").count(), 6, "{output}");

        // Without the indexed source, the skipped line cannot be assumed blank.
        let without_source = render_nodes(&structure.nodes, None);
        assert!(without_source.contains("@@ 1 @@\nfn first()\n"));
        assert!(without_source.contains("@@ 3 @@\nfn second()\n"));
    }

    #[test]
    fn adjacent_scored_nested_symbols_share_a_hunk_with_individual_scores() {
        let source = "impl Api {\n  fn run(&self) {\n    fn open() {}\n    fn frame() {}\n    fn close() {}\n  }\n}\n";
        let structure = crate::parse::parse("api.rs", source).unwrap().structure;
        let selected: Vec<_> = structure
            .nodes
            .iter()
            .filter(|node| {
                matches!(
                    node.name.as_str(),
                    "Api" | "run" | "open" | "frame" | "close"
                )
            })
            .cloned()
            .collect();
        let annotations: HashMap<_, _> = selected
            .iter()
            .filter(|node| matches!(node.name.as_str(), "open" | "frame" | "close"))
            .map(|node| (node.id, format!("score={}", node.name)))
            .collect();
        assert_eq!(annotations.len(), 3);
        let output = render_with_hits(
            &selected,
            Some("api.rs"),
            Detail::Compact,
            None,
            Some(&structure),
            Descriptions::default(),
            HitDetails {
                annotations: Some(&annotations),
                callees: None,
                extras: None,
            },
        );
        assert!(
            output.contains("@@ 1-7 @@\nimpl Api\n  fn run(&self)\n    fn open()  // score=open\n    fn frame()  // score=frame\n    fn close()  // score=close\n"),
            "{output}"
        );
        assert_eq!(output.matches("@@").count(), 2, "{output}");
        assert_eq!(output.matches("fn run(&self)").count(), 1, "{output}");
    }

    #[test]
    fn interface_and_adjacent_members_share_its_single_source_range() {
        let source = format!(
            "{}export interface PipeAddress {{\n  readonly read: string\n  readonly write: string\n}}\n",
            "\n".repeat(80)
        );
        let structure = crate::parse::parse("pipe.ts", &source).unwrap().structure;
        let output = render_nodes(&structure.nodes, Some("pipe.ts"));
        assert_eq!(
            output,
            "*** pipe.ts\n\n@@ 81-84 @@\nexport interface PipeAddress\n  readonly read: string\n  readonly write: string\n"
        );
    }

    #[test]
    fn markdown_chunk_and_fields_share_one_range() {
        let source = include_str!("parse/mod.rs");
        let structure = crate::parse::parse("src/parse/mod.rs", source)
            .unwrap()
            .structure;
        let parent = structure
            .nodes
            .iter()
            .find(|node| node.name == "MarkdownChunk" && node.kind == "struct")
            .unwrap();
        let selected: Vec<_> = structure
            .nodes
            .iter()
            .filter(|node| node.id == parent.id || node.parent_id == Some(parent.id))
            .cloned()
            .collect();
        let expected = format!(
            "@@ {}-{} @@\npub struct MarkdownChunk\n  pub heading_path: Vec<String>\n  pub start_line: usize\n  pub end_line: usize\n  pub content: String\n  pub source_hash: String\n  pub embedding_input: String\n",
            parent.start_line, parent.end_line
        );
        for output in [
            render_with_structure(
                &selected,
                None,
                Detail::Compact,
                Some(source),
                Some(&structure),
            ),
            render_nodes(&selected, None),
        ] {
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn parent_and_child_share_a_range_across_four_blank_lines_not_five() {
        for (blanks, shared) in [(4, true), (5, false)] {
            let source = format!(
                "struct Fields {{\n{}  first: i32,\n}}\n",
                "\n".repeat(blanks)
            );
            let structure = crate::parse::parse("fields.rs", &source).unwrap().structure;
            let output = render_with_structure(
                &structure.nodes,
                None,
                Detail::Compact,
                Some(&source),
                Some(&structure),
            );
            assert_eq!(
                output.matches("@@").count(),
                if shared { 2 } else { 4 },
                "{output}"
            );
            assert!(output.contains("struct Fields\n"), "{output}");
            assert!(output.contains("  first: i32\n"), "{output}");
        }
    }

    #[test]
    fn container_with_gaps_keeps_member_ranges_separate() {
        let source = "export interface PipeAddress {\n  readonly read: string\n\n  readonly write: string\n}\n";
        let structure = crate::parse::parse("pipe.ts", source).unwrap().structure;
        let output = render_nodes(&structure.nodes, None);
        assert!(
            output.contains("@@ 1-5 @@\nexport interface PipeAddress\n  readonly read: string\n"),
            "{output}"
        );
        assert!(
            output.contains("@@ 4 @@\n  readonly write: string\n"),
            "{output}"
        );
    }

    #[test]
    fn filtered_children_do_not_make_an_incomplete_container_look_complete() {
        let source = "export interface PipeAddress {\n  readonly read: string; readonly hidden: string\n  readonly write: string\n}\n";
        let structure = crate::parse::parse("pipe.ts", source).unwrap().structure;
        let selected = crate::filter::Selection::compile(&serde_json::json!({
            "regexp": ["^PipeAddress\\.(read|write)$"]
        }))
        .unwrap()
        .select_structure(&structure);
        assert_eq!(selected.len(), 3);
        let output = render_with_structure(
            &selected,
            None,
            Detail::Compact,
            Some(source),
            Some(&structure),
        );
        assert!(
            output.contains("@@ 1-4 @@\nexport interface PipeAddress\n  readonly read: string\n"),
            "{output}"
        );
        assert!(
            output.contains("@@ 3 @@\n  readonly write: string\n"),
            "{output}"
        );
        assert!(!output.contains("hidden"));
    }

    #[test]
    fn filtered_nested_declarations_join_their_adjacent_parent() {
        let source = "impl Service {\n  pub fn run(&self) {\n    fn helper() {}\n  }\n}\n";
        let structure = crate::parse::parse("service.rs", source).unwrap().structure;
        let selected = crate::filter::Selection::compile(&serde_json::json!({
            "regexp": "^Service\\.run$"
        }))
        .unwrap()
        .select_structure(&structure);
        let output = render_with_structure(
            &selected,
            None,
            Detail::Compact,
            Some(source),
            Some(&structure),
        );
        assert!(
            output.contains("@@ 1-5 @@\nimpl Service\n  pub fn run(&self)\n"),
            "{output}"
        );
        assert_eq!(output.matches("@@").count(), 2, "{output}");
        assert!(!output.contains("helper"));
    }

    #[test]
    fn markdown_outline_folds_headings_across_blank_lines_not_prose() {
        let source = "# Guide\n\n## Setup\n\n### Details\nRead this.\n\n## More\nNext section.\n";
        let structure = crate::parse::parse("guide.md", source).unwrap().structure;
        let output = render_with_source(
            &structure.nodes,
            Some("guide.md"),
            Detail::Compact,
            Some(source),
        );
        assert!(
            output.contains("@@ 1-9 @@\n# Guide\n  ## Setup\n    ### Details\n"),
            "{output}"
        );
        assert!(output.contains("@@ 8-9 @@\n  ## More\n"), "{output}");
        assert_eq!(output.matches("# Guide").count(), 1);
        assert!(!output.contains("Read this") && !output.contains("Next section"));
    }

    #[test]
    fn markdown_headings_follow_the_four_blank_line_limit() {
        let source = "# First\n\n\n\n\n# Second\n\n\n\n\n\n# Third\n";
        let structure = crate::parse::parse("guide.md", source).unwrap().structure;
        let output = render_with_structure(
            &structure.nodes,
            Some("guide.md"),
            Detail::Compact,
            Some(source),
            Some(&structure),
        );
        assert!(
            output.contains("@@ 1-11 @@\n# First\n# Second\n"),
            "{output}"
        );
        assert!(output.contains("@@ 12 @@\n# Third\n"), "{output}");
    }

    #[test]
    fn setext_heading_span_covers_its_two_source_lines() {
        let source = "Guide\n=====\n\n## Setup\n";
        let structure = crate::parse::parse("guide.md", source).unwrap().structure;
        let output = render_with_source(&structure.nodes, None, Detail::Compact, Some(source));
        assert!(
            output.contains("@@ 1-4 @@\nGuide\n=====\n  ## Setup\n"),
            "{output}"
        );
    }

    #[test]
    fn markdown_heading_ranges_cover_sections_until_peers_or_eof() {
        let source = "# Root\nIntro\n### Child\nBody\n\nPeer\n----\nPeer body\n# Last\nLast body";
        for source in [
            source.to_owned(),
            format!("{source}\n"),
            format!("{source}\n").replace('\n', "\r\n"),
        ] {
            let structure = crate::parse::parse("guide.md", &source).unwrap().structure;
            let expected = [
                "@@ 1-8 @@\n# Root\n",
                "@@ 3-5 @@\n### Child\n",
                "@@ 6-8 @@\nPeer\n----\n",
                "@@ 9-10 @@\n# Last\n",
            ];
            for (node, expected) in structure.nodes.iter().zip(expected) {
                assert_eq!(render_node(node, 0, None, Detail::Compact), expected);
            }
            let output = render_with_structure(
                &structure.nodes,
                None,
                Detail::Compact,
                Some(&source),
                Some(&structure),
            );
            assert_eq!(
                output,
                "@@ 1-8 @@\n# Root\n\n@@ 3-5 @@\n  ### Child\n\n@@ 6-8 @@\n  Peer\n  ----\n\n@@ 9-10 @@\n# Last\n"
            );
        }
    }

    #[test]
    fn descriptions_render_as_language_comments_on_declaration_lines() {
        let python = crate::parse::parse("x.py", "def run():\n    pass\n")
            .unwrap()
            .structure;
        let output = render_context_with_description(
            &python.nodes,
            None,
            Detail::Compact,
            Some("First line.\nSecond line."),
        );
        assert!(
            output.contains("def run():  # First line. Second line.\n"),
            "{output}"
        );
        assert_eq!(
            comment_block("x.py", "File purpose.\nSecond sentence."),
            "# File purpose.\n# Second sentence.\n"
        );
        assert_eq!(
            comment("x.md", "contains --> marker"),
            "<!-- contains - -> marker -->"
        );
        assert_eq!(
            comment("x.rs", "Explains behavior."),
            "// Explains behavior."
        );
    }

    #[test]
    fn single_symbol_scores_follow_the_symbol_in_map_renderers() {
        let structure = crate::parse::parse("x.rs", "fn run() {}\n")
            .unwrap()
            .structure;
        let node = &structure.nodes[0];
        assert_eq!(
            render_node(node, 0, Some("score=0.90"), Detail::Compact),
            "@@ 1 @@\nfn run()  // score=0.90\n"
        );
        assert_eq!(
            render_with_hits(
                &structure.nodes,
                None,
                Detail::Compact,
                None,
                Some(&structure),
                Descriptions::default(),
                HitDetails {
                    annotations: Some(&HashMap::from([(node.id, "score=0.90".to_owned())])),
                    callees: None,
                    extras: None,
                },
            ),
            "@@ 1 @@\nfn run()  // score=0.90\n"
        );
    }
}
