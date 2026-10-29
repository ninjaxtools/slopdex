//! Shared AST access: comments are layout, not expressions or bindings.

use anyhow::{Context, Result};
use tree_sitter::{Language, Node, Parser, Tree};

/// The tree-sitter grammar for every name [`super::language_for_path`] returns.
pub(crate) fn grammar(language: &str) -> Option<Language> {
    Some(match language {
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "javascript" | "jsx" => tree_sitter_javascript::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "c" => tree_sitter_c::LANGUAGE.into(),
        "bash" => tree_sitter_bash::LANGUAGE.into(),
        "markdown" => tree_sitter_md_025::LANGUAGE.into(),
        "json" => tree_sitter_json::LANGUAGE.into(),
        "terraform" => tree_sitter_hcl::LANGUAGE.into(),
        "yaml" => tree_sitter_yaml::LANGUAGE.into(),
        "toml" => tree_sitter_toml_ng::LANGUAGE.into(),
        "xml" => tree_sitter_xml::LANGUAGE_XML.into(),
        "html" => tree_sitter_html::LANGUAGE.into(),
        "css" => tree_sitter_css::LANGUAGE.into(),
        _ => return None,
    })
}

/// Parse `source` with `grammar`, returning the parser for recovery reparses.
/// Parser initialization/failure is an error; syntax errors stay in the tree.
pub(super) fn parse_with(
    grammar: &Language,
    language: &str,
    path: &str,
    source: &str,
) -> Result<(Parser, Tree)> {
    let mut parser = Parser::new();
    parser
        .set_language(grammar)
        .with_context(|| format!("Cannot initialize {language} parser for {path}"))?;
    let tree = parser
        .parse(source, None)
        .with_context(|| format!("Cannot parse {path}: tree-sitter returned no tree"))?;
    Ok((parser, tree))
}

/// Parse a code file; unknown names fall back to the JavaScript grammar, as
/// every JS-family language shares it.
pub(super) fn parse_code(language: &str, path: &str, source: &str) -> Result<(Parser, Tree)> {
    let grammar = grammar(language).unwrap_or_else(|| tree_sitter_javascript::LANGUAGE.into());
    parse_with(&grammar, language, path, source)
}

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
    if node.kind() == "string" {
        // An unterminated string (MISSING closing quote) ends in arbitrary
        // content, possibly mid-character; only strip a matching ASCII pair.
        let bytes = value.as_bytes();
        if bytes.len() >= 2
            && matches!(
                (bytes[0], bytes[bytes.len() - 1]),
                (b'"', b'"') | (b'\'', b'\'') | (b'`', b'`')
            )
        {
            return value[1..value.len() - 1].to_owned();
        }
        value.to_owned()
    } else {
        value.trim_start_matches('#').to_owned()
    }
}

/// Maximum AST depth a recursive extractor descends.
const MAX_DEPTH: usize = 512;
/// Maximum stack a recursive extractor may consume. Frames of unoptimized
/// builds are several kilobytes, so the level count alone cannot protect a
/// default 2 MiB thread stack.
const MAX_STACK_BYTES: usize = 768 * 1024;

const DEPTH_MESSAGE: &str =
    "Nesting deeper than the supported syntax depth; declarations below this point were skipped.";

/// Recursion guard for the declaration walkers. Tree-sitter trees are heap
/// allocated, but the walkers recurse once per syntax level; a region nested
/// too deeply is skipped and reported once instead of overflowing the stack.
#[derive(Default)]
pub(super) struct Descent {
    depth: usize,
    base: usize,
    /// The first region that was skipped.
    pub(super) truncated: Option<super::Diagnostic>,
}

impl Descent {
    /// Returns whether `node` may be visited; pair every `true` with `leave`.
    #[inline(never)]
    pub(super) fn enter(&mut self, node: Node<'_>) -> bool {
        let probe = 0u8;
        let here = std::hint::black_box(std::ptr::addr_of!(probe)) as usize;
        if self.depth == 0 {
            self.base = here;
        }
        if self.depth >= MAX_DEPTH || self.base.abs_diff(here) >= MAX_STACK_BYTES {
            if self.truncated.is_none() {
                self.truncated = Some(super::Diagnostic {
                    message: DEPTH_MESSAGE.to_owned(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                });
            }
            return false;
        }
        self.depth += 1;
        true
    }

    pub(super) fn leave(&mut self) {
        self.depth -= 1;
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
    fn js_key_strips_only_matching_ascii_quotes() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        for (source, expected) in [
            ("const o = { \"\u{e9}\n: 1 };", "\"\u{e9}"),
            ("const o = { \"caf\u{e9}\": 1 };", "caf\u{e9}"),
            ("const o = { 'k': 1 };", "k"),
            ("const o = { \"\n: 1 };", "\""),
        ] {
            let tree = parser.parse(source, None).unwrap();
            let mut stack = vec![tree.root_node()];
            let mut keys = Vec::new();
            while let Some(node) = stack.pop() {
                if node.kind() == "string" {
                    keys.push(js_key(source, node));
                }
                let mut cursor = node.walk();
                stack.extend(node.children(&mut cursor));
            }
            assert_eq!(keys, [expected], "{source:?}");
        }
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
