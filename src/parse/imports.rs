//! Import paths and bindings, kept separate from display signatures.

use super::ImportBinding;
use super::syntax::text;
use tree_sitter::Node;

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|n| !n.is_extra())
        .collect()
}

fn field(source: &str, node: Node<'_>, field: &str) -> Option<String> {
    node.child_by_field_name(field)
        .map(|n| text(source, n).to_owned())
}

// Qualified paths are tokens, not source slices: layout and comment extras
// between their components must not become part of the imported path.
fn path_text(source: &str, node: Node<'_>) -> String {
    if node.child_count() == 0 {
        return text(source, node).to_owned();
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|n| !n.is_extra())
        .map(|n| path_text(source, n))
        .collect()
}

fn path_field(source: &str, node: Node<'_>, field: &str) -> Option<String> {
    node.child_by_field_name(field)
        .map(|n| path_text(source, n))
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && matches!(
            (bytes[0], bytes[bytes.len() - 1]),
            (b'\'', b'\'') | (b'"', b'"') | (b'`', b'`') | (b'<', b'>')
        )
    {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

fn binding(
    path: String,
    source: Option<String>,
    name: Option<String>,
    alias: Option<String>,
) -> ImportBinding {
    ImportBinding {
        path,
        source,
        name,
        alias,
        wildcard: false,
    }
}

fn wildcard_binding(path: String, source: Option<String>, alias: Option<String>) -> ImportBinding {
    ImportBinding {
        wildcard: true,
        ..binding(path, source, Some("*".into()), alias)
    }
}

fn python_path(module: Option<&str>, name: &str) -> String {
    module.map_or_else(
        || name.to_owned(),
        |m| {
            if m.ends_with('.') {
                format!("{m}{name}")
            } else {
                format!("{m}.{name}")
            }
        },
    )
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
            if result.is_empty()
                && let Some(module) = module
            {
                let mut cursor = node.walk();
                let wildcard = node.children(&mut cursor).any(|n| n.kind() == "*");
                result.push(if wildcard {
                    wildcard_binding(format!("{module}.*"), Some(module), None)
                } else {
                    binding(module.clone(), Some(module), None, None)
                });
            }
        }
        "python" => {
            let module = if node.kind() == "future_import_statement" {
                Some("__future__".to_owned())
            } else {
                path_field(source, node, "module_name")
            };
            let mut cursor = node.walk();
            for name in node.children_by_field_name("name", &mut cursor) {
                let (name, alias) = if name.kind() == "aliased_import" {
                    (
                        path_field(source, name, "name").unwrap_or_default(),
                        field(source, name, "alias"),
                    )
                } else {
                    (path_text(source, name), None)
                };
                let path = python_path(module.as_deref(), &name);
                result.push(binding(path, module.clone(), Some(name), alias));
            }
            if children(node).iter().any(|n| n.kind() == "wildcard_import") {
                let path = python_path(module.as_deref(), "*");
                result.push(wildcard_binding(path, module, None));
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
                let mut path = path_text(source, path);
                let wildcard = children(node).iter().any(|n| n.kind() == "asterisk");
                if wildcard {
                    path.push_str(".*");
                }
                let name = path.rsplit('.').next().map(str::to_owned);
                result.push(if wildcard {
                    wildcard_binding(path, None, None)
                } else {
                    binding(path, None, name, None)
                });
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
        } else if prefix.ends_with("::") {
            format!("{prefix}{path}")
        } else {
            format!("{prefix}::{path}")
        }
    };
    match node.kind() {
        "scoped_use_list" => {
            let next = path_field(source, node, "path").map_or_else(|| "::".into(), |p| join(&p));
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
            if let Some(path) = path_field(source, node, "path") {
                let full = join(&path);
                let name = full.rsplit("::").next().map(str::to_owned);
                result.push(binding(full, None, name, field(source, node, "alias")));
            }
        }
        "use_wildcard" => {
            result.push(wildcard_binding(join(&path_text(source, node)), None, None));
        }
        "identifier" | "scoped_identifier" | "self" | "super" | "crate" | "metavariable" => {
            let full = join(&path_text(source, node));
            let name = full.rsplit("::").next().map(str::to_owned);
            result.push(binding(full, None, name, None));
        }
        _ => {}
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
        "import_alias" => {
            let mut names = children(node)
                .into_iter()
                .filter(|n| matches!(n.kind(), "identifier" | "nested_identifier"));
            if let (Some(alias), Some(target)) = (names.next(), names.next()) {
                result.push(binding(
                    path_text(source, target),
                    None,
                    None,
                    Some(text(source, alias).to_owned()),
                ));
            }
        }
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
                let alias = field(source, node, "alias").map(|s| unquote(&s));
                result.push(make(&unquote(&name), alias));
            }
        }
        "namespace_import" | "namespace_export" => {
            let mut cursor = node.walk();
            let alias = node
                .children(&mut cursor)
                .find(|n| matches!(n.kind(), "identifier" | "string" | "default"))
                .map(|n| unquote(text(source, n)));
            let path = module.map_or_else(|| "*".into(), |m| format!("{m}.*"));
            result.push(wildcard_binding(path, module.map(str::to_owned), alias));
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
