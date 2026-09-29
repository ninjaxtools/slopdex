//! Tree-sitter callable extraction and recoverable syntax diagnostics.

use super::{CallSite, Callable, Diagnostic, ParsedFile};
use crate::hash;
use anyhow::{Context, Result};
use std::collections::HashSet;
use tree_sitter::{Language, Node, Parser, Tree};

pub(super) fn parse(language: &'static str, path: &str, source: &str) -> Result<ParsedFile> {
    let grammar: Language = match language {
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "c" => tree_sitter_c::LANGUAGE.into(),
        "bash" => tree_sitter_bash::LANGUAGE.into(),
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
        structure: super::structure::extract(language, tree.root_node(), source),
        ..ParsedFile::default()
    };
    collect_calls(tree.root_node(), source, language, &mut result.structure);
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

fn collect_calls(
    node: Node<'_>,
    source: &str,
    language: &str,
    structure: &mut super::FileStructure,
) {
    let callee = match node.kind() {
        "call_expression" | "call" => node.child_by_field_name("function"),
        "method_call_expression" => node.child_by_field_name("method"),
        "method_invocation" => node.child_by_field_name("name"),
        "new_expression" => node
            .child_by_field_name("constructor")
            .or_else(|| node.named_child(0)),
        "object_creation_expression" => node.child_by_field_name("type"),
        "command" if language == "bash" => node.named_child(0),
        _ => None,
    };
    if let Some(callee) = callee {
        let name = text(source, callee).trim_start_matches('#');
        let (receiver, name) =
            if matches!(node.kind(), "method_call_expression" | "method_invocation") {
                (
                    node.child_by_field_name("receiver")
                        .or_else(|| node.child_by_field_name("object"))
                        .map(|n| text(source, n)),
                    name,
                )
            } else if let Some((receiver, name)) = name.rsplit_once("::") {
                (Some(receiver), name)
            } else if let Some((receiver, name)) = name.rsplit_once('.') {
                (Some(receiver), name)
            } else {
                (None, name)
            };
        let (receiver, name) = if node.kind() == "new_expression" {
            (Some(name), "constructor")
        } else if node.kind() == "object_creation_expression" {
            (Some(name), name)
        } else {
            (receiver, name)
        };
        let identifier = |word: &str| {
            !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | '#'))
        };
        if identifier(name)
            && receiver.is_none_or(|r| identifier(r) || r.split("::").all(identifier))
        {
            let owner = structure
                .nodes
                .iter_mut()
                .filter(|n| {
                    matches!(
                        n.kind.as_str(),
                        "function" | "method" | "constructor" | "generator"
                    ) && n.start_byte <= node.start_byte()
                        && node.end_byte() <= n.end_byte
                })
                .min_by_key(|n| n.end_byte - n.start_byte);
            if let Some(owner) = owner {
                let site = CallSite {
                    name: name.to_owned(),
                    receiver: receiver.map(str::to_owned),
                };
                if !owner.calls.contains(&site) {
                    owner.calls.push(site);
                }
            }
        }
    }
    for child in children(node) {
        collect_calls(child, source, language, structure);
    }
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
            ("bash", "function_definition") => {
                if let Some(name) = field(source, node, "name") {
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
    let value = unwrap(statement.named_child(0)?);
    let literals = match value.kind() {
        "string" => vec![value],
        "concatenated_string" => children(value),
        _ => return None,
    };
    for literal in literals {
        if literal.kind() == "comment" {
            continue;
        }
        // Tree-sitter also labels bytes and formatted literals as strings.
        // Only constant text (optionally raw or legacy Unicode) is a docstring.
        let prefix = text(source, literal).split(['\'', '"']).next()?;
        if literal.kind() != "string" || !prefix.chars().all(|c| matches!(c, 'r' | 'R' | 'u' | 'U'))
        {
            return None;
        }
    }
    Some(text(source, value))
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

#[cfg(test)]
mod tests {
    use super::super::{language_for_path, parse};
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
    fn shell_functions_are_searchable_and_structured() {
        for path in ["script.sh", "script.bash", "script.zsh"] {
            let source =
                "#!/bin/bash\nload() {\n  echo ready\n  nested() { :; }\n}\nfunction save { :; }\n";
            let parsed = clean(path, source);
            assert_eq!(
                symbols(&parsed),
                [
                    ("load", "function"),
                    ("load.nested", "function"),
                    ("save", "function")
                ]
            );
            assert_eq!(
                parsed
                    .structure
                    .nodes
                    .iter()
                    .map(|node| node.qualified_name.as_str())
                    .collect::<Vec<_>>(),
                ["load", "load.nested", "save"]
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
    fn javascript_async_generators_accessors_and_private_methods() {
        let parsed = clean(
            "stream.js",
            "async function* stream(limit) { yield limit; }\n\
             const bound = async function* internal() { yield 1; };\n\
             class Store {\n\
               get value() { return 1; }\n\
               set value(next) { this.current = next; }\n\
               async *entries() { yield this.value; }\n\
               #reset() {}\n\
               #source = function* () { yield 2; };\n\
             }",
        );
        assert_eq!(
            symbols(&parsed),
            [
                ("stream", "generator"),
                ("bound", "generator"),
                ("Store.value", "method"),
                ("Store.value", "method"),
                ("Store.entries", "generator"),
                ("Store.reset", "method"),
                ("Store.source", "generator"),
            ]
        );
        for (callable, signature) in parsed.callables.iter().zip([
            "async *stream(limit)",
            "async *bound()",
            "value()",
            "value(next)",
            "async *entries()",
            "reset()",
            "*source()",
        ]) {
            assert_eq!(callable.signature.as_deref(), Some(signature));
        }
        assert!(parsed.callables[5].source.starts_with("#reset()"));
        assert_ne!(
            parsed.callables[2].source_hash,
            parsed.callables[3].source_hash
        );
    }

    #[test]
    fn typescript_overload_signatures_do_not_duplicate_implementations() {
        let parsed = clean(
            "overloads.ts",
            "function convert(value: string): string;\n\
             function convert(value: number): number;\n\
             function convert(value: string | number) { return value; }\n\
             abstract class Store {\n\
               abstract absent(): void;\n\
               load(value: string): string;\n\
               load(value: number): number;\n\
               load(value: string | number) { return value; }\n\
             }\n\
             declare namespace External { function absent(): void; }",
        );
        assert_eq!(
            symbols(&parsed),
            [("convert", "function"), ("Store.load", "method")]
        );
        assert_eq!(parsed.callables[0].start_line, 3);
        assert_eq!(parsed.callables[1].start_line, 8);
        assert_eq!(
            parsed.callables[1].signature.as_deref(),
            Some("load(value: string | number)")
        );
    }

    #[test]
    fn native_bound_closures_keep_names_and_ignore_unbound_callbacks() {
        for (path, source, expected) in [
            (
                "bindings.py",
                "def outer():\n    (first := (lambda value: value))\n    obj.second = lambda: 2\n    invoke(lambda: 3)\n    first_tuple, second_tuple = (lambda: 4), (lambda: 5)\n",
                vec![
                    ("outer", "function"),
                    ("outer.first", "function"),
                    ("outer.obj.second", "function"),
                ],
            ),
            (
                "bindings.rs",
                "fn outer() { let first = (move |value: i32| value); let mut second = || 2; invoke(|| 3); let (ignored,) = (|| 4,); }",
                vec![
                    ("outer", "function"),
                    ("outer.first", "function"),
                    ("outer.second", "function"),
                ],
            ),
            (
                "bindings.go",
                "package bindings\nfunc outer() {\nvar first, second = func() int { return 1 }, func() int { return 2 }\nobj.run = func() {}\ninvoke(func() {})\n}\n",
                vec![
                    ("outer", "function"),
                    ("outer.first", "function"),
                    ("outer.second", "function"),
                    ("outer.obj.run", "function"),
                ],
            ),
            (
                "Bindings.java",
                "class Bindings { void outer() { Runnable first = (() -> {}), second = () -> {}; invoke(() -> {}); } }",
                vec![
                    ("Bindings.outer", "method"),
                    ("Bindings.outer.first", "function"),
                    ("Bindings.outer.second", "function"),
                ],
            ),
        ] {
            let parsed = clean(path, source);
            assert_eq!(symbols(&parsed), expected, "{path}");
            for callable in &parsed.callables[1..] {
                assert!(
                    callable
                        .signature
                        .as_deref()
                        .unwrap()
                        .starts_with(&format!("{} = ", callable.name)),
                    "{path}: {callable:?}"
                );
            }
        }
    }

    #[test]
    fn python_documentation_requires_a_leading_constant_string() {
        // Python's lexical reference explicitly excludes f-strings, even without
        // replacement fields; bytes literals likewise are not string docstrings.
        for (statement, documentation) in [
            ("\"plain café\"", Some("\"plain café\"")),
            ("# comment\n    r\"raw\\text\"", Some("r\"raw\\text\"")),
            ("u\"joined \" \"text\"", Some("u\"joined \" \"text\"")),
            ("(\"parenthesized\")", Some("\"parenthesized\"")),
            ("(U\"joined \" R\"text\")", Some("U\"joined \" R\"text\"")),
            ("f\"not documentation\"", None),
            ("f\"computed {1 + 1}\"", None),
            ("RF\"not {1 + 1}\"", None),
            ("\"plain \" f\"formatted\"", None),
            ("b\"bytes\"", None),
            ("BR\"bytes\"", None),
            ("pass\n    \"too late\"", None),
        ] {
            let source = format!("def describe():\n    {statement}\n    return 1\n");
            let parsed = clean("documentation.py", &source);
            assert_eq!(symbols(&parsed), [("describe", "function")]);
            let embedding = &parsed.callables[0].embedding_input;
            if let Some(documentation) = documentation {
                assert!(
                    embedding.contains(&format!("\ndocumentation:\n{documentation}\nsource:\n")),
                    "{statement}: {embedding}"
                );
            } else {
                assert!(
                    !embedding.contains("\ndocumentation:\n"),
                    "{statement}: {embedding}"
                );
            }
            assert!(embedding.ends_with(&parsed.callables[0].source));
        }
    }

    #[test]
    fn callable_hashes_track_source_not_path_position_or_enclosing_scope() {
        let source = "function café() {\r\n  return '🚀';\r\n}";
        let original = clean("original.js", source).callables.remove(0);
        let relocated = clean(
            "other/moved.js",
            &format!("// moved\r\n\r\n{source}\r\n// trailing"),
        )
        .callables
        .remove(0);
        assert_eq!(original.source, source);
        assert_eq!(
            (
                original.start_line,
                original.start_column,
                original.end_line,
                original.end_column
            ),
            (1, 1, 3, 2)
        );
        assert_eq!((relocated.start_line, relocated.end_line), (3, 5));
        assert_eq!(original.source_hash, relocated.source_hash);
        assert_eq!(original.embedding_input, relocated.embedding_input);

        let nested = clean("scoped.ts", &format!("namespace Example {{ {source} }}"))
            .callables
            .remove(0);
        assert_eq!(nested.qualified_name, "Example.café");
        assert_eq!(nested.source, original.source);
        assert_eq!(nested.source_hash, original.source_hash);
        assert_ne!(nested.embedding_input, original.embedding_input);
        assert!(nested.embedding_input.contains("symbol: Example.café\n"));

        for changed in [source.replace("🚀", "🌍"), source.replace("\r\n", "\n")] {
            let changed = clean("original.js", &changed).callables.remove(0);
            assert_ne!(changed.source_hash, original.source_hash);
        }
    }

    #[test]
    fn typescript_recovery_preserves_unicode_multiline_imports_and_literal_text() {
        let source = "// café 🚀\r\nexport type * from './模型';\r\nasync function café(original: <T>() => Promise<T>) {\r\n  const literal = \"export type *; import('do not mask')\";\r\n  return await original<typeof import(\r\n    './模型'\r\n  )>();\r\n}\r\nfunction after() {}";
        for path in ["recovery.ts", "recovery.tsx"] {
            let parsed = clean(path, source);
            assert_eq!(
                symbols(&parsed),
                [("café", "function"), ("after", "function")]
            );
            let recovered = &parsed.callables[0];
            let expected = source
                .split_once("async function")
                .unwrap()
                .1
                .split_once("\r\nfunction after")
                .unwrap()
                .0;
            assert_eq!(recovered.source, format!("async function{expected}"));
            assert_eq!(
                (
                    recovered.start_line,
                    recovered.end_line,
                    recovered.end_column
                ),
                (3, 8, 2)
            );
            assert!(recovered.source.contains("import(\r\n    './模型'\r\n  )"));
            assert!(recovered.embedding_input.ends_with(&recovered.source));
            assert_eq!(
                (
                    parsed.callables[1].start_line,
                    parsed.callables[1].start_column
                ),
                (9, 1)
            );
        }
    }

    #[test]
    fn malformed_outer_callable_keeps_healthy_nested_and_following_functions() {
        let source = "function outer() {\n  function healthy() { return 'é'; }\n  const broken = ;\n}\nfunction after() {}";
        let parsed = parse("nested.js", source).unwrap();
        assert_eq!(
            symbols(&parsed),
            [("outer.healthy", "function"), ("after", "function")]
        );
        assert_eq!(
            parsed.callables[0].source,
            "function healthy() { return 'é'; }"
        );
        assert_eq!(
            (
                parsed.callables[0].start_line,
                parsed.callables[0].start_column
            ),
            (2, 3)
        );
        assert_eq!(
            parsed.callables[0].source_hash,
            hash(&parsed.callables[0].source)
        );
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("callable outer;"));
        assert_eq!(
            (parsed.errors[0].start_line, parsed.errors[0].end_line),
            (1, 4)
        );
    }

    #[test]
    fn missing_tokens_outside_callables_have_local_diagnostics() {
        for path in ["missing.js", "missing.ts"] {
            let parsed = parse(
                path,
                "function good() {}\nconst value = (1 + 2;\nfunction after() {}",
            )
            .unwrap();
            assert_eq!(
                symbols(&parsed),
                [("good", "function"), ("after", "function")]
            );
            assert_eq!(parsed.errors.len(), 1, "{path}: {:?}", parsed.errors);
            assert!(
                parsed.errors[0].message.contains("expected )"),
                "{path}: {:?}",
                parsed.errors
            );
            assert_eq!(
                (parsed.errors[0].start_line, parsed.errors[0].end_line),
                (2, 2)
            );
            assert_eq!(parsed.callables[1].start_line, 3);
        }
    }
}
