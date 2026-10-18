//! Syntactic symbol uses with lexical bindings. Unknown bindings deliberately
//! block lookup: a local or parameter is not evidence about its runtime type.

use std::collections::{BTreeSet, HashMap, HashSet};

use tree_sitter::Node;

use super::syntax::{children, text};
use super::{FileStructure, ReferenceSite};

/// Uncollapsed lexical locations for source edits. Unlike the stored graph,
/// these include top-level uses and unknown bindings, and are never persisted.
#[derive(Clone, Debug)]
pub(crate) struct EditSite {
    pub range: std::ops::Range<usize>,
    pub name: String,
    pub receiver: Option<String>,
    pub binding_id: Option<usize>,
    /// Several indexed declarations may denote one C linkage identity (a
    /// prototype and its definition). Empty for an unknown local/parameter.
    pub binding_ids: Vec<usize>,
    pub bound: bool,
    pub declaration: bool,
    pub shorthand: bool,
}

pub(crate) fn edit_sites(
    root: Node<'_>,
    source: &str,
    language: &str,
    structure: &FileStructure,
) -> Vec<EditSite> {
    let mut sites = Vec::new();
    collect_inner(
        root,
        source,
        language,
        &mut structure.clone(),
        Some(&mut sites),
    );
    sites
}

#[derive(Default)]
struct Scope {
    parent: Option<usize>,
    callable: bool,
    members: bool,
    bindings: HashMap<String, Vec<Binding>>,
}

struct Binding {
    id: Option<usize>,
    visible_from: usize,
    implicit: bool,
    lexical: bool,
}

enum Lookup {
    Symbol(usize),
    Implicit,
    Unknown(Vec<usize>),
    Missing,
}

/// Uses an iterative walk, like call extraction, so deeply nested expressions
/// do not add recursion. Each reference belongs to its innermost declaration;
/// container references do not accidentally include their children's bodies.
pub(super) fn collect(root: Node<'_>, source: &str, language: &str, structure: &mut FileStructure) {
    collect_inner(root, source, language, structure, None);
}

fn collect_inner(
    root: Node<'_>,
    source: &str,
    language: &str,
    structure: &mut FileStructure,
    mut sites: Option<&mut Vec<EditSite>>,
) {
    if language == "bash" {
        // Shell expansion, command lookup, and dynamic sourcing require their
        // own binding model. Preserve the existing callable graph unchanged.
        return;
    }
    let mut scopes = vec![Scope::default()];
    let headers = language == "c" && structure.nodes.iter().any(|node| node.kind == "import");
    let mut by_start: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut by_end: HashMap<usize, Vec<usize>> = HashMap::new();
    for symbol in &structure.nodes {
        by_start
            .entry(symbol.start_byte)
            .or_default()
            .push(symbol.id);
        by_end.entry(symbol.end_byte).or_default().push(symbol.id);
    }
    let mut entries = Vec::new();
    let mut declared = HashSet::new();
    let mut stack = vec![(root, 0)];
    while let Some((node, outer)) = stack.pop() {
        if matches!(
            node.kind(),
            "string" | "template_string" | "concatenated_string"
        ) {
            // Literal contents are not code; interpolation expressions are.
            stack.extend(
                children(node)
                    .into_iter()
                    .rev()
                    .filter(|child| {
                        matches!(
                            child.kind(),
                            "interpolation" | "template_substitution" | "string"
                        )
                    })
                    .map(|child| (child, outer)),
            );
            continue;
        }
        if ignored(node) {
            continue;
        }
        if import_syntax(node.kind())
            && let Some(import) = by_end
                .get(&node.end_byte())
                .into_iter()
                .flatten()
                .map(|&id| &structure.nodes[id])
                .find(|symbol| {
                    symbol.kind == "import"
                        && symbol.start_byte <= node.start_byte()
                        && symbol.end_byte == node.end_byte()
                })
        {
            for binding in &import.imports {
                let local = binding
                    .alias
                    .as_deref()
                    .or(binding.name.as_deref())
                    .or_else(|| {
                        (language == "go")
                            .then(|| binding.path.rsplit('/').next().unwrap_or(&binding.path))
                    });
                if let Some(local) = local.filter(|name| !matches!(*name, "*" | "." | "_")) {
                    scopes[outer]
                        .bindings
                        .entry(local.to_owned())
                        .or_default()
                        .push(Binding {
                            id: Some(import.id),
                            visible_from: 0,
                            implicit: false,
                            lexical: false,
                        });
                }
            }
            continue;
        }
        let scope = if let Some((callable, members)) = scope_kind(node, language) {
            let id = scopes.len();
            scopes.push(Scope {
                parent: Some(outer),
                callable,
                members,
                ..Scope::default()
            });
            id
        } else {
            outer
        };
        let parameter = parameter_syntax(node.kind());
        let patterns = binding_patterns(node, language);
        for pattern in patterns {
            for target in pattern_names(pattern) {
                declared.insert(target.id());
                let name = text(source, target).trim_start_matches('#');
                if name == "_" || name.is_empty() {
                    continue;
                }
                let mut binding_scope = if parameter
                    || matches!(
                        node.kind(),
                        "for_in_statement"
                            | "enhanced_for_statement"
                            | "for_expression"
                            | "match_arm"
                    ) {
                    scope
                } else {
                    outer
                };
                if language == "c" && node.kind() == "enumerator" {
                    binding_scope = scopes[outer].parent.unwrap_or(outer);
                }
                // Python assignments bind throughout the function, even when
                // nested in a loop/conditional. JavaScript `var` also hoists.
                if language == "python" && !declaration_scope(node.kind())
                    || matches!(language, "javascript" | "jsx" | "typescript" | "tsx")
                        && node.kind() == "variable_declarator"
                        && node
                            .parent()
                            .is_some_and(|parent| parent.kind() == "variable_declaration")
                {
                    while scopes[binding_scope].parent.is_some()
                        && !scopes[binding_scope].callable
                        && !scopes[binding_scope].members
                    {
                        binding_scope = scopes[binding_scope].parent.unwrap();
                    }
                }
                // Named function/class expressions bind their internal name in
                // the expression's scope, not in the surrounding scope.
                if matches!(
                    node.kind(),
                    "function_expression" | "generator_function" | "class"
                ) {
                    binding_scope = scope;
                }
                let sequential = matches!(language, "rust" | "go")
                    && matches!(
                        node.kind(),
                        "let_declaration" | "short_var_declaration" | "var_spec"
                    )
                    && enclosing_callable(&scopes, outer);
                let id = symbol_id(structure, &by_start, &by_end, node, name);
                if let Some(sites) = sites.as_deref_mut() {
                    sites.push(EditSite {
                        range: target.byte_range(),
                        name: name.to_owned(),
                        receiver: None,
                        binding_id: id,
                        binding_ids: id.into_iter().collect(),
                        bound: true,
                        declaration: true,
                        shorthand: target.kind() == "shorthand_property_identifier_pattern",
                    });
                }
                scopes[binding_scope]
                    .bindings
                    .entry(name.to_owned())
                    .or_default()
                    .push(Binding {
                        id,
                        visible_from: if sequential { node.end_byte() } else { 0 },
                        implicit: parameter
                            && matches!(language, "python" | "rust")
                            && matches!(name, "self" | "cls"),
                        lexical: parameter,
                    });
            }
        }
        entries.push((node, scope));
        stack.extend(children(node).into_iter().rev().map(|child| (child, scope)));
    }

    let mut owners: Vec<_> = structure.nodes.iter().map(|node| node.id).collect();
    owners.sort_by_key(|&id| {
        let node = &structure.nodes[id];
        (node.start_byte, std::cmp::Reverse(node.end_byte), id)
    });
    let mut next = 0;
    let mut active = Vec::new();
    let mut seen = BTreeSet::new();
    for (node, scope) in entries {
        let Some((receiver, name)) = reference_target(node, source, &declared) else {
            continue;
        };
        let root_name = receiver.unwrap_or(name);
        let binding = lookup(&scopes, scope, root_name, node.start_byte(), language);
        if let Some(sites) = sites.as_deref_mut() {
            // Static member targets end in the selected property/type name.
            // Use its own token range, not the receiver or the whole expression.
            let end = node.end_byte();
            let range = if receiver.is_some() {
                ["property", "attribute", "field", "name"]
                    .iter()
                    .find_map(|field| node.child_by_field_name(field))
                    .map(|name| name.byte_range())
                    .unwrap_or(end - name.len()..end)
            } else if node.kind() == "method_invocation" {
                node.child_by_field_name("name")
                    .map(|name| name.byte_range())
                    .unwrap_or(node.byte_range())
            } else {
                node.byte_range()
            };
            sites.push(EditSite {
                range,
                name: name.to_owned(),
                receiver: receiver.map(str::to_owned),
                binding_id: match binding {
                    Lookup::Symbol(id) => Some(id),
                    _ => None,
                },
                binding_ids: match &binding {
                    Lookup::Symbol(id) => vec![*id],
                    Lookup::Unknown(ids) => ids.clone(),
                    _ => Vec::new(),
                },
                bound: !matches!(binding, Lookup::Missing),
                declaration: false,
                shorthand: node.kind() == "shorthand_property_identifier",
            });
        }
        let binding_id = match binding {
            Lookup::Symbol(id) => Some(id),
            Lookup::Missing | Lookup::Implicit
                if receiver.is_some()
                    && matches!(root_name, "self" | "this" | "Self" | "cls")
                    && enclosing_members(&scopes, scope) =>
            {
                None
            }
            Lookup::Missing if headers && receiver.is_none() => None,
            _ => continue,
        };
        // Rust dot access is on a value, not a module/type namespace. `self`
        // remains useful without attempting to infer other receiver types.
        if language == "rust" && node.kind() == "field_expression" && receiver != Some("self") {
            continue;
        }
        while next < owners.len() && structure.nodes[owners[next]].start_byte <= node.start_byte() {
            active.push(owners[next]);
            next += 1;
        }
        active.retain(|&id| structure.nodes[id].end_byte >= node.start_byte());
        let owner = active
            .iter()
            .copied()
            .filter(|&id| {
                let symbol = &structure.nodes[id];
                symbol.kind != "import" && node.end_byte() <= symbol.end_byte
            })
            .min_by_key(|&id| {
                let symbol = &structure.nodes[id];
                (symbol.end_byte - symbol.start_byte, id)
            });
        if let Some(owner) = owner
            && seen.insert((
                owner,
                name.to_owned(),
                receiver.map(str::to_owned),
                binding_id,
            ))
        {
            structure.nodes[owner].references.push(ReferenceSite {
                name: name.to_owned(),
                receiver: receiver.map(str::to_owned),
                binding_id,
            });
        }
    }
}

fn symbol_id(
    structure: &FileStructure,
    by_start: &HashMap<usize, Vec<usize>>,
    by_end: &HashMap<usize, Vec<usize>>,
    declaration: Node<'_>,
    name: &str,
) -> Option<usize> {
    let declaration = if declaration.kind() == "variable_declarator" {
        declaration
            .parent()
            .filter(|parent| matches!(parent.kind(), "field_declaration" | "constant_declaration"))
            .unwrap_or(declaration)
    } else {
        declaration
    };
    let candidates: BTreeSet<_> = by_start
        .get(&declaration.start_byte())
        .into_iter()
        .flatten()
        .chain(by_end.get(&declaration.end_byte()).into_iter().flatten())
        .map(|&id| &structure.nodes[id])
        .filter(|symbol| {
            symbol.kind != "impl"
                && (symbol.name == name || symbol.names.iter().any(|n| n == name))
                && (symbol.end_byte == declaration.end_byte()
                    || symbol.start_byte == declaration.start_byte())
                && (symbol.start_byte <= declaration.start_byte()
                    && declaration.end_byte() <= symbol.end_byte
                    || declaration.start_byte() <= symbol.start_byte
                        && symbol.end_byte <= declaration.end_byte())
        })
        .map(|symbol| symbol.id)
        .collect();
    (candidates.len() == 1).then(|| *candidates.first().unwrap())
}

fn lookup(scopes: &[Scope], mut scope: usize, name: &str, at: usize, language: &str) -> Lookup {
    loop {
        let current = &scopes[scope];
        let skip = current.members
            && matches!(
                language,
                "python" | "javascript" | "jsx" | "typescript" | "tsx"
            );
        if let Some(bindings) = current.bindings.get(name) {
            let bindings: Vec<_> = bindings
                .iter()
                .filter(|binding| binding.visible_from <= at && (!skip || binding.lexical))
                .collect();
            if !bindings.is_empty() {
                let ids: BTreeSet<_> = bindings.iter().filter_map(|binding| binding.id).collect();
                return if bindings.iter().all(|binding| binding.id.is_some()) && ids.len() == 1 {
                    Lookup::Symbol(*ids.first().unwrap())
                } else if bindings.iter().all(|binding| binding.implicit) {
                    Lookup::Implicit
                } else {
                    Lookup::Unknown(if bindings.iter().all(|binding| binding.id.is_some()) {
                        ids.into_iter().collect()
                    } else {
                        Vec::new()
                    })
                };
            }
        }
        let Some(parent) = current.parent else {
            return Lookup::Missing;
        };
        scope = parent;
    }
}

fn enclosing_callable(scopes: &[Scope], scope: usize) -> bool {
    std::iter::successors(Some(scope), |id| scopes[*id].parent).any(|id| scopes[id].callable)
}

fn enclosing_members(scopes: &[Scope], scope: usize) -> bool {
    std::iter::successors(Some(scope), |id| scopes[*id].parent).any(|id| scopes[id].members)
}

fn scope_kind(node: Node<'_>, language: &str) -> Option<(bool, bool)> {
    let kind = node.kind();
    if matches!(
        kind,
        "function_declaration"
            | "function_definition"
            | "function_item"
            | "function_signature_item"
            | "function_expression"
            | "generator_function"
            | "generator_function_declaration"
            | "arrow_function"
            | "method_definition"
            | "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration"
            | "lambda"
            | "lambda_expression"
            | "closure_expression"
            | "func_literal"
            | "function_signature"
            | "method_signature"
            | "abstract_method_signature"
            | "call_signature"
            | "construct_signature"
            | "function_type"
            | "constructor_type"
            | "index_signature"
    ) {
        return Some((true, false));
    }
    if matches!(
        kind,
        "class"
            | "class_declaration"
            | "class_definition"
            | "abstract_class_declaration"
            | "record_declaration"
            | "struct_item"
            | "union_item"
            | "enum_item"
            | "impl_item"
            | "trait_item"
            | "interface_declaration"
            | "enum_declaration"
            | "object"
            | "struct_specifier"
            | "union_specifier"
            | "enum_specifier"
            | "struct_type"
            | "interface_type"
            | "type_alias_declaration"
            | "type_item"
            | "type_spec"
            | "type_alias"
            | "type_alias_statement"
    ) {
        return Some((false, true));
    }
    if matches!(kind, "mod_item" | "internal_module" | "module")
        || language != "python"
            && matches!(
                kind,
                "block"
                    | "statement_block"
                    | "compound_statement"
                    | "for_statement"
                    | "for_in_statement"
                    | "enhanced_for_statement"
                    | "catch_clause"
                    | "match_arm"
                    | "for_expression"
                    | "if_expression"
                    | "while_expression"
            )
    {
        return Some((false, false));
    }
    if language == "c" && kind == "function_declarator" {
        let mut parent = node.parent();
        while let Some(node) = parent {
            if node.kind() == "function_definition" {
                return None;
            }
            if node.kind() == "declaration" {
                return Some((true, false));
            }
            parent = node.parent();
        }
    }
    None
}

fn declaration_scope(kind: &str) -> bool {
    matches!(
        kind,
        "function_definition"
            | "class_definition"
            | "function_declaration"
            | "function_item"
            | "class_declaration"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "mod_item"
    )
}

fn parameter_syntax(kind: &str) -> bool {
    matches!(
        kind,
        "parameters"
            | "formal_parameters"
            | "lambda_parameters"
            | "closure_parameters"
            | "inferred_parameters"
            | "parameter"
            | "parameter_declaration"
            | "variadic_parameter_declaration"
            | "self_parameter"
            | "formal_parameter"
            | "spread_parameter"
            | "catch_formal_parameter"
            | "required_parameter"
            | "optional_parameter"
            | "type_parameter"
            | "type_parameter_declaration"
            | "const_parameter"
            | "lifetime_parameter"
            | "type_parameters"
            | "type_parameter_list"
            | "arrow_function"
            | "catch_clause"
            | "index_signature"
            | "mapped_type_clause"
    )
}

fn binding_patterns<'a>(node: Node<'a>, language: &str) -> Vec<Node<'a>> {
    let field = |name| {
        node.child_by_field_name(name)
            .into_iter()
            .collect::<Vec<_>>()
    };
    let names = || {
        let mut cursor = node.walk();
        node.children_by_field_name("name", &mut cursor)
            .collect::<Vec<_>>()
    };
    match node.kind() {
        "parameters"
        | "formal_parameters"
        | "lambda_parameters"
        | "closure_parameters"
        | "inferred_parameters" => children(node)
            .into_iter()
            .filter(|child| {
                matches!(
                    child.kind(),
                    "identifier"
                        | "tuple_pattern"
                        | "list_pattern"
                        | "object_pattern"
                        | "array_pattern"
                        | "default_parameter"
                        | "typed_parameter"
                        | "typed_default_parameter"
                        | "list_splat_pattern"
                        | "dictionary_splat_pattern"
                        | "assignment_pattern"
                        | "rest_pattern"
                )
            })
            .collect(),
        "parameter" => field("pattern"),
        "self_parameter" => children(node)
            .into_iter()
            .filter(|child| child.kind() == "self")
            .collect(),
        "parameter_declaration" | "variadic_parameter_declaration" if language == "go" => names(),
        "parameter_declaration" if language == "c" => node
            .child_by_field_name("declarator")
            .and_then(declarator_name)
            .into_iter()
            .collect(),
        "formal_parameter" | "spread_parameter" | "catch_formal_parameter" => field("name"),
        "required_parameter" | "optional_parameter" => field("pattern"),
        "type_parameter" | "type_parameter_declaration" | "const_parameter" => node
            .child_by_field_name("name")
            .or_else(|| node.named_child(0))
            .into_iter()
            .collect(),
        "type_parameters" | "type_parameter_list" => children(node)
            .into_iter()
            .filter(|child| child.kind() == "identifier")
            .collect(),
        "arrow_function" => field("parameter"),
        "catch_clause" => field("parameter"),
        "index_signature" | "mapped_type_clause" => field("name"),
        "let_declaration" => field("pattern"),
        "let_condition" | "for_expression" | "match_arm" => field("pattern"),
        "short_var_declaration" if language == "go" => field("left"),
        "range_clause" if language == "go" => field("left"),
        "var_spec" | "const_spec" => names(),
        "assignment" | "augmented_assignment" if language == "python" => field("left"),
        "named_expression" => field("name"),
        "for_statement" | "for_in_clause" if language == "python" => field("left"),
        "for_in_statement" => field("left"),
        "as_pattern" => field("alias"),
        "global_statement" | "nonlocal_statement" => children(node),
        "variable_declarator" => node
            .child_by_field_name("name")
            .or_else(|| {
                node.child_by_field_name("declarator")
                    .and_then(declarator_name)
            })
            .into_iter()
            .collect(),
        "declaration" | "type_definition" | "field_declaration" if language == "c" => {
            let mut cursor = node.walk();
            node.children_by_field_name("declarator", &mut cursor)
                .filter_map(declarator_name)
                .collect()
        }
        "function_definition" if language == "c" => node
            .child_by_field_name("declarator")
            .and_then(declarator_name)
            .into_iter()
            .collect(),
        "pair" => field("key")
            .into_iter()
            .filter(|key| key.kind() != "computed_property_name")
            .collect(),
        "struct_specifier" | "union_specifier" | "enum_specifier" if language == "c" => {
            let declaration = node.child_by_field_name("body").is_some()
                || node.parent().is_some_and(|parent| {
                    matches!(parent.kind(), "translation_unit" | "compound_statement")
                        || parent.kind() == "declaration"
                            && parent.child_by_field_name("declarator").is_none()
                });
            if declaration { names() } else { Vec::new() }
        }
        "property_signature"
        | "public_field_definition"
        | "property_definition"
        | "field_definition"
        | "method_definition"
        | "method_signature"
        | "abstract_method_signature"
        | "function_declaration"
        | "function_definition"
        | "function_item"
        | "function_signature_item"
        | "function_signature"
        | "function_expression"
        | "generator_function"
        | "generator_function_declaration"
        | "class"
        | "class_declaration"
        | "class_definition"
        | "abstract_class_declaration"
        | "interface_declaration"
        | "type_alias_declaration"
        | "enum_declaration"
        | "enum_assignment"
        | "struct_item"
        | "union_item"
        | "enum_item"
        | "enum_variant"
        | "trait_item"
        | "mod_item"
        | "internal_module"
        | "module"
        | "type_item"
        | "associated_type"
        | "const_item"
        | "static_item"
        | "field_declaration"
        | "macro_definition"
        | "type_spec"
        | "type_alias"
        | "method_declaration"
        | "constructor_declaration"
        | "record_declaration"
        | "enum_constant"
        | "struct_specifier"
        | "union_specifier"
        | "enum_specifier"
        | "enumerator"
        | "preproc_def"
        | "preproc_function_def"
        | "enhanced_for_statement" => names(),
        _ => Vec::new(),
    }
}

fn declarator_name(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        if matches!(
            node.kind(),
            "identifier" | "field_identifier" | "type_identifier"
        ) {
            return Some(node);
        }
        node = super::syntax::declarator_inner(node)?;
    }
}

fn pattern_names(root: Node<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "identifier"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "private_property_identifier"
            | "shorthand_property_identifier_pattern"
            | "self" => result.push(node),
            "member_expression"
            | "attribute"
            | "selector_expression"
            | "subscript"
            | "field_expression" => {}
            "pair_pattern" => stack.extend(node.child_by_field_name("value")),
            "assignment_pattern" | "object_assignment_pattern" => {
                stack.extend(node.child_by_field_name("left"))
            }
            "default_parameter" | "typed_default_parameter" => {
                stack.extend(node.child_by_field_name("name"))
            }
            "typed_parameter" => stack.extend(node.named_child(0)),
            "type_annotation" | "generic_type" | "scoped_type_identifier" | "scoped_identifier" => {
            }
            "computed_property_name" => {}
            _ => stack.extend(children(node).into_iter().rev()),
        }
    }
    result
}

fn import_syntax(kind: &str) -> bool {
    matches!(
        kind,
        "import_statement"
            | "import_from_statement"
            | "future_import_statement"
            | "use_declaration"
            | "extern_crate_declaration"
            | "import_spec"
            | "import_declaration"
            | "preproc_include"
            | "import_alias"
            | "export_statement"
    )
}

fn ignored(node: Node<'_>) -> bool {
    node.is_extra()
        || node.is_error()
        || matches!(
            node.kind(),
            "comment"
                | "line_comment"
                | "block_comment"
                | "string"
                | "string_literal"
                | "raw_string_literal"
                | "interpreted_string_literal"
                | "char_literal"
                | "character_literal"
                | "template_string"
                | "concatenated_string"
                | "token_tree"
                | "attribute_item"
                | "inner_attribute_item"
        )
}

fn reference_target<'a>(
    node: Node<'_>,
    source: &'a str,
    declared: &HashSet<usize>,
) -> Option<(Option<&'a str>, &'a str)> {
    if declared.contains(&node.id()) {
        return None;
    }
    let fields = match node.kind() {
        "member_expression" | "field_access" => Some(("object", "property")),
        "attribute" => Some(("object", "attribute")),
        "selector_expression" => Some(("operand", "field")),
        "field_expression" => Some(("value", "field")),
        "scoped_identifier" | "scoped_type_identifier" => Some(("path", "name")),
        "nested_type_identifier" => Some(("module", "name")),
        "qualified_type" => Some(("package", "name")),
        "method_invocation" => Some(("object", "name")),
        _ => None,
    };
    if let Some((receiver_field, name_field)) = fields {
        if node.kind() == "method_invocation" && node.child_by_field_name(receiver_field).is_none()
        {
            let name = node.child_by_field_name(name_field)?;
            return identifier_node(name).then(|| (None, text(source, name)));
        }
        let receiver = node.child_by_field_name(receiver_field)?;
        let name = node.child_by_field_name(name_field).or_else(|| {
            (node.kind() == "field_access")
                .then(|| node.child_by_field_name("field"))
                .flatten()
        })?;
        if identifier_node(receiver) && identifier_node(name) {
            return Some((
                Some(text(source, receiver)),
                text(source, name).trim_start_matches('#'),
            ));
        }
        return None;
    }
    if !matches!(
        node.kind(),
        "identifier" | "type_identifier" | "shorthand_property_identifier"
    ) {
        return None;
    }
    if let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "member_expression"
                | "attribute"
                | "selector_expression"
                | "field_expression"
                | "field_access"
                | "scoped_identifier"
                | "scoped_type_identifier"
                | "nested_type_identifier"
                | "qualified_type"
                | "method_invocation"
        ) && ["property", "attribute", "field", "name"]
            .iter()
            .any(|field| parent.child_by_field_name(field) == Some(node))
        {
            return None;
        }
        if matches!(
            parent.kind(),
            "pair"
                | "field_initializer"
                | "labeled_statement"
                | "break_statement"
                | "continue_statement"
                | "goto_statement"
        ) && ["key", "field", "label"]
            .iter()
            .any(|field| parent.child_by_field_name(field) == Some(node))
        {
            return None;
        }
    }
    Some((None, text(source, node)))
}

fn identifier_node(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "identifier"
            | "type_identifier"
            | "property_identifier"
            | "field_identifier"
            | "private_property_identifier"
            | "package_identifier"
            | "self"
            | "this"
            | "super"
    )
}

#[cfg(test)]
mod tests {
    use crate::parse::parse;

    fn refs(path: &str, source: &str, owner: &str) -> Vec<(String, Option<String>, String)> {
        let parsed = parse(path, source).unwrap();
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let symbol = parsed
            .structure
            .nodes
            .iter()
            .find(|node| node.qualified_name == owner)
            .unwrap();
        symbol
            .references
            .iter()
            .map(|site| {
                (
                    site.name.clone(),
                    site.receiver.clone(),
                    site.binding_id
                        .map(|id| parsed.structure.nodes[id].qualified_name.clone())
                        .unwrap_or_else(|| "<implicit>".into()),
                )
            })
            .collect()
    }

    #[test]
    fn declarations_comments_and_literals_are_not_references() {
        for (path, source, owner) in [
            (
                "a.ts",
                "const VALUE = 1;\n// VALUE\nfunction empty(VALUE: number) { const text = 'VALUE'; const template = `VALUE`; return { VALUE: 0 }; }",
                "empty",
            ),
            (
                "a.rs",
                "const VALUE: usize = 1;\n// VALUE\nfn empty(VALUE: usize) { let text = r#\"VALUE\"#; }",
                "empty",
            ),
            (
                "a.py",
                "VALUE = 1\n# VALUE\ndef empty(VALUE):\n    text = 'VALUE'\n    return VALUE\n",
                "empty",
            ),
            (
                "a.go",
                "package a\nconst VALUE = 1\n// VALUE\nfunc empty(VALUE int) { text := `VALUE`; _ = text }\n",
                "empty",
            ),
            (
                "A.java",
                "class A { static int VALUE; void empty(int VALUE) { String text = \"VALUE\"; } }",
                "A.empty",
            ),
            (
                "a.c",
                "int VALUE;\n/* VALUE */\nvoid empty(int VALUE) { char *text = \"VALUE\"; }",
                "empty",
            ),
        ] {
            assert!(refs(path, source, owner).is_empty(), "{path}");
        }
    }

    #[test]
    fn references_belong_to_innermost_declarations_and_include_signatures() {
        let source = "interface Shape {}\nconst VALUE = 1;\nclass Holder extends Shape { field: Shape; method(arg: Shape): Shape { function nested() { return VALUE; } return arg; } }";
        assert_eq!(
            refs("a.ts", source, "Holder"),
            [("Shape".into(), None, "Shape".into())]
        );
        assert_eq!(
            refs("a.ts", source, "Holder.field"),
            [("Shape".into(), None, "Shape".into())]
        );
        assert_eq!(
            refs("a.ts", source, "Holder.method"),
            [("Shape".into(), None, "Shape".into())]
        );
        assert_eq!(
            refs("a.ts", source, "Holder.method.nested"),
            [("VALUE".into(), None, "VALUE".into())]
        );
    }

    #[test]
    fn closures_catches_loops_and_function_types_do_not_leak_bindings() {
        let source = "const VALUE = 1;\nconst arrow = VALUE => VALUE;\ntype Callback = (VALUE: number) => number;\nfunction run() { try {} catch (VALUE) { VALUE; } for (const VALUE of []) { VALUE; } return VALUE; }";
        assert!(refs("a.ts", source, "arrow").is_empty());
        assert_eq!(
            refs("a.ts", source, "run"),
            [("VALUE".into(), None, "VALUE".into())]
        );
        let source = "const VALUE: usize = 1;\nfn run() { for VALUE in [] { let _ = VALUE; } let _ = VALUE; }";
        assert_eq!(
            refs("a.rs", source, "run"),
            [("VALUE".into(), None, "VALUE".into())]
        );
    }

    #[test]
    fn python_function_wide_locals_and_enclosing_parameters_shadow_globals() {
        let source = "VALUE = 1\ndef outer(VALUE):\n    def inner():\n        return VALUE\n    return inner\ndef local():\n    print(VALUE)\n    VALUE = 2\n    return VALUE\n";
        assert!(refs("a.py", source, "outer.inner").is_empty());
        assert!(refs("a.py", source, "local").is_empty());
    }

    #[test]
    fn reference_metadata_is_distinct_from_calls_and_deduplicated() {
        let parsed = parse(
            "a.rs",
            "fn helper() {}\nfn run() { let f = helper; let g = helper; helper(); helper(); }\n",
        )
        .unwrap();
        let run = parsed
            .structure
            .nodes
            .iter()
            .find(|node| node.name == "run")
            .unwrap();
        assert_eq!(run.calls.len(), 1);
        assert_eq!(run.references.len(), 1);
        assert_eq!(run.references[0].name, "helper");
        let parsed = parse("a.rs", "fn helper() {}\nfn run() { let f = helper; }\n").unwrap();
        let run = parsed
            .structure
            .nodes
            .iter()
            .find(|node| node.name == "run")
            .unwrap();
        assert!(run.calls.is_empty());
        assert_eq!(run.references.len(), 1);
        let json = serde_json::to_value(run).unwrap();
        assert!(json.get("references").is_none());
        assert_eq!(json["referenceSites"][0]["name"], "helper");
        assert_eq!(
            serde_json::from_value::<super::super::StructureNode>(json)
                .unwrap()
                .references,
            run.references
        );
    }

    #[test]
    fn string_interpolations_are_code_but_literal_contents_are_not() {
        for (path, source) in [
            (
                "a.ts",
                "const VALUE = 1;\nfunction run() { return `VALUE ${VALUE}`; }",
            ),
            (
                "a.py",
                "VALUE = 1\ndef run():\n    return f'VALUE {VALUE}'\n",
            ),
        ] {
            assert_eq!(
                refs(path, source, "run"),
                [("VALUE".into(), None, "VALUE".into())],
                "{path}"
            );
        }
    }

    #[test]
    fn c_enumerators_bind_in_the_enclosing_lexical_scope() {
        assert_eq!(
            refs(
                "a.c",
                "enum Mode { VALUE = 1 };\nint run(void) { return VALUE; }",
                "run"
            ),
            [("VALUE".into(), None, "Mode.VALUE".into())]
        );
    }
}
