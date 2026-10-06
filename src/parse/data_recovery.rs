//! Bounded repairs of unexpected scalar tokens. Never blank an ERROR subtree:
//! grammars can include healthy siblings, strings, or heredocs in that range.

use super::document::text;
use tree_sitter::{Node, Parser, Tree};

const MAX_REPAIRS: usize = 16;
const MAX_CANDIDATES: usize = 16;

fn errors(root: Node<'_>) -> usize {
    let mut count = 0;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        count += usize::from(node.is_error() || node.is_missing());
        if node.has_error() || node.is_error() {
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
    }
    count
}

fn candidates(root: Node<'_>, source: &str, language: &str) -> Vec<usize> {
    let mut result = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "string"
                | "string_lit"
                | "quoted_template"
                | "heredoc_template"
                | "string_value"
                | "block_scalar"
                | "single_quote_scalar"
                | "double_quote_scalar"
        ) {
            continue;
        }
        if node.child_count() == 0
            && node.parent().is_some_and(|parent| parent.is_error())
            && node
                .prev_sibling()
                .is_some_and(|previous| matches!(previous.kind(), "=" | ":" | ","))
            && matches!(
                (language, text(source, node)),
                ("terraform", ")") | ("toml", "?") | ("css", "@")
            )
        {
            result.push(node.start_byte());
        }
        if node.has_error() || node.is_error() {
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
    }
    // YAML's scanner can stop before an unexpected flow delimiter and omit the
    // entire suffix from its tree. The last AST token proves the value position.
    if language == "yaml" && root.is_error() {
        let mut cursor = root.walk();
        let last = root.children(&mut cursor).last();
        if last.is_some_and(|node| matches!(node.kind(), "," | ":")) {
            let suffix = &source[root.end_byte()..];
            let byte = root.end_byte() + suffix.len() - suffix.trim_start().len();
            if source.as_bytes().get(byte) == Some(&b'}') {
                result.push(byte);
            }
        }
    }
    result
}

pub(super) fn recover(parser: &mut Parser, mut tree: Tree, source: &str, language: &str) -> Tree {
    if errors(tree.root_node()) == 0 {
        return tree;
    }
    let mut repaired = source.as_bytes().to_vec();
    for _ in 0..MAX_REPAIRS {
        let score = errors(tree.root_node());
        if score == 0 {
            break;
        }
        let mut improved = None;
        for byte in candidates(tree.root_node(), source, language)
            .into_iter()
            .take(MAX_CANDIDATES)
        {
            let old = repaired[byte];
            if old == b'0' {
                continue;
            }
            repaired[byte] = b'0';
            if let Some(candidate) = parser.parse(&repaired, None)
                && errors(candidate.root_node()) < score
                && candidate.root_node().end_byte() >= tree.root_node().end_byte()
            {
                improved = Some(candidate);
                break;
            }
            repaired[byte] = old;
        }
        let Some(candidate) = improved else {
            break;
        };
        tree = candidate;
    }
    tree
}
