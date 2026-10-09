//! Canonical, lossless-in-size declaration metadata. Extraction never truncates.
//!
//! IDs are zero-based and file-local. Parents precede children. Byte offsets are
//! zero-based, half-open; lines and UTF-8 byte columns are one-based, with an
//! exclusive end (including an end at column 1 of the following line).

use super::syntax::{children, text, unwrap_value};
use serde::{Deserialize, Serialize};
use tree_sitter::Node;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileStructure {
    pub nodes: Vec<StructureNode>,
}

/// A syntactic call made within a callable. Resolution is deferred until all
/// indexed files are available; dynamic receivers are deliberately omitted.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CallSite {
    pub name: String,
    pub receiver: Option<String>,
}

/// One imported binding; `path` is the fully expanded source path when known.
/// `name` is the imported name, `alias` the local binding, and `source` the
/// module specifier (without quotes). Side-effect imports have no name/alias.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImportBinding {
    pub path: String,
    pub source: Option<String>,
    pub name: Option<String>,
    pub alias: Option<String>,
    pub wildcard: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StructureNode {
    pub id: usize,
    pub parent_id: Option<usize>,
    pub language: String,
    /// Stable semantic kind, e.g. function, method, class, struct, interface,
    /// enum, variant, field, type, trait, impl, module, constant, variable,
    /// import, macro, constructor, heading.
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    /// Source-provided prose attached to this declaration or heading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// All declared/imported names, including aliases, for symbol filtering.
    pub names: Vec<String>,
    /// Complete declaration signature, without executable bodies/initializers.
    pub signature: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub attributes: Vec<String>,
    pub imports: Vec<ImportBinding>,
    pub heading_level: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<CallSite>,
}

pub(super) fn extract(language: &str, root: Node<'_>, source: &str) -> FileStructure {
    let mut collector = Collector {
        language,
        source,
        nodes: Vec::new(),
    };
    collector.walk(root, None, false);
    FileStructure {
        nodes: collector.nodes,
    }
}

fn compact(value: &str, language: &str) -> String {
    // Normalize layout without changing literal types such as "two  words".
    let mut result = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut raw_hashes = None;
    let mut space = false;
    for (offset, c) in value.char_indices() {
        if let Some(delimiter) = quote {
            result.push(c);
            if let Some(hashes) = raw_hashes {
                if c == '"'
                    && value[offset + 1..]
                        .bytes()
                        .take(hashes)
                        .filter(|b| *b == b'#')
                        .count()
                        == hashes
                {
                    quote = None;
                    raw_hashes = None;
                }
            } else if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == delimiter {
                quote = None;
            }
        } else if c.is_whitespace() {
            space = !result.is_empty();
        } else {
            if space {
                result.push(' ');
                space = false;
            }
            result.push(c);
            let rest = &value[offset + c.len_utf8()..];
            let lifetime = language == "rust"
                && c == '\''
                && rest.starts_with(|c: char| c.is_alphabetic() || c == '_')
                && !rest
                    .trim_start_matches(|c: char| c.is_alphanumeric() || c == '_')
                    .starts_with('\'');
            if matches!(c, '\'' | '"' | '`') && !lifetime {
                quote = Some(c);
                if language == "rust" && c == '"' {
                    let prefix = &value[..offset];
                    let hashes = prefix.bytes().rev().take_while(|b| *b == b'#').count();
                    if prefix[..prefix.len() - hashes].ends_with('r') {
                        raw_hashes = Some(hashes);
                    }
                }
            }
        }
    }
    result.trim_end().to_owned()
}

fn unquote(value: &str) -> String {
    value.trim_matches(['\'', '"', '`', '<', '>']).to_owned()
}

fn callable(kind: &str) -> bool {
    matches!(kind, "function" | "method" | "constructor" | "generator")
}

fn container(kind: &str) -> bool {
    matches!(
        kind,
        "class"
            | "struct"
            | "union"
            | "enum"
            | "interface"
            | "trait"
            | "impl"
            | "module"
            | "type"
            | "variant"
    )
}

fn value(node: Node<'_>) -> Option<Node<'_>> {
    let value = node
        .child_by_field_name("value")
        .or_else(|| node.child_by_field_name("right"))?;
    Some(unwrap_value(value))
}

fn function_value(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "arrow_function"
            | "function_expression"
            | "generator_function"
            | "lambda"
            | "closure_expression"
            | "func_literal"
            | "lambda_expression"
    )
}

fn js_declaration_value(node: Node<'_>) -> bool {
    function_value(node)
        || node.kind() == "class"
        || node.kind() == "object" && contains_js_declaration(node)
}

fn contains_js_declaration(node: Node<'_>) -> bool {
    // Anonymous callbacks that compute property values do not turn a data
    // object into a declaration. Keep bindings that scope real declarations.
    matches!(
        node.kind(),
        "class"
            | "class_declaration"
            | "abstract_class_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration"
            | "internal_module"
            | "module"
            | "function_declaration"
            | "generator_function_declaration"
            | "method_definition"
    ) || matches!(node.kind(), "function_expression" | "generator_function")
        && node.child_by_field_name("name").is_some()
        || matches!(
            node.kind(),
            "variable_declarator"
                | "assignment_expression"
                | "pair"
                | "public_field_definition"
                | "property_definition"
                | "field_definition"
        ) && value(node).is_some_and(|v| function_value(v) || v.kind() == "class")
        || children(node).into_iter().any(contains_js_declaration)
}

struct Collector<'a> {
    language: &'a str,
    source: &'a str,
    nodes: Vec<StructureNode>,
}

impl Collector<'_> {
    fn field(&self, node: Node<'_>, name: &str) -> Option<String> {
        node.child_by_field_name(name)
            .map(|n| compact(text(self.source, n), self.language))
    }

    fn push(
        &mut self,
        node: Node<'_>,
        parent: Option<usize>,
        kind: &str,
        names: Vec<String>,
        signature: String,
    ) -> usize {
        let id = self.nodes.len();
        let name = names.first().cloned().unwrap_or_default();
        let mut qualified_name = parent.map_or_else(
            || name.clone(),
            |p| {
                let prefix = &self.nodes[p].qualified_name;
                if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                }
            },
        );
        if self.language == "go"
            && node.kind() == "method_declaration"
            && let Some(mut receiver) = node
                .child_by_field_name("receiver")
                .and_then(|n| n.named_child(0))
                .and_then(|n| n.child_by_field_name("type"))
        {
            while receiver.kind() == "pointer_type" {
                let Some(inner) = receiver.named_child(0) else {
                    break;
                };
                receiver = inner;
            }
            qualified_name = format!("{}.{}", text(self.source, receiver), name);
        }
        let attributes = self.attributes(node);
        let mut range = if self.language == "bash" {
            super::syntax::shell_range(node)
        } else {
            node
        };
        if let Some(wrapper) = node.parent()
            && matches!(
                wrapper.kind(),
                "decorated_definition" | "export_statement" | "ambient_declaration"
            )
        {
            range = wrapper;
        }
        let mut start_byte = range.start_byte();
        let mut start = range.start_position();
        let mut previous = node.prev_named_sibling();
        while let Some(attr) = previous {
            if self.language == "rust" && matches!(attr.kind(), "line_comment" | "block_comment") {
                previous = attr.prev_named_sibling();
                continue;
            }
            if !matches!(attr.kind(), "attribute_item" | "inner_attribute_item") {
                break;
            }
            start_byte = attr.start_byte();
            start = attr.start_position();
            previous = attr.prev_named_sibling();
        }
        self.nodes.push(StructureNode {
            id,
            parent_id: parent,
            language: self.language.to_owned(),
            kind: kind.to_owned(),
            name,
            qualified_name,
            description: None,
            names,
            signature,
            start_byte,
            end_byte: range.end_byte(),
            start_line: start.row + 1,
            start_column: start.column + 1,
            end_line: range.end_position().row + 1,
            end_column: range.end_position().column + 1,
            attributes,
            imports: Vec::new(),
            heading_level: None,
            calls: Vec::new(),
        });
        id
    }

    fn attributes(&self, node: Node<'_>) -> Vec<String> {
        let mut attrs = Vec::new();
        let mut prev = node.prev_named_sibling();
        while let Some(n) = prev {
            if self.language == "rust" && matches!(n.kind(), "line_comment" | "block_comment") {
                prev = n.prev_named_sibling();
                continue;
            }
            if !matches!(n.kind(), "attribute_item" | "inner_attribute_item") {
                break;
            }
            attrs.push(text(self.source, n).to_owned());
            prev = n.prev_named_sibling();
        }
        attrs.reverse();
        let mut candidates = children(node);
        if let Some(mut p) = node.parent() {
            if matches!(p.kind(), "lexical_declaration" | "variable_declaration")
                && let Some(outer) = p.parent()
            {
                p = outer;
            }
            if p.kind() == "decorated_definition" {
                candidates.extend(children(p));
            }
            if matches!(p.kind(), "export_statement" | "ambient_declaration") {
                for token in ["export", "default", "declare"] {
                    let prefix = &self.source[p.start_byte()..node.start_byte()];
                    if prefix.split_whitespace().any(|s| s == token) {
                        attrs.push(token.to_owned());
                    }
                }
            }
        }
        for child in candidates {
            if matches!(
                child.kind(),
                "decorator" | "attribute_item" | "annotation" | "marker_annotation"
            ) {
                attrs.push(text(self.source, child).to_owned());
            } else if child.kind() == "modifiers" {
                for annotation in children(child) {
                    if matches!(annotation.kind(), "annotation" | "marker_annotation") {
                        attrs.push(text(self.source, annotation).to_owned());
                    }
                }
            }
        }
        attrs
    }

    fn walk(&mut self, node: Node<'_>, parent: Option<usize>, in_callable: bool) {
        let syntax = node.kind();
        // Inline object members belong to their type expression, not the
        // nearest named scope. Only direct object aliases expose child symbols.
        if matches!(self.language, "typescript" | "tsx")
            && syntax == "object_type"
            && node
                .parent()
                .is_none_or(|parent| parent.kind() != "type_alias_declaration")
        {
            return;
        }
        if matches!(
            syntax,
            "comment"
                | "line_comment"
                | "block_comment"
                | "attribute_item"
                | "inner_attribute_item"
                | "decorator"
                | "annotation"
                | "marker_annotation"
        ) {
            return;
        }
        if self.is_import(syntax) {
            self.import(node, parent);
            return;
        }
        if syntax == "export_statement" && node.child_by_field_name("source").is_some() {
            self.import(node, parent);
            return;
        }
        if self.language == "bash"
            && syntax == "redirected_statement"
            && let Some(body) = node.child_by_field_name("body")
            && body.kind() == "function_definition"
        {
            self.walk(body, parent, in_callable);
            for sibling in super::syntax::shell_siblings(body) {
                self.walk(sibling, parent, in_callable);
            }
            return;
        }
        if self.language == "python"
            && self.python_assignment_declarations(node, parent, in_callable)
        {
            return;
        }
        if self.language == "c"
            && syntax == "function_definition"
            && let Some(ty) = node.child_by_field_name("type")
        {
            self.walk(ty, parent, in_callable);
        }
        if self.language == "go"
            && matches!(
                syntax,
                "var_spec" | "short_var_declaration" | "assignment_statement"
            )
            && self.go_bound_declarations(node, parent, in_callable)
        {
            return;
        }
        if self.language == "java"
            && syntax == "class_body"
            && let Some(expression) = node
                .parent()
                .filter(|p| p.kind() == "object_creation_expression")
        {
            let name = format!(
                "<anonymous@{}:{}>",
                node.start_position().row + 1,
                node.start_position().column + 1
            );
            let ty = self.field(expression, "type").unwrap_or_default();
            let id = self.push(
                node,
                parent,
                "class",
                vec![name.clone()],
                format!("class {name}: {ty}"),
            );
            for child in children(node) {
                self.walk(child, Some(id), false);
            }
            return;
        }
        if self.language == "rust" && syntax == "ordered_field_declaration_list" {
            let mut index = 0;
            let mut visibility = None;
            let mut attrs = Vec::new();
            let mut cursor = node.walk();
            let types: Vec<_> = node.children_by_field_name("type", &mut cursor).collect();
            for child in children(node) {
                if child.kind() == "visibility_modifier" {
                    visibility = Some(child);
                } else if child.kind() == "attribute_item" {
                    attrs.push(text(self.source, child).to_owned());
                } else if types.contains(&child) {
                    let prefix = visibility.map_or("", |n| text(self.source, n));
                    let signature = format!("{prefix} {index}: {}", text(self.source, child))
                        .trim()
                        .to_owned();
                    let id = self.push(child, parent, "field", vec![index.to_string()], signature);
                    self.nodes[id].attributes = std::mem::take(&mut attrs);
                    if let Some(start) = visibility.take() {
                        self.nodes[id].start_byte = start.start_byte();
                        self.nodes[id].start_line = start.start_position().row + 1;
                        self.nodes[id].start_column = start.start_position().column + 1;
                    }
                    index += 1;
                }
            }
            return;
        }
        // C declarations and Java fields can declare multiple independent names.
        if (self.language == "c"
            && matches!(
                syntax,
                "declaration" | "type_definition" | "field_declaration"
            ))
            || (self.language == "java"
                && matches!(syntax, "field_declaration" | "constant_declaration"))
        {
            self.multi_declaration(node, parent, in_callable);
            return;
        }
        if self.language == "go" && syntax == "field_declaration" {
            let names = self.field_names(node, "name");
            let names = if names.is_empty() {
                self.field(node, "type").into_iter().collect()
            } else {
                names
            };
            for name in names {
                let id = self.push(
                    node,
                    parent,
                    "field",
                    vec![name],
                    self.signature(node, "field"),
                );
                if let Some(ty) = node.child_by_field_name("type") {
                    for child in children(ty) {
                        self.walk(child, Some(id), false);
                    }
                }
            }
            return;
        }
        let classification = self.classify(node, parent, in_callable);
        if let Some((kind, names)) = classification {
            let id = self.push(node, parent, kind, names, self.signature(node, kind));
            // A bound closure/class/object is represented by its binding, not by
            // a second anonymous declaration with the same identity.
            if matches!(
                syntax,
                "variable_declarator"
                    | "assignment"
                    | "pair"
                    | "const_spec"
                    | "var_spec"
                    | "let_declaration"
                    | "short_var_declaration"
                    | "assignment_expression"
                    | "named_expression"
                    | "public_field_definition"
                    | "property_definition"
                    | "field_definition"
            ) {
                if let Some(value) = value(node) {
                    if callable(kind) || container(kind) || value.kind() == "object" {
                        self.walk_bound(value, id, kind, in_callable);
                    } else {
                        self.walk(value, parent, in_callable);
                    }
                }
                return;
            }
            let local = if callable(kind) {
                true
            } else if container(kind) {
                false
            } else {
                in_callable
            };
            for child in children(node) {
                // Names/types/parameter expressions cannot introduce declarations
                // belonging to this scope; only bodies and declaration lists do.
                if node.child_by_field_name("name") == Some(child) {
                    continue;
                }
                if self.language == "bash" && child.kind().ends_with("redirect") {
                    continue;
                }
                if self.language == "c"
                    && syntax == "function_definition"
                    && node.child_by_field_name("type") == Some(child)
                {
                    continue;
                }
                if self.language == "java"
                    && syntax == "record_declaration"
                    && child.kind() == "formal_parameters"
                {
                    for parameter in children(child) {
                        if let Some(name) = self.field(parameter, "name") {
                            self.push(
                                parameter,
                                Some(id),
                                "field",
                                vec![name],
                                self.signature(parameter, "field"),
                            );
                        }
                    }
                    continue;
                }
                if matches!(
                    child.kind(),
                    "parameters"
                        | "formal_parameters"
                        | "parameter_list"
                        | "type_parameters"
                        | "type_parameter_list"
                        | "modifiers"
                ) {
                    continue;
                }
                let child_parent = if callable(kind) || container(kind) {
                    Some(id)
                } else {
                    parent
                };
                self.walk(child, child_parent, local);
            }
            if self.language == "bash" && syntax == "function_definition" {
                for redirect in super::syntax::shell_redirections(node) {
                    self.walk(redirect, Some(id), true);
                }
            }
        } else {
            if function_value(node) {
                // Anonymous closures introduce executable scope without a symbol.
                if matches!(self.language, "javascript" | "jsx" | "typescript" | "tsx") {
                    for child in children(node) {
                        self.walk(child, parent, true);
                    }
                } else if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body, parent, true);
                }
                return;
            }
            for child in children(node) {
                self.walk(child, parent, in_callable);
            }
        }
    }

    fn walk_bound(&mut self, value: Node<'_>, id: usize, kind: &str, in_callable: bool) {
        if callable(kind) {
            if let Some(body) = value.child_by_field_name("body") {
                self.walk(body, Some(id), true);
            }
        } else {
            for child in children(value) {
                if value.child_by_field_name("name") != Some(child) {
                    // Object bindings preserve the surrounding executable scope;
                    // only actual declaration containers (such as classes) reset it.
                    self.walk(child, Some(id), in_callable && !container(kind));
                }
            }
        }
    }

    fn go_bound_declarations(
        &mut self,
        node: Node<'_>,
        parent: Option<usize>,
        local: bool,
    ) -> bool {
        let targets: Vec<_> = if node.kind() == "var_spec" {
            let mut cursor = node.walk();
            node.children_by_field_name("name", &mut cursor).collect()
        } else {
            node.child_by_field_name("left")
                .map(super::syntax::children)
                .unwrap_or_default()
        };
        let values = node
            .child_by_field_name("value")
            .or_else(|| node.child_by_field_name("right"))
            .map(super::syntax::children)
            .unwrap_or_default();
        if targets.len() != values.len()
            || !values
                .iter()
                .any(|n| super::syntax::unwrap_value(*n).kind() == "func_literal")
        {
            return false;
        }
        for (target, value) in targets.into_iter().zip(values) {
            let value = super::syntax::unwrap_value(value);
            if value.kind() == "func_literal"
                && let Some(target) = go_callable_target(self.source, target)
            {
                let name = text(self.source, target).to_owned();
                let signature = format!("{name} = {}", self.signature(value, "function"));
                let id = self.push(value, parent, "function", vec![name], signature);
                self.walk_bound(value, id, "function", local);
            } else {
                if !local
                    && node.kind() == "var_spec"
                    && target.kind() == "identifier"
                    && text(self.source, target) != "_"
                {
                    let name = text(self.source, target).to_owned();
                    let ty = self.field(node, "type").unwrap_or_default();
                    self.push(
                        node,
                        parent,
                        "variable",
                        vec![name.clone()],
                        format!("var {name} {ty}").trim_end().to_owned(),
                    );
                }
                self.walk(value, parent, local);
            }
        }
        true
    }

    fn python_assignment_declarations(
        &mut self,
        node: Node<'_>,
        parent: Option<usize>,
        local: bool,
    ) -> bool {
        if node.kind() != "assignment"
            || node
                .child_by_field_name("right")
                .is_none_or(|right| right.kind() != "assignment")
        {
            return false;
        }
        let mut assignments = Vec::new();
        let mut assignment = node;
        let initializer = loop {
            assignments.push(assignment);
            let Some(right) = assignment.child_by_field_name("right") else {
                return false;
            };
            let right = super::syntax::unwrap_value(right);
            if right.kind() != "assignment" {
                break right;
            }
            assignment = right;
        };
        let mut bound = false;
        for assignment in assignments {
            let Some(target) = assignment.child_by_field_name("left") else {
                continue;
            };
            if initializer.kind() == "lambda" && matches!(target.kind(), "identifier" | "attribute")
            {
                let name = text(self.source, target).to_owned();
                let signature = format!("{name} = {}", self.signature(initializer, "function"));
                let id = self.push(assignment, parent, "function", vec![name], signature);
                self.walk_bound(initializer, id, "function", local);
                bound = true;
            } else if let Some((kind, names)) = self.classify(assignment, parent, local) {
                self.push(
                    assignment,
                    parent,
                    kind,
                    names,
                    self.signature(assignment, kind),
                );
            }
            self.walk(target, parent, local);
        }
        if !bound {
            self.walk(initializer, parent, local);
        }
        true
    }

    fn field_names(&self, node: Node<'_>, field: &str) -> Vec<String> {
        let mut cursor = node.walk();
        node.children_by_field_name(field, &mut cursor)
            .map(|n| compact(text(self.source, n), self.language))
            .collect()
    }

    fn classify(
        &self,
        node: Node<'_>,
        parent: Option<usize>,
        local: bool,
    ) -> Option<(&'static str, Vec<String>)> {
        let syntax = node.kind();
        let mut name = self
            .field(node, "name")
            .or_else(|| self.field(node, "property"));
        if matches!(self.language, "javascript" | "jsx" | "typescript" | "tsx") {
            name = node
                .child_by_field_name("name")
                .or_else(|| node.child_by_field_name("property"))
                .map(|key| compact(&super::syntax::js_key(self.source, key), self.language));
        }
        let member = parent.is_some_and(|p| {
            matches!(
                self.nodes[p].kind.as_str(),
                "class" | "struct" | "interface" | "trait" | "impl" | "enum"
            )
        });
        let mut kind = match self.language {
            "javascript" | "jsx" | "typescript" | "tsx" => match syntax {
                "class_declaration" | "abstract_class_declaration" | "class" => "class",
                "interface_declaration" => "interface",
                "type_alias_declaration" => "type",
                "enum_declaration" => "enum",
                "enum_assignment" => "variant",
                "internal_module" | "module" => {
                    name = name.map(|n| unquote(&n));
                    "module"
                }
                "function_declaration"
                | "function_signature"
                | "generator_function_declaration" => "function",
                "function_expression" | "generator_function" if name.is_some() => "function",
                "method_definition" | "method_signature" | "abstract_method_signature" => "method",
                "call_signature" => {
                    name = Some("call".into());
                    "method"
                }
                "construct_signature" => {
                    name = Some("new".into());
                    "constructor"
                }
                "index_signature" => {
                    name = Some("index".into());
                    "field"
                }
                "public_field_definition"
                | "property_definition"
                | "field_definition"
                | "property_signature" => "field",
                "variable_declarator"
                    if !local || value(node).is_some_and(js_declaration_value) =>
                {
                    "variable"
                }
                "assignment_expression"
                    if value(node).is_some_and(|v| {
                        js_declaration_value(v) || !local && v.kind() == "object"
                    }) =>
                {
                    let left = node.child_by_field_name("left")?;
                    name = match left.kind() {
                        "identifier" => Some(text(self.source, left).to_owned()),
                        "member_expression" => self
                            .field(left, "property")
                            .map(|n| n.trim_start_matches('#').to_owned()),
                        _ => None,
                    };
                    "variable"
                }
                "pair"
                    if !local && parent.is_some()
                        || value(node).is_some_and(js_declaration_value) =>
                {
                    name = node.child_by_field_name("key").map(|key| {
                        compact(&super::syntax::js_key(self.source, key), self.language)
                    });
                    "field"
                }
                "shorthand_property_identifier" if !local && parent.is_some() => {
                    name = Some(text(self.source, node).to_owned());
                    "field"
                }
                "property_identifier" if node.parent().is_some_and(|n| n.kind() == "enum_body") => {
                    name = Some(text(self.source, node).to_owned());
                    "variant"
                }
                _ => return None,
            },
            "rust" => match syntax {
                "function_item" | "function_signature_item" => {
                    if member {
                        "method"
                    } else {
                        "function"
                    }
                }
                "struct_item" => "struct",
                "union_item" => "union",
                "enum_item" => "enum",
                "enum_variant" => "variant",
                "trait_item" => "trait",
                "mod_item" => "module",
                "impl_item" => {
                    name = node.child_by_field_name("type").map(|ty| {
                        let ty = text(self.source, ty).to_owned();
                        node.child_by_field_name("trait")
                            .map(|tr| text(self.source, tr))
                            .map_or_else(|| ty.clone(), |tr| format!("<{ty} as {tr}>"))
                    });
                    "impl"
                }
                "foreign_mod_item" => {
                    name = children(node)
                        .into_iter()
                        .find(|n| n.kind() == "extern_modifier")
                        .map(|n| compact(text(self.source, n), self.language));
                    "module"
                }
                "type_item" | "associated_type" => "type",
                "const_item" => "constant",
                "static_item" => "variable",
                "field_declaration" => "field",
                "macro_definition" => "macro",
                "let_declaration" if value(node).is_some_and(function_value) => {
                    name = node
                        .child_by_field_name("pattern")
                        .and_then(super::syntax::rust_binding)
                        .map(|binding| text(self.source, binding).to_owned());
                    "function"
                }
                _ => return None,
            },
            "python" => match syntax {
                "function_definition" => {
                    if member {
                        "method"
                    } else {
                        "function"
                    }
                }
                "class_definition" => "class",
                "assignment" if !local || value(node).is_some_and(function_value) => {
                    let target = node.child_by_field_name("left")?;
                    let bound = value(node).is_some_and(function_value);
                    let declaration_target = match target.kind() {
                        "identifier" => true,
                        "attribute" => bound,
                        "pattern_list" | "tuple_pattern" | "list_pattern" => !bound,
                        _ => false,
                    };
                    if !declaration_target {
                        return None;
                    }
                    name = Some(compact(text(self.source, target), self.language));
                    if member { "field" } else { "variable" }
                }
                "named_expression" if value(node).is_some_and(function_value) => "function",
                "type_alias_statement" => {
                    name = node
                        .child_by_field_name("left")
                        .and_then(python_type_identifier)
                        .map(|binding| text(self.source, binding).to_owned());
                    "type"
                }
                _ => return None,
            },
            "go" => match syntax {
                "function_declaration" => "function",
                "method_declaration" | "method_elem" => "method",
                "type_elem" if node.parent().is_some_and(|n| n.kind() == "interface_type") => {
                    name = Some(compact(text(self.source, node), self.language));
                    "type"
                }
                "type_spec" | "type_alias" => {
                    match node.child_by_field_name("type").map(|n| n.kind()) {
                        Some("struct_type") => "struct",
                        Some("interface_type") => "interface",
                        _ => "type",
                    }
                }
                "const_spec" | "var_spec" if syntax == "const_spec" || !local => {
                    let names: Vec<_> = self
                        .field_names(node, "name")
                        .into_iter()
                        .filter(|name| name != "_")
                        .collect();
                    return (!names.is_empty()).then_some((
                        if syntax == "const_spec" {
                            "constant"
                        } else {
                            "variable"
                        },
                        names,
                    ));
                }
                "short_var_declaration" if value(node).is_some_and(function_value) => {
                    name = node
                        .child_by_field_name("left")
                        .and_then(|left| {
                            let targets = super::syntax::children(left);
                            (targets.len() == 1).then(|| targets[0])
                        })
                        .and_then(|target| go_callable_target(self.source, target))
                        .map(|binding| text(self.source, binding).to_owned());
                    "function"
                }
                "package_clause" => {
                    name = node.named_child(0).map(|n| text(self.source, n).to_owned());
                    "module"
                }
                _ => return None,
            },
            "java" => match syntax {
                "variable_declarator" if value(node).is_some_and(function_value) => "function",
                "class_declaration" | "record_declaration" => "class",
                "interface_declaration" | "annotation_type_declaration" => "interface",
                "enum_declaration" => "enum",
                "enum_constant" => "variant",
                "method_declaration" | "annotation_type_element_declaration" => "method",
                "constructor_declaration" | "compact_constructor_declaration" => "constructor",
                "package_declaration" => {
                    name = children(node)
                        .into_iter()
                        .find(|n| matches!(n.kind(), "identifier" | "scoped_identifier"))
                        .map(|n| text(self.source, n).to_owned());
                    "module"
                }
                _ => return None,
            },
            "c" => match syntax {
                "function_definition" => {
                    name = node
                        .child_by_field_name("declarator")
                        .and_then(|n| declarator_name(self.source, n));
                    "function"
                }
                "struct_specifier" | "union_specifier" | "enum_specifier"
                    if node.child_by_field_name("body").is_some()
                        || node.parent().is_some_and(|declaration| {
                            matches!(
                                declaration.kind(),
                                "translation_unit" | "compound_statement"
                            ) || declaration.kind() == "declaration"
                                && !children(declaration)
                                    .iter()
                                    .any(|child| child.kind().ends_with("declarator"))
                        }) =>
                {
                    match syntax {
                        "struct_specifier" => "struct",
                        "union_specifier" => "union",
                        _ => "enum",
                    }
                }
                "enumerator" => "variant",
                "preproc_def" | "preproc_function_def" => "macro",
                _ => return None,
            },
            "bash" => match syntax {
                "function_definition" => {
                    name = super::shell::function_name(self.source, node);
                    "function"
                }
                _ => return None,
            },
            _ => return None,
        };
        if matches!(
            syntax,
            "variable_declarator"
                | "assignment"
                | "assignment_expression"
                | "pair"
                | "var_spec"
                | "const_spec"
                | "public_field_definition"
                | "property_definition"
                | "field_definition"
        ) {
            if let Some(value) = value(node) {
                match value.kind() {
                    "arrow_function"
                    | "function_expression"
                    | "generator_function"
                    | "lambda"
                    | "closure_expression"
                    | "func_literal"
                    | "lambda_expression" => {
                        kind = if matches!(
                            syntax,
                            "public_field_definition" | "property_definition" | "field_definition"
                        ) {
                            "method"
                        } else {
                            "function"
                        }
                    }
                    "class" => {
                        kind = "class";
                        // Named class expressions use their own scope in the
                        // existing callable extractor, even when bound to an alias.
                        if let Some(class_name) = self.field(value, "name") {
                            name = Some(class_name);
                        }
                    }
                    "object" => kind = "variable",
                    _ => {}
                }
            }
            if kind == "variable"
                && matches!(
                    self.language,
                    "javascript" | "jsx" | "typescript" | "tsx" | "python"
                )
                && (syntax == "variable_declarator"
                    && node
                        .parent()
                        .is_some_and(|n| text(self.source, n).trim_start().starts_with("const "))
                    || name.as_ref().is_some_and(|n| {
                        n.chars().any(char::is_alphabetic)
                            && n.chars().all(|c| !c.is_alphabetic() || c.is_uppercase())
                    }))
            {
                kind = "constant";
            }
        }
        if callable(kind)
            && (syntax.contains("generator")
                || value(node).is_some_and(|v| v.kind() == "generator_function")
                || {
                    let mut cursor = node.walk();
                    node.children(&mut cursor).any(|n| n.kind() == "*")
                })
        {
            kind = "generator";
        }
        if kind == "method"
            && matches!(name.as_deref(), Some("__init__" | "__new__"))
            && self.language == "python"
        {
            kind = "constructor";
        }
        if kind == "method"
            && name.as_deref() == Some("constructor")
            && matches!(self.language, "javascript" | "jsx" | "typescript" | "tsx")
            && syntax == "method_definition"
            && node
                .parent()
                .is_some_and(|parent| parent.kind() == "class_body")
        {
            kind = "constructor";
        }
        let mut names = if matches!(syntax, "const_spec" | "var_spec") {
            self.field_names(node, "name")
        } else {
            name.into_iter().collect()
        };
        if kind == "class"
            && let Some(binding) = self.field(node, "name")
            && !names.contains(&binding)
        {
            names.push(binding);
        }
        if matches!(syntax, "variable_declarator" | "assignment")
            && let Some(pattern) = node
                .child_by_field_name("name")
                .or_else(|| node.child_by_field_name("left"))
            && matches!(
                pattern.kind(),
                "object_pattern"
                    | "array_pattern"
                    | "pattern_list"
                    | "tuple_pattern"
                    | "list_pattern"
            )
        {
            names.clear();
            binding_names(self.source, pattern, &mut names);
        }
        if names.is_empty() && matches!(kind, "class" | "struct" | "union" | "enum" | "module") {
            names.push(format!(
                "<anonymous@{}:{}>",
                node.start_position().row + 1,
                node.start_position().column + 1
            ));
        }
        if names.is_empty() {
            return None;
        }
        Some((kind, names))
    }

    fn signature(&self, node: Node<'_>, kind: &str) -> String {
        if self.language == "bash" && node.kind() == "function_definition" {
            return super::shell::function_signature(self.source, node);
        }
        let mut edits = Vec::new();
        self.signature_edits(node, node, kind, &mut edits);
        edits.sort_unstable();
        let mut signature = String::new();
        let mut position = node.start_byte();
        for (start, end) in edits {
            if start < position || start > node.end_byte() {
                continue;
            }
            signature.push_str(&self.source[position..start]);
            position = end.min(node.end_byte());
        }
        signature.push_str(&self.source[position..node.end_byte()]);
        let signature = compact(
            signature.trim().trim_end_matches([';', ',']).trim_end(),
            self.language,
        );
        if self.language == "go" {
            let keyword = match node.kind() {
                "type_spec" | "type_alias" => "type ",
                "const_spec" => "const ",
                "var_spec" => "var ",
                _ => "",
            };
            return format!("{keyword}{signature}");
        }
        if node.kind() == "variable_declarator"
            && matches!(self.language, "javascript" | "jsx" | "typescript" | "tsx")
            && let Some(parent) = node
                .parent()
                .filter(|n| matches!(n.kind(), "lexical_declaration" | "variable_declaration"))
        {
            let keyword = text(self.source, parent)
                .split_whitespace()
                .next()
                .unwrap_or("");
            return format!("{keyword} {signature}");
        }
        signature
    }

    fn signature_edits(
        &self,
        root: Node<'_>,
        node: Node<'_>,
        kind: &str,
        edits: &mut Vec<(usize, usize)>,
    ) {
        let edits_start = edits.len();
        let syntax = node.kind();
        if matches!(
            syntax,
            "comment" | "line_comment" | "block_comment" | "decorator" | "attribute_item"
        ) {
            edits.push((node.start_byte(), node.end_byte()));
            return;
        }
        // These AST nodes are declaration bodies, never initializer delimiters
        // inferred by searching for braces in text (which breaks generic types).
        if node != root
            && matches!(
                syntax,
                "statement_block"
                    | "block"
                    | "class_body"
                    | "interface_body"
                    | "enum_body"
                    | "declaration_list"
                    | "field_declaration_list"
                    | "enumerator_list"
                    | "enum_variant_list"
                    | "annotation_type_body"
                    | "constructor_body"
                    | "ordered_field_declaration_list"
            )
        {
            edits.push((node.start_byte(), node.end_byte()));
            return;
        }
        if node != root
            && matches!(syntax, "object_type" | "interface_type" | "struct_type")
            && container(kind)
            // Only a direct object alias has its members rendered as separate
            // declarations. An object inside a union/intersection (or another
            // type expression) is part of the type signature itself.
            && (syntax != "object_type" || node.parent() == Some(root))
        {
            if let Some(body) = node.child_by_field_name("body") {
                edits.push((body.start_byte(), body.end_byte()));
            } else if syntax == "object_type" {
                edits.push((node.start_byte(), node.end_byte()));
                return;
            } else {
                // Go interfaces have their braces directly on interface_type.
                let mut c = node.walk();
                if let Some(open) = node.children(&mut c).find(|n| n.kind() == "{") {
                    edits.push((open.start_byte(), node.end_byte()));
                    return;
                }
            }
        }
        if syntax == "macro_definition" {
            let mut c = node.walk();
            if let Some(open) = node
                .children(&mut c)
                .find(|n| matches!(n.kind(), "{" | "(" | "["))
            {
                edits.push((open.start_byte(), node.end_byte()));
                return;
            }
        }
        if syntax == "enum_constant"
            && let Some(args) = node.child_by_field_name("arguments")
        {
            edits.push((args.start_byte(), args.end_byte()));
        }
        if let Some(body) = node.child_by_field_name("body")
            && (callable(kind)
                || container(kind)
                || matches!(
                    syntax,
                    "arrow_function"
                        | "lambda"
                        | "function_expression"
                        | "closure_expression"
                        | "func_literal"
                        | "lambda_expression"
                ))
        {
            edits.push((body.start_byte(), body.end_byte()));
        }
        let value = match syntax {
            "assignment"
            | "assignment_expression"
            | "assignment_pattern"
            | "object_assignment_pattern" => node.child_by_field_name("right"),
            "default_parameter" | "typed_default_parameter" => node.child_by_field_name("value"),
            "required_parameter"
            | "optional_parameter"
            | "annotation_type_element_declaration"
            | "pair" => node.child_by_field_name("value"),
            "variable_declarator"
            | "init_declarator"
            | "const_item"
            | "static_item"
            | "enum_variant"
            | "enumerator"
            | "enum_assignment"
            | "public_field_definition"
            | "property_definition"
            | "field_definition"
            | "const_spec"
            | "var_spec"
            | "preproc_def"
            | "preproc_function_def" => node.child_by_field_name("value"),
            _ => None,
        };
        if let Some(value) = value {
            let bound_function = self::value(node)
                .is_some_and(|v| function_value(v) || v.kind() == "class")
                && matches!(
                    syntax,
                    "variable_declarator"
                        | "assignment"
                        | "assignment_expression"
                        | "pair"
                        | "var_spec"
                        | "public_field_definition"
                        | "property_definition"
                        | "field_definition"
                );
            if !bound_function {
                let mut start = value.start_byte();
                let mut c = node.walk();
                for token in node.children(&mut c) {
                    if token.end_byte() <= value.start_byte()
                        && (matches!(token.kind(), "=" | "default")
                            || syntax == "pair" && token.kind() == ":")
                    {
                        start = token.start_byte();
                    }
                }
                edits.push((start, value.end_byte()));
            }
        }
        let excluded = edits[edits_start..].to_vec();
        for child in children(node) {
            if excluded
                .iter()
                .any(|(start, end)| child.start_byte() >= *start && child.end_byte() <= *end)
            {
                continue;
            }
            self.signature_edits(root, child, kind, edits);
        }
    }

    fn multi_declaration(&mut self, node: Node<'_>, parent: Option<usize>, local: bool) {
        let mut cursor = node.walk();
        let mut declarations: Vec<_> = node
            .children_by_field_name("declarator", &mut cursor)
            .collect();
        if self.language == "java" {
            declarations = children(node)
                .into_iter()
                .filter(|n| n.kind() == "variable_declarator")
                .collect();
        }
        let anonymous_binding = !declarations.is_empty()
            && node.child_by_field_name("type").is_some_and(|ty| {
                matches!(
                    ty.kind(),
                    "struct_specifier" | "union_specifier" | "enum_specifier"
                ) && ty.child_by_field_name("name").is_none()
                    && ty.child_by_field_name("body").is_some()
            });
        // Emit named definitions and forward declarations, but attach anonymous
        // typedef members to the actual typedef binding instead of a fake tag.
        if let Some(ty) = node.child_by_field_name("type")
            && !anonymous_binding
            && matches!(
                ty.kind(),
                "struct_specifier" | "union_specifier" | "enum_specifier"
            )
            && (ty.child_by_field_name("body").is_some() || declarations.is_empty())
        {
            self.walk(ty, parent, local);
        }
        for declaration in &declarations {
            let Some(name) = declarator_name(self.source, *declaration) else {
                continue;
            };
            let is_function = function_declarator(*declaration);
            let bound_function =
                self.language == "java" && value(*declaration).is_some_and(function_value);
            let kind = if bound_function {
                "function"
            } else if node.kind() == "type_definition" {
                "type"
            } else if node.kind() == "field_declaration" || self.language == "java" {
                "field"
            } else if is_function {
                "function"
            } else {
                "variable"
            };
            if local && kind == "variable" {
                if let Some(value) = value(*declaration) {
                    self.walk(value, parent, true);
                }
                if anonymous_binding && let Some(ty) = node.child_by_field_name("type") {
                    self.c_nested_types(ty, parent, local);
                }
                continue;
            }
            let prefix_end = declarations
                .first()
                .map_or(declaration.start_byte(), |n| n.start_byte());
            let prefix = self.source[node.start_byte()..prefix_end].trim();
            // Strip inline C tag bodies from the shared type prefix too.
            let prefix = if let Some(ty) = node.child_by_field_name("type")
                && ty.child_by_field_name("body").is_some()
            {
                format!(
                    "{}{}{}",
                    &self.source[node.start_byte()..ty.start_byte()],
                    self.signature(ty, "struct"),
                    &self.source[ty.end_byte()..prefix_end]
                )
            } else {
                compact(prefix, self.language)
            };
            let signature = format!("{} {}", prefix, self.signature(*declaration, kind))
                .trim()
                .to_owned();
            let id = self.push(
                if bound_function { *declaration } else { node },
                parent,
                kind,
                vec![name],
                compact(&signature, self.language),
            );
            if let Some(value) = value(*declaration) {
                if bound_function {
                    self.walk_bound(value, id, kind, local);
                } else {
                    self.walk(value, parent, local);
                }
            }
            if self.language == "c" {
                for child in children(*declaration) {
                    if child.kind() == "parameter_list" {
                        self.walk(child, Some(id), true);
                    } else if child.kind().ends_with("declarator") {
                        self.c_parameters(child, Some(id));
                    }
                }
            }
            if anonymous_binding
                && let Some(body) = node
                    .child_by_field_name("type")
                    .and_then(|n| n.child_by_field_name("body"))
            {
                for child in children(body) {
                    self.walk(child, Some(id), false);
                }
            }
        }
    }

    fn c_parameters(&mut self, node: Node<'_>, parent: Option<usize>) {
        for child in children(node) {
            if child.kind() == "parameter_list" {
                self.walk(child, parent, true);
            } else if child.kind().ends_with("declarator") {
                self.c_parameters(child, parent);
            }
        }
    }

    fn c_nested_types(&mut self, node: Node<'_>, parent: Option<usize>, local: bool) {
        if matches!(
            node.kind(),
            "struct_specifier" | "union_specifier" | "enum_specifier"
        ) && node.child_by_field_name("name").is_some()
            && node.child_by_field_name("body").is_some()
        {
            self.walk(node, parent, local);
        } else {
            for child in children(node) {
                self.c_nested_types(child, parent, local);
            }
        }
    }

    fn is_import(&self, syntax: &str) -> bool {
        matches!(
            syntax,
            "import_statement"
                | "import_from_statement"
                | "future_import_statement"
                | "use_declaration"
                | "extern_crate_declaration"
                | "import_spec"
                | "import_declaration"
                | "preproc_include"
                | "import_alias"
        ) && !(self.language == "go" && syntax == "import_declaration")
    }

    fn import(&mut self, node: Node<'_>, parent: Option<usize>) {
        let imports = super::imports::extract(self.language, node, self.source);
        let mut names = Vec::new();
        for binding in &imports {
            for name in [
                Some(&binding.path),
                binding.name.as_ref(),
                binding.alias.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        if names.is_empty() {
            names.push(compact(text(self.source, node), self.language));
        }
        let id = self.push(
            node,
            parent,
            "import",
            names,
            compact(text(self.source, node), self.language)
                .trim_end_matches(';')
                .to_owned(),
        );
        self.nodes[id].imports = imports;
    }
}

fn declarator_name(source: &str, node: Node<'_>) -> Option<String> {
    if matches!(
        node.kind(),
        "identifier" | "field_identifier" | "type_identifier"
    ) {
        return Some(text(source, node).to_owned());
    }
    if let Some(name) = node.child_by_field_name("name") {
        return Some(text(source, name).to_owned());
    }
    if let Some(inner) = super::syntax::declarator_inner(node) {
        return declarator_name(source, inner);
    }
    None
}

pub(super) fn go_callable_target<'tree>(source: &str, node: Node<'tree>) -> Option<Node<'tree>> {
    (matches!(node.kind(), "identifier" | "selector_expression") && text(source, node) != "_")
        .then_some(node)
}

fn python_type_identifier(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        match node.kind() {
            "identifier" => return Some(node),
            "type" | "generic_type" => {
                node = super::syntax::children(node).into_iter().next()?;
            }
            _ => return None,
        }
    }
}

fn binding_names(source: &str, node: Node<'_>, names: &mut Vec<String>) {
    match node.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            names.push(text(source, node).to_owned())
        }
        "pair_pattern" => {
            if let Some(value) = node.child_by_field_name("value") {
                binding_names(source, value, names);
            }
        }
        "assignment_pattern" | "object_assignment_pattern" => {
            if let Some(left) = node.child_by_field_name("left") {
                binding_names(source, left, names);
            }
        }
        "attribute" | "subscript" => {}
        _ => {
            for child in children(node) {
                binding_names(source, child, names);
            }
        }
    }
}

fn function_declarator(mut node: Node<'_>) -> bool {
    // The innermost declarator operator distinguishes functions (including
    // functions returning pointers) from variables holding function pointers.
    let mut function = false;
    loop {
        match node.kind() {
            "function_declarator" => function = true,
            "pointer_declarator" | "array_declarator" => function = false,
            _ => {}
        }
        let next = super::syntax::declarator_inner(node);
        let Some(next) = next else {
            return function;
        };
        node = next;
    }
}

#[cfg(test)]
#[path = "structure_tests.rs"]
mod tests;
