//! Source descriptions share one association policy across all parser backends.
//! Backends supply AST comment spans and declaration anchors; attachment happens
//! after parsing. Attached leading comments also extend symbol source ranges,
//! callable content hashes, and embedding inputs.
//!
//! Whitespace joins comments and declarations unless it contains a blank line.
//! File headers ignore leading blank lines and may also describe an adjacent
//! first declaration. Delimiters and block-comment stars are removed; internal
//! prose line breaks are retained. Python constant docstrings have their quote
//! delimiters/prefixes removed and indentation cleaned; escapes remain source
//! text. Comments and docstrings combine with a paragraph break.

use super::{FileStructure, ParsedFile, code, data::document::Positions};
use std::{collections::HashMap, ops::Range};
use tree_sitter::Node;

#[derive(Default)]
pub(super) struct SourceDescriptions {
    pub comments: Vec<Range<usize>>,
    anchors: HashMap<usize, usize>,
    docstrings: HashMap<usize, String>,
}

impl SourceDescriptions {
    pub fn collect_code(
        &mut self,
        root: Node<'_>,
        source: &str,
        language: &str,
        structure: &FileStructure,
    ) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if matches!(node.kind(), "comment" | "line_comment" | "block_comment") {
                // A shebang is an interpreter directive, not documentation.
                if !source[node.byte_range()].starts_with("#!") {
                    self.comments.push(node.byte_range());
                }
                continue;
            }
            if language == "python"
                && matches!(node.kind(), "function_definition" | "class_definition")
                && let Some(value) = node
                    .child_by_field_name("body")
                    .and_then(|body| code::python_docstring_node(source, body))
                && let Some(description) = python_prose(source, value)
            {
                let wrapper = node
                    .parent()
                    .filter(|parent| parent.kind() == "decorated_definition")
                    .unwrap_or(node);
                self.docstrings.insert(wrapper.start_byte(), description);
            }
            // Strings and shell heredoc bodies must never be scanned as prose
            // comments. Actual interpolation expressions still have AST nodes.
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        for symbol in &structure.nodes {
            let Some(mut node) = root.descendant_for_byte_range(
                symbol.start_byte,
                (symbol.start_byte + 1).min(source.len()),
            ) else {
                continue;
            };
            while node.end_byte() < symbol.end_byte {
                let Some(parent) = node.parent() else {
                    break;
                };
                node = parent;
            }
            // Attributes can make the structural range start before the AST
            // declaration. In that case the structural start is the anchor.
            if node.start_byte() != symbol.start_byte {
                continue;
            }
            while let Some(parent) = node.parent() {
                let wrapper = matches!(
                    parent.kind(),
                    "lexical_declaration"
                        | "variable_declaration"
                        | "field_declaration"
                        | "constant_declaration"
                        | "declaration"
                        | "type_definition"
                        | "export_statement"
                        | "ambient_declaration"
                        | "decorated_definition"
                ) || matches!(
                    parent.kind(),
                    "const_declaration" | "var_declaration" | "type_declaration"
                ) && super::syntax::children(parent).len() == 1;
                if !wrapper {
                    break;
                }
                node = parent;
            }
            self.anchors.insert(symbol.start_byte, node.start_byte());
        }
    }

    pub fn apply(mut self, source: &str, parsed: &mut ParsedFile) {
        // Some grammars include the newline in a line-comment node. Trim layout
        // before measuring gaps so CRLF and LF have identical blank-line rules.
        for span in &mut self.comments {
            let raw = &source[span.clone()];
            span.end -= raw.len() - raw.trim_end().len();
        }
        self.comments.sort_by_key(|span| span.start);
        self.comments.dedup();
        let mut groups: Vec<CommentGroup> = Vec::new();
        for span in self.comments {
            let line_start = source[..span.start].rfind('\n').map_or(0, |byte| byte + 1);
            let follows_comment = groups.last().is_some_and(|previous| {
                previous.span.end >= line_start && adjacent(source, previous.span.end, span.start)
            });
            if !source[line_start..span.start].trim().is_empty() && !follows_comment {
                continue; // A trailing comment describes no subsequent symbol.
            }
            let prose = comment_prose(&source[span.clone()]);
            if let Some(previous) = groups.last_mut()
                && adjacent(source, previous.span.end, span.start)
            {
                previous.span.end = span.end;
                if let Some(prose) = prose {
                    if !previous.prose.is_empty() {
                        previous.prose.push('\n');
                    }
                    previous.prose.push_str(&prose);
                }
            } else {
                groups.push(CommentGroup {
                    span,
                    prose: prose.unwrap_or_default(),
                });
            }
        }
        parsed.description = groups
            .first()
            .filter(|group| source[..group.span.start].trim().is_empty())
            .and_then(|group| nonempty(group.prose.clone()));

        let mut comment_starts = HashMap::new();
        for symbol in &mut parsed.structure.nodes {
            let anchor = self
                .anchors
                .get(&symbol.start_byte)
                .copied()
                .unwrap_or(symbol.start_byte);
            let comments = preceding(source, &groups, anchor);
            if let Some(group) = comments {
                comment_starts.insert(symbol.id, group.span.start);
            }
            let docstring = self.docstrings.get(&symbol.start_byte).cloned();
            symbol.description = combine(
                comments.and_then(|group| nonempty(group.prose.clone())),
                docstring,
            );
        }
        let positions = Positions::new(source);
        let mut declarations: HashMap<&str, Vec<&super::StructureNode>> = HashMap::new();
        for symbol in &parsed.structure.nodes {
            declarations
                .entry(&symbol.qualified_name)
                .or_default()
                .push(symbol);
        }
        for callable in &mut parsed.callables {
            let start = positions.line_start(callable.start_line - 1) + callable.start_column - 1;
            let end = positions.line_start(callable.end_line - 1) + callable.end_column - 1;
            // Bound closures have different ranges from their declarations.
            // Match the innermost declaration with the same symbol identity.
            let declaration = declarations
                .get(callable.qualified_name.as_str())
                .into_iter()
                .flatten()
                .filter(|symbol| symbol.start_byte <= start && end <= symbol.end_byte)
                .min_by_key(|symbol| symbol.end_byte - symbol.start_byte);
            callable.description = declaration
                .and_then(|symbol| symbol.description.clone())
                .or_else(|| {
                    combine(
                        preceding(source, &groups, start)
                            .and_then(|group| nonempty(group.prose.clone())),
                        self.docstrings.get(&start).cloned(),
                    )
                });
            let comment_start = declaration
                .and_then(|symbol| comment_starts.get(&symbol.id).copied())
                .or_else(|| preceding(source, &groups, start).map(|group| group.span.start));
            if let Some(comment_start) = comment_start {
                let point = positions.point(comment_start);
                callable.start_line = point.row + 1;
                callable.start_column = point.column + 1;
                callable.line_count = callable.end_line - callable.start_line + 1;
                // Callable embedding inputs end with their exact source text.
                callable
                    .embedding_input
                    .truncate(callable.embedding_input.len() - callable.source.len());
                callable.source = source[comment_start..end].to_owned();
                callable.source_hash = crate::hash(&callable.source);
                callable.embedding_input.push_str(&callable.source);
            }
        }
        for symbol in &mut parsed.structure.nodes {
            if let Some(&start) = comment_starts.get(&symbol.id) {
                let point = positions.point(start);
                symbol.declaration_start_line = Some(symbol.start_line);
                symbol.declaration_start_byte = Some(symbol.start_byte);
                symbol.start_byte = start;
                symbol.start_line = point.row + 1;
                symbol.start_column = point.column + 1;
            }
        }
    }
}

struct CommentGroup {
    span: Range<usize>,
    prose: String,
}

fn adjacent(source: &str, end: usize, start: usize) -> bool {
    end <= start
        && source[end..start].chars().all(char::is_whitespace)
        && source[end..start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            <= 1
}

fn preceding<'a>(
    source: &str,
    groups: &'a [CommentGroup],
    start: usize,
) -> Option<&'a CommentGroup> {
    let index = groups.partition_point(|group| group.span.end <= start);
    let group = groups.get(index.checked_sub(1)?)?;
    adjacent(source, group.span.end, start).then_some(group)
}

fn nonempty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn combine(comments: Option<String>, docstring: Option<String>) -> Option<String> {
    match (comments, docstring) {
        (Some(comments), Some(docstring)) => Some(format!("{comments}\n\n{docstring}")),
        (comments, docstring) => comments.or(docstring),
    }
}

fn comment_prose(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let (body, block) = if let Some(body) = raw.strip_prefix("<!--") {
        (body.strip_suffix("-->").unwrap_or(body), false)
    } else if let Some(body) = raw.strip_prefix("/*") {
        let body = body.strip_suffix("*/").unwrap_or(body);
        (body.strip_prefix(['*', '!']).unwrap_or(body), true)
    } else if let Some(body) = raw.strip_prefix("//") {
        (body.strip_prefix(['/', '!']).unwrap_or(body), false)
    } else if let Some(body) = raw.strip_prefix('#') {
        (body, false)
    } else {
        // Markdown's spans can include indentation or container markers.
        let start = raw.find("<!--")?;
        return comment_prose(&raw[start..]);
    };
    let lines: Vec<_> = body
        .lines()
        .map(|line| {
            let line = line.trim();
            if block {
                line.strip_prefix('*').unwrap_or(line).trim()
            } else {
                line
            }
        })
        .collect();
    nonempty(lines.join("\n").trim().to_owned())
}

fn python_prose(source: &str, value: Node<'_>) -> Option<String> {
    let literals = if value.kind() == "concatenated_string" {
        super::syntax::children(value)
    } else {
        vec![value]
    };
    let mut prose = String::new();
    for literal in literals {
        let raw = &source[literal.byte_range()];
        let quote = raw.find(['\'', '"'])?;
        let raw = &raw[quote..];
        let delimiter = if raw.starts_with("\"\"\"") || raw.starts_with("'''") {
            3
        } else {
            1
        };
        prose.push_str(raw.get(delimiter..raw.len().checked_sub(delimiter)?)?);
    }
    // Python's cleandoc convention: trim the first line separately, dedent the
    // remainder together, and remove empty leading/trailing lines.
    let expanded = prose.replace('\t', "        ");
    let lines: Vec<_> = expanded.lines().collect();
    let indent = lines
        .iter()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    let cleaned = lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                line.trim().to_owned()
            } else {
                line.get(indent.min(line.len())..)
                    .unwrap_or("")
                    .trim_end()
                    .to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    nonempty(cleaned.trim().to_owned())
}
