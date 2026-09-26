//! Callable extraction and heading-aware Markdown chunking.
//!
//! Locations are one-based; end columns are exclusive, in UTF-8 bytes (the
//! tree-sitter convention). Syntax errors are recoverable diagnostics, while
//! parser initialization/failure is returned as an error.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use tree_sitter::{Language, Node, Parser, Tree};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Callable {
    pub language: String,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub signature: Option<String>,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub line_count: usize,
    pub source: String,
    pub source_hash: String,
    pub embedding_input: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownChunk {
    pub heading_path: Vec<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
    pub source_hash: String,
    pub embedding_input: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub message: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ParsedFile {
    pub callables: Vec<Callable>,
    pub chunks: Vec<MarkdownChunk>,
    pub errors: Vec<Diagnostic>,
}

pub fn language_for_path(path: &str) -> Option<&'static str> {
    let file = path.rsplit(['/', '\\']).next()?;
    let (stem, extension) = file.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    Some(match extension.to_ascii_lowercase().as_str() {
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "jsx",
        "py" | "pyw" => "python",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "md" | "markdown" => "markdown",
        _ => return None,
    })
}

pub fn parse(path: &str, source: &str) -> Result<ParsedFile> {
    let Some(language) = language_for_path(path) else {
        return Ok(ParsedFile::default());
    };
    if language == "markdown" {
        return Ok(ParsedFile {
            chunks: chunk_markdown(source),
            ..ParsedFile::default()
        });
    }
    let grammar: Language = match language {
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "c" => tree_sitter_c::LANGUAGE.into(),
        _ => tree_sitter_javascript::LANGUAGE.into(),
    };
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .with_context(|| format!("Cannot initialize {language} parser for {path}"))?;
    let mut tree = parser
        .parse(source, None)
        .with_context(|| format!("Cannot parse {path}: tree-sitter returned no tree"))?;
    if tree.root_node().has_error()
        && matches!(language, "typescript" | "tsx")
        && let Some(recovered) = recover_typescript(&mut parser, &tree, source)
    {
        tree = recovered;
    }
    let javascript = matches!(language, "typescript" | "tsx" | "javascript" | "jsx");
    let mut collector = Collector {
        source,
        language,
        candidates: Vec::new(),
    };
    if javascript {
        collector.walk_js(tree.root_node(), &[]);
    } else {
        collector.walk_native(tree.root_node(), &[]);
    }
    collector.candidates.sort_by_key(|c| c.node.start_byte());
    let mut result = ParsedFile {
        errors: diagnostics(tree.root_node(), source, &collector.candidates),
        ..ParsedFile::default()
    };
    for candidate in collector.candidates {
        if candidate.node.has_error() {
            continue;
        }
        let qualified_name = qualified(&candidate.scope, &candidate.name);
        match extract(source, language, &candidate, &qualified_name) {
            Ok(callable) => result.callables.push(callable),
            Err(error) => result.errors.push(diagnostic(
                candidate.node,
                format!("Cannot extract {qualified_name}: {error}"),
            )),
        }
    }
    Ok(result)
}

fn hash(source: &str) -> String {
    format!("{:x}", Sha256::digest(source.as_bytes()))
}

fn text<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    // Recovered trees keep byte offsets identical to the original source.
    source.get(node.byte_range()).unwrap_or("")
}

fn field<'a>(source: &'a str, node: Node<'_>, name: &str) -> Option<&'a str> {
    node.child_by_field_name(name).map(|n| text(source, n))
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn scoped(scope: &[String], name: &str) -> Vec<String> {
    let mut result = scope.to_vec();
    result.push(name.to_owned());
    result
}

fn qualified(scope: &[String], name: &str) -> String {
    scoped(scope, name).join(".")
}

struct Candidate<'tree> {
    node: Node<'tree>,
    name: String,
    kind: &'static str,
    scope: Vec<String>,
    signature: Option<String>,
    documentation: Option<String>,
}

struct Collector<'source, 'tree> {
    source: &'source str,
    language: &'static str,
    candidates: Vec<Candidate<'tree>>,
}

fn js_kind(node: Node<'_>, name: &str) -> &'static str {
    if node.kind() == "method_definition" && name == "constructor" {
        "constructor"
    } else if node.kind().contains("generator") || has_token(node, "*") {
        "generator"
    } else if node.kind() == "method_definition" {
        "method"
    } else {
        "function"
    }
}

fn has_token(node: Node<'_>, token: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| child.kind() == token)
}

fn binding_name(source: &str, node: Node<'_>) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    matches!(
        name.kind(),
        "identifier" | "property_identifier" | "private_property_identifier"
    )
    .then(|| text(source, name).trim_start_matches('#').to_owned())
}

fn assignment_name(source: &str, node: Node<'_>) -> Option<String> {
    let left = node.child_by_field_name("left")?;
    match left.kind() {
        "identifier" => Some(text(source, left).to_owned()),
        "member_expression" => {
            field(source, left, "property").map(|name| name.trim_start_matches('#').to_owned())
        }
        _ => None,
    }
}

fn key_name(name: &str) -> String {
    name.replace(['\'', '"'], "")
}

fn object_scope(source: &str, node: Node<'_>) -> Option<String> {
    let parent = node.parent()?;
    match parent.kind() {
        "variable_declarator" if parent.child_by_field_name("value") == Some(node) => {
            binding_name(source, parent)
        }
        "pair" if parent.child_by_field_name("value") == Some(node) => {
            field(source, parent, "key").map(key_name)
        }
        "assignment_expression" if parent.child_by_field_name("right") == Some(node) => {
            assignment_name(source, parent)
        }
        _ => None,
    }
}

fn unwrap(mut node: Node<'_>) -> Node<'_> {
    while node.kind() == "parenthesized_expression" && node.named_child_count() == 1 {
        node = node.named_child(0).unwrap();
    }
    node
}

impl<'tree> Collector<'_, 'tree> {
    fn add_js(&mut self, node: Node<'tree>, name: String, kind: &'static str, scope: &[String]) {
        let body = node.child_by_field_name("body");
        if body.is_none() && !node.has_error() {
            return;
        }
        self.candidates.push(Candidate {
            node,
            name: name.clone(),
            kind,
            scope: scope.to_vec(),
            signature: Some(js_signature(self.source, node, &name)),
            documentation: None,
        });
        if let Some(body) = body {
            self.walk_js(body, &scoped(scope, &name));
        }
    }

    fn walk_js(&mut self, node: Node<'tree>, scope: &[String]) {
        let source = self.source;
        match node.kind() {
            "class_declaration"
            | "abstract_class_declaration"
            | "class"
            | "object"
            | "internal_module"
            | "module" => {
                let name = match node.kind() {
                    "object" => object_scope(source, node),
                    "internal_module" | "module" => field(source, node, "name").map(key_name),
                    _ => field(source, node, "name")
                        .map(str::to_owned)
                        .or_else(|| object_scope(source, node)),
                };
                let next = name
                    .map(|n| scoped(scope, &n))
                    .unwrap_or_else(|| scope.to_vec());
                for child in children(node) {
                    self.walk_js(child, &next);
                }
                return;
            }
            "function_declaration" | "generator_function_declaration" | "method_definition" => {
                if let Some(name) = field(source, node, "name") {
                    let name = name.trim_start_matches('#');
                    self.add_js(node, name.to_owned(), js_kind(node, name), scope);
                }
                return;
            }
            _ => {}
        }
        let (name, value, method) = match node.kind() {
            "public_field_definition" | "property_definition" | "field_definition" => (
                field(source, node, "name")
                    .or_else(|| field(source, node, "property"))
                    .map(|n| n.trim_start_matches('#').to_owned()),
                node.child_by_field_name("value"),
                true,
            ),
            "variable_declarator" => (
                binding_name(source, node),
                node.child_by_field_name("value"),
                false,
            ),
            "pair" => (
                field(source, node, "key").map(key_name),
                node.child_by_field_name("value"),
                false,
            ),
            "assignment_expression" => (
                assignment_name(source, node),
                node.child_by_field_name("right"),
                false,
            ),
            _ => (None, None, false),
        };
        if let (Some(name), Some(value)) = (name, value) {
            let value = unwrap(value);
            if matches!(
                value.kind(),
                "arrow_function" | "function_expression" | "generator_function"
            ) {
                let kind = js_kind(value, &name);
                self.add_js(
                    value,
                    name,
                    if method && kind != "generator" {
                        "method"
                    } else {
                        kind
                    },
                    scope,
                );
                return;
            }
        }
        if matches!(node.kind(), "function_expression" | "generator_function")
            && let Some(name) = field(source, node, "name")
        {
            self.add_js(node, name.to_owned(), js_kind(node, name), scope);
            return;
        }
        for child in children(node) {
            self.walk_js(child, scope);
        }
    }

    fn add_native(
        &mut self,
        node: Node<'tree>,
        name: &str,
        kind: &'static str,
        scope: &[String],
        bound: bool,
    ) -> bool {
        let body = node.child_by_field_name("body");
        if body.is_none_or(|n| n.is_missing()) && !node.has_error() {
            return false;
        }
        let source_node = if self.language == "python" {
            node.parent()
                .filter(|p| p.kind() == "decorated_definition")
                .unwrap_or(node)
        } else {
            node
        };
        let header = self
            .source
            .get(node.start_byte()..body.map_or(node.end_byte(), |n| n.start_byte()))
            .unwrap_or("")
            .trim_end();
        let documentation = if self.language == "python" {
            body.and_then(|b| python_docstring(self.source, b))
                .map(str::to_owned)
        } else {
            None
        };
        self.candidates.push(Candidate {
            node: source_node,
            name: name.to_owned(),
            kind,
            scope: scope.to_vec(),
            signature: Some(if bound {
                format!("{name} = {header}")
            } else {
                header.to_owned()
            }),
            documentation,
        });
        if let Some(body) = body {
            self.walk_native(body, &scoped(scope, name));
        }
        true
    }

    fn add_bound(
        &mut self,
        value: Option<Node<'tree>>,
        name: Option<Node<'tree>>,
        scope: &[String],
    ) -> bool {
        let (Some(value), Some(name)) = (value, name) else {
            return false;
        };
        let value = unwrap(value);
        if !matches!(
            name.kind(),
            "identifier" | "attribute" | "selector_expression"
        ) {
            return false;
        }
        let closure = match self.language {
            "python" => "lambda",
            "rust" => "closure_expression",
            "go" => "func_literal",
            "java" => "lambda_expression",
            _ => return false,
        };
        value.kind() == closure
            && self.add_native(value, text(self.source, name), "function", scope, true)
    }

    fn walk_native(&mut self, node: Node<'tree>, scope: &[String]) {
        let source = self.source;
        match (self.language, node.kind()) {
            ("python", "class_definition") => {
                if let Some(body) = node.child_by_field_name("body") {
                    let next = field(source, node, "name")
                        .map(|n| scoped(scope, n))
                        .unwrap_or_else(|| scope.to_vec());
                    self.walk_native(body, &next);
                }
                return;
            }
            ("python", "function_definition") => {
                if let Some(name) = field(source, node, "name") {
                    let definition = node
                        .parent()
                        .filter(|p| p.kind() == "decorated_definition")
                        .unwrap_or(node);
                    let method = definition
                        .parent()
                        .and_then(|p| p.parent())
                        .is_some_and(|p| p.kind() == "class_definition");
                    let kind = if method && matches!(name, "__init__" | "__new__") {
                        "constructor"
                    } else if node.child_by_field_name("body").is_some_and(contains_yield) {
                        "generator"
                    } else if method {
                        "method"
                    } else {
                        "function"
                    };
                    self.add_native(node, name, kind, scope, false);
                }
                return;
            }
            ("python", "assignment" | "named_expression")
                if self.add_bound(
                    node.child_by_field_name("right")
                        .or_else(|| node.child_by_field_name("value")),
                    node.child_by_field_name("left")
                        .or_else(|| node.child_by_field_name("name")),
                    scope,
                ) =>
            {
                return;
            }
            ("rust", "mod_item" | "trait_item" | "impl_item") => {
                let name = if node.kind() == "impl_item" {
                    field(source, node, "type").map(|ty| match field(source, node, "trait") {
                        Some(tr) => format!("<{ty} as {tr}>"),
                        None => ty.to_owned(),
                    })
                } else {
                    field(source, node, "name").map(str::to_owned)
                };
                if let Some(body) = node.child_by_field_name("body") {
                    let next = name
                        .map(|n| scoped(scope, &n))
                        .unwrap_or_else(|| scope.to_vec());
                    self.walk_native(body, &next);
                }
                return;
            }
            ("rust", "function_item") => {
                if let Some(name) = field(source, node, "name") {
                    let method = node
                        .parent()
                        .and_then(|p| p.parent())
                        .is_some_and(|p| matches!(p.kind(), "impl_item" | "trait_item"));
                    self.add_native(
                        node,
                        name,
                        if method { "method" } else { "function" },
                        scope,
                        false,
                    );
                }
                return;
            }
            ("rust", "let_declaration")
                if self.add_bound(
                    node.child_by_field_name("value"),
                    node.child_by_field_name("pattern"),
                    scope,
                ) =>
            {
                return;
            }
            ("go", "function_declaration" | "method_declaration") => {
                if let Some(name) = field(source, node, "name") {
                    let receiver = go_receiver(source, node);
                    let next = receiver
                        .map(|r| scoped(scope, r))
                        .unwrap_or_else(|| scope.to_vec());
                    self.add_native(
                        node,
                        name,
                        if receiver.is_some() {
                            "method"
                        } else {
                            "function"
                        },
                        &next,
                        false,
                    );
                }
                return;
            }
            ("go", "short_var_declaration" | "assignment_statement" | "var_spec") => {
                let names: Vec<_> = if node.kind() == "var_spec" {
                    let mut cursor = node.walk();
                    node.children_by_field_name("name", &mut cursor).collect()
                } else {
                    node.child_by_field_name("left")
                        .map(children)
                        .unwrap_or_default()
                };
                let values = node
                    .child_by_field_name("right")
                    .or_else(|| node.child_by_field_name("value"))
                    .map(children)
                    .unwrap_or_default();
                if names.len() == values.len() {
                    for (name, value) in names.into_iter().zip(values) {
                        if !self.add_bound(Some(value), Some(name), scope) {
                            self.walk_native(value, scope);
                        }
                    }
                    return;
                }
            }
            (
                "java",
                "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "annotation_type_declaration"
                | "enum_constant",
            ) => {
                let next = field(source, node, "name")
                    .map(|n| scoped(scope, n))
                    .unwrap_or_else(|| scope.to_vec());
                for child in children(node) {
                    self.walk_native(child, &next);
                }
                return;
            }
            ("java", "class_body")
                if node
                    .parent()
                    .is_some_and(|p| p.kind() == "object_creation_expression") =>
            {
                let pos = node.start_position();
                let next = scoped(
                    scope,
                    &format!("<anonymous@{}:{}>", pos.row + 1, pos.column + 1),
                );
                for child in children(node) {
                    self.walk_native(child, &next);
                }
                return;
            }
            (
                "java",
                "method_declaration"
                | "constructor_declaration"
                | "compact_constructor_declaration",
            ) => {
                if let Some(name) = field(source, node, "name") {
                    self.add_native(
                        node,
                        name,
                        if node.kind() == "method_declaration" {
                            "method"
                        } else {
                            "constructor"
                        },
                        scope,
                        false,
                    );
                }
                return;
            }
            ("java", "variable_declarator")
                if self.add_bound(
                    node.child_by_field_name("value"),
                    node.child_by_field_name("name"),
                    scope,
                ) =>
            {
                return;
            }
            ("c", "function_definition") => {
                if let Some(name) = node
                    .child_by_field_name("declarator")
                    .and_then(|n| c_function_name(source, n))
                {
                    self.add_native(node, name, "function", scope, false);
                }
                return;
            }
            _ => {}
        }
        if matches!(
            node.kind(),
            "lambda" | "closure_expression" | "func_literal" | "lambda_expression"
        ) {
            return;
        }
        for child in children(node) {
            self.walk_native(child, scope);
        }
    }
}

fn contains_yield(node: Node<'_>) -> bool {
    match node.kind() {
        "yield" => true,
        "function_definition" | "class_definition" | "lambda" => false,
        _ => children(node).into_iter().any(contains_yield),
    }
}

fn python_docstring<'a>(source: &'a str, body: Node<'_>) -> Option<&'a str> {
    let statement = children(body).into_iter().find(|n| n.kind() != "comment")?;
    if statement.kind() != "expression_statement" {
        return None;
    }
    let value = statement.named_child(0)?;
    matches!(value.kind(), "string" | "concatenated_string").then(|| text(source, value))
}

fn go_receiver<'a>(source: &'a str, node: Node<'_>) -> Option<&'a str> {
    let mut ty = node
        .child_by_field_name("receiver")?
        .named_child(0)?
        .child_by_field_name("type")?;
    while ty.kind() == "pointer_type" {
        ty = ty.named_child(0)?;
    }
    Some(text(source, ty))
}

fn c_function_name<'a>(source: &'a str, mut node: Node<'_>) -> Option<&'a str> {
    loop {
        if node.kind() == "identifier" {
            return Some(text(source, node));
        }
        // Only follow the declarator spine, never a parameter's identifier.
        node = node.child_by_field_name("declarator").or_else(|| {
            (node.kind() == "parenthesized_declarator")
                .then(|| node.named_child(0))
                .flatten()
        })?;
    }
}

fn js_signature(source: &str, node: Node<'_>, name: &str) -> String {
    let parameters = field(source, node, "parameters")
        .map(str::to_owned)
        .or_else(|| field(source, node, "parameter").map(|p| format!("({p})")))
        .unwrap_or_default();
    format!(
        "{}{}{name}{}{}{}",
        if has_token(node, "async") {
            "async "
        } else {
            ""
        },
        if has_token(node, "*") { "*" } else { "" },
        field(source, node, "type_parameters").unwrap_or(""),
        parameters,
        field(source, node, "return_type").unwrap_or("")
    )
}

fn extract(
    source: &str,
    language: &str,
    candidate: &Candidate<'_>,
    qualified_name: &str,
) -> Result<Callable> {
    let node = candidate.node;
    let source = source
        .get(node.byte_range())
        .context("invalid UTF-8 source range")?;
    let signature = candidate.signature.clone();
    let start = node.start_position();
    let end = node.end_position();
    let mut embedding_input = format!(
        "language: {language}\nkind: {}\nsymbol: {qualified_name}",
        candidate.kind
    );
    if let Some(signature) = &signature {
        embedding_input.push_str(&format!("\nsignature: {signature}"));
    }
    if let Some(documentation) = &candidate.documentation {
        embedding_input.push_str(&format!("\ndocumentation:\n{documentation}"));
    }
    embedding_input.push_str("\nsource:\n");
    embedding_input.push_str(source);
    Ok(Callable {
        language: language.to_owned(),
        kind: candidate.kind.to_owned(),
        name: candidate.name.clone(),
        qualified_name: qualified_name.to_owned(),
        signature,
        start_line: start.row + 1,
        start_column: start.column + 1,
        end_line: end.row + 1,
        end_column: end.column + 1,
        line_count: end.row - start.row + 1,
        source: source.to_owned(),
        source_hash: hash(source),
        embedding_input,
    })
}

fn diagnostic(node: Node<'_>, message: String) -> Diagnostic {
    Diagnostic {
        message,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    }
}

fn diagnostics(root: Node<'_>, source: &str, candidates: &[Candidate<'_>]) -> Vec<Diagnostic> {
    if !root.has_error() {
        return Vec::new();
    }
    let mut errors = Vec::new();
    let mut broken = HashSet::new();
    for candidate in candidates.iter().filter(|c| c.node.has_error()) {
        errors.push(diagnostic(
            candidate.node,
            format!(
                "Cannot fully parse callable {}; omitted from the searchable index.",
                qualified(&candidate.scope, &candidate.name)
            ),
        ));
        broken.insert(candidate.node.id());
    }
    let mut stack = vec![(root, false)];
    while let Some((node, inside)) = stack.pop() {
        let inside = inside || broken.contains(&node.id());
        if node.is_error() || node.is_missing() {
            if !inside {
                errors.push(diagnostic(
                    node,
                    if node.is_missing() {
                        format!("Tree-sitter expected {}.", node.kind())
                    } else {
                        "Tree-sitter could not parse this source region.".to_owned()
                    },
                ));
            }
            let mut cursor = node.walk();
            let tokens: Vec<_> = node.children(&mut cursor).collect();
            for (index, token) in tokens.iter().enumerate() {
                if !matches!(token.kind(), "function" | "def" | "fn" | "func") {
                    continue;
                }
                let next =
                    index + 1 + usize::from(tokens.get(index + 1).is_some_and(|n| n.kind() == "*"));
                if let Some(name) = tokens
                    .get(next)
                    .filter(|n| matches!(n.kind(), "identifier" | "field_identifier"))
                {
                    errors.push(diagnostic(
                        node,
                        format!(
                            "Cannot parse declaration for {}; name recovered from an error node.",
                            text(source, *name)
                        ),
                    ));
                }
            }
            continue;
        }
        let mut cursor = node.walk();
        let nodes: Vec<_> = node
            .children(&mut cursor)
            .filter(|n| n.has_error() || n.is_error() || n.is_missing())
            .collect();
        stack.extend(nodes.into_iter().rev().map(|n| (n, inside)));
    }
    errors
}

// Mask only known grammar gaps, preserving every byte offset and newline. A
// replacement is accepted only when the entire resulting tree is error-free.
fn recover_typescript(parser: &mut Parser, tree: &Tree, source: &str) -> Option<Tree> {
    let mut masked = source.as_bytes().to_vec();
    let mut changed = false;
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_missing() && node.kind() == "!" {
            let arguments = node.parent().and_then(|parent| match parent.kind() {
                "type_arguments" => Some(parent),
                "non_null_expression" => parent
                    .named_child(0)
                    .filter(|child| child.kind() == "instantiation_expression")
                    .and_then(|child| child.child_by_field_name("type_arguments")),
                _ => None,
            });
            if let Some(arguments) = arguments
                && source[arguments.end_byte()..].trim_start().starts_with('`')
            {
                mask(
                    &mut masked,
                    arguments.start_byte(),
                    arguments.end_byte(),
                    false,
                );
                changed = true;
            }
        }
        if node.is_error()
            && text(source, node) == "type"
            && let Some(parent) = node.parent().filter(|p| p.kind() == "export_statement")
            && source[parent.start_byte()..node.start_byte()].trim() == "export"
            && source[node.end_byte()..parent.end_byte()]
                .trim_start()
                .starts_with('*')
        {
            mask(&mut masked, node.start_byte(), node.end_byte(), false);
            changed = true;
        }
        // An import token is syntactic: comments and string contents cannot
        // accidentally become recovery targets.
        if node.kind() == "import"
            && let Some(end) = import_expression_end(source, node.end_byte())
        {
            mask(&mut masked, node.start_byte(), end, true);
            changed = true;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    if !changed {
        return None;
    }
    let recovered = parser.parse(&masked, None)?;
    (!recovered.root_node().has_error()).then_some(recovered)
}

fn mask(bytes: &mut [u8], start: usize, end: usize, identifier: bool) {
    for byte in &mut bytes[start..end] {
        if !matches!(*byte, b'\r' | b'\n') {
            *byte = b' ';
        }
    }
    if identifier && start < end {
        bytes[start] = b'X';
    }
}

fn import_expression_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut i = start;
    let whitespace = |i: &mut usize| {
        while bytes.get(*i).is_some_and(u8::is_ascii_whitespace) {
            *i += 1;
        }
    };
    whitespace(&mut i);
    if bytes.get(i) != Some(&b'(') {
        return None;
    }
    i += 1;
    whitespace(&mut i);
    let quote = *bytes.get(i)?;
    if !matches!(quote, b'\'' | b'"') {
        return None;
    }
    i += 1;
    while let Some(&byte) = bytes.get(i) {
        if byte == quote {
            break;
        }
        if byte == b'\\' {
            i += 1;
        }
        i += 1;
    }
    if bytes.get(i) != Some(&quote) {
        return None;
    }
    i += 1;
    whitespace(&mut i);
    (bytes.get(i) == Some(&b')')).then_some(i + 1)
}

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

fn chunk_markdown(source: &str) -> Vec<MarkdownChunk> {
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
    use super::*;

    fn symbols(parsed: &ParsedFile) -> Vec<(&str, &str)> {
        parsed
            .callables
            .iter()
            .map(|c| (c.qualified_name.as_str(), c.kind.as_str()))
            .collect()
    }

    fn clean(path: &str, source: &str) -> ParsedFile {
        let parsed = parse(path, source).unwrap();
        assert!(parsed.errors.is_empty(), "{path}: {:?}", parsed.errors);
        for callable in &parsed.callables {
            assert!(
                !callable.signature.as_deref().unwrap_or("").is_empty(),
                "{}",
                callable.name
            );
            assert_eq!(
                callable.line_count,
                callable.end_line - callable.start_line + 1
            );
            let lines: Vec<_> = source.split('\n').collect();
            let mut selected = lines[callable.start_line - 1..callable.end_line].to_vec();
            let last = selected.len() - 1;
            selected[last] = &selected[last][..callable.end_column - 1];
            selected[0] = &selected[0][callable.start_column - 1..];
            assert_eq!(selected.join("\n"), callable.source);
            assert_eq!(callable.source_hash, hash(&callable.source));
            assert!(
                callable
                    .embedding_input
                    .contains(&format!("symbol: {}", callable.qualified_name))
            );
        }
        parsed
    }

    #[test]
    fn path_extensions_and_unsupported_files() {
        for (ext, language) in [
            ("ts", "typescript"),
            ("mts", "typescript"),
            ("cts", "typescript"),
            ("tsx", "tsx"),
            ("js", "javascript"),
            ("mjs", "javascript"),
            ("cjs", "javascript"),
            ("jsx", "jsx"),
            ("py", "python"),
            ("pyw", "python"),
            ("rs", "rust"),
            ("go", "go"),
            ("java", "java"),
            ("c", "c"),
            ("h", "c"),
            ("md", "markdown"),
            ("markdown", "markdown"),
        ] {
            assert_eq!(
                language_for_path(&format!("dir/source.{}", ext.to_uppercase())),
                Some(language)
            );
        }
        for path in [
            "file.cpp",
            "file.txt",
            ".ts",
            "dir.ts/file",
            "file",
            "file.",
        ] {
            assert_eq!(language_for_path(path), None, "{path}");
            assert!(
                parse(path, "function ignored() {}")
                    .unwrap()
                    .callables
                    .is_empty()
            );
        }
        assert_eq!(language_for_path("C:\\src\\source.TS"), Some("typescript"));
    }

    #[test]
    fn typescript_declarations_fields_and_nested_scopes() {
        let parsed = clean(
            "service.ts",
            r#"// café 🚀
export async function load<T>(id: T): Promise<T> {
  function normalize(value: T) { return value; }
  return normalize(id);
}
const save = (value: string) => value;
namespace Services {
  export class Store {
    constructor() {}
    static find(id: string) { return id; }
    #load = () => 1;
    save = function internalName() { return 2; };
    *values() { yield 1; }
  }
}
interface Definition { absent(): string; }
declare function external(): void;
"#,
        );
        assert_eq!(
            symbols(&parsed),
            vec![
                ("load", "function"),
                ("load.normalize", "function"),
                ("save", "function"),
                ("Services.Store.constructor", "constructor"),
                ("Services.Store.find", "method"),
                ("Services.Store.load", "method"),
                ("Services.Store.save", "method"),
                ("Services.Store.values", "generator"),
            ]
        );
        assert_eq!(
            parsed.callables[0].signature.as_deref(),
            Some("async load<T>(id: T): Promise<T>")
        );
        assert_eq!(parsed.callables[0].start_line, 2);
        assert_eq!(parsed.callables[0].line_count, 4);
        assert_eq!(parsed.callables[7].signature.as_deref(), Some("*values()"));
    }

    #[test]
    fn javascript_objects_assignments_and_named_expressions() {
        let parsed = clean(
            "service.js",
            r#"
const api = {
  get(id) { return id; },
  put: function internal(value) { return value; },
  nested: { 'run': () => 1 },
};
module.exports.remove = (id) => id;
const Store = class { load = () => 1; };
const wrapper = (n => n * 2);
invoke(function named() {});
invoke(() => 1);
"#,
        );
        assert_eq!(
            symbols(&parsed),
            vec![
                ("api.get", "method"),
                ("api.put", "function"),
                ("api.nested.run", "function"),
                ("remove", "function"),
                ("Store.load", "method"),
                ("wrapper", "function"),
                ("named", "function"),
            ]
        );
        assert_eq!(parsed.callables[5].signature.as_deref(), Some("wrapper(n)"));
    }

    #[test]
    fn jsx_and_tsx_grammars() {
        for (path, source) in [
            (
                "View.jsx",
                "export const View = ({title}) => <section>{title}</section>; class Screen { render() { return <View />; } }",
            ),
            (
                "View.tsx",
                "export const View = <T,>({title}: {title: T}) => <section>{String(title)}</section>; class Screen { render(): JSX.Element { return <View title='Hi' />; } }",
            ),
        ] {
            let parsed = clean(path, source);
            assert_eq!(
                symbols(&parsed),
                vec![("View", "function"), ("Screen.render", "method")]
            );
            assert_eq!(
                parsed.callables[0].language,
                language_for_path(path).unwrap()
            );
        }
    }

    #[test]
    fn python_decorators_docstrings_lambdas_and_generator_scope() {
        let parsed = clean(
            "service.py",
            r#"# café 🚀
@logged
async def load(value: str) -> str:
    """Normalize a value for storage."""
    def normalize(text):
        return text.strip()
    double = (lambda n: n * 2)
    return normalize(value)
class Store:
    def __init__(self):
        self.values = []
    @staticmethod
    def create():
        def nested():
            yield 1
        return list(nested())
    def values(self):
        yield from self.values
"#,
        );
        assert_eq!(
            symbols(&parsed),
            vec![
                ("load", "function"),
                ("load.normalize", "function"),
                ("load.double", "function"),
                ("Store.__init__", "constructor"),
                ("Store.create", "method"),
                ("Store.create.nested", "generator"),
                ("Store.values", "generator"),
            ]
        );
        let load = &parsed.callables[0];
        assert_eq!(load.start_line, 2);
        assert_eq!(load.end_line, 8);
        assert_eq!(
            load.signature.as_deref(),
            Some("async def load(value: str) -> str:")
        );
        assert!(load.source.starts_with("@logged\nasync def"));
        assert!(
            load.embedding_input
                .contains("documentation:\n\"\"\"Normalize a value for storage.\"\"\"\nsource:")
        );
        assert!(
            !parsed.callables[1]
                .embedding_input
                .contains("documentation:")
        );
        assert_eq!(
            parsed.callables[2].signature.as_deref(),
            Some("double = lambda n:")
        );
    }

    #[test]
    fn rust_modules_traits_impls_and_bound_closures() {
        let parsed = clean(
            "service.rs",
            r#"// café 🚀
mod api {
    trait Read {
        fn read(&self);
        fn default(&self) -> i32 { 1 }
    }
    impl<T> Read for Store<T> {
        fn read(&self) { let next = |x: i32| x + 1; }
    }
    impl Store {
        pub fn new() -> Self { todo!() }
        pub async fn load(&self) -> i32 { 1 }
    }
    fn outer() { fn inner() {} }
}
extern "C" { fn external(); }
macro_rules! generated { () => { fn hidden() {} } }
"#,
        );
        assert_eq!(
            symbols(&parsed),
            vec![
                ("api.Read.default", "method"),
                ("api.<Store<T> as Read>.read", "method"),
                ("api.<Store<T> as Read>.read.next", "function"),
                ("api.Store.new", "method"),
                ("api.Store.load", "method"),
                ("api.outer", "function"),
                ("api.outer.inner", "function"),
            ]
        );
        assert_eq!(
            parsed.callables[4].signature.as_deref(),
            Some("pub async fn load(&self) -> i32")
        );
    }

    #[test]
    fn go_receivers_and_one_to_one_closure_bindings() {
        let parsed = clean(
            "service.go",
            r#"package service
func (s *Store[T]) Get(value int) (int, error) {
    next := func(x int) int { return x + 1 }
    return next(value), nil
}
func (s Store[T]) Save() {}
func Load() {}
func external()
var run = func() {}
func outer() {
    first, second := func() {}, func() {}
    invoke(func() {})
}
"#,
        );
        assert_eq!(
            symbols(&parsed),
            vec![
                ("Store[T].Get", "method"),
                ("Store[T].Get.next", "function"),
                ("Store[T].Save", "method"),
                ("Load", "function"),
                ("run", "function"),
                ("outer", "function"),
                ("outer.first", "function"),
                ("outer.second", "function"),
            ]
        );
        assert_eq!(
            parsed.callables[0].signature.as_deref(),
            Some("func (s *Store[T]) Get(value int) (int, error)")
        );
    }

    #[test]
    fn java_overloads_records_enums_and_anonymous_classes() {
        let parsed = clean(
            "Store.java",
            r#"package app;
abstract class Store {
    Store() {}
    public int get(int value) { return value; }
    public String get(String value) { return value; }
    abstract void external();
    Runnable run = () -> {};
    class Nested { void save() {} }
    Object task = new Object() { void execute() {} };
}
interface Read { void read(); default int value() { return 1; } }
record Item(int id) { Item { if (id < 0) throw new IllegalArgumentException(); } }
enum Mode { ONE { int value() { return 1; } }; abstract int value(); }
"#,
        );
        let names: Vec<_> = parsed
            .callables
            .iter()
            .map(|c| c.qualified_name.as_str())
            .collect();
        assert_eq!(
            &names[..6],
            [
                "Store.Store",
                "Store.get",
                "Store.get",
                "Store.run",
                "Store.Nested.save",
                "Store.<anonymous@9:32>.execute"
            ]
        );
        assert_eq!(&names[6..], ["Read.value", "Item.Item", "Mode.ONE.value"]);
        assert_eq!(parsed.callables[0].kind, "constructor");
        assert_eq!(parsed.callables[7].kind, "constructor");
        assert_eq!(
            parsed.callables[1].signature.as_deref(),
            Some("public int get(int value)")
        );
        assert_eq!(
            parsed.callables[2].signature.as_deref(),
            Some("public String get(String value)")
        );
    }

    #[test]
    fn c_declarator_spines_and_preprocessor_branches() {
        let source = r#"int prototype(int value);
typedef int (*Callback)(int);
static int *get(int value) { return 0; }
int (*factory(void))(int) { return 0; }
#ifdef ENABLED
int load(void) { const char *text = "int fake(void) {}"; return 1; }
#else
int fallback(void) { return 0; }
#endif
"#;
        for path in ["service.c", "service.h"] {
            let parsed = clean(path, source);
            assert_eq!(
                symbols(&parsed),
                vec![
                    ("get", "function"),
                    ("factory", "function"),
                    ("load", "function"),
                    ("fallback", "function")
                ]
            );
            assert_eq!(
                parsed.callables[1].signature.as_deref(),
                Some("int (*factory(void))(int)")
            );
        }
    }

    #[test]
    fn malformed_callables_keep_healthy_siblings_and_diagnostics() {
        let parsed = parse(
            "store.ts",
            "class Store {\n  broken(value: ) { return value; }\n  good() { return 1; }\n}\n",
        )
        .unwrap();
        assert_eq!(symbols(&parsed), vec![("Store.good", "method")]);
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("Store.broken"));
        assert_eq!(
            (parsed.errors[0].start_line, parsed.errors[0].end_line),
            (2, 2)
        );
        for (path, source) in [
            ("bad.ts", "function good() {} function broken( {"),
            ("bad.py", "def good():\n    return 1\ndef broken(\n"),
            ("bad.rs", "fn good() {} fn broken( {"),
        ] {
            let parsed = parse(path, source).unwrap();
            assert_eq!(symbols(&parsed), vec![("good", "function")], "{path}");
            assert!(
                parsed.errors.iter().any(|e| e.message.contains("broken")),
                "{path}: {:?}",
                parsed.errors
            );
        }
        for (path, source) in [
            ("bad.go", "package main\nfunc good() {}\n???"),
            ("bad.java", "class Good { void good() {} }\n???"),
            ("bad.c", "int good(void) { return 1; }\n???"),
        ] {
            let parsed = parse(path, source).unwrap();
            assert_eq!(parsed.callables.len(), 1, "{path}");
            assert_eq!(parsed.callables[0].name, "good");
            assert!(!parsed.errors.is_empty(), "{path}");
        }
    }

    #[test]
    fn modern_typescript_grammar_recovery_preserves_source() {
        for source in [
            "export type * from './types.js';\nexport type * as Models from './models.js';\nfunction good() {}",
            "async function good(original: <T>() => Promise<T>) { return await original<typeof import('./application.js')>(); }\ntype Input = import('ai').InferToolInput<typeof good>;",
            "const query = sql<{ id: number }>`SELECT id FROM records`;\nfunction good() { return query; }",
        ] {
            let parsed = clean("modern.ts", source);
            assert_eq!(symbols(&parsed), vec![("good", "function")]);
            assert!(source.contains(&parsed.callables[0].source));
        }
        let parsed = parse(
            "broken.ts",
            "export type * from './types'; function broken(value: ) {} function good() {}",
        )
        .unwrap();
        assert!(!parsed.errors.is_empty());
        assert_eq!(symbols(&parsed), vec![("good", "function")]);
    }

    #[test]
    fn byte_columns_unicode_crlf_large_files_and_known_hash() {
        let parsed = clean(
            "unicode.js",
            "// café 🚀\r\nconst café = '🚀'; function load() { return café; }\r\n",
        );
        let load = &parsed.callables[0];
        assert_eq!(load.start_line, 2);
        assert_eq!(load.start_column, "const café = '🚀'; ".len() + 1);
        let large = format!(
            "{}\nfunction afterPadding() {{}}",
            "// padding\n".repeat(4_000)
        );
        assert_eq!(clean("large.ts", &large).callables[0].name, "afterPadding");
        assert_eq!(
            hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn markdown_heading_ancestry_preambles_and_empty_sections() {
        let parsed = clean(
            "guide.MD",
            "Preamble\r\n\r\n# Guide\r\n### Details\r\n\r\nBody\r\n\r\n## Next ##\r\n\r\nMore\r\n## Empty\r\n",
        );
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
}
