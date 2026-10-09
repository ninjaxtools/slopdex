//! Comment spans from Markdown's block and inline ASTs. Code spans and fenced
//! or indented examples are literal content, even when they look like HTML.

use super::super::syntax::text;
use super::document::{self, Positions, children};
use anyhow::{Context, Result};
use std::ops::Range;
use tree_sitter::{Node, Parser};

struct HtmlLiteral {
    span: Range<usize>,
    tag_start: usize,
}

fn markdown_literals(root: Node<'_>) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "code_span" | "backslash_escape") {
            result.push(node.byte_range());
        } else {
            stack.extend(children(node));
        }
    }
    result
}

/// The inline Markdown grammar can split a valid HTML start tag at a literal
/// '<' in an attribute. Use the HTML AST to recognize its opaque value instead
/// of interpreting the resulting nested Markdown comment token as a comment.
fn html_literals(root: Node<'_>, source: &str, excluded: &[Range<usize>]) -> Vec<HtmlLiteral> {
    let mut result = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let origin = if matches!(node.kind(), "quoted_attribute_value" | "attribute_value") {
            let mut parent = node.parent();
            let mut tag = None;
            while let Some(ancestor) = parent {
                if matches!(ancestor.kind(), "start_tag" | "self_closing_tag") {
                    tag = Some(ancestor);
                    break;
                }
                parent = ancestor.parent();
            }
            tag.filter(|tag| !tag.has_error())
                .map(|tag| tag.start_byte())
        } else if crate::parse::data::html_text_element(node, source) {
            Some(node.start_byte())
        } else {
            None
        };
        if let Some(tag_start) = origin {
            if !excluded.iter().any(|span| span.contains(&tag_start)) {
                result.push(HtmlLiteral {
                    span: node.byte_range(),
                    tag_start,
                });
            }
        } else {
            stack.extend(children(node));
        }
    }
    result
}

fn special_html_spans(
    root: Node<'_>,
    source: &str,
    end: usize,
    protected: &[Range<usize>],
) -> Vec<Range<usize>> {
    let mut result: Vec<Range<usize>> = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if protected
            .iter()
            .chain(&result)
            .any(|span| span.start <= node.start_byte() && node.end_byte() <= span.end)
        {
            continue;
        }
        let raw = text(source, node);
        if node.kind() == "html_tag" && raw.starts_with("<!--") {
            continue;
        }
        // These are explicit Markdown AST opener tokens, or a complete HTML
        // token. Searches are confined to this block/inline region, never prose
        // elsewhere in the file or delimiters inside a quoted HTML attribute.
        let closer = match node.kind() {
            "<![CDATA[" => Some("]]>"),
            "<?" => Some("?>"),
            "html_tag" if raw.starts_with("<![CDATA[") => Some("]]>"),
            "html_tag" if raw.starts_with("<?") => Some("?>"),
            _ => None,
        };
        if let Some(closer) = closer
            && let Some(length) = source[node.start_byte()..end].find(closer)
        {
            result.push(node.start_byte()..node.start_byte() + length + closer.len());
            continue;
        }
        let mut cursor = node.walk();
        let tokens: Vec<_> = node.children(&mut cursor).collect();
        stack.extend(tokens.into_iter().rev());
    }
    result
}

fn protected_html_spans(
    html: Node<'_>,
    inline: Node<'_>,
    source: &str,
    region: Node<'_>,
    markdown: bool,
) -> Vec<Range<usize>> {
    let excluded = if markdown {
        markdown_literals(inline)
    } else {
        Vec::new()
    };
    let mut html_literals = html_literals(html, source, &excluded);
    let mut protected = excluded.clone();
    protected.extend(html_literals.iter().map(|literal| literal.span.clone()));
    let mut special = special_html_spans(inline, source, region.end_byte(), &protected);
    if !markdown {
        let raw = text(source, region);
        let start = raw.trim_start_matches([' ', '\t']);
        let closer = if start.starts_with("<![CDATA[") {
            Some("]]>")
        } else if start.starts_with("<?") {
            Some("?>")
        } else {
            None
        };
        if let Some(closer) = closer {
            let byte = region.start_byte() + raw.len() - start.len();
            let end = start
                .find(closer)
                .map_or(region.end_byte(), |length| byte + length + closer.len());
            special.push(byte..end);
        }
    }
    // Tags inside CDATA/PI are text; the HTML grammar must not let a false
    // inner tag extend protection beyond the enclosing literal's delimiter.
    html_literals.retain(|literal| !special.iter().any(|span| span.contains(&literal.tag_start)));
    let mut result = excluded;
    result.extend(html_literals.into_iter().map(|literal| literal.span));
    result.extend(special);
    result
}

fn comment_tokens(root: Node<'_>, source: &str, markdown: bool) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if markdown && matches!(node.kind(), "code_span" | "backslash_escape") {
            continue;
        }
        if node.kind() == "comment"
            || node.kind() == "html_tag" && text(source, node).starts_with("<!--")
        {
            result.push(node.byte_range());
        } else {
            stack.extend(children(node));
        }
    }
    result
}

pub(super) fn collect(root: Node<'_>, source: &str) -> Result<Vec<Range<usize>>> {
    let mut inline_parser = Parser::new();
    inline_parser
        .set_language(&tree_sitter_md_025::INLINE_LANGUAGE.into())
        .context("Cannot initialize Markdown inline parser")?;
    let mut html_parser = Parser::new();
    html_parser
        .set_language(&tree_sitter_html::LANGUAGE.into())
        .context("Cannot initialize Markdown HTML parser")?;
    let positions = Positions::new(source);
    let mut comments = Vec::new();
    let mut prefixes = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "fenced_code_block" | "indented_code_block" => continue,
            "block_continuation"
            | "block_quote_marker"
            | "list_marker_plus"
            | "list_marker_minus"
            | "list_marker_star"
            | "list_marker_dot"
            | "list_marker_parenthesis" => prefixes.push(node.byte_range()),
            "inline" | "pipe_table_cell" => {
                let mut range = node.range();
                let mut ranges = Vec::new();
                for child in children(node)
                    .into_iter()
                    .filter(|child| child.kind() == "block_continuation")
                {
                    prefixes.push(child.byte_range());
                    if range.start_byte < child.start_byte() {
                        ranges.push(tree_sitter::Range {
                            end_byte: child.start_byte(),
                            end_point: child.start_position(),
                            ..range
                        });
                    }
                    range.start_byte = child.end_byte();
                    range.start_point = child.end_position();
                }
                if range.start_byte < range.end_byte {
                    ranges.push(range);
                }
                if ranges.is_empty() {
                    continue;
                }
                inline_parser
                    .set_included_ranges(&ranges)
                    .context("Cannot select Markdown inline ranges")?;
                let tree = inline_parser
                    .parse(source, None)
                    .context("Cannot parse Markdown inline content")?;
                html_parser
                    .set_included_ranges(&ranges)
                    .context("Cannot select Markdown inline HTML ranges")?;
                let html = html_parser
                    .parse(source, None)
                    .context("Cannot parse Markdown inline HTML")?;
                let protected =
                    protected_html_spans(html.root_node(), tree.root_node(), source, node, true);
                comments.extend(
                    comment_tokens(tree.root_node(), source, true)
                        .into_iter()
                        .filter(|comment| {
                            !protected.iter().any(|span| span.contains(&comment.start))
                        }),
                );
                continue;
            }
            "html_block" => {
                html_parser
                    .set_included_ranges(&[node.range()])
                    .context("Cannot select Markdown HTML range")?;
                let tree = html_parser
                    .parse(source, None)
                    .context("Cannot parse Markdown HTML content")?;
                inline_parser
                    .set_included_ranges(&[node.range()])
                    .context("Cannot select Markdown HTML token range")?;
                let inline = inline_parser
                    .parse(source, None)
                    .context("Cannot parse Markdown HTML tokens")?;
                let protected =
                    protected_html_spans(tree.root_node(), inline.root_node(), source, node, false);
                let found: Vec<_> = comment_tokens(tree.root_node(), source, false)
                    .into_iter()
                    .filter(|comment| !protected.iter().any(|span| span.contains(&comment.start)))
                    .collect();
                let raw = text(source, node);
                let start = raw.trim_start_matches([' ', '\t']);
                if found.is_empty() && start.starts_with("<!--") {
                    // An unclosed comment is an HTML block through EOF, even
                    // when the HTML grammar cannot produce a comment token.
                    let byte = node.start_byte() + raw.len() - start.len();
                    let end = start
                        .find("-->")
                        .map_or(node.end_byte(), |end| byte + end + 3);
                    comments.push(byte..end);
                } else {
                    comments.extend(found);
                }
            }
            _ => {}
        }
        stack.extend(children(node));
    }
    for comment in &mut comments {
        let row = positions.point(comment.start).row;
        let start = positions.line_start(row);
        let end = source[start..]
            .find('\n')
            .map_or(source.len(), |end| start + end);
        let prefix = document::clean_range(source, start..comment.start, &prefixes);
        // A container marker on a comment-only line is not visible prose. A
        // marker next to surviving prose on the same line must be preserved.
        if prefix.trim().is_empty()
            && (comment.end >= end || source[comment.end..end].trim().is_empty())
        {
            comment.start = start;
        }
    }
    Ok(comments)
}
