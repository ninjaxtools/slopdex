//! Shared AST access: comments are layout, not expressions or bindings.

use anyhow::{Context, Result};
use tree_sitter::{Language, Node, ParseOptions, Parser, Tree};

/// Per-parse wall-clock budget. Ordinary files under the size cap parse in
/// well under a second; grammars with superlinear worst cases (notably bash
/// command substitution nesting) must not stall indexing indefinitely.
const PARSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn parse_bounded_flag(
    parser: &mut Parser,
    source: &[u8],
    timeout: std::time::Duration,
    timed_out: &mut bool,
) -> Option<Tree> {
    let started = std::time::Instant::now();
    // The progress callback returns `true` to HALT parsing (it is consulted
    // alongside the cancellation flag and the deprecated timeout clock, at
    // most once per operation-count cycle, so checking the clock directly
    // is cheap enough).
    let mut deadline = |_: &tree_sitter::ParseState| {
        if started.elapsed() > timeout {
            *timed_out = true;
            return true;
        }
        false
    };
    let len = source.len();
    parser.parse_with_options(
        &mut |offset, _| {
            if offset < len {
                &source[offset..]
            } else {
                Default::default()
            }
        },
        None,
        Some(ParseOptions::new().progress_callback(&mut deadline)),
    )
}

/// Parse with the shared per-file deadline; `None` covers both tree-sitter
/// failure and a deadline abort (recovery callers simply skip).
pub(super) fn parse_bounded(parser: &mut Parser, source: impl AsRef<[u8]>) -> Option<Tree> {
    parse_bounded_flag(parser, source.as_ref(), PARSE_TIMEOUT, &mut false)
}

/// The tree-sitter grammar for a serialized language name. Delegates to the
/// total registry mapping [`super::LanguageId::grammar`]; unknown names have
/// no grammar.
pub(crate) fn grammar(language: &str) -> Option<Language> {
    super::LanguageId::from_name(language).map(super::LanguageId::grammar)
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
    let mut timed_out = false;
    let tree = parse_bounded_flag(
        &mut parser,
        source.as_bytes(),
        PARSE_TIMEOUT,
        &mut timed_out,
    )
    .with_context(|| {
        if timed_out {
            format!(
                "Cannot parse {path}: exceeded the {}s per-file parse limit",
                PARSE_TIMEOUT.as_secs()
            )
        } else {
            format!("Cannot parse {path}: tree-sitter returned no tree")
        }
    })?;
    Ok((parser, tree))
}

/// Parse a code file for a registry language. There is no string-named
/// entry point: unknown language names fail at
/// [`super::LanguageId::from_name`] / [`super::LanguageId::for_path`]
/// instead of being parsed with another language's grammar.
pub(super) fn parse_code(
    language: super::LanguageId,
    path: &str,
    source: &str,
) -> Result<(Parser, Tree)> {
    parse_with(&language.grammar(), language.as_str(), path, source)
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
    fn parse_code_rejects_unknown_languages_instead_of_falling_back() {
        // Unknown names have no registry entry, hence no grammar to fall
        // back to; supported dispatch only ever yields registry languages.
        for language in ["unknown", "", "cpp", "JavaScript"] {
            assert_eq!(super::super::LanguageId::from_name(language), None);
            assert!(grammar(language).is_none());
        }
        assert!(
            parse_code(
                super::super::LanguageId::JavaScript,
                "file.js",
                "function f() {}"
            )
            .is_ok()
        );
        assert!(parse_code(super::super::LanguageId::Rust, "file.rs", "fn f() {}").is_ok());
    }

    #[test]
    fn parse_deadline_aborts_and_reports_the_timeout() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        let source = "function f() { return 1; }\n".repeat(20_000);
        let mut timed_out = false;
        let tree = parse_bounded_flag(
            &mut parser,
            source.as_bytes(),
            std::time::Duration::ZERO,
            &mut timed_out,
        );
        assert!(tree.is_none());
        assert!(timed_out);
        // The same source parses normally with the real budget.
        assert!(parse_bounded(&mut parser, &source).is_some());
    }

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
