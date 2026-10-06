//! Bash grammar recovery. Returned trees use offsets into the untouched source.
//!
//! Integration: call `recover` for Bash even when the initial tree is error-free,
//! use `function_name` in both collectors, and use `function_signature` for Bash
//! function headers. All source slicing still uses the untouched original text.
//! Function redirections must be visited in the function scope, including an
//! enclosing `redirected_statement` whose body is a function definition.
//! Use `callable_has_error` for Bash candidate validity and callable diagnostics:
//! a bounding wrapper can contain an erroneous sibling outside the callable.
//!
//! Recovery canonicalizes heredoc delimiters and masks literal heredoc data,
//! exposes executable backticks to the heredoc scanner, and scaffolds unsupported
//! compound-command function bodies. Temporary insertions/deletions are undone
//! with tree edits, restoring original byte offsets AND original row/columns.
//! Do not incrementally reparse the returned tree: its grammar scaffold has been
//! translated into original coordinates, rather than parsed incrementally there.

use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};

pub(super) fn recover(parser: &mut Parser, mut tree: Tree, source: &str) -> Tree {
    let mut edits = Vec::new();
    let mut heredoc_headers = HashSet::new();
    let mut function_headers = HashSet::new();
    let mut data = Vec::new();
    loop {
        let before = edits.len();
        recover_heredocs(
            parser,
            &tree,
            source,
            &mut edits,
            &mut heredoc_headers,
            &mut data,
        );
        // Correct heredoc context before inspecting possible function headers:
        // a broken scanner may have parsed literal data as executable commands.
        if edits.len() != before {
            let Some(recovered) = reparse(parser, source, &edits) else {
                return tree;
            };
            tree = recovered;
            continue;
        }
        recover_functions(parser, &tree, source, &mut edits, &mut function_headers);
        if edits.len() == before {
            return tree;
        }
        let Some(recovered) = reparse(parser, source, &edits) else {
            return tree;
        };
        tree = recovered;
    }
}

/// Shell line continuations join identifier fragments without adding spaces.
pub(super) fn function_name(source: &str, node: Node<'_>) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    Some(join_continuations(&source[name.byte_range()]))
}

/// The callable header excludes its body and all executable redirection data.
pub(super) fn function_signature(source: &str, node: Node<'_>) -> String {
    let end = node
        .child_by_field_name("body")
        .map_or(node.end_byte(), |n| n.start_byte());
    let mut header = source.as_bytes()[node.start_byte()..end].to_vec();
    for child in descendants(node).filter(|n| n.kind() == "comment" && n.end_byte() <= end) {
        for byte in &mut header
            [child.start_byte() - node.start_byte()..child.end_byte() - node.start_byte()]
        {
            if !matches!(*byte, b'\r' | b'\n') {
                *byte = b' ';
            }
        }
    }
    join_continuations(std::str::from_utf8(&header).unwrap_or(""))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A source-range wrapper may contain a malformed header sibling. Its errors
/// must not invalidate the function's body or its owned redirection expressions.
pub(super) fn callable_has_error(node: Node<'_>) -> bool {
    if node.kind() != "redirected_statement"
        || node
            .child_by_field_name("body")
            .is_none_or(|body| body.kind() != "function_definition")
    {
        return node.has_error();
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| {
        if child.kind() == "heredoc_redirect" {
            let right = child.child_by_field_name("right");
            let mut cursor = child.walk();
            child
                .children(&mut cursor)
                .any(|part| Some(part) != right && part.kind() != "pipeline" && part.has_error())
        } else {
            child.has_error()
        }
    })
}

fn join_continuations(value: &str) -> String {
    value.replace("\\\r\n", "").replace("\\\n", "")
}

fn descendants(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    let mut stack = vec![node];
    std::iter::from_fn(move || {
        let node = stack.pop()?;
        let mut cursor = node.walk();
        stack.extend(
            node.children(&mut cursor)
                .collect::<Vec<_>>()
                .into_iter()
                .rev(),
        );
        Some(node)
    })
}

#[derive(Clone)]
struct Edit {
    range: Range<usize>,
    replacement: Vec<u8>,
}

fn replace(edits: &mut Vec<Edit>, range: Range<usize>, replacement: &[u8]) {
    edits.push(Edit {
        range,
        replacement: replacement.to_vec(),
    });
}

fn mask(source: &str, edits: &mut Vec<Edit>, range: Range<usize>) {
    if range.is_empty() {
        return;
    }
    let replacement = source.as_bytes()[range.clone()]
        .iter()
        // Whitespace-only padding triggers another scanner bug: its indentation
        // scan consumes the first following `$` as content. Inert dots keep the
        // scanner in literal-content mode until the expansion boundary.
        .map(|byte| {
            if matches!(*byte, b'\r' | b'\n') {
                *byte
            } else {
                b'.'
            }
        })
        .collect::<Vec<_>>();
    replace(edits, range, &replacement);
}

fn advance(mut point: Point, bytes: &[u8]) -> Point {
    for byte in bytes {
        if *byte == b'\n' {
            point.row += 1;
            point.column = 0;
        } else {
            point.column += 1;
        }
    }
    point
}

fn reparse(parser: &mut Parser, source: &str, edits: &[Edit]) -> Option<Tree> {
    let mut edits = edits.to_vec();
    edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
    let mut input = Vec::new();
    let mut inverses = Vec::new();
    let mut position = 0;
    let mut point = Point::new(0, 0);
    for edit in edits {
        if edit.range.start < position || edit.range.end > source.len() {
            return None;
        }
        let prefix = &source.as_bytes()[position..edit.range.start];
        input.extend_from_slice(prefix);
        point = advance(point, prefix);
        let start_byte = input.len();
        let old_end_position = advance(point, &edit.replacement);
        inverses.push(InputEdit {
            start_byte,
            old_end_byte: start_byte + edit.replacement.len(),
            new_end_byte: start_byte + edit.range.len(),
            start_position: point,
            old_end_position,
            new_end_position: advance(point, &source.as_bytes()[edit.range.clone()]),
        });
        input.extend_from_slice(&edit.replacement);
        point = old_end_position;
        position = edit.range.end;
    }
    input.extend_from_slice(&source.as_bytes()[position..]);
    let mut tree = parser.parse(&input, None)?;
    for inverse in inverses.iter().rev() {
        tree.edit(inverse);
    }
    Some(tree)
}

struct Word {
    range: Range<usize>,
    value: String,
    quoted: bool,
}

/// Quote removal for a heredoc delimiter; no parameter/command expansion occurs.
fn word(source: &str, start: usize) -> Option<Word> {
    let bytes = source.as_bytes();
    let mut i = start;
    let mut value = Vec::new();
    let mut quote = None;
    let mut quoted = false;
    while let Some(&byte) = bytes.get(i) {
        if quote.is_none() && (byte.is_ascii_whitespace() || b";|&()<>".contains(&byte)) {
            break;
        }
        if quote.is_none() && byte == b'$' && bytes.get(i + 1) == Some(&b'\'') {
            let (decoded, end) = ansi_quote(source, i + 2)?;
            quoted = true;
            value.extend(decoded);
            i = end;
        } else if quote.is_none() && byte == b'$' && bytes.get(i + 1) == Some(&b'"') {
            quoted = true;
            quote = Some(b'"');
            i += 2;
        } else if quote == Some(b'\'') {
            if byte == b'\'' {
                quote = None;
            } else {
                value.push(byte);
            }
            i += 1;
        } else if byte == b'\\' {
            let next = *bytes.get(i + 1)?;
            let continuation = next == b'\n' || next == b'\r' && bytes.get(i + 2) == Some(&b'\n');
            if continuation {
                i += 2 + usize::from(next == b'\r');
            } else if quote.is_none() || b"$`\"\\".contains(&next) {
                quoted = true;
                value.push(next);
                i += 2;
            } else {
                value.push(byte);
                i += 1;
            }
        } else if quote == Some(b'"') {
            if byte == b'"' {
                quote = None;
            } else {
                value.push(byte);
            }
            i += 1;
        } else if matches!(byte, b'\'' | b'"') {
            quoted = true;
            quote = Some(byte);
            i += 1;
        } else {
            value.push(byte);
            i += 1;
        }
    }
    if i == start || quote.is_some() {
        return None;
    }
    Some(Word {
        range: start..i,
        value: String::from_utf8(value).ok()?,
        quoted,
    })
}

fn ansi_quote(source: &str, mut i: usize) -> Option<(Vec<u8>, usize)> {
    let bytes = source.as_bytes();
    let mut value = Vec::new();
    while let Some(&byte) = bytes.get(i) {
        if byte == b'\'' {
            return Some((value, i + 1));
        }
        if byte != b'\\' {
            value.push(byte);
            i += 1;
            continue;
        }
        let escaped = *bytes.get(i + 1)?;
        i += 2;
        let decoded = match escaped {
            b'a' => 7,
            b'b' => 8,
            b'e' | b'E' => 27,
            b'f' => 12,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 11,
            b'\\' | b'\'' | b'"' | b'?' => escaped,
            b'c' => {
                let byte = *bytes.get(i)?;
                i += 1;
                byte.to_ascii_uppercase() ^ 64
            }
            b'x' | b'u' | b'U' | b'0'..=b'7' => {
                let (radix, maximum) = if escaped.is_ascii_digit() {
                    (8, 3)
                } else {
                    (
                        16,
                        if escaped == b'x' {
                            2
                        } else if escaped == b'u' {
                            4
                        } else {
                            8
                        },
                    )
                };
                let mut number = 0;
                let mut count = 0;
                if radix == 8 {
                    number = u32::from(escaped - b'0');
                    count = 1;
                }
                while count < maximum {
                    let Some(digit) = bytes.get(i).and_then(|b| char::from(*b).to_digit(radix))
                    else {
                        break;
                    };
                    number = number.checked_mul(radix)?.checked_add(digit)?;
                    i += 1;
                    count += 1;
                }
                if count == 0 {
                    value.extend([b'\\', escaped]);
                } else if matches!(escaped, b'u' | b'U') {
                    let mut encoded = [0; 4];
                    value.extend_from_slice(
                        char::from_u32(number)?.encode_utf8(&mut encoded).as_bytes(),
                    );
                } else {
                    value.push(number as u8);
                }
                continue;
            }
            _ => {
                value.push(b'\\');
                escaped
            }
        };
        value.push(decoded);
    }
    None
}

fn space(source: &str, mut i: usize) -> usize {
    let bytes = source.as_bytes();
    loop {
        if matches!(bytes.get(i), Some(b' ' | b'\t' | b'\r')) {
            i += 1;
        } else if bytes.get(i..i + 2) == Some(b"\\\n") {
            i += 2;
        } else if bytes.get(i..i + 3) == Some(b"\\\r\n") {
            i += 3;
        } else {
            return i;
        }
    }
}

fn layout(source: &str, mut i: usize) -> usize {
    loop {
        i = space(source, i);
        if source.as_bytes().get(i) == Some(&b'\n') {
            i += 1;
        } else if source.as_bytes().get(i) == Some(&b'#') {
            i = source[i..].find('\n').map_or(source.len(), |n| i + n + 1);
        } else {
            return i;
        }
    }
}

fn logical_line_end(parser: &mut Parser, source: &str, mut i: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut quote = None;
    while let Some(&byte) = bytes.get(i) {
        if byte == b'\\' && quote != Some(b'\'') {
            i += if bytes.get(i + 1..i + 3) == Some(b"\r\n") {
                3
            } else {
                2
            };
        } else if quote != Some(b'\'')
            && byte == b'$'
            && matches!(bytes.get(i + 1), Some(b'(' | b'{'))
        {
            i = expansion_end(parser, source, i, source.len());
        } else if quote.is_none()
            && byte == b'#'
            && (i == 0 || bytes[i - 1].is_ascii_whitespace() || b";|&()".contains(&bytes[i - 1]))
        {
            return source[i..].find('\n').map(|n| i + n);
        } else if quote == Some(byte) {
            quote = None;
            i += 1;
        } else if quote.is_none() && matches!(byte, b'\'' | b'"' | b'`') {
            quote = Some(byte);
            i += 1;
        } else if quote.is_none() && byte == b'\n' {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

struct Heredoc {
    operator: Range<usize>,
    delimiter: Word,
    tabs: bool,
}

fn heredoc(source: &str, start: usize) -> Option<Heredoc> {
    let bytes = source.as_bytes();
    if bytes.get(start..start + 2) != Some(b"<<")
        || bytes.get(start + 2) == Some(&b'<')
        || start > 0 && bytes[start - 1] == b'<'
        || bytes[..start]
            .iter()
            .rev()
            .take_while(|byte| **byte == b'\\')
            .count()
            % 2
            != 0
    {
        return None;
    }
    let tabs = bytes.get(start + 2) == Some(&b'-');
    let end = start + 2 + usize::from(tabs);
    Some(Heredoc {
        operator: start..end,
        delimiter: word(source, space(source, end))?,
        tabs,
    })
}

fn terminator(source: &str, start: usize, doc: &Heredoc) -> Option<Range<usize>> {
    let mut start = start;
    while start <= source.len() {
        let end = source[start..]
            .find('\n')
            .map_or(source.len(), |n| start + n);
        let line = source[start..end].trim_end_matches('\r');
        let line = if doc.tabs {
            line.trim_start_matches('\t')
        } else {
            line
        };
        if line == doc.delimiter.value {
            return Some(start..end);
        }
        if end == source.len() {
            break;
        }
        start = end + 1;
    }
    None
}

fn opaque_ranges(tree: &Tree) -> Vec<Range<usize>> {
    descendants(tree.root_node())
        .filter_map(|n| {
            if matches!(
                n.kind(),
                "comment"
                    | "raw_string"
                    | "ansi_c_string"
                    | "string_content"
                    | "heredoc_content"
                    | "regex"
            ) || n.kind() == "heredoc_body" && n.named_child_count() == 0
            {
                Some(n.byte_range())
            } else if n.kind() == "binary_expression" {
                n.child_by_field_name("operator")
                    .filter(|op| op.kind().starts_with("<<"))
                    .map(|op| op.byte_range())
            } else {
                None
            }
        })
        .collect()
}

/// Match executable expansion syntax with the Bash grammar, including case
/// patterns, nested substitutions, and quotes that defeat delimiter counting.
fn expansion_end(parser: &mut Parser, source: &str, start: usize, end: usize) -> usize {
    let Some(tree) = parser.parse(&source[start..end], None) else {
        return end;
    };
    descendants(tree.root_node())
        .find(|n| {
            n.start_byte() == 0
                && matches!(
                    n.kind(),
                    "command_substitution" | "arithmetic_expansion" | "expansion"
                )
        })
        .filter(|n| {
            n.child(n.child_count().saturating_sub(1))
                .is_some_and(|last| !last.is_missing() && matches!(last.kind(), ")" | "))" | "}"))
        })
        .map_or(end, |n| start + n.end_byte())
}

fn islands(parser: &mut Parser, source: &str, range: Range<usize>) -> Vec<Range<usize>> {
    let bytes = source.as_bytes();
    let mut result = Vec::new();
    let mut i = range.start;
    while i < range.end {
        if bytes[i] == b'\\' {
            i += 2;
        } else if bytes.get(i..i + 2) == Some(b"$$") {
            // A PID expansion followed by `(` is data, not a substitution
            // starting at its second dollar sign.
            i += 2;
        } else if bytes[i] == b'$' && matches!(bytes.get(i + 1), Some(b'(' | b'{')) {
            let end = expansion_end(parser, source, i, range.end);
            result.push(i..end);
            i = end;
        } else if bytes[i] == b'`' {
            let start = i;
            i += 1;
            while i < range.end {
                if bytes[i] == b'\\' {
                    i += 2;
                } else if bytes[i] == b'`' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            result.push(start..i.min(range.end));
        } else {
            i += 1;
        }
    }
    result
}

fn recover_heredocs(
    parser: &mut Parser,
    tree: &Tree,
    source: &str,
    edits: &mut Vec<Edit>,
    seen: &mut HashSet<usize>,
    data: &mut Vec<Range<usize>>,
) {
    let opaque = opaque_ranges(tree);
    let mut headers = source
        .match_indices("<<")
        .filter(|(start, _)| {
            !seen.contains(start)
                && !opaque
                    .iter()
                    .chain(data.iter())
                    .any(|range| range.contains(start))
        })
        .filter_map(|(start, _)| heredoc(source, start))
        .collect::<Vec<_>>();
    headers.sort_by_key(|doc| doc.operator.start);
    let mut groups: BTreeMap<usize, Vec<Heredoc>> = BTreeMap::new();
    for doc in headers {
        if let Some(line) = logical_line_end(parser, source, doc.delimiter.range.end) {
            groups.entry(line).or_default().push(doc);
        }
    }
    for (line, docs) in groups {
        if docs
            .iter()
            .any(|doc| data.iter().any(|r| r.contains(&doc.operator.start)))
        {
            continue;
        }
        let mut start = line + 1;
        let mut bodies = Vec::new();
        for doc in &docs {
            let end = terminator(source, start, doc);
            let body_end = end.as_ref().map_or(source.len(), |end| end.start);
            let executable = if doc.delimiter.quoted {
                Vec::new()
            } else {
                islands(parser, source, start..body_end)
            };
            bodies.push((start..body_end, end.clone(), executable));
            let Some(end) = end else {
                // Keep the terminator absent. Canonicalization still masks all
                // remaining data so a false indented/prefix match cannot turn
                // an unfinished document into executable declarations.
                break;
            };
            start = (end.end + 1).min(source.len());
        }
        let quoted = docs.iter().all(|doc| doc.delimiter.quoted);
        let unfinished = bodies.last().is_some_and(|(_, end, _)| end.is_none());
        if !unfinished {
            scaffold_header_sibling(
                parser,
                tree,
                source,
                docs[0].delimiter.range.end..line,
                edits,
            );
        }
        let trim_tail = unfinished
            .then(|| {
                bodies
                    .iter()
                    .flat_map(|(_, _, executable)| executable)
                    .last()
                    .map(|island| island.end)
            })
            .flatten();
        for (index, doc) in docs.iter().enumerate() {
            seen.insert(doc.operator.start);
            if index == 0 {
                if let Some(descriptor) = descendants(tree.root_node()).find(|node| {
                    node.kind() == "file_descriptor" && node.end_byte() == doc.operator.start
                }) {
                    // A function's optional file_redirect greedily consumes
                    // `3<` from `3<<`. Restore the descriptor's original extent
                    // through inverse edits after parsing the heredoc wrapper.
                    replace(edits, descriptor.byte_range(), b"");
                }
                replace(edits, doc.operator.clone(), b"<<");
                replace(
                    edits,
                    doc.delimiter.range.clone(),
                    if quoted { b"'_'" } else { b"_" },
                );
            } else {
                // Bash reads queued heredocs in header order. One canonical
                // heredoc spans all bodies; secondary headers become redirects.
                replace(edits, doc.operator.clone(), b"<");
                replace(edits, doc.delimiter.range.clone(), b"_");
            }
        }
        for (index, (body, end, executable)) in bodies.iter().enumerate() {
            let mut position = body.start;
            for island in executable {
                mask(source, edits, position..island.start);
                data.push(position..island.start);
                if source.as_bytes()[island.start] == b'`' {
                    replace(edits, island.start..island.start, b"$(");
                    replace(edits, island.end..island.end, b")");
                }
                position = island.end;
            }
            mask(
                source,
                edits,
                position..body.end.min(trim_tail.unwrap_or(body.end)),
            );
            data.push(position..body.end);
            if let Some(end) = end {
                if trim_tail.is_none_or(|trim| end.start < trim) {
                    if index + 1 == docs.len() {
                        replace(edits, end.clone(), b"_");
                    } else {
                        mask(source, edits, end.clone());
                    }
                }
                data.push(end.clone());
            }
        }
        if let Some(trim) = trim_tail {
            // After an expansion the scanner incorrectly invents heredoc_end
            // from any literal tail at EOF. End the scaffold at the expansion
            // instead, then restore the tail's original extent with tree edits.
            replace(edits, trim..source.len(), b"");
        }
    }
}

fn scaffold_header_sibling(
    parser: &mut Parser,
    tree: &Tree,
    source: &str,
    header: Range<usize>,
    edits: &mut Vec<Edit>,
) {
    let operator = descendants(tree.root_node())
        .filter(|node| {
            header.contains(&node.start_byte())
                && matches!(node.kind(), "&&" | "||" | "|" | "|&")
                && !inside_expansion(*node)
        })
        .min_by_key(|node| node.start_byte());
    let Some(operator) = operator else {
        return;
    };
    let mut ancestor = operator.parent();
    while let Some(node) = ancestor {
        if node.kind() == "redirected_statement"
            && node
                .child_by_field_name("body")
                .is_some_and(|body| body.kind() == "function_definition")
            && !callable_has_error(node)
        {
            // The grammar already kept this healthy function's association;
            // leave any separate sibling diagnostic exactly as parsed.
            return;
        }
        ancestor = node.parent();
    }
    let start = layout(source, operator.end_byte());
    if start >= header.end {
        return;
    }
    let Some(fragment) = parser.parse(&source[start..header.end], None) else {
        return;
    };
    if !fragment.root_node().has_error() {
        return;
    }
    let Some(body) = sibling_function_body(source, start) else {
        return;
    };
    if body >= header.end || !matches!(source.as_bytes()[body], b'{' | b'(') {
        return;
    }
    // An empty unclosed body is recovered as `command` plus ERROR, with no
    // missing closer. Count actual grammar tokens (not braces in text/literals)
    // to identify the closers even when the declaration shape was lost.
    let mut closers = Vec::new();
    for token in descendants(fragment.root_node()).filter(|node| {
        node.child_count() == 0 && !node.is_missing() && node.start_byte() >= body - start
    }) {
        match token.kind() {
            "{" | "${" => closers.push("}"),
            "(" | "$(" => closers.push(")"),
            "[[" => closers.push("]]"),
            "if" => closers.push("fi"),
            "do" => closers.push("done"),
            "case" => closers.push("esac"),
            closing if closers.last().copied() == Some(closing) => {
                closers.pop();
            }
            _ => {}
        }
    }
    closers.reverse();
    if closers.is_empty() {
        return;
    }
    // Close the malformed header statement before the heredoc body, retaining
    // an explicit ERROR inside that sibling. Without this scaffold its missing
    // closer swallows the whole redirected_statement and loses the association.
    // The offending marker and all closers become zero-width at the original
    // repair position when reparse applies inverse edits; the error flag survives.
    // Keep the repair before the real header newline: the heredoc scanner starts
    // consuming its body at that newline even inside an incomplete sibling.
    let marker = " : ${:}; ";
    let mut scaffold = marker.to_owned();
    scaffold.push_str(&closers.join(" "));
    scaffold.push(' ');
    let insertion = descendants(fragment.root_node())
        .filter(|node| node.kind() == "comment" && node.end_byte() == header.end - start)
        .map(|node| start + node.start_byte())
        .next()
        .unwrap_or(header.end);
    replace(edits, insertion..insertion, scaffold.as_bytes());
}

fn sibling_function_body(source: &str, start: usize) -> Option<usize> {
    let mut name = word(source, start)?;
    let keyword = name.value == "function";
    if keyword {
        name = word(source, layout(source, name.range.end))?;
    }
    if name.quoted {
        return None;
    }
    let mut body = layout(source, name.range.end);
    if source.as_bytes().get(body) == Some(&b'(') {
        let close = layout(source, body + 1);
        if source.as_bytes().get(close) != Some(&b')') {
            return None;
        }
        body = layout(source, close + 1);
    } else if !keyword {
        return None;
    }
    Some(body)
}

fn inside_expansion(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "command_substitution"
                | "process_substitution"
                | "arithmetic_expansion"
                | "expansion"
                | "string"
        ) {
            return true;
        }
        node = parent;
    }
    false
}

fn recover_functions(
    parser: &mut Parser,
    tree: &Tree,
    source: &str,
    edits: &mut Vec<Edit>,
    seen: &mut HashSet<usize>,
) {
    let opaque = opaque_ranges(tree);
    let candidates = descendants(tree.root_node())
        .filter(|n| {
            n.kind() == "function"
                || n.kind() == "word"
                    && n.parent().is_some_and(|p| {
                        p.kind() == "command_name" || p.kind() == "function_definition"
                    })
        })
        .map(|n| {
            let definition = n.parent().filter(|p| p.kind() == "function_definition");
            (
                n.start_byte(),
                n.kind() != "function" && definition.is_some(),
                definition
                    .and_then(|p| p.child_by_field_name("body"))
                    .map(|body| body.start_byte()),
            )
        })
        .collect::<Vec<_>>();
    for (start, defined, existing_body) in candidates {
        if seen.contains(&start) || opaque.iter().any(|r| r.contains(&start)) {
            continue;
        }
        let Some(mut name) = word(source, start) else {
            continue;
        };
        let keyword_form = !defined && name.value == "function";
        if keyword_form {
            let Some(actual) = word(source, layout(source, name.range.end)) else {
                continue;
            };
            name = actual;
        }
        let mut body = layout(source, name.range.end);
        if source.as_bytes().get(body) == Some(&b'(') {
            let close = layout(source, body + 1);
            if source.as_bytes().get(close) != Some(&b')') {
                continue;
            }
            body = layout(source, close + 1);
        } else if !keyword_form {
            continue;
        }
        if source[name.range.clone()].contains("\\\n")
            || source[name.range.clone()].contains("\\\r\n")
        {
            let bytes = source.as_bytes();
            let mut i = name.range.start;
            while i < name.range.end {
                let len = if bytes.get(i..i + 2) == Some(b"\\\n") {
                    2
                } else if bytes.get(i..i + 3) == Some(b"\\\r\n") {
                    3
                } else {
                    0
                };
                if len > 0 {
                    replace(edits, i..i + len, b"");
                    i += len;
                } else {
                    i += 1;
                }
            }
            seen.insert(start);
        }
        if existing_body == Some(body) {
            continue;
        }
        let Some(keyword) = word(source, body) else {
            continue;
        };
        if !matches!(
            keyword.value.as_str(),
            "while" | "until" | "for" | "select" | "case"
        ) {
            continue;
        }
        let Some(parsed) = parser.parse(&source[body..], None) else {
            continue;
        };
        let Some(command) = descendants(parsed.root_node()).find(|n| {
            n.start_byte() == 0
                && matches!(
                    n.kind(),
                    "while_statement"
                        | "for_statement"
                        | "c_style_for_statement"
                        | "case_statement"
                )
        }) else {
            continue;
        };
        let end = body + command.end_byte();
        let closing = if keyword.value == "case" {
            "esac"
        } else {
            "done"
        };
        if !descendants(command)
            .any(|n| n.kind() == closing && n.end_byte() == command.end_byte() && !n.is_missing())
        {
            continue;
        }
        replace(edits, body..body, b"{ ");
        replace(edits, end..end, b"; }");
        seen.insert(start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser() -> Parser {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .unwrap();
        parser
    }

    fn clean(source: &str) -> Tree {
        let mut parser = parser();
        let tree = parser.parse(source, None).unwrap();
        let tree = recover(&mut parser, tree, source);
        assert!(
            !tree.root_node().has_error(),
            "{source:?}\n{}",
            tree.root_node().to_sexp()
        );
        original_coordinates(source, &tree);
        tree
    }

    fn original_coordinates(source: &str, tree: &Tree) {
        for node in descendants(tree.root_node()) {
            assert!(node.end_byte() <= source.len(), "{node:?}");
            assert!(
                source.is_char_boundary(node.start_byte())
                    && source.is_char_boundary(node.end_byte()),
                "{node:?}"
            );
            assert_eq!(
                node.start_position(),
                advance(Point::new(0, 0), &source.as_bytes()[..node.start_byte()]),
                "{node:?}"
            );
            assert_eq!(
                node.end_position(),
                advance(Point::new(0, 0), &source.as_bytes()[..node.end_byte()]),
                "{node:?}"
            );
        }
    }

    fn symbols(source: &str, tree: &Tree) -> Vec<String> {
        fn walk(source: &str, node: Node<'_>, scope: &str, result: &mut Vec<String>) {
            let scope = if node.kind() == "function_definition" {
                let name = function_name(source, node).unwrap();
                let name = if scope.is_empty() {
                    name
                } else {
                    format!("{scope}.{name}")
                };
                result.push(name.clone());
                name
            } else {
                scope.to_owned()
            };
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(source, child, &scope, result);
            }
        }
        let mut result = Vec::new();
        walk(source, tree.root_node(), "", &mut result);
        result
    }

    #[test]
    fn audit_reproductions_recover_with_original_coordinates() {
        for (source, expected) in [
            (
                "healthy() while false; do nested() { :; }; done\n",
                vec!["healthy", "healthy.nested"],
            ),
            (
                "healthy() for item in a; do nested() { :; }; done\n",
                vec!["healthy", "healthy.nested"],
            ),
            (
                "healthy() case value in *) nested() { :; };; esac\n",
                vec!["healthy", "healthy.nested"],
            ),
            ("foo\\\nbar() { :; }\n", vec!["foobar"]),
            (
                "cat <<EO'F'\n$(fake() { :; })\nEOF\nhealthy() { :; }\n",
                vec!["healthy"],
            ),
            (
                "cat <<EO\\F\n$(fake() { :; })\nEOF\nhealthy() { :; }\n",
                vec!["healthy"],
            ),
            (
                "cat <<EOF\n  EOF\nfake() { :; }\nEOF\nhealthy() { :; }\n",
                vec!["healthy"],
            ),
            (
                "cat <<-EOF\n  EOF\nfake() { :; }\nEOF\nhealthy() { :; }\n",
                vec!["healthy"],
            ),
            (
                "cat <<EOF\nEOF_extra\nfake() { :; }\nEOF\nhealthy() { :; }\n",
                vec!["healthy"],
            ),
            (
                "cat <<EOF\n`healthy() { :; }; healthy`\nEOF\n",
                vec!["healthy"],
            ),
            (
                "cat <<A <<B\none\nA\ntwo\nB\nhealthy() { :; }\n",
                vec!["healthy"],
            ),
        ] {
            let tree = clean(source);
            assert_eq!(symbols(source, &tree), expected, "{source:?}");
        }
    }

    #[test]
    fn signatures_exclude_redirections_and_join_continued_names() {
        for (source, expected) in [
            (
                "healthy() { :; } >\"$(printf runtime_data)\"\n",
                "healthy()",
            ),
            ("foo\\\r\nbar() # explanation\r\n{ :; }\r\n", "foobar()"),
            ("healthy() while false; do :; done\n", "healthy()"),
        ] {
            let tree = clean(source);
            let node = descendants(tree.root_node())
                .find(|n| n.kind() == "function_definition")
                .unwrap();
            assert_eq!(function_signature(source, node), expected);
        }
    }

    #[test]
    fn all_delimiter_quote_fragments_disable_data_expansion() {
        for delimiter in [
            "'EOF'",
            "\"EOF\"",
            "\\EOF",
            "EO\\F",
            "E'O'F",
            "E\"O\"F",
            "'E'OF",
            "$'EOF'",
            "$'E\\x4fF'",
            "$\"EOF\"",
        ] {
            let source = format!(
                "cat <<{delimiter} # don't inspect 'quotes\n$(fake() while false; do :; done)\n`also_fake() {{ :; }}`\nEOF\nhealthy() {{ :; }}\n"
            );
            let tree = clean(&source);
            assert_eq!(symbols(&source, &tree), ["healthy"], "{delimiter}");
        }
        let source = "cat <<''\n\nhealthy() { :; }\n";
        assert_eq!(symbols(source, &clean(source)), ["healthy"]);
    }

    #[test]
    fn exact_terminators_and_multiple_queued_documents_preserve_only_code() {
        for line in ["EOF_extra", "EOF extra", " EOF", "\tEOF", "EOF\t"] {
            let source = format!("cat <<EOF\n{line}\nfake() {{ :; }}\nEOF\nhealthy() {{ :; }}\n");
            assert_eq!(symbols(&source, &clean(&source)), ["healthy"], "{line:?}");
        }
        for (source, expected) in [
            (
                "cat <<-EOF\n\tdata\n\tEOF\nhealthy() { :; }\n",
                vec!["healthy"],
            ),
            (
                "cat <<'A' <<B\n$(fake() { :; })\nA\n`healthy() { :; }`\nB\n",
                vec!["healthy"],
            ),
            (
                "cat <<A <<'B'\n$(healthy() { :; })\nA\n$(fake() { :; })\nB\n",
                vec!["healthy"],
            ),
            (
                "cat <<EOF && after() { :; }\nfake() { :; }\nEOF\n",
                vec!["after"],
            ),
        ] {
            assert_eq!(symbols(source, &clean(source)), expected, "{source:?}");
        }
    }

    #[test]
    fn nested_and_escaped_heredoc_expansions_preserve_executable_scopes() {
        for (source, expected) in [
            (
                "cat <<EOF\n$$(fake() { :; })\nEOF\nhealthy() { :; }\n",
                vec!["healthy"],
            ),
            ("cat <<EO\\\nF\n$(healthy() { :; })\nEOF\n", vec!["healthy"]),
            (
                "outer() { cat <<EOF\ntext $(healthy() while false; do nested() { :; }; done) text\nEOF\n}\n",
                vec!["outer", "outer.healthy", "outer.healthy.nested"],
            ),
            (
                "cat <<EOF\n\\$(fake() { :; })\n\\`also_fake() { :; }\\`\n$(healthy() { :; })\nEOF\n",
                vec!["healthy"],
            ),
            (
                "cat <<EOF\n${value:-$(healthy() { :; })}\nEOF\n",
                vec!["healthy"],
            ),
            (
                "cat <<EOF\n$(case value in *) healthy() { :; };; esac)\nEOF\n",
                vec!["healthy"],
            ),
            (
                "cat <<EOF\n$(cat <<INNER\nfake() { :; }\nINNER\nhealthy() { :; }; healthy)\nEOF\n",
                vec!["healthy"],
            ),
            (
                "cat <<OUTER \"$(cat <<INNER\nfake() { :; }\nINNER\n)\"\nouter_data() { :; }\nOUTER\nhealthy() { :; }\n",
                vec!["healthy"],
            ),
        ] {
            assert_eq!(symbols(source, &clean(source)), expected, "{source:?}");
        }
    }

    #[test]
    fn supported_literal_and_arithmetic_syntax_is_not_reinterpreted() {
        let source = r#"# cat <<EOF
value='cat <<EOF; fake() while false; do :; done'
printf '%s' "cat <<EOF; fake() for item in a; do :; done"
value=$((1 << 2))
((value <<= 1))
cat <<< 'fake() { :; }'
healthy() { :; }
"#;
        let initial = parser().parse(source, None).unwrap();
        let tree = clean(source);
        assert_eq!(tree.root_node().to_sexp(), initial.root_node().to_sexp());
        assert_eq!(symbols(source, &tree), ["healthy"]);
    }

    #[test]
    fn compound_function_forms_and_continuations_keep_full_original_ranges() {
        for source in [
            "function healthy while false; do nested() { :; }; done\n",
            "function healthy() until true; do nested() { :; }; done\n",
            "healthy( ) # explanation\nfor item in a; do nested() { :; }; done\n",
            "healthy() select item in a; do nested() { :; }; done\n",
            "healthy() for ((i=0; i<2; i++)); do nested() { :; }; done\n",
            "healthy() while false; do nested() case value in *) deep() { :; };; esac; done\n",
        ] {
            let tree = clean(source);
            let names = symbols(source, &tree);
            assert_eq!(&names[..2], ["healthy", "healthy.nested"], "{source:?}");
            let node = descendants(tree.root_node())
                .find(|n| n.kind() == "function_definition")
                .unwrap();
            assert_eq!(&source[node.byte_range()], source.trim_end());
        }
        let source = "# café 🚀\nfoo\\\nba\\\nr() { :; }\n";
        let tree = clean(source);
        assert_eq!(symbols(source, &tree), ["foobar"]);
        let node = descendants(tree.root_node())
            .find(|n| n.kind() == "function_definition")
            .unwrap();
        assert_eq!(&source[node.byte_range()], "foo\\\nba\\\nr() { :; }");
    }

    #[test]
    fn unrelated_syntax_errors_are_not_hidden_by_recovery() {
        for source in [
            "healthy() while false; do :; done\nbroken() {",
            "healthy() while false; do missing\n",
            "cat <<'EOF'\nliteral() { :; }\nEOF\nbroken() {",
        ] {
            let mut parser = parser();
            let initial = parser.parse(source, None).unwrap();
            let tree = recover(&mut parser, initial, source);
            assert!(tree.root_node().has_error(), "{source:?}");
        }
    }

    #[test]
    fn escaped_redirects_are_not_heredocs_and_unicode_offsets_remain_exact() {
        let source = "printf '%s' \\<<EOF\nreal() { :; }\nEOF\nhealthy() { :; }\n";
        let tree = clean(source);
        assert_eq!(symbols(source, &tree), ["real", "healthy"]);
        for source in [
            "# café 🚀\ncat <<'🌈'\n$(fake() { :; })\n🌈\ncafé() { :; }\n",
            "cat <<EOF\n$(café() { :; })\nEOF\n",
            "caf\\\né() while false; do :; done\n",
        ] {
            assert_eq!(symbols(source, &clean(source)), ["café"], "{source:?}");
        }
    }

    #[test]
    fn unfinished_documents_have_missing_terminators_without_literal_symbols() {
        for (source, expected) in [
            ("f() { :; } <<EOF\n  EOF\nfake() { :; }\n", vec!["f"]),
            ("f() { :; } <<EO\\F\n EOF\n$(fake() { :; })\n", vec!["f"]),
            (
                "f() { :; } <<EOF\n EOF\n$(inside() { :; })\ncafé 🚀\n",
                vec!["f", "inside"],
            ),
            (
                "cat <<A <<B\n$(inside() { :; })\nA\n EOF\nfake() { :; }\n",
                vec!["inside"],
            ),
            (
                "cat <<A <<B\nliteral\nA\n`inside() { :; }`\nrest\n",
                vec!["inside"],
            ),
        ] {
            let mut parser = parser();
            let initial = parser.parse(source, None).unwrap();
            let tree = recover(&mut parser, initial, source);
            assert!(
                tree.root_node().has_error(),
                "{source:?}\n{}",
                tree.root_node().to_sexp()
            );
            assert!(
                descendants(tree.root_node())
                    .any(|node| node.kind() == "heredoc_end" && node.is_missing()),
                "{source:?}\n{}",
                tree.root_node().to_sexp()
            );
            assert_eq!(symbols(source, &tree), expected, "{source:?}");
            original_coordinates(source, &tree);
        }
    }

    #[test]
    fn descriptor_function_heredocs_restore_the_whole_original_wrapper() {
        for source in [
            "f() { :; } 3<<EOF\n$(inside() { :; })\nEOF\n",
            "f() { :; } 12<<-EOF >out\n\t$(inside() { :; })\n\tEOF\n",
            "f() { :; } 3<<A 4<<B\nfirst\nA\n$(inside() { :; })\nB\n",
        ] {
            let tree = clean(source);
            let function = descendants(tree.root_node())
                .find(|node| {
                    node.kind() == "function_definition"
                        && function_name(source, *node).as_deref() == Some("f")
                })
                .unwrap();
            let wrapper = function.parent().unwrap();
            assert_eq!(wrapper.kind(), "redirected_statement");
            assert_eq!(&source[wrapper.byte_range()], source.trim_end_matches('\n'));
            assert!(!callable_has_error(wrapper));
        }
    }

    #[test]
    fn malformed_header_siblings_keep_association_and_their_own_error() {
        for header in [
            "&& broken() {",
            "|| broken() { # missing closer",
            "| broken() { echo value;",
            "&& broken() { healthy() { :; };",
            "&& broken() { echo ${value:-\"}\"};",
            "&& broken() ( echo value;",
        ] {
            let source = format!("f() {{ :; }} <<EOF {header}\n$(inside() {{ :; }})\nEOF\n");
            let mut parser = parser();
            let initial = parser.parse(&source, None).unwrap();
            let tree = recover(&mut parser, initial, &source);
            assert!(
                tree.root_node().has_error(),
                "{header}\n{}",
                tree.root_node().to_sexp()
            );
            let function = descendants(tree.root_node())
                .find(|node| {
                    node.kind() == "function_definition"
                        && function_name(&source, *node).as_deref() == Some("f")
                })
                .unwrap();
            let wrapper = function.parent().unwrap();
            assert_eq!(
                wrapper.kind(),
                "redirected_statement",
                "{header}\n{}",
                tree.root_node().to_sexp()
            );
            assert_eq!(&source[wrapper.byte_range()], source.trim_end_matches('\n'));
            assert!(!callable_has_error(wrapper), "{header}");
            let broken = descendants(wrapper)
                .find(|node| {
                    node.kind() == "function_definition"
                        && function_name(&source, *node).as_deref() == Some("broken")
                })
                .unwrap();
            assert!(callable_has_error(broken), "{header}");
            let body = descendants(wrapper)
                .find(|node| node.kind() == "heredoc_body")
                .unwrap();
            assert!(
                descendants(body).any(|node| node.kind() == "function_definition"
                    && function_name(&source, node).as_deref() == Some("inside"))
            );
            original_coordinates(&source, &tree);
        }
    }

    #[test]
    fn errors_in_owned_redirections_still_invalidate_the_callable() {
        let source = "f() { :; } <<EOF\n$(broken() {)\nEOF\n";
        let mut parser = parser();
        let initial = parser.parse(source, None).unwrap();
        let tree = recover(&mut parser, initial, source);
        let wrapper = descendants(tree.root_node())
            .find(|node| node.kind() == "redirected_statement")
            .unwrap();
        assert!(callable_has_error(wrapper));
        assert!(tree.root_node().has_error());
        original_coordinates(source, &tree);
    }
}
