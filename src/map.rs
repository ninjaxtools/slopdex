//! Source-ordered declaration excerpts shared by map and semantic results.
//! Ranges refer to the original file; signatures deliberately omit bodies.

use crate::parse::{FileStructure, StructureNode};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

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
    pub extras: Option<&'a HashMap<usize, String>>,
}

/// Generated text is rendered as a language-appropriate comment, not source.
pub fn comment(language: &str, description: &str) -> String {
    let description = description.trim();
    match crate::parse::language_for_path(language).unwrap_or(language) {
        "python" => format!("# {description}"),
        "markdown" => {
            // `--` is invalid inside HTML comments, including generated descriptions.
            format!("<!-- {} -->", description.replace("--", "- -"))
        }
        "c" => format!("/* {description} */"),
        _ => format!("// {description}"),
    }
}

pub fn comment_block(language: &str, description: &str) -> String {
    description
        .lines()
        .map(|line| format!("{}\n", comment(language, line)))
        .collect()
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
    let mut children: HashMap<usize, Vec<&StructureNode>> = HashMap::new();
    for node in &ordered {
        if let Some(parent_id) = node.parent_id {
            children.entry(parent_id).or_default().push(node);
        }
    }
    let mut full_children: HashMap<Option<usize>, Vec<&StructureNode>> = HashMap::new();
    if let Some(structure) = full_structure {
        let mut full: Vec<_> = structure.nodes.iter().collect();
        full.sort_by_key(|node| (node.start_byte, node.id));
        for node in full {
            full_children.entry(node.parent_id).or_default().push(node);
        }
    }
    let indexed_children: HashMap<_, _> = full_children
        .iter()
        .filter_map(|(id, nodes)| id.map(|id| (id, nodes.clone())))
        .collect();
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
    let foldable: HashSet<_> = ordered
        .iter()
        .filter(|node| {
            foldable_parent(node, &children)
                && full_structure.is_none_or(|_| {
                    if !foldable_parent(node, &indexed_children) {
                        return false;
                    }
                    let Some(full) = full_children.get(&Some(node.id)) else {
                        return false;
                    };
                    children.get(&node.id).is_some_and(|selected| {
                        selected.iter().map(|n| n.id).eq(full.iter().map(|n| n.id))
                    })
                })
        })
        .map(|node| node.id)
        .collect();
    let source_lines = if nodes.first().is_some_and(|node| node.kind == "heading") {
        source.map(|source| source.lines().collect::<Vec<_>>())
    } else {
        None
    };
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
        let adjacent_sibling = group.last().is_some_and(|previous| {
            let parent_present = node.parent_id.is_none_or(|id| by_id.contains_key(&id));
            let previous_parent_present =
                previous.parent_id.is_none_or(|id| by_id.contains_key(&id));
            let end = display_end(previous);
            let adjacent_child = group.first().is_some_and(|parent| {
                foldable.contains(&parent.id)
                    && node.parent_id == Some(parent.id)
                    && (previous.id == parent.id || previous.parent_id == Some(parent.id))
                    && if previous.id == parent.id {
                        node.start_line == parent.start_line + 1
                    } else {
                        end.checked_add(1) == Some(node.start_line)
                    }
            });
            adjacent_child
                || (parent_present
                    && previous_parent_present
                    && previous.kind == node.kind
                    && !foldable.contains(&node.id)
                    && if node.kind == "heading" {
                        end < node.start_line
                            && (end + 1 == node.start_line
                                || source_lines.as_ref().is_some_and(|lines| {
                                    lines.get(end..node.start_line - 1).is_some_and(|gap| {
                                        gap.iter().all(|line| line.trim().is_empty())
                                    })
                                }))
                            && full_structure.is_none_or(|_| {
                                full_heading_next.get(&previous.id) == Some(&node.id)
                            })
                    } else {
                        previous.parent_id == node.parent_id
                            && end.checked_add(1) == Some(node.start_line)
                            && full_structure
                                .is_none_or(|_| full_next.get(&previous.id) == Some(&node.id))
                    })
        });
        if (!adjacent_sibling
            || hits.annotations.is_some_and(|scores| {
                scores.contains_key(&node.id)
                    || group
                        .iter()
                        .any(|previous| scores.contains_key(&previous.id))
            }))
            && !group.is_empty()
        {
            append_group(
                &mut output,
                &group,
                &by_id,
                detail,
                descriptions.symbols,
                hits.annotations,
                hits.extras,
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
            hits.annotations,
            hits.extras,
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
    annotations: Option<&HashMap<usize, String>>,
    extras: Option<&HashMap<usize, String>>,
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
        annotations
            .and_then(|annotations| annotations.get(&first.id).map(String::as_str))
            .or(qualified),
    ));
    for node in group {
        output.push_str(&declaration(
            node,
            depth(node, by_id),
            detail,
            descriptions.and_then(|descriptions| descriptions.get(&node.id).map(String::as_str)),
        ));
        if let Some(extra) = extras.and_then(|extras| extras.get(&node.id)) {
            output.push_str(extra);
        }
    }
}

/// Only collapse a container into its children when they occupy the lines
/// immediately after its header and up to its closing line. Other declaration
/// shapes keep their own ranges so an omitted body is not mistaken for context.
fn foldable_parent(parent: &StructureNode, children: &HashMap<usize, Vec<&StructureNode>>) -> bool {
    if !matches!(
        parent.kind.as_str(),
        "class" | "struct" | "union" | "enum" | "interface" | "trait" | "impl" | "module" | "type"
    ) {
        return false;
    }
    let Some(inside) = children.get(&parent.id).filter(|inside| !inside.is_empty()) else {
        return false;
    };
    if inside.iter().any(|node| children.contains_key(&node.id)) {
        return false;
    }
    let mut previous_end = parent.start_line;
    for node in inside {
        if previous_end.checked_add(1) != Some(node.start_line) {
            return false;
        }
        previous_end = display_end(node);
    }
    let parent_end = display_end(parent);
    parent_end == previous_end || previous_end.checked_add(1) == Some(parent_end)
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
    if node.kind == "heading" {
        node.start_line + node.signature.lines().count().saturating_sub(1)
    } else {
        inclusive_end(node.end_line, node.end_column, node.start_line)
    }
}

pub fn render_node(
    node: &StructureNode,
    depth: usize,
    annotation: Option<&str>,
    detail: Detail,
) -> String {
    let end = display_end(node);
    let mut output = hunk(node.start_line, end, annotation);
    output.push_str(&declaration(node, depth, detail, None));
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
    let mut output = hunk(matched.start_line, display_end(matched), annotation);
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
        ));
    }
    output
}

pub fn render_declarations(nodes: &[StructureNode], detail: Detail) -> String {
    let mut output = String::new();
    for (depth, node) in nodes.iter().enumerate() {
        output.push_str(&declaration(node, depth, detail, None));
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
        if index + 1 == line_count
            && let Some(description) =
                description.filter(|description| !description.trim().is_empty())
        {
            let flattened = description.split_whitespace().collect::<Vec<_>>().join(" ");
            output.push_str("  ");
            output.push_str(&comment(&node.language, &flattened));
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
            matches!(
                node.kind.as_str(),
                "function" | "method" | "constructor" | "generator"
            ) && (qualified == Some(node.qualified_name.as_str())
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
    fn source_order_and_ranges_without_bodies_or_kind_sections() {
        let source = "use std::io;\n#[derive(Clone)]\npub struct S { pub x: i32 }\nimpl S { pub fn go(&self) { secret(); } }";
        let structure = crate::parse::parse("x.rs", source).unwrap().structure;
        let output = render_nodes(&structure.nodes, Some("x.rs"));
        assert!(
            output.starts_with("*** x.rs\n\n@@ 1 @@\nuse std::io\n"),
            "{output}"
        );
        assert!(output.contains("@@ 2-3 @@\npub struct S\n"), "{output}");
        assert!(output.contains("@@ 4 @@\n  pub fn go(&self)\n"), "{output}");
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
    fn adjacent_methods_share_one_hunk_but_gaps_and_nesting_do_not() {
        let source = "impl Writer {\n    fn write(&mut self) {\n        do_write();\n    }\n    fn flush(&mut self) {\n        do_flush();\n    }\n\n    fn close(&mut self) {\n        do_close();\n    }\n}\n";
        let structure = crate::parse::parse("writer.rs", source).unwrap().structure;
        let output = render_nodes(&structure.nodes, Some("writer.rs"));
        assert!(
            output.contains("@@ 2-7 @@\n  fn write(&mut self)\n  fn flush(&mut self)\n"),
            "{output}"
        );
        assert!(
            output.contains("@@ 9-11 @@\n  fn close(&mut self)\n"),
            "{output}"
        );
        assert!(!output.contains("do_write") && !output.contains("do_flush"));
        assert_eq!(output.matches("@@ 2-7 @@").count(), 1);
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
    fn container_with_gaps_keeps_member_ranges_separate() {
        let source = "export interface PipeAddress {\n  readonly read: string\n\n  readonly write: string\n}\n";
        let structure = crate::parse::parse("pipe.ts", source).unwrap().structure;
        let output = render_nodes(&structure.nodes, None);
        assert!(
            output.contains("@@ 1-5 @@\nexport interface PipeAddress\n"),
            "{output}"
        );
        assert!(
            output.contains("@@ 2 @@\n  readonly read: string\n"),
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
            output.contains("@@ 1-4 @@\nexport interface PipeAddress\n"),
            "{output}"
        );
        assert!(
            output.contains("@@ 2 @@\n  readonly read: string\n"),
            "{output}"
        );
        assert!(
            output.contains("@@ 3 @@\n  readonly write: string\n"),
            "{output}"
        );
        assert!(!output.contains("hidden"));
    }

    #[test]
    fn filtered_nested_declarations_keep_the_parent_hunk_separate() {
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
        assert!(output.contains("@@ 1-5 @@\nimpl Service\n"), "{output}");
        assert!(
            output.contains("@@ 2-4 @@\n  pub fn run(&self)\n"),
            "{output}"
        );
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
            output.contains("@@ 1-5 @@\n# Guide\n  ## Setup\n    ### Details\n"),
            "{output}"
        );
        assert!(output.contains("@@ 8 @@\n  ## More\n"), "{output}");
        assert_eq!(output.matches("# Guide").count(), 1);
        assert!(!output.contains("Read this") && !output.contains("Next section"));
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
}
