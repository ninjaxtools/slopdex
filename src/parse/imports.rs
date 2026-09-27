//! Import paths and bindings, kept separate from display signatures.

use super::ImportBinding;
use tree_sitter::Node;

fn text<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn field(source: &str, node: Node<'_>, field: &str) -> Option<String> {
    node.child_by_field_name(field)
        .map(|n| text(source, n).to_owned())
}

fn unquote(value: &str) -> String {
    value.trim_matches(['\'', '"', '`', '<', '>']).to_owned()
}

fn binding(
    path: String,
    source: Option<String>,
    name: Option<String>,
    alias: Option<String>,
) -> ImportBinding {
    let wildcard = name.as_deref() == Some("*") || path.ends_with('*');
    ImportBinding {
        path,
        source,
        name,
        alias,
        wildcard,
    }
}

pub(super) fn extract(language: &str, node: Node<'_>, source: &str) -> Vec<ImportBinding> {
    let mut result = Vec::new();
    match language {
        "rust" => {
            if let Some(argument) = node.child_by_field_name("argument") {
                rust_use(argument, source, "", &mut result);
            } else if let Some(name) = field(source, node, "name") {
                result.push(binding(
                    name.clone(),
                    None,
                    Some(name),
                    field(source, node, "alias"),
                ));
            }
        }
        "javascript" | "jsx" | "typescript" | "tsx" => {
            let module = field(source, node, "source").map(|s| unquote(&s));
            js_bindings(node, source, module.as_deref(), &mut result);
            if result.is_empty() {
                if let Some(module) = module {
                    let mut cursor = node.walk();
                    let wildcard = node.children(&mut cursor).any(|n| n.kind() == "*");
                    result.push(if wildcard {
                        binding(format!("{module}.*"), Some(module), Some("*".into()), None)
                    } else {
                        binding(module.clone(), Some(module), None, None)
                    });
                } else if let Some(name) = field(source, node, "name").or_else(|| {
                    node.named_child(0)
                        .filter(|n| n.kind() == "identifier")
                        .map(|n| text(source, n).to_owned())
                }) {
                    let value = children(node).into_iter().find(|n| {
                        matches!(
                            n.kind(),
                            "nested_identifier" | "call_expression" | "require_clause"
                        )
                    });
                    let path = value.map_or_else(|| name.clone(), |n| text(source, n).to_owned());
                    result.push(binding(path, None, None, Some(name)));
                }
            }
        }
        "python" => {
            let module = field(source, node, "module_name");
            let mut cursor = node.walk();
            for name in node.children_by_field_name("name", &mut cursor) {
                let (name, alias) = if name.kind() == "aliased_import" {
                    (
                        field(source, name, "name").unwrap_or_default(),
                        field(source, name, "alias"),
                    )
                } else {
                    (text(source, name).to_owned(), None)
                };
                let path = module.as_ref().map_or_else(
                    || name.clone(),
                    |m| {
                        if m.ends_with('.') {
                            format!("{m}{name}")
                        } else {
                            format!("{m}.{name}")
                        }
                    },
                );
                result.push(binding(path, module.clone(), Some(name), alias));
            }
            if children(node).iter().any(|n| n.kind() == "wildcard_import") {
                let path = module
                    .as_ref()
                    .map_or_else(|| "*".into(), |m| format!("{m}.*"));
                result.push(binding(path, module, Some("*".into()), None));
            }
        }
        "go" => {
            if let Some(path) = field(source, node, "path") {
                let path = unquote(&path);
                let alias = field(source, node, "name");
                result.push(binding(path.clone(), Some(path), None, alias));
            }
        }
        "java" => {
            if let Some(path) = children(node)
                .into_iter()
                .find(|n| matches!(n.kind(), "identifier" | "scoped_identifier"))
            {
                let mut path = text(source, path).to_owned();
                if children(node).iter().any(|n| n.kind() == "asterisk") {
                    path.push_str(".*");
                }
                let name = path.rsplit('.').next().map(str::to_owned);
                result.push(binding(path, None, name, None));
            }
        }
        "c" => {
            if let Some(path) = field(source, node, "path") {
                let path = unquote(&path);
                result.push(binding(path.clone(), Some(path), None, None));
            }
        }
        _ => {}
    }
    result
}

fn rust_use(node: Node<'_>, source: &str, prefix: &str, result: &mut Vec<ImportBinding>) {
    let join = |path: &str| {
        if prefix.is_empty() {
            path.to_owned()
        } else if path == "self" {
            prefix.to_owned()
        } else {
            format!("{prefix}::{path}")
        }
    };
    match node.kind() {
        "scoped_use_list" => {
            let path = field(source, node, "path").unwrap_or_default();
            let next = join(&path);
            if let Some(list) = node.child_by_field_name("list") {
                rust_use(list, source, &next, result);
            }
        }
        "use_list" => {
            for child in children(node) {
                rust_use(child, source, prefix, result);
            }
        }
        "use_as_clause" => {
            if let Some(path) = field(source, node, "path") {
                let full = join(&path);
                let name = full.rsplit("::").next().map(str::to_owned);
                result.push(binding(full, None, name, field(source, node, "alias")));
            }
        }
        _ => {
            let full = join(text(source, node));
            let name = full.rsplit("::").next().map(str::to_owned);
            result.push(binding(full, None, name, None));
        }
    }
}

fn js_bindings(
    node: Node<'_>,
    source: &str,
    module: Option<&str>,
    result: &mut Vec<ImportBinding>,
) {
    let make = |name: &str, alias: Option<String>| {
        let path = module.map_or_else(|| name.to_owned(), |m| format!("{m}.{name}"));
        binding(
            path,
            module.map(str::to_owned),
            Some(name.to_owned()),
            alias,
        )
    };
    match node.kind() {
        "import_require_clause" => {
            if let Some(module) = field(source, node, "source") {
                let module = unquote(&module);
                let alias = children(node)
                    .into_iter()
                    .find(|n| n.kind() == "identifier")
                    .map(|n| text(source, n).to_owned());
                result.push(binding(module.clone(), Some(module), None, alias));
            }
        }
        "import_specifier" | "export_specifier" => {
            if let Some(name) = field(source, node, "name") {
                result.push(make(&unquote(&name), field(source, node, "alias")));
            }
        }
        "namespace_import" | "namespace_export" => {
            let alias = children(node)
                .into_iter()
                .find(|n| n.kind() == "identifier")
                .map(|n| text(source, n).to_owned());
            result.push(make("*", alias));
        }
        "identifier" if node.parent().is_some_and(|p| p.kind() == "import_clause") => {
            result.push(make("default", Some(text(source, node).to_owned())));
        }
        _ => {
            for child in children(node) {
                js_bindings(child, source, module, result);
            }
        }
    }
}
