//! Tree-sitter Markdown headings and heading-aware search chunks.

use super::data::document::{self, Positions, children, text};
use super::{FileStructure, MarkdownChunk, ParsedFile, StructureNode};
use crate::hash;
use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

#[path = "markdown_comments.rs"]
mod comments;

const MARKDOWN_MAX_BYTES: usize = 8192;
const MARKDOWN_MAX_LINES: usize = 120;
const MARKDOWN_HEADING_BYTES: usize = 2048;

struct Heading {
    level: usize,
    title: String,
    source: String,
    start_byte: usize,
    start_row: usize,
    start_column: usize,
    end_row: usize,
}

fn blocks(root: Node<'_>, source: &str, comments: &[std::ops::Range<usize>]) -> Vec<Heading> {
    let mut headings = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "atx_heading" | "setext_heading" if !node.has_error() => {
                let atx = node.kind() == "atx_heading";
                let marker = if atx {
                    node.named_child(0)
                } else {
                    children(node).into_iter().find(|child| {
                        matches!(child.kind(), "setext_h1_underline" | "setext_h2_underline")
                    })
                };
                let level = if atx {
                    marker.and_then(|m| {
                        m.kind()
                            .strip_prefix("atx_h")?
                            .strip_suffix("_marker")?
                            .parse()
                            .ok()
                    })
                } else {
                    marker.and_then(|child| match child.kind() {
                        "setext_h1_underline" => Some(1),
                        "setext_h2_underline" => Some(2),
                        _ => None,
                    })
                };
                if let Some(level) = level {
                    let content_node = if atx {
                        node.child_by_field_name("heading_content")
                    } else {
                        children(node)
                            .into_iter()
                            .find(|child| child.kind() == "paragraph")
                            .and_then(|paragraph| {
                                children(paragraph)
                                    .into_iter()
                                    .find(|child| child.kind() == "inline")
                            })
                    };
                    let content = content_node
                        .map(|content| {
                            let mut range = content.byte_range();
                            if atx {
                                // Comments cannot create a closing marker that
                                // was not present in the original heading.
                                let raw = text(source, content).trim_end_matches([' ', '\t']);
                                let without_hashes = raw.trim_end_matches('#');
                                if without_hashes.is_empty()
                                    || without_hashes.ends_with([' ', '\t'])
                                {
                                    range.end = range.start + without_hashes.len();
                                }
                            }
                            let mut excluded = comments.to_vec();
                            excluded.extend(
                                children(content)
                                    .into_iter()
                                    .filter(|child| child.kind() == "block_continuation")
                                    .map(|child| child.byte_range()),
                            );
                            document::clean_range(source, range, &excluded)
                        })
                        .unwrap_or_default();
                    let title = content.trim();
                    let signature = if atx {
                        let end = source[node.start_byte()..]
                            .find('\n')
                            .map_or(source.len(), |end| node.start_byte() + end);
                        document::clean_range(source, node.start_byte()..end, comments)
                            .trim_start_matches(' ')
                            .trim_end_matches(['\r', '\n'])
                            .to_owned()
                    } else {
                        format!(
                            "{title}\n{}",
                            text(source, marker.unwrap()).trim_end_matches(['\r', '\n'])
                        )
                    };
                    let end_row = marker.unwrap().start_position().row;
                    headings.push(Heading {
                        level,
                        title: title.to_owned(),
                        source: signature,
                        start_byte: node.start_byte(),
                        start_row: node.start_position().row,
                        start_column: node.start_position().column,
                        end_row,
                    });
                }
                continue;
            }
            "html_block" => continue,
            "fenced_code_block" | "indented_code_block" => continue,
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    headings.sort_by_key(|heading| heading.start_byte);
    headings
}

/// Bounded text search units for structured data, without interpreting `#` as headings.
pub(super) fn parse_plain(source: &str) -> Vec<MarkdownChunk> {
    let lines: Vec<_> = source
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let mut chunks = Vec::new();
    flush_markdown(&lines, 0, lines.len(), 0, &[], &mut chunks);
    chunks
}

pub(super) fn parse(source: &str) -> Result<ParsedFile> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_md_025::LANGUAGE.into())
        .context("Cannot initialize Markdown parser")?;
    let mut tree = parser
        .parse(source, None)
        .context("Cannot parse Markdown: tree-sitter returned no tree")?;
    // The block grammar accepts a closing fence indented four columns beyond
    // its opener. CommonMark treats it as code. Mask only those false closers
    // and reparse with identical byte offsets, as for TypeScript grammar gaps.
    let mut masked = source.as_bytes().to_vec();
    let positions = Positions::new(source);
    loop {
        let mut changed = false;
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "fenced_code_block" {
                let mut cursor = node.walk();
                let fences: Vec<_> = node
                    .children(&mut cursor)
                    .filter(|child| child.kind() == "fenced_code_block_delimiter")
                    .collect();
                if let Some(closer) = fences.get(1) {
                    let indent = fence_indent(node, *closer, source, &positions);
                    if indent > 3 && masked[closer.start_byte()] != b'X' {
                        masked[closer.start_byte()] = b'X';
                        changed = true;
                    }
                }
                continue;
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        if !changed {
            break;
        }
        tree = parser
            .parse(&masked, None)
            .context("Cannot recover Markdown fence: tree-sitter returned no tree")?;
    }
    let mut comments = comments::collect(tree.root_node(), source)?;
    let detected = blocks(tree.root_node(), source, &comments);
    let structure = structure(source, &detected);
    let filtered = document::without_spans(source, &mut comments);
    let lines: Vec<&str> = filtered
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut chunks = Vec::new();
    let mut headings: Vec<Heading> = Vec::new();
    let mut section_start = 0;
    let mut body_start = 0;
    for heading in detected {
        flush_markdown(
            &lines,
            body_start,
            heading.start_row,
            section_start,
            &headings,
            &mut chunks,
        );
        while headings.last().is_some_and(|h| h.level >= heading.level) {
            headings.pop();
        }
        section_start = heading.start_row;
        body_start = heading.end_row + 1;
        headings.push(heading);
    }
    flush_markdown(
        &lines,
        body_start,
        lines.len(),
        section_start,
        &headings,
        &mut chunks,
    );
    Ok(ParsedFile {
        chunks,
        structure,
        ..ParsedFile::default()
    })
}

/// Container prefixes are explicit AST nodes. Count only content indentation,
/// with CommonMark's four-column tab stops, rather than physical line columns.
fn fence_indent(block: Node<'_>, closer: Node<'_>, source: &str, positions: &Positions) -> usize {
    let mut start = positions.line_start(closer.start_position().row);
    let mut stack = vec![block];
    while let Some(node) = stack.pop() {
        if node.kind() == "block_continuation"
            && node.start_position().row == closer.start_position().row
            && node.end_byte() <= closer.start_byte()
        {
            start = start.max(node.end_byte());
        }
        stack.extend(children(node));
    }
    source[start..closer.start_byte()]
        .bytes()
        .fold(0, |column, byte| {
            if byte == b'\t' {
                column + 4 - column % 4
            } else {
                column + 1
            }
        })
}

/// Structural headings are independent of embedding chunks: no size limits,
/// continuation chunks, or synthetic headings are introduced into this model.
fn structure(source: &str, headings: &[Heading]) -> FileStructure {
    let mut result = FileStructure::default();
    let mut stack: Vec<usize> = Vec::new();
    for heading in headings {
        let Heading {
            level,
            title,
            source: signature,
            start_byte: byte,
            start_row: heading_row,
            start_column: heading_column,
            ..
        } = heading;
        while stack
            .last()
            .is_some_and(|id| result.nodes[*id].heading_level.unwrap() >= *level)
        {
            let id = stack.pop().unwrap();
            set_heading_end(
                &mut result.nodes[id],
                *byte,
                heading_row + 1,
                heading_column + 1,
            );
        }
        let parent_id = stack.last().copied();
        let qualified_name = parent_id.map_or_else(
            || title.clone(),
            |p| format!("{}.{}", result.nodes[p].qualified_name, title),
        );
        let id = result.nodes.len();
        result.nodes.push(StructureNode {
            id,
            parent_id,
            language: "markdown".into(),
            kind: "heading".into(),
            name: title.clone(),
            names: vec![title.clone()],
            qualified_name,
            signature: signature.clone(),
            start_byte: *byte,
            start_line: heading_row + 1,
            start_column: heading_column + 1,
            heading_level: Some(*level),
            ..StructureNode::default()
        });
        stack.push(id);
    }
    let end_line = source.bytes().filter(|b| *b == b'\n').count() + 1;
    let end_column = source.rsplit('\n').next().unwrap_or("").len() + 1;
    for id in stack {
        set_heading_end(&mut result.nodes[id], source.len(), end_line, end_column);
    }
    result
}

fn set_heading_end(node: &mut StructureNode, end: usize, line: usize, column: usize) {
    node.end_byte = end;
    node.end_line = line;
    node.end_column = column;
}

fn utf8_prefix(text: &str, limit: usize) -> &str {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn flush_markdown(
    lines: &[&str],
    mut start: usize,
    mut end: usize,
    section_start: usize,
    headings: &[Heading],
    chunks: &mut Vec<MarkdownChunk>,
) {
    while start < end && lines[start].trim().is_empty() {
        start += 1;
    }
    while end > start && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    if start == end {
        return;
    }
    let heading_path: Vec<_> = headings.iter().map(|h| h.title.clone()).collect();
    let mut prefix = String::new();
    // Reserve most of the byte budget for body text, even for enormous titles.
    for heading in headings {
        if !prefix.is_empty() {
            prefix.push('\n');
        }
        let remaining = MARKDOWN_HEADING_BYTES.saturating_sub(prefix.len());
        prefix.push_str(utf8_prefix(&heading.source, remaining));
        if prefix.len() >= MARKDOWN_HEADING_BYTES {
            break;
        }
    }
    if !prefix.is_empty() {
        prefix.push_str("\n\n");
    }
    let budget = MARKDOWN_MAX_BYTES - prefix.len();
    let mut body = String::new();
    let mut first_line = start;
    let mut last_line = start;
    let mut first_chunk = true;
    let emit = |body: &mut String,
                first_line: usize,
                last_line: usize,
                first_chunk: &mut bool,
                chunks: &mut Vec<MarkdownChunk>| {
        let trimmed = body.trim_matches('\n');
        if !trimmed.trim().is_empty() {
            let content = format!("{prefix}{trimmed}");
            chunks.push(MarkdownChunk {
                heading_path: heading_path.clone(),
                start_line: if *first_chunk && !headings.is_empty() {
                    section_start + 1
                } else {
                    first_line + 1
                },
                end_line: last_line + 1,
                source_hash: hash(&content),
                embedding_input: content.clone(),
                content,
            });
            *first_chunk = false;
        }
        body.clear();
    };
    for (index, line) in lines.iter().enumerate().take(end).skip(start) {
        let mut remaining = *line;
        if !body.is_empty()
            && (body.len() + 1 + remaining.len() > budget
                || index - first_line >= MARKDOWN_MAX_LINES)
        {
            emit(&mut body, first_line, last_line, &mut first_chunk, chunks);
        }
        if body.is_empty() {
            first_line = index;
        } else {
            body.push('\n');
        }
        last_line = index;
        // Split very long individual lines at UTF-8 boundaries as well.
        while remaining.len() > budget - body.len() {
            let part = utf8_prefix(remaining, budget - body.len());
            body.push_str(part);
            remaining = &remaining[part.len()..];
            emit(&mut body, first_line, last_line, &mut first_chunk, chunks);
            first_line = index;
        }
        body.push_str(remaining);
    }
    emit(&mut body, first_line, last_line, &mut first_chunk, chunks);
}

#[cfg(test)]
mod tests {
    use super::super::parse;
    use super::*;

    #[test]
    fn markdown_heading_ancestry_preambles_and_empty_sections() {
        let path = "guide.MD";
        let parsed = parse(
            path,
            "Preamble\r\n\r\n# Guide\r\n### Details\r\n\r\nBody\r\n\r\n## Next ##\r\n\r\nMore\r\n## Empty\r\n",
        )
        .unwrap();
        assert!(parsed.errors.is_empty(), "{path}: {:?}", parsed.errors);
        let chunks = parsed.chunks;
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].content, "Preamble");
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[1].heading_path, ["Guide", "Details"]);
        assert_eq!(chunks[1].content, "# Guide\n### Details\n\nBody");
        assert_eq!((chunks[1].start_line, chunks[1].end_line), (4, 6));
        assert_eq!(chunks[2].heading_path, ["Guide", "Next"]);
        assert_eq!(chunks[2].content, "# Guide\n## Next ##\n\nMore");
        for chunk in chunks {
            assert_eq!(chunk.source_hash, hash(&chunk.content));
            assert_eq!(chunk.embedding_input, chunk.content);
        }
        assert!(
            parse("empty.md", "# Heading\n\n## Empty\n")
                .unwrap()
                .chunks
                .is_empty()
        );
    }

    #[test]
    fn markdown_fenced_headings_and_literal_comments() {
        for fence in ["```", "~~~", "````", "~~~~"] {
            for comment in [
                "<!-- literal -->",
                "<!--\n# Literal heading\n-->",
                "<!--\n# Unclosed literal",
            ] {
                let example = format!("{fence}html\n{comment}\n{fence}");
                let source = format!("# Example\n\n{example}\n\nAfter.\n\n## Next\n\nNext body.\n");
                let chunks = parse("fences.md", &source).unwrap().chunks;
                assert_eq!(chunks.len(), 2, "{source}");
                assert_eq!(chunks[0].heading_path, ["Example"]);
                assert_eq!(
                    chunks[0].content,
                    format!("# Example\n\n{example}\n\nAfter.")
                );
                assert_eq!(chunks[1].heading_path, ["Example", "Next"]);
            }
        }
        let source = "# Real\n````md\n# Hidden\n```\n## Still hidden\n~~~~\n### Also hidden\n````\n## Visible\nBody";
        let chunks = parse("fences.md", source).unwrap().chunks;
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1].heading_path, ["Real", "Visible"]);
    }

    #[test]
    fn markdown_html_comments_do_not_create_headings_or_fences() {
        for fence in ["```", "~~~"] {
            let source = format!(
                "<!--\n{fence}md\n# Hidden\n-->\n<!-- Also hidden -->\n# Visible\n\nVisible body.\n"
            );
            let chunks = parse("comments.md", &source).unwrap().chunks;
            assert_eq!(chunks.len(), 1);
            assert_eq!(chunks[0].heading_path, ["Visible"]);
            assert_eq!((chunks[0].start_line, chunks[0].end_line), (6, 8));
        }
        let chunks = parse("empty-title.md", "# #\n\nEmpty heading body.")
            .unwrap()
            .chunks;
        assert_eq!(chunks[0].heading_path, [""]);
    }

    #[test]
    fn html_blocks_do_not_produce_headings() {
        let source = "<div>\n# Hidden\n</div>\n\n# Visible\nBody\n";
        let parsed = parse("page.md", source).unwrap();
        let names: Vec<_> = parsed
            .structure
            .nodes
            .iter()
            .map(|node| node.name.as_str())
            .collect();
        assert_eq!(names, ["Visible"]);
        assert_eq!(parsed.chunks.last().unwrap().heading_path, ["Visible"]);
    }

    #[test]
    fn markdown_long_sections_and_single_unicode_lines_are_bounded() {
        let body = (0..500)
            .map(|i| format!("line {i}: {}", "text ".repeat(30)))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = parse("long.md", &format!("# Guide\n\n{body}"))
            .unwrap()
            .chunks;
        assert!(chunks.len() > 5);
        let reconstructed = chunks
            .iter()
            .map(|c| c.content.strip_prefix("# Guide\n\n").unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(reconstructed, body);
        for chunk in &chunks {
            assert!(chunk.content.len() <= MARKDOWN_MAX_BYTES);
            assert_eq!(chunk.heading_path, ["Guide"]);
            assert!(chunk.start_line <= chunk.end_line);
        }
        let body = "🚀é".repeat(10_000);
        let chunks = parse("unicode.md", &format!("# Guide\n{body}"))
            .unwrap()
            .chunks;
        assert!(chunks.iter().all(|c| c.content.len() <= MARKDOWN_MAX_BYTES));
        let reconstructed = chunks
            .iter()
            .map(|c| c.content.strip_prefix("# Guide\n\n").unwrap())
            .collect::<String>();
        assert_eq!(reconstructed, body);
        assert_eq!(chunks[0].start_line, 1);
        assert!(
            chunks
                .iter()
                .skip(1)
                .all(|c| c.start_line == 2 && c.end_line == 2)
        );
        let huge_heading = format!("# {}\n{}", "🚀".repeat(10_000), "body".repeat(5_000));
        assert!(
            parse("heading.md", &huge_heading)
                .unwrap()
                .chunks
                .iter()
                .all(|c| c.content.len() <= MARKDOWN_MAX_BYTES)
        );
    }

    #[test]
    fn markdown_fence_state_survives_chunk_boundaries() {
        let source = format!(
            "# Guide\n```md\n{}\n# Not a heading\n```\n## Next\nBody",
            "example\n".repeat(300)
        );
        let chunks = parse("long-fence.md", &source).unwrap().chunks;
        assert!(chunks.len() >= 4);
        assert!(
            chunks[..chunks.len() - 1]
                .iter()
                .all(|c| c.heading_path == ["Guide"])
        );
        assert_eq!(chunks.last().unwrap().heading_path, ["Guide", "Next"]);
        assert!(chunks.iter().any(|c| c.content.contains("# Not a heading")));
    }

    #[test]
    fn markdown_line_limit_splits_exactly_without_losing_body_lines() {
        for count in [119_usize, 120, 121, 240, 241] {
            let body = (1..=count)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n");
            for heading in ["", "# Guide\n"] {
                let chunks = parse("lines.md", &format!("{heading}{body}\n"))
                    .unwrap()
                    .chunks;
                assert_eq!(
                    chunks.len(),
                    count.div_ceil(120),
                    "{count} lines, {heading:?}"
                );
                let prefix = if heading.is_empty() {
                    ""
                } else {
                    "# Guide\n\n"
                };
                let bodies: Vec<_> = chunks
                    .iter()
                    .map(|c| c.content.strip_prefix(prefix).unwrap())
                    .collect();
                assert_eq!(bodies.join("\n"), body);
                for (index, (chunk, body)) in chunks.iter().zip(bodies).enumerate() {
                    assert!(body.lines().count() <= 120);
                    let offset = usize::from(!heading.is_empty());
                    assert_eq!(
                        chunk.start_line,
                        if index == 0 {
                            1
                        } else {
                            index * 120 + offset + 1
                        }
                    );
                    assert_eq!(chunk.end_line, ((index + 1) * 120).min(count) + offset);
                }
            }
        }
    }

    #[test]
    fn markdown_byte_limit_accounts_for_heading_prefix_and_utf8_boundaries() {
        for prefix in ["", "# Guide\n\n"] {
            let budget = 8192 - prefix.len();
            for body in [
                "a".repeat(budget - 1),
                "a".repeat(budget),
                "a".repeat(budget + 1),
                format!("{}🚀", "a".repeat(budget - 1)),
            ] {
                let chunks = parse("bytes.md", &format!("{prefix}{body}"))
                    .unwrap()
                    .chunks;
                assert_eq!(chunks.len(), if body.len() <= budget { 1 } else { 2 });
                assert_eq!(
                    chunks
                        .iter()
                        .map(|c| c.content.strip_prefix(prefix).unwrap())
                        .collect::<String>(),
                    body
                );
                for (index, chunk) in chunks.iter().enumerate() {
                    assert!(chunk.content.len() <= 8192);
                    assert_eq!(chunk.source_hash, hash(&chunk.content));
                    assert_eq!(chunk.embedding_input, chunk.content);
                    let body_line = if prefix.is_empty() { 1 } else { 3 };
                    assert_eq!(chunk.start_line, if index == 0 { 1 } else { body_line });
                    assert_eq!(chunk.end_line, body_line);
                }
            }
        }
    }

    #[test]
    fn markdown_atx_heading_syntax_and_indentation() {
        for (line, title) in [
            ("# Plain", Some("Plain")),
            ("   ##\tCafé 🚀 ### \t", Some("Café 🚀")),
            ("###### C#", Some("C#")),
            ("# C# ###", Some("C#")),
            ("##", Some("")),
            ("    # Indented code", None),
            ("\t# Tab-indented code", None),
            ("#No separator", None),
            ("####### Too deep", None),
            ("\\# Escaped", None),
        ] {
            let source = format!("{line}\nBody");
            let chunks = parse("headings.md", &source).unwrap().chunks;
            assert_eq!(chunks.len(), 1, "{line}");
            let chunk = &chunks[0];
            if let Some(title) = title {
                assert_eq!(chunk.heading_path, [title], "{line}");
                assert_eq!(
                    chunk.content,
                    format!("{}\n\nBody", line.trim_start_matches(' '))
                );
            } else {
                assert!(chunk.heading_path.is_empty(), "{line}");
                assert_eq!(chunk.content, source);
            }
            assert_eq!((chunk.start_line, chunk.end_line), (1, 2));
        }
    }

    #[test]
    fn markdown_fence_closers_require_matching_markers_and_no_info_string() {
        for (opening, invalid, closing) in [
            ("   ````rust", "```` trailing text\n```\n~~~~", "  `````\t"),
            ("~~~text", "~~~ trailing text\n```\n    ~~~", "~~~~ "),
        ] {
            let example = format!("{opening}\n{invalid}\n## Hidden\n{closing}");
            let source = format!("# Real\n{example}\n## Visible\nBody");
            let chunks = parse("fences.md", &source).unwrap().chunks;
            assert_eq!(chunks.len(), 2);
            assert_eq!(chunks[0].heading_path, ["Real"]);
            assert_eq!(chunks[0].content, format!("# Real\n\n{example}"));
            assert_eq!(chunks[1].heading_path, ["Real", "Visible"]);
        }
        // A backtick in an opening info string prevents it from opening a fence.
        let chunks = parse("invalid-fence.md", "```bad`info\n# Visible\nBody")
            .unwrap()
            .chunks;
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].heading_path.is_empty());
        assert_eq!(chunks[1].heading_path, ["Visible"]);
        let chunks = parse("unclosed.md", "# Real\n~~~\n## Hidden\nBody")
            .unwrap()
            .chunks;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].heading_path, ["Real"]);
        assert!(chunks[0].content.ends_with("~~~\n## Hidden\nBody"));
    }

    #[test]
    fn markdown_comment_removal_preserves_locations_and_content_hashes() {
        let source = "\r\n# Guide\r\n\r\nBefore\r\n<!-- hidden\r\n## Fake\r\n-->\r\nAfter\r\n\r\n";
        let parsed = parse("comments.md", source).unwrap();
        assert_eq!(parsed.chunks.len(), 1);
        let chunk = &parsed.chunks[0];
        assert_eq!(chunk.heading_path, ["Guide"]);
        assert_eq!((chunk.start_line, chunk.end_line), (2, 8));
        assert_eq!(chunk.content, "# Guide\n\nBefore\n\n\n\nAfter");
        assert_eq!(chunk.source_hash, hash(&chunk.content));
        assert_eq!(chunk.embedding_input, chunk.content);
        let changed = parse(
            "moved.markdown",
            &source
                .replace("hidden", "different hidden text")
                .replace("\r\n", "\n"),
        )
        .unwrap();
        assert_eq!(changed.chunks[0].source_hash, chunk.source_hash);
        let unclosed = parse("unclosed.md", "# Guide\nBefore\n<!--\n# Hidden\nAfter").unwrap();
        assert_eq!(unclosed.chunks.len(), 1);
        assert_eq!(unclosed.chunks[0].content, "# Guide\n\nBefore");
        assert_eq!(
            (unclosed.chunks[0].start_line, unclosed.chunks[0].end_line),
            (1, 2)
        );
    }

    #[test]
    fn markdown_long_unicode_headings_preserve_full_paths_and_reset_ancestry() {
        let title = "🚀é".repeat(500);
        let body = "正文".repeat(2000);
        let source = format!("# {title}\n### Child\n{body}\n## Sibling\nNext\n# New root\nLast");
        let chunks = parse("headings.md", &source).unwrap().chunks;
        assert_eq!(chunks.len(), 4);
        let child_chunks = &chunks[..2];
        assert_eq!(
            child_chunks
                .iter()
                .map(|c| c.content.split_once("\n\n").unwrap().1)
                .collect::<String>(),
            body
        );
        for chunk in child_chunks {
            assert_eq!(chunk.heading_path, [title.as_str(), "Child"]);
            assert!(chunk.content.len() <= 8192);
            let heading = chunk.content.split_once("\n\n").unwrap().0;
            assert!(heading.len() <= 2048);
            assert!(format!("# {title}").starts_with(heading));
        }
        assert_eq!((chunks[0].start_line, chunks[0].end_line), (2, 3));
        assert_eq!((chunks[1].start_line, chunks[1].end_line), (3, 3));
        assert_eq!(chunks[2].heading_path, [title.as_str(), "Sibling"]);
        assert!(chunks[2].content.ends_with("\n\nNext"));
        assert_eq!(chunks[3].heading_path, ["New root"]);
        assert_eq!(chunks[3].content, "# New root\n\nLast");
        assert_eq!((chunks[3].start_line, chunks[3].end_line), (6, 7));
    }
    #[test]
    fn setext_chunk_paths_match_structure_headings() {
        let source = "Root\n====\nintro\n\nChild\n-----\nchild body\n\n# Peer\npeer body\n\n```\nFake\n----\n```\n";
        let parsed = crate::parse::parse("x.md", source).unwrap();
        assert_eq!(
            parsed
                .structure
                .nodes
                .iter()
                .map(|n| n.qualified_name.as_str())
                .collect::<Vec<_>>(),
            ["Root", "Root.Child", "Peer"]
        );
        assert!(
            parsed
                .chunks
                .iter()
                .any(|c| c.heading_path == ["Root", "Child"] && c.content.contains("child body"))
        );
        assert!(
            parsed
                .chunks
                .iter()
                .any(|c| c.heading_path == ["Root"] && c.content.contains("intro"))
        );
        assert!(parsed.chunks.iter().all(|c| {
            parsed
                .structure
                .nodes
                .iter()
                .any(|n| n.qualified_name == c.heading_path.join("."))
        }));
    }
}
