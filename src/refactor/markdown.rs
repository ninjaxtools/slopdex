//! Rewrite parsed Markdown destinations, never prose or code examples.

use std::{collections::BTreeMap, ops::Range};

use anyhow::{Context, Result};
use tree_sitter_md_025::MarkdownParser;

use crate::parse::FileStructure;

use super::syntax::{Edit, edited, relative_path, text, walk};

pub(super) fn move_file(
    files: BTreeMap<String, (String, FileStructure)>,
    from: &str,
    to: &str,
) -> Result<BTreeMap<String, String>> {
    let mut parser = MarkdownParser::default();
    let mut result = BTreeMap::new();
    for (path, (source, _)) in files {
        let new_path = if path == from { to } else { &path };
        let tree = parser
            .parse(source.as_bytes(), None)
            .with_context(|| format!("Cannot parse Markdown links in {path}"))?;
        let mut edits = Vec::new();
        for tree in std::iter::once(tree.block_tree()).chain(tree.inline_trees()) {
            for node in walk(tree.root_node()) {
                // Wiki links have renderer-dependent resolution; do not treat
                // their destinations as ordinary relative file URLs.
                if node.kind() != "link_destination"
                    || node.parent().is_some_and(|node| node.kind() == "wiki_link")
                {
                    continue;
                }
                let raw = text(&source, node);
                let (raw, range) = destination(raw, node.byte_range());
                if let Some(replacement) = rewrite(raw, &path, new_path, from, to) {
                    edits.push(Edit {
                        range,
                        text: replacement,
                    });
                }
            }
        }
        result.insert(new_path.to_owned(), edited(&source, edits)?);
    }
    Ok(result)
}

fn destination(raw: &str, range: Range<usize>) -> (&str, Range<usize>) {
    if let Some(inner) = raw.strip_prefix('<').and_then(|raw| raw.strip_suffix('>')) {
        (inner, range.start + 1..range.end - 1)
    } else {
        (raw, range)
    }
}

fn rewrite(raw: &str, path: &str, new_path: &str, from: &str, to: &str) -> Option<String> {
    let (url, suffix_start) = decode_url(raw);
    // Keep external URLs, network paths, and same-document fragments/queries.
    if url.starts_with("//") || scheme(&url) {
        return None;
    }
    let end = url.find(['#', '?']).unwrap_or(url.len());
    let link = &url[..end];
    if link.is_empty() {
        return None;
    }
    let decoded = percent_decode(link)?;
    let target = resolve(path, &decoded)?;
    let new_target = if target == from { to } else { &target };
    if path == new_path && target == new_target {
        return None;
    }
    if resolve(new_path, &decoded).as_deref() == Some(new_target) {
        return None;
    }
    let mut replacement = if link.starts_with('/') {
        format!("/{new_target}")
    } else {
        let relative = relative_path(new_path, new_target);
        let relative = if relative.is_empty() {
            ".".into()
        } else {
            relative
        };
        if link.starts_with("./") && !relative.starts_with("../") {
            format!("./{relative}")
        } else {
            relative
        }
    };
    if link.ends_with('/') && !replacement.ends_with('/') {
        replacement.push('/');
    }
    // Encode newly introduced spaces, URL delimiters and Markdown punctuation.
    // The original query/fragment (including Markdown escapes) stays verbatim.
    Some(format!(
        "{}{}",
        percent_encode(&replacement),
        &raw[suffix_start..]
    ))
}

fn scheme(url: &str) -> bool {
    let Some((prefix, _)) = url.split_once(':') else {
        return false;
    };
    let mut chars = prefix.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn resolve(path: &str, link: &str) -> Option<String> {
    let mut parts: Vec<_> = if link.starts_with('/') {
        Vec::new()
    } else {
        path.split('/').take(path.split('/').count() - 1).collect()
    };
    for part in link.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn decode_url(raw: &str) -> (String, usize) {
    let mut result = String::new();
    let mut offset = 0;
    let mut suffix = None;
    while offset < raw.len() {
        let rest = &raw[offset..];
        let c = rest.chars().next().unwrap();
        let mut length = c.len_utf8();
        let mut decoded = std::borrow::Cow::Borrowed(&rest[..length]);
        if rest.starts_with('\\') && rest.as_bytes().get(1).is_some_and(u8::is_ascii_punctuation) {
            length = 2;
            decoded = std::borrow::Cow::Borrowed(&rest[1..2]);
        } else if rest.starts_with('&') {
            // CommonMark entities are semicolon-terminated. Bound the search
            // by the longest HTML entity name, avoiding quadratic scanning.
            if let Some(end) = rest.bytes().take(64).position(|byte| byte == b';') {
                let entity = &rest[..=end];
                let value = html_escape::decode_html_entities(entity);
                if value != entity {
                    length = end + 1;
                    decoded = value;
                }
            }
        }
        if suffix.is_none() && decoded.contains(['#', '?']) {
            suffix = Some(offset);
        }
        result.push_str(&decoded);
        offset += length;
    }
    (result, suffix.unwrap_or(raw.len()))
}

fn percent_decode(raw: &str) -> Option<String> {
    let mut result = Vec::new();
    let mut bytes = raw.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = (bytes.next()? as char).to_digit(16)?;
            let low = (bytes.next()? as char).to_digit(16)?;
            result.push((high * 16 + low) as u8);
        } else {
            result.push(byte);
        }
    }
    String::from_utf8(result).ok()
}

fn percent_encode(raw: &str) -> String {
    use std::fmt::Write;

    let mut result = String::new();
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            result.push(byte as char);
        } else {
            write!(result, "%{byte:02X}").unwrap();
        }
    }
    result
}
