//! Compact declaration maps rendered exclusively from canonical parse metadata.
//! Rendering limits never alter the underlying nodes or JSON representation.

use crate::parse::{FileStructure, StructureNode};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct RenderOptions {
    /// Unicode character limit per signature/attribute; None disables it.
    pub max_signature_chars: Option<usize>,
    /// Maximum children displayed per parent; None disables it.
    pub max_children: Option<usize>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            max_signature_chars: Some(180),
            max_children: Some(30),
        }
    }
}

pub fn render(structure: &FileStructure, path: Option<&str>) -> String {
    render_nodes(&structure.nodes, path)
}

/// Accepts filtered nodes as well as whole files. A node whose parent was
/// filtered out is rendered as a root and retains its qualified name.
pub fn render_nodes(nodes: &[StructureNode], path: Option<&str>) -> String {
    render_with_options(nodes, path, &RenderOptions::default())
}

pub fn render_with_options(
    nodes: &[StructureNode],
    path: Option<&str>,
    options: &RenderOptions,
) -> String {
    let ids: HashSet<_> = nodes.iter().map(|n| n.id).collect();
    let mut children: HashMap<usize, Vec<&StructureNode>> = HashMap::new();
    let mut roots = Vec::new();
    for node in nodes {
        if let Some(parent) = node.parent_id.filter(|id| ids.contains(id)) {
            children.entry(parent).or_default().push(node);
        } else {
            roots.push(node);
        }
    }
    roots.sort_by_key(|n| (n.start_byte, n.id));
    for nodes in children.values_mut() {
        nodes.sort_by_key(|n| (n.start_byte, n.id));
    }
    let mut sections = Vec::new();
    if let Some(path) = path {
        sections.push(path.to_owned());
    }
    for section in [
        "imports", "mod", "consts", "types", "traits", "impls", "fns", "classes", "macros",
        "headings", "other",
    ] {
        let entries: Vec<_> = roots
            .iter()
            .copied()
            .filter(|n| section_for(&n.kind) == section)
            .collect();
        if entries.is_empty() {
            continue;
        }
        let mut lines = vec![format!("{section}:")];
        let mut visited = HashSet::new();
        for entry in entries {
            render_node(entry, 1, &children, options, &mut visited, &mut lines);
        }
        sections.push(lines.join("\n"));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("{}\n", sections.join("\n\n"))
    }
}

fn section_for(kind: &str) -> &'static str {
    match kind {
        "import" => "imports",
        "module" | "package" => "mod",
        "constant" | "variable" | "field" => "consts",
        "type" | "struct" | "union" | "enum" | "interface" | "variant" => "types",
        "trait" => "traits",
        "impl" => "impls",
        "function" | "method" | "constructor" | "generator" => "fns",
        "class" => "classes",
        "macro" => "macros",
        "heading" => "headings",
        _ => "other",
    }
}

fn render_node(
    node: &StructureNode,
    depth: usize,
    children: &HashMap<usize, Vec<&StructureNode>>,
    options: &RenderOptions,
    visited: &mut HashSet<usize>,
    lines: &mut Vec<String>,
) {
    if !visited.insert(node.id) {
        return;
    }
    let indent = "  ".repeat(depth);
    for attr in &node.attributes {
        for line in display(attr, options.max_signature_chars).lines() {
            lines.push(format!("{indent}{line}"));
        }
    }
    let signature = if node.signature.is_empty() {
        &node.name
    } else {
        &node.signature
    };
    let signature = display(signature, options.max_signature_chars);
    let signature = if depth == 1 && node.parent_id.is_some() && node.qualified_name != node.name {
        format!("{}: {signature}", node.qualified_name)
    } else {
        signature
    };
    // End coordinates are exclusive; a column-1 end belongs to the prior line
    // for human-readable inclusive line ranges.
    let end = if node.end_column == 1 && node.end_line > node.start_line {
        node.end_line - 1
    } else {
        node.end_line
    };
    let range = if end <= node.start_line {
        format!("[{}]", node.start_line)
    } else {
        format!("[{}-{end}]", node.start_line)
    };
    let mut signature_lines = signature.lines();
    lines.push(format!(
        "{indent}{} {range}",
        signature_lines.next().unwrap_or("")
    ));
    for line in signature_lines {
        lines.push(format!("{indent}{line}"));
    }
    if let Some(children_of_node) = children.get(&node.id) {
        let count = options
            .max_children
            .unwrap_or(usize::MAX)
            .min(children_of_node.len());
        for child in &children_of_node[..count] {
            render_node(child, depth + 1, children, options, visited, lines);
        }
        if count < children_of_node.len() {
            lines.push(format!(
                "{indent}  [{} more truncated]",
                children_of_node.len() - count
            ));
        }
    }
}

fn display(value: &str, limit: Option<usize>) -> String {
    // Signatures are already normalized by the parser. Re-tokenizing whitespace
    // here changes literal types, template strings and attribute arguments.
    let value = value.trim().to_owned();
    let Some(limit) = limit else {
        return value;
    };
    if value.chars().count() <= limit {
        return value;
    }
    let mut result: String = value.chars().take(limit.saturating_sub(1)).collect();
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hierarchical_ranges_attributes_and_filtered_roots() {
        let parsed = crate::parse::parse(
            "x.rs",
            "#[derive(Clone)]\nstruct S { x: i32 }\nimpl S { fn go(&self) { secret(); } }",
        )
        .unwrap();
        let output = render(&parsed.structure, Some("x.rs"));
        assert!(output.starts_with("x.rs\n\ntypes:"));
        assert!(
            output.contains("#[derive(Clone)]\n  struct S [1-2]\n    x: i32 [2]"),
            "{output}"
        );
        assert!(!output.contains("secret"));
        let filtered: Vec<_> = parsed
            .structure
            .nodes
            .into_iter()
            .filter(|n| n.kind == "method")
            .collect();
        assert!(render_nodes(&filtered, None).contains("S.go: fn go(&self)"));
    }

    #[test]
    fn truncation_is_display_only_and_unicode_safe() {
        let source = format!(
            "struct S {{ {} }}",
            (0..50)
                .map(|n| format!("field_{n}: VeryLongUnicode型"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let structure = crate::parse::parse("x.rs", &source).unwrap().structure;
        let before = serde_json::to_string(&structure).unwrap();
        let output = render_with_options(
            &structure.nodes,
            None,
            &RenderOptions {
                max_signature_chars: Some(16),
                max_children: Some(2),
            },
        );
        assert!(output.contains('…'));
        assert!(output.contains("[48 more truncated]"));
        assert_eq!(before, serde_json::to_string(&structure).unwrap());
        assert!(
            render_with_options(
                &structure.nodes,
                None,
                &RenderOptions {
                    max_signature_chars: None,
                    max_children: None
                }
            )
            .contains("field_49: VeryLongUnicode型")
        );
    }

    #[test]
    fn literal_and_attribute_whitespace_survives_rendering() {
        let parsed = crate::parse::parse("x.ts", "type Literal = 'two  words' | `line one\n  line two`; @decorate('a  b') class C { run() { implementation(); } }").unwrap();
        let output = render(&parsed.structure, None);
        assert!(output.contains("'two  words'"), "{output}");
        assert!(output.contains("`line one"));
        assert!(output.contains("    line two`"));
        assert!(output.contains("@decorate('a  b')"));
        assert!(!output.contains("implementation"));
        // Preserve raw attributes without attempting to interpret their quoting.
        let attribute = "#[example(r###\"one \"  two\"###)]";
        assert_eq!(display(attribute, None), attribute);
        assert_eq!(display("`first\n  second`", None), "`first\n  second`");
    }
}
