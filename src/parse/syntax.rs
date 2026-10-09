//! Shared AST access: comments are layout, not expressions or bindings.

use tree_sitter::Node;

pub(super) fn text<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    // Recovered trees keep byte offsets identical to the original source.
    source.get(node.byte_range()).unwrap_or("")
}

pub(super) fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !matches!(child.kind(), "comment" | "line_comment" | "block_comment"))
        .collect()
}

pub(super) fn unwrap_value(mut node: Node<'_>) -> Node<'_> {
    while matches!(node.kind(), "parenthesized_expression" | "expression_list") {
        let inner = children(node);
        if inner.len() != 1 {
            break;
        }
        node = inner[0];
    }
    node
}

pub(super) fn rust_binding(mut node: Node<'_>) -> Option<Node<'_>> {
    while matches!(
        node.kind(),
        "ref_pattern" | "mut_pattern" | "reference_pattern"
    ) {
        node = node
            .child_by_field_name("pattern")
            .or_else(|| children(node).into_iter().next())?;
    }
    (node.kind() == "identifier").then_some(node)
}

pub(super) fn declarator_inner(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("declarator").or_else(|| {
        matches!(
            node.kind(),
            "parenthesized_declarator" | "attributed_declarator"
        )
        .then(|| {
            children(node).into_iter().find(|child| {
                child.kind().ends_with("_declarator")
                    || matches!(
                        child.kind(),
                        "identifier" | "field_identifier" | "type_identifier"
                    )
            })
        })
        .flatten()
    })
}

pub(super) fn js_key(source: &str, node: Node<'_>) -> String {
    let value = source.get(node.byte_range()).unwrap_or("");
    if node.kind() == "string" && value.len() >= 2 {
        value[1..value.len() - 1].to_owned()
    } else {
        value.trim_start_matches('#').to_owned()
    }
}

pub(super) fn shell_range(node: Node<'_>) -> Node<'_> {
    node.parent()
        .filter(|parent| {
            parent.kind() == "redirected_statement"
                && parent.child_by_field_name("body") == Some(node)
        })
        .unwrap_or(node)
}

pub(super) fn shell_redirections(node: Node<'_>) -> Vec<Node<'_>> {
    let mut parts: Vec<_> = children(node)
        .into_iter()
        .filter(|child| child.kind().ends_with("redirect"))
        .collect();
    let range = shell_range(node);
    if range != node {
        parts.extend(children(range).into_iter().filter(|child| *child != node));
    }
    parts
        .into_iter()
        .flat_map(|part| {
            if part.kind() == "heredoc_redirect" {
                let sibling = part.child_by_field_name("right");
                children(part)
                    .into_iter()
                    .filter(|child| Some(*child) != sibling && child.kind() != "pipeline")
                    .collect()
            } else {
                vec![part]
            }
        })
        .collect()
}

pub(super) fn shell_siblings(node: Node<'_>) -> Vec<Node<'_>> {
    let range = shell_range(node);
    children(range)
        .into_iter()
        .filter(|part| part.kind() == "heredoc_redirect")
        .flat_map(|part| {
            let right = part.child_by_field_name("right");
            children(part)
                .into_iter()
                .filter(|child| Some(*child) == right || child.kind() == "pipeline")
                .collect::<Vec<_>>()
        })
        .collect()
}

pub(super) fn shell_owner(mut node: Node<'_>) -> Option<Node<'_>> {
    let mut bypass = false;
    while let Some(parent) = node.parent() {
        if parent.kind() == "heredoc_redirect"
            && (parent.child_by_field_name("right") == Some(node) || node.kind() == "pipeline")
        {
            bypass = true;
        }
        let owner = if parent.kind() == "function_definition" {
            Some(parent)
        } else if parent.kind() == "redirected_statement" {
            parent
                .child_by_field_name("body")
                .filter(|body| body.kind() == "function_definition")
        } else {
            None
        };
        if let Some(owner) = owner {
            if !bypass {
                return Some(owner);
            }
            bypass = false;
        }
        node = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    #[test]
    fn text_reads_original_unicode_source_after_offset_preserving_repairs() {
        let source = "const café = ???;";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("const café = 123;", None).unwrap();
        let declaration = tree.root_node().named_child(0).unwrap();
        let binding = declaration.named_child(0).unwrap();
        let name = binding.child_by_field_name("name").unwrap();
        let value = binding.child_by_field_name("value").unwrap();

        assert_eq!(text(source, name), "café");
        assert_eq!(text(source, value), "???");
    }

    #[test]
    fn text_returns_empty_for_invalid_source_ranges() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("x", None).unwrap();
        let node = tree.root_node();

        assert_eq!(text("", node), "");
        assert_eq!(text("é", node), "");
    }
}
