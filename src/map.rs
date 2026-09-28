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
    let by_id: HashMap<_, _> = nodes.iter().map(|node| (node.id, node)).collect();
    let mut ordered: Vec<_> = nodes.iter().collect();
    ordered.sort_by_key(|node| (node.start_byte, node.id));
    let mut output = path.map(file_header).unwrap_or_default();
    for node in ordered {
        if !output.is_empty() {
            output.push('\n');
        }
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
        let qualified =
            (depth == 0 && node.parent_id.is_some() && node.qualified_name != node.name)
                .then_some(node.qualified_name.as_str());
        output.push_str(&render_node(node, depth, qualified, detail));
    }
    output
}

pub fn render_node(
    node: &StructureNode,
    depth: usize,
    annotation: Option<&str>,
    detail: Detail,
) -> String {
    let end = inclusive_end(node.end_line, node.end_column, node.start_line);
    let mut output = hunk(node.start_line, end, annotation);
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
    output.push_str(&indent);
    output.push_str(&signature);
    output.push('\n');
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
}
