//! Shared repository-relative path and symbol selection for search and structure maps.

use crate::{
    parse::{FileStructure, StructureNode},
    symbols,
};
use anyhow::{Context, Result, bail, ensure};
use ignore::overrides::{Override, OverrideBuilder};
use regex::{RegexSet, RegexSetBuilder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct Selection {
    globs: Override,
    names: Option<RegexSet>,
    semantic_names: Option<HashSet<String>>,
    kinds: HashSet<&'static str>,
    private: bool,
}

impl Selection {
    /// Compile shared path, regex, and semantic selectors, plus map kinds/visibility.
    /// String lists accept either a string (including legacy `regexp`) or an array.
    /// Glob rules use gitignore syntax with inverted `!`: the last matching rule
    /// wins, and any positive rule requires a positive match. `ignoreCase` affects
    /// regexes only; paths are always matched case-sensitively.
    pub fn compile(options: &Value) -> Result<Self> {
        let mut globs = OverrideBuilder::new("");
        for pattern in strings(options, "glob")? {
            ensure!(!pattern.trim().is_empty(), "glob must not be empty");
            globs
                .add(pattern)
                .with_context(|| format!("invalid glob: {pattern}"))?;
        }
        let patterns = strings(options, "regexp")?;
        let queries = strings(options, "symbolQuery")?;
        for query in &queries {
            ensure!(
                !symbols::normalize(query).is_empty(),
                "symbolQuery must contain nonempty normalized queries"
            );
        }
        if let Some(value) = options
            .get("symbolThreshold")
            .filter(|value| !value.is_null())
        {
            let threshold = value.as_f64().context("symbolThreshold must be a number")?;
            ensure!(
                threshold.is_finite() && (-1.0..=1.0).contains(&threshold),
                "symbolThreshold must be finite and in [-1,1]"
            );
        }
        let ignore_case = match options.get("ignoreCase") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            _ => bail!("ignoreCase must be a boolean"),
        };
        let names = if patterns.is_empty() {
            None
        } else {
            Some(
                RegexSetBuilder::new(patterns)
                    .case_insensitive(ignore_case)
                    .build()
                    .context("invalid regexp")?,
            )
        };
        let mut kinds = HashSet::new();
        for list in strings(options, "kinds")? {
            for kind in list.split(',') {
                kinds.extend(kind_group(&normalize_kind(kind)?)?.iter().copied());
            }
        }
        let private = match options.get("private") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            _ => bail!("private must be a boolean"),
        };
        Ok(Self {
            globs: globs.build().context("compile selection globs")?,
            names,
            semantic_names: (!queries.is_empty()).then(HashSet::new),
            kinds,
            private,
        })
    }

    /// Match an already normalized repository-relative file path, never a cwd path.
    pub fn path_matches(&self, path: &str) -> bool {
        !self.globs.matched(path, false).is_ignore()
    }

    /// Match exactly the supplied name. Search/cross-search should supply
    /// `qualifiedName`; Markdown search should supply its joined heading path.
    /// Multiple expressions are ORed, without rewriting anchors or adding names.
    pub fn name_matches(&self, name: &str) -> bool {
        self.names.as_ref().is_none_or(|names| names.is_match(name))
    }

    /// Resolve the union of normalized names matching the semantic queries.
    pub(crate) fn with_symbol_names(mut self, names: HashSet<String>) -> Self {
        self.semantic_names = Some(names);
        self
    }

    pub(crate) fn semantic_name_matches(&self, name: &str) -> bool {
        self.semantic_names
            .as_ref()
            .is_none_or(|names| names.contains(&symbols::normalize(name)))
    }

    /// Declaration names use the same qualified-name contract as search. Imports
    /// additionally expose their source paths and aliases; multi-binding names
    /// are qualified in the declaration's enclosing scope. Semantic selection
    /// independently matches bare names/aliases, then intersects the regex family.
    pub fn symbol_matches(&self, node: &StructureNode) -> bool {
        let prefix = if node.kind == "import" {
            ""
        } else {
            node.qualified_name.strip_suffix(&node.name).unwrap_or("")
        };
        let regex_matches = self.name_matches(&node.qualified_name)
            || node
                .names
                .iter()
                .any(|name| self.name_matches(&format!("{prefix}{name}")));
        let semantic_matches = self.semantic_name_matches(&node.name)
            || node
                .names
                .iter()
                .any(|name| self.semantic_name_matches(name));
        regex_matches && semantic_matches
    }

    pub fn kind_matches(&self, kind: &str) -> bool {
        self.kinds.is_empty() || self.kinds.contains(kind)
    }

    /// Keep direct kind+name matches and all their ancestors as context. Never
    /// expand a matched parent to its unmatched children. IDs and source order
    /// remain unchanged, even if IDs are sparse or parents follow their children.
    pub fn select_structure(&self, structure: &FileStructure) -> Vec<StructureNode> {
        let parents: HashMap<_, _> = structure
            .nodes
            .iter()
            .map(|node| (node.id, node.parent_id))
            .collect();
        let nodes: HashMap<_, _> = structure.nodes.iter().map(|node| (node.id, node)).collect();
        let mut visibility = HashMap::new();
        let mut selected = HashSet::new();
        for node in &structure.nodes {
            if (!self.private
                && symbol_is_private(node.id, &nodes, &mut visibility, &mut HashSet::new()))
                || !self.kind_matches(&node.kind)
                || !self.symbol_matches(node)
            {
                continue;
            }
            let mut current = Some(node.id);
            while let Some(id) = current {
                if !selected.insert(id) {
                    break;
                }
                current = parents.get(&id).copied().flatten();
            }
        }
        structure
            .nodes
            .iter()
            .filter(|node| selected.contains(&node.id))
            .cloned()
            .collect()
    }
}

fn symbol_is_private(
    id: usize,
    nodes: &HashMap<usize, &StructureNode>,
    cache: &mut HashMap<usize, bool>,
    visiting: &mut HashSet<usize>,
) -> bool {
    if let Some(value) = cache.get(&id) {
        return *value;
    }
    let Some(node) = nodes.get(&id).copied() else {
        return false;
    };
    if !visiting.insert(id) {
        return false;
    }
    let parent = node.parent_id.and_then(|id| nodes.get(&id).copied());
    let inherited_private = parent.is_some_and(|parent| {
        parent.kind != "impl" && symbol_is_private(parent.id, nodes, cache, visiting)
    });
    let private = inherited_private || locally_private(node, parent);
    visiting.remove(&id);
    cache.insert(id, private);
    private
}

fn locally_private(node: &StructureNode, parent: Option<&StructureNode>) -> bool {
    if matches!(node.kind.as_str(), "heading" | "import") {
        return false;
    }
    if parent.is_some_and(|parent| {
        matches!(
            parent.kind.as_str(),
            "function" | "method" | "constructor" | "generator"
        )
    }) {
        return true;
    }
    // In languages without a `private` modifier, a leading underscore on a
    // callable or variable conventionally marks implementation-only names.
    // Explicit exports and Rust's `pub` visibility still take precedence.
    let underscore_private = matches!(node.language.as_str(), "c" | "bash" | "javascript" | "jsx")
        && matches!(
            node.kind.as_str(),
            "function" | "method" | "generator" | "variable" | "constant" | "field"
        )
        && !node.names.is_empty()
        && node
            .names
            .iter()
            .all(|name| name.starts_with('_') && name.len() > 1);
    match node.language.as_str() {
        "rust" => rust_private(node, parent),
        "javascript" | "jsx" => {
            javascript_private(node, parent)
                || underscore_private
                    && !node.attributes.iter().any(|attr| attr == "export")
                    && !node.signature.trim_start().starts_with("export ")
        }
        "typescript" | "tsx" => javascript_private(node, parent),
        "python" => node.names.iter().all(|name| name.starts_with('_')),
        "go" => go_private(node),
        "java" => java_private(node, parent),
        "c" => has_word(&node.signature, "static") || underscore_private,
        "bash" => underscore_private,
        _ => false,
    }
}

fn rust_private(node: &StructureNode, parent: Option<&StructureNode>) -> bool {
    if node.kind == "impl" {
        return true;
    }
    if parent.is_some_and(|parent| {
        matches!(parent.kind.as_str(), "trait" | "enum" | "variant")
            || parent.kind == "impl" && parent.signature.contains(" for ")
    }) {
        return false;
    }
    node.kind == "macro"
        && !node
            .attributes
            .iter()
            .any(|attr| attr.contains("macro_export"))
        || node.kind != "macro" && !node.signature.trim_start().starts_with("pub")
}

fn javascript_private(node: &StructureNode, parent: Option<&StructureNode>) -> bool {
    let signature = node.signature.trim_start();
    if parent.is_some() {
        return signature.starts_with('#')
            || has_word(signature, "private")
            || has_word(signature, "protected");
    }
    !node.attributes.iter().any(|attr| attr == "export")
        && !signature.starts_with("export ")
        && !signature.starts_with("module.exports")
        && !signature.starts_with("exports.")
}

fn go_private(node: &StructureNode) -> bool {
    if node.kind == "module" {
        return false;
    }
    let name = node
        .name
        .rsplit(['.', '*'])
        .find(|part| !part.is_empty())
        .unwrap_or(&node.name);
    !name.chars().next().is_some_and(char::is_uppercase)
        || node.kind == "method"
            && node
                .qualified_name
                .split('.')
                .next()
                .and_then(|part| part.trim_start_matches('*').chars().next())
                .is_some_and(char::is_lowercase)
}

fn java_private(node: &StructureNode, parent: Option<&StructureNode>) -> bool {
    if node.kind == "module" {
        return false;
    }
    if parent.is_some_and(|parent| parent.kind == "interface") {
        return has_word(&node.signature, "private");
    }
    if node.kind == "variant" && parent.is_some_and(|parent| parent.kind == "enum") {
        return false;
    }
    !has_word(&node.signature, "public")
}

fn has_word(value: &str, expected: &str) -> bool {
    value
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|word| word == expected)
}

pub(crate) fn strings<'a>(options: &'a Value, key: &str) -> Result<Vec<&'a str>> {
    match options.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(value)) => Ok(vec![value]),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .with_context(|| format!("{key} must contain strings"))
            })
            .collect(),
        _ => bail!("{key} must be a string or an array of strings"),
    }
}

/// Normalize CLI/API aliases to a stable kind/group name, rejecting typos.
pub fn normalize_kind(input: &str) -> Result<String> {
    let lower = input.trim().to_ascii_lowercase();
    let kind = match lower.as_str() {
        "fn" | "fns" | "function" | "functions" | "callable" | "callables" => "functions",
        "type" | "types" => "types",
        "method" | "methods" => "method",
        "constructor" | "constructors" => "constructor",
        "generator" | "generators" => "generator",
        "class" | "classes" => "class",
        "struct" | "structs" => "struct",
        "union" | "unions" => "union",
        "interface" | "interfaces" => "interface",
        "trait" | "traits" => "trait",
        "enum" | "enums" => "enum",
        "variant" | "variants" => "variant",
        "field" | "fields" | "property" | "properties" => "field",
        "alias" | "aliases" | "type-alias" | "type-aliases" => "alias",
        "impl" | "impls" | "implementation" | "implementations" => "impl",
        "mod" | "module" | "modules" | "namespace" | "namespaces" | "package" | "packages" => {
            "module"
        }
        "const" | "consts" | "constant" | "constants" => "constant",
        "var" | "vars" | "variable" | "variables" => "variable",
        "import" | "imports" => "import",
        "macro" | "macros" => "macro",
        "heading" | "headings" => "heading",
        _ => bail!("unknown structure kind: {input}"),
    };
    Ok(kind.to_owned())
}

fn kind_group(kind: &str) -> Result<&'static [&'static str]> {
    Ok(match kind {
        "functions" => &["function", "method", "constructor", "generator"],
        "types" => &[
            "type",
            "class",
            "struct",
            "union",
            "interface",
            "trait",
            "enum",
        ],
        "alias" => &["type"],
        "method" => &["method"],
        "constructor" => &["constructor"],
        "generator" => &["generator"],
        "class" => &["class"],
        "struct" => &["struct"],
        "union" => &["union"],
        "interface" => &["interface"],
        "trait" => &["trait"],
        "enum" => &["enum"],
        "variant" => &["variant"],
        "field" => &["field"],
        "impl" => &["impl"],
        "module" => &["module"],
        "constant" => &["constant"],
        "variable" => &["variable"],
        "import" => &["import"],
        "macro" => &["macro"],
        "heading" => &["heading"],
        _ => bail!("unknown structure kind: {kind}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ordered_globs_require_positive_matches_and_allow_reinclusion() {
        let selection =
            Selection::compile(&json!({"glob": ["*.rs", "!src/**", "src/keep.rs"]})).unwrap();
        assert!(selection.path_matches("lib.rs"));
        assert!(selection.path_matches("nested/lib.rs"));
        assert!(!selection.path_matches("lib.py"));
        assert!(!selection.path_matches("src/lib.rs"));
        assert!(selection.path_matches("src/keep.rs"));
        let reversed = Selection::compile(&json!({"glob": ["src/keep.rs", "!src/**"]})).unwrap();
        assert!(!reversed.path_matches("src/keep.rs"));
        assert!(!reversed.path_matches("other.rs"));
        let exclusions = Selection::compile(&json!({"glob": ["!*.md"]})).unwrap();
        assert!(exclusions.path_matches("src/lib.rs"));
        assert!(!exclusions.path_matches("docs/README.md"));
    }

    #[test]
    fn globs_are_root_relative_with_basename_and_double_star_support() {
        let selection =
            Selection::compile(&json!({"glob": ["/README.md", "src/*.rs", "docs/**/guide.md"]}))
                .unwrap();
        for path in [
            "README.md",
            "src/lib.rs",
            "docs/guide.md",
            "docs/a/b/guide.md",
        ] {
            assert!(selection.path_matches(path), "{path}");
        }
        for path in [
            "nested/README.md",
            "nested/src/lib.rs",
            "src/nested/lib.rs",
            "docs/other.md",
        ] {
            assert!(!selection.path_matches(path), "{path}");
        }
        assert!(
            Selection::compile(&json!({}))
                .unwrap()
                .path_matches("any/path")
        );
    }

    #[test]
    fn regexes_are_or_combined_and_keep_qualified_name_anchors() {
        let selection = Selection::compile(
            &json!({"regexp": ["^Service\\.run$", "^Guide\\.Setup$"], "ignoreCase": true}),
        )
        .unwrap();
        assert!(selection.name_matches("service.RUN"));
        assert!(selection.name_matches("Guide.Setup"));
        assert!(!selection.name_matches("run"));
        assert!(!selection.name_matches("Other.Service.run"));
        let legacy = Selection::compile(&json!({"regexp": "^Service\\.run$"})).unwrap();
        assert!(legacy.name_matches("Service.run"));
        assert!(!legacy.name_matches("service.run"));
        assert!(
            Selection::compile(&json!({"regexp": []}))
                .unwrap()
                .name_matches("anything")
        );
    }

    #[test]
    fn invalid_selection_is_rejected_before_matching() {
        for options in [
            json!({"glob": ["["]}),
            json!({"glob": [1]}),
            json!({"regexp": "["}),
            json!({"regexp": ["valid", "("]}),
            json!({"regexp": true}),
            json!({"ignoreCase": "true"}),
            json!({"kinds": ["methdos"]}),
            json!({"kinds": "functions,"}),
            json!({"private": "true"}),
            json!({"symbolQuery": true}),
            json!({"symbolQuery": ["valid", 1]}),
            json!({"symbolQuery": ""}),
            json!({"symbolQuery": ["valid", " _::.!🙂 "]}),
            json!({"symbolThreshold": "0.5"}),
            json!({"symbolThreshold": [0.5]}),
            json!({"symbolThreshold": -1.1}),
            json!({"symbolThreshold": 1.1}),
        ] {
            assert!(Selection::compile(&options).is_err(), "{options}");
        }
    }

    #[test]
    fn semantic_queries_fail_closed_until_resolved_and_match_normalized_names() {
        let selection =
            Selection::compile(&json!({"symbolQuery": ["read file", "write file"]})).unwrap();
        assert!(selection.name_matches("anything"));
        assert!(!selection.semantic_name_matches("readFile"));
        let selection =
            selection.with_symbol_names(HashSet::from(["read file".into(), "write file".into()]));
        for name in ["readFile", "Read_File", "write file", "WRITE_FILE"] {
            assert!(selection.semantic_name_matches(name), "{name}");
        }
        assert!(!selection.semantic_name_matches("Service.readFile"));
        assert!(!selection.semantic_name_matches("deleteFile"));
        for options in [
            json!({}),
            json!({"symbolQuery": []}),
            json!({"symbolThreshold": -1}),
            json!({"symbolThreshold": 1}),
        ] {
            assert!(
                Selection::compile(&options)
                    .unwrap()
                    .semantic_name_matches("anything")
            );
        }
    }

    #[test]
    fn regex_and_semantic_families_match_independently_and_intersect() {
        let selection = Selection::compile(&json!({
            "symbolQuery": ["read file", "write file"],
            "regexp": ["^Service\\.", "^fs(?:\\.|$)"],
            "glob": "*.ts", "kinds": "functions,imports", "private": true
        }))
        .unwrap()
        .with_symbol_names(HashSet::from(["read file".into(), "write file".into()]));
        assert!(selection.path_matches("src/service.ts"));
        assert!(!selection.path_matches("src/service.py"));
        let parsed = crate::parse::parse("service.ts", "export class Service { readFile() {} writeFile() {} deleteFile() {} } export class Other { readFile() {} } import { readFile as load } from 'fs';").unwrap();
        let nodes = selection.select_structure(&parsed.structure);
        assert!(
            nodes
                .iter()
                .any(|node| node.qualified_name == "Service.readFile")
        );
        assert!(
            nodes
                .iter()
                .any(|node| node.qualified_name == "Service.writeFile")
        );
        assert!(
            !nodes
                .iter()
                .any(|node| node.qualified_name == "Other.readFile" || node.name == "deleteFile")
        );
        assert!(
            nodes.iter().any(|node| node.kind == "import"),
            "{:?}",
            parsed.structure.nodes
        );
        // The regex can match the source path while semantics match an alias.
        let alias = StructureNode {
            kind: "import".into(),
            qualified_name: "fs".into(),
            name: "fs".into(),
            names: vec!["readFile".into()],
            ..StructureNode::default()
        };
        assert!(selection.symbol_matches(&alias));
        assert!(!selection.symbol_matches(&StructureNode {
            names: vec!["deleteFile".into()],
            ..alias
        }));
    }

    #[test]
    fn semantic_headings_use_bare_titles_without_parent_semantics() {
        let parsed =
            crate::parse::parse("guide.md", "# Guide\n## Setup\n### Linux\n## Other\n").unwrap();
        let selection = Selection::compile(&json!({"symbolQuery": "setup"}))
            .unwrap()
            .with_symbol_names(HashSet::from(["setup".into()]));
        let nodes = selection.select_structure(&parsed.structure);
        assert_eq!(
            nodes
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            ["Guide", "Setup"]
        );
        assert!(!selection.symbol_matches(&nodes[0]));
        assert!(selection.symbol_matches(&nodes[1]));
        let parent = Selection::compile(&json!({"symbolQuery": "guide"}))
            .unwrap()
            .with_symbol_names(HashSet::from(["guide".into()]));
        assert_eq!(parent.select_structure(&parsed.structure).len(), 1);
    }

    #[test]
    fn semantic_selection_intersects_kind_and_visibility() {
        let parsed = crate::parse::parse(
            "files.rs",
            "pub struct ReadFile; pub fn readFile() {} fn writeFile() {}",
        )
        .unwrap();
        let names = HashSet::from(["read file".into(), "write file".into()]);
        for (private, expected) in [
            (false, vec!["readFile"]),
            (true, vec!["readFile", "writeFile"]),
        ] {
            let selection = Selection::compile(&json!({
                "symbolQuery": ["read file", "write file"], "kinds": "functions", "private": private
            }))
            .unwrap()
            .with_symbol_names(names.clone());
            assert_eq!(
                selection
                    .select_structure(&parsed.structure)
                    .iter()
                    .map(|node| node.name.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn kind_aliases_expand_groups_and_comma_separated_lists() {
        let selection =
            Selection::compile(&json!({"kinds": [" FNS,Consts", "Types", "headings"]})).unwrap();
        for kind in [
            "function",
            "method",
            "constructor",
            "generator",
            "constant",
            "type",
            "interface",
            "class",
            "struct",
            "heading",
        ] {
            assert!(selection.kind_matches(kind), "{kind}");
        }
        for kind in ["import", "field", "variable"] {
            assert!(!selection.kind_matches(kind), "{kind}");
        }
        let methods = Selection::compile(&json!({"kinds": "methods"})).unwrap();
        assert!(methods.kind_matches("method"));
        assert!(!methods.kind_matches("function"));
    }

    #[test]
    fn structure_selection_keeps_ancestors_without_unmatched_children() {
        let parsed = crate::parse::parse(
            "example.ts",
            "namespace API { export class Service { run() {} stop() {} } function helper() {} }",
        )
        .unwrap();
        let selection = Selection::compile(
            &json!({"kinds": "methods", "regexp": "^API\\.Service\\.run$", "private": true}),
        )
        .unwrap();
        let nodes = selection.select_structure(&parsed.structure);
        assert_eq!(
            nodes
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            ["API", "Service", "run"]
        );
        let parent_only =
            Selection::compile(&json!({"regexp": "^API\\.Service$", "private": true})).unwrap();
        let nodes = parent_only.select_structure(&parsed.structure);
        assert_eq!(
            nodes
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            ["API", "Service"]
        );
        assert!(
            Selection::compile(&json!({"regexp": "missing", "private": true}))
                .unwrap()
                .select_structure(&parsed.structure)
                .is_empty()
        );
        assert!(
            Selection::compile(&json!({"regexp": "^run$", "private": true}))
                .unwrap()
                .select_structure(&parsed.structure)
                .is_empty()
        );
    }

    #[test]
    fn structure_selection_excludes_private_symbols_by_default() {
        for (path, source, public, private) in [
            (
                "example.rs",
                "pub fn visible() {} fn hidden() {}",
                "visible",
                "hidden",
            ),
            (
                "example.ts",
                "export function visible() {} function hidden() {}",
                "visible",
                "hidden",
            ),
            (
                "example.py",
                "def visible(): pass\ndef _hidden(): pass\n",
                "visible",
                "_hidden",
            ),
            (
                "example.go",
                "package example\nfunc Visible() {}\nfunc hidden() {}\n",
                "Visible",
                "hidden",
            ),
            (
                "Example.java",
                "public class Visible {} class Hidden {}",
                "Visible",
                "Hidden",
            ),
            (
                "example.c",
                "void visible(void) {} static void hidden(void) {}",
                "visible",
                "hidden",
            ),
        ] {
            let structure = crate::parse::parse(path, source).unwrap().structure;
            let selected = Selection::compile(&json!({}))
                .unwrap()
                .select_structure(&structure);
            assert!(selected.iter().any(|node| node.name == public), "{path}");
            assert!(!selected.iter().any(|node| node.name == private), "{path}");
            let complete = Selection::compile(&json!({"private": true}))
                .unwrap()
                .select_structure(&structure);
            assert!(complete.iter().any(|node| node.name == private), "{path}");
        }
    }

    #[test]
    fn underscore_callables_and_variables_follow_languages_without_private_modifiers() {
        for (path, source, public, private) in [
            (
                "example.c",
                "void visible(void) {} void _helper(void) {}",
                "visible",
                "_helper",
            ),
            (
                "example.sh",
                "visible() { :; }\n_helper() { :; }\n",
                "visible",
                "_helper",
            ),
            (
                "example.js",
                "export class Service { visible() {} _helper() {} }",
                "visible",
                "_helper",
            ),
            (
                "example.py",
                "visible = 1\n_helper = 2\n",
                "visible",
                "_helper",
            ),
        ] {
            let structure = crate::parse::parse(path, source).unwrap().structure;
            let selected = Selection::compile(&json!({}))
                .unwrap()
                .select_structure(&structure);
            assert!(selected.iter().any(|node| node.name == public), "{path}");
            assert!(!selected.iter().any(|node| node.name == private), "{path}");
            let complete = Selection::compile(&json!({"private": true}))
                .unwrap()
                .select_structure(&structure);
            assert!(complete.iter().any(|node| node.name == private), "{path}");
        }

        for (path, source) in [
            ("example.rs", "pub fn _exported() {}"),
            ("example.ts", "export class Service { _exported() {} }"),
            ("example.js", "export function _exported() {}"),
        ] {
            let structure = crate::parse::parse(path, source).unwrap().structure;
            let selected = Selection::compile(&json!({}))
                .unwrap()
                .select_structure(&structure);
            assert!(
                selected.iter().any(|node| node.name == "_exported"),
                "{path}"
            );
        }
    }

    #[test]
    fn markdown_paths_and_import_aliases_are_symbols() {
        let markdown =
            crate::parse::parse("guide.md", "# Guide\n## Setup\n### Linux\n## Other\n").unwrap();
        let selection = Selection::compile(&json!({"regexp": "^Guide\\.Setup$"})).unwrap();
        let nodes = selection.select_structure(&markdown.structure);
        assert_eq!(
            nodes
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            ["Guide", "Setup"]
        );
        let code = crate::parse::parse(
            "example.ts",
            "import { readFile as load, writeFile as save } from 'fs';",
        )
        .unwrap();
        let selection =
            Selection::compile(&json!({"regexp": "^save$", "kinds": "imports"})).unwrap();
        assert_eq!(selection.select_structure(&code.structure).len(), 1);
    }

    #[test]
    fn ancestor_closure_uses_ids_and_handles_cycles() {
        let structure = FileStructure {
            nodes: vec![
                StructureNode {
                    id: 30,
                    parent_id: Some(10),
                    name: "match".into(),
                    qualified_name: "match".into(),
                    kind: "method".into(),
                    ..StructureNode::default()
                },
                StructureNode {
                    id: 10,
                    parent_id: Some(30),
                    name: "parent".into(),
                    kind: "class".into(),
                    ..StructureNode::default()
                },
                StructureNode {
                    id: 20,
                    parent_id: Some(10),
                    name: "sibling".into(),
                    kind: "method".into(),
                    ..StructureNode::default()
                },
            ],
        };
        let selection =
            Selection::compile(&json!({"regexp": "^match$", "kinds": "methods", "private": true}))
                .unwrap();
        assert_eq!(
            selection
                .select_structure(&structure)
                .iter()
                .map(|node| node.id)
                .collect::<Vec<_>>(),
            [30, 10]
        );
    }
}
