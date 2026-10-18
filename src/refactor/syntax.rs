//! Shared syntax and byte-edit primitives for language-specific refactorings.

use std::{collections::BTreeMap, ops::Range};

use anyhow::{Context, Result, bail, ensure};
use tree_sitter::{Node, Parser, Tree};

#[derive(Clone)]
pub(super) struct Edit {
    pub range: Range<usize>,
    pub text: String,
}

pub(super) type Edits = BTreeMap<String, Vec<Edit>>;

pub(super) fn tree(path: &str, source: &str) -> Result<Tree> {
    let language = match crate::parse::language_for_path(path) {
        Some("typescript") => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Some("tsx") => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Some("javascript" | "jsx") => tree_sitter_javascript::LANGUAGE.into(),
        Some("rust") => tree_sitter_rust::LANGUAGE.into(),
        Some("go") => tree_sitter_go::LANGUAGE.into(),
        Some("java") => tree_sitter_java::LANGUAGE.into(),
        Some("c") => tree_sitter_c::LANGUAGE.into(),
        _ => bail!("Unsupported refactoring language: {path}"),
    };
    let mut parser = Parser::new();
    parser.set_language(&language)?;
    let tree = parser
        .parse(source, None)
        .context("Tree-sitter returned no tree")?;
    ensure!(
        !tree.root_node().has_error(),
        "Cannot safely refactor invalid syntax in {path}"
    );
    Ok(tree)
}

pub(super) fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|node| !node.is_extra())
        .collect()
}

pub(super) fn walk(node: Node<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        result.push(node);
        stack.extend(children(node).into_iter().rev());
    }
    result
}

pub(super) fn text<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.byte_range()]
}

pub(super) fn edited(source: &str, mut edits: Vec<Edit>) -> Result<String> {
    edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
    edits.dedup_by(|a, b| a.range == b.range && a.text == b.text);
    let mut result = String::new();
    let mut end = 0;
    for edit in edits {
        ensure!(
            edit.range.start >= end && edit.range.end >= edit.range.start,
            "Conflicting refactoring edits; no files were changed"
        );
        result.push_str(
            source
                .get(end..edit.range.start)
                .context("Invalid edit boundary")?,
        );
        result.push_str(&edit.text);
        end = edit.range.end;
    }
    result.push_str(source.get(end..).context("Invalid edit boundary")?);
    Ok(result)
}

pub(super) fn relative_path(from: &str, to: &str) -> String {
    let from: Vec<_> = from.split('/').collect();
    let from = &from[..from.len() - 1];
    let to: Vec<_> = to.split('/').collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    format!(
        "{}{}",
        "../".repeat(from.len() - common),
        to[common..].join("/")
    )
}
