//! Shared repository-relative path and symbol selection for search and structure maps.

use crate::parse::{FileStructure, StructureNode};
use anyhow::{Context, Result, bail, ensure};
use ignore::overrides::{Override, OverrideBuilder};
use regex::{RegexSet, RegexSetBuilder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct Selection {
    globs: Override,
    names: Option<RegexSet>,
    kinds: HashSet<&'static str>,
}

impl Selection {
    /// Compile `glob`, `regexp`, `ignoreCase`, and map-only `kinds` options.
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
        Ok(Self {
            globs: globs.build().context("compile selection globs")?,
            names,
            kinds,
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

    /// Declaration names use the same qualified-name contract as search. Imports
    /// additionally expose their source paths and aliases; multi-binding names
    /// are qualified in the declaration's enclosing scope.
    pub fn symbol_matches(&self, node: &StructureNode) -> bool {
        if self.name_matches(&node.qualified_name) {
            return true;
        }
        let prefix = if node.kind == "import" {
            ""
        } else {
            node.qualified_name.strip_suffix(&node.name).unwrap_or("")
        };
        node.names
            .iter()
            .any(|name| self.name_matches(&format!("{prefix}{name}")))
    }

    pub fn kind_matches(&self, kind: &str) -> bool {
        self.kinds.is_empty() || self.kinds.contains(kind)
    }

    /// Keep direct kind+regex matches and all their ancestors as context. Never
    /// expand a matched parent to its unmatched children. IDs and source order
    /// remain unchanged, even if IDs are sparse or parents follow their children.
    pub fn select_structure(&self, structure: &FileStructure) -> Vec<StructureNode> {
        let parents: HashMap<_, _> = structure
            .nodes
            .iter()
            .map(|node| (node.id, node.parent_id))
            .collect();
        let mut selected = HashSet::new();
        for node in &structure.nodes {
            if !self.kind_matches(&node.kind) || !self.symbol_matches(node) {
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

fn strings<'a>(options: &'a Value, key: &str) -> Result<Vec<&'a str>> {
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
        ] {
            assert!(Selection::compile(&options).is_err(), "{options}");
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
        let selection =
            Selection::compile(&json!({"kinds": "methods", "regexp": "^API\\.Service\\.run$"}))
                .unwrap();
        let nodes = selection.select_structure(&parsed.structure);
        assert_eq!(
            nodes
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            ["API", "Service", "run"]
        );
        let parent_only = Selection::compile(&json!({"regexp": "^API\\.Service$"})).unwrap();
        let nodes = parent_only.select_structure(&parsed.structure);
        assert_eq!(
            nodes
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            ["API", "Service"]
        );
        assert!(
            Selection::compile(&json!({"regexp": "missing"}))
                .unwrap()
                .select_structure(&parsed.structure)
                .is_empty()
        );
        assert!(
            Selection::compile(&json!({"regexp": "^run$"}))
                .unwrap()
                .select_structure(&parsed.structure)
                .is_empty()
        );
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
            Selection::compile(&json!({"regexp": "^match$", "kinds": "methods"})).unwrap();
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
