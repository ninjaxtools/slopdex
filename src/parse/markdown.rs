//! Heading-aware Markdown chunking.

use super::MarkdownChunk;
use crate::hash;

const MARKDOWN_MAX_BYTES: usize = 8192;
const MARKDOWN_MAX_LINES: usize = 120;
const MARKDOWN_HEADING_BYTES: usize = 2048;

struct Heading {
    level: usize,
    title: String,
    source: String,
}

fn markdown_indent(line: &str) -> Option<&str> {
    let spaces = line.bytes().take_while(|b| *b == b' ').count();
    (spaces <= 3).then(|| &line[spaces..])
}

fn fence_marker(line: &str) -> Option<(u8, usize, &str)> {
    let line = markdown_indent(line)?;
    let marker = *line.as_bytes().first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let length = line.bytes().take_while(|b| *b == marker).count();
    (length >= 3).then(|| (marker, length, &line[length..]))
}

fn markdown_heading(line: &str) -> Option<Heading> {
    let source = markdown_indent(line)?;
    let level = source.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &source[level..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let title = rest.trim();
    let without_hashes = title.trim_end_matches('#');
    let title = if without_hashes.is_empty() || without_hashes.ends_with([' ', '\t']) {
        without_hashes.trim_end()
    } else {
        title
    };
    Some(Heading {
        level,
        title: title.to_owned(),
        source: source.to_owned(),
    })
}

pub(super) fn parse(source: &str) -> Vec<MarkdownChunk> {
    let mut lines: Vec<&str> = source
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut chunks = Vec::new();
    let mut headings: Vec<Heading> = Vec::new();
    let mut section_start = 0;
    let mut body_start = 0;
    let mut fence: Option<(u8, usize)> = None;
    let mut html_comment = false;
    for index in 0..lines.len() {
        let line = lines[index];
        if let Some((marker, length)) = fence {
            if fence_marker(line).is_some_and(|(m, n, rest)| {
                m == marker && n >= length && rest.trim_matches([' ', '\t']).is_empty()
            }) {
                fence = None;
            }
            continue;
        }
        if html_comment {
            lines[index] = "";
            if line.contains("-->") {
                html_comment = false;
            }
            continue;
        }
        if markdown_indent(line).is_some_and(|l| l.starts_with("<!--")) {
            lines[index] = "";
            html_comment = !line.contains("-->");
            continue;
        }
        if let Some((marker, length, rest)) = fence_marker(line)
            && (marker != b'`' || !rest.contains('`'))
        {
            fence = Some((marker, length));
            continue;
        }
        if let Some(heading) = markdown_heading(line) {
            flush_markdown(
                &lines,
                body_start,
                index,
                section_start,
                &headings,
                &mut chunks,
            );
            while headings.last().is_some_and(|h| h.level >= heading.level) {
                headings.pop();
            }
            headings.push(heading);
            section_start = index;
            body_start = index + 1;
        }
    }
    flush_markdown(
        &lines,
        body_start,
        lines.len(),
        section_start,
        &headings,
        &mut chunks,
    );
    chunks
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
}
