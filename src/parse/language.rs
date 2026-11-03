//! Central typed vocabularies for languages and declaration kinds.
//!
//! [`LanguageId`] owns the single language registry: serialized name, file
//! extensions, format group, and tree-sitter grammar. [`NodeKind`] is the
//! closed set of semantic declaration kinds a [`super::StructureNode`] can
//! carry. Both serialize as their historic string names, so JSON payloads,
//! database TEXT columns, and cache keys built from those strings are
//! unchanged by the typing.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::formats::FormatGroup;

/// One supported source language. Variant order must match [`LANGUAGES`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LanguageId {
    TypeScript,
    Tsx,
    #[default]
    JavaScript,
    Jsx,
    Python,
    Rust,
    Go,
    Java,
    C,
    Bash,
    Markdown,
    Json,
    Terraform,
    Yaml,
    Toml,
    Xml,
    Html,
    Css,
}

struct LanguageInfo {
    id: LanguageId,
    name: &'static str,
    extensions: &'static [&'static str],
    group: FormatGroup,
}

/// The single language registry table: name, extensions, and format group.
/// Grammar selection lives in [`LanguageId::grammar`], keyed by the same enum.
const LANGUAGES: [LanguageInfo; 18] = [
    LanguageInfo {
        id: LanguageId::TypeScript,
        name: "typescript",
        extensions: &["ts", "mts", "cts"],
        group: FormatGroup::Code,
    },
    LanguageInfo {
        id: LanguageId::Tsx,
        name: "tsx",
        extensions: &["tsx"],
        group: FormatGroup::Code,
    },
    LanguageInfo {
        id: LanguageId::JavaScript,
        name: "javascript",
        extensions: &["js", "mjs", "cjs"],
        group: FormatGroup::Code,
    },
    LanguageInfo {
        id: LanguageId::Jsx,
        name: "jsx",
        extensions: &["jsx"],
        group: FormatGroup::Code,
    },
    LanguageInfo {
        id: LanguageId::Python,
        name: "python",
        extensions: &["py", "pyw"],
        group: FormatGroup::Code,
    },
    LanguageInfo {
        id: LanguageId::Rust,
        name: "rust",
        extensions: &["rs"],
        group: FormatGroup::Code,
    },
    LanguageInfo {
        id: LanguageId::Go,
        name: "go",
        extensions: &["go"],
        group: FormatGroup::Code,
    },
    LanguageInfo {
        id: LanguageId::Java,
        name: "java",
        extensions: &["java"],
        group: FormatGroup::Code,
    },
    LanguageInfo {
        id: LanguageId::C,
        name: "c",
        extensions: &["c", "h"],
        group: FormatGroup::Code,
    },
    LanguageInfo {
        id: LanguageId::Bash,
        name: "bash",
        extensions: &["sh", "bash", "zsh"],
        group: FormatGroup::Code,
    },
    LanguageInfo {
        id: LanguageId::Markdown,
        name: "markdown",
        extensions: &["md", "markdown"],
        group: FormatGroup::Docs,
    },
    LanguageInfo {
        id: LanguageId::Json,
        name: "json",
        extensions: &["json"],
        group: FormatGroup::Config,
    },
    LanguageInfo {
        id: LanguageId::Terraform,
        name: "terraform",
        extensions: &["tf", "tfvars", "hcl"],
        group: FormatGroup::Config,
    },
    LanguageInfo {
        id: LanguageId::Yaml,
        name: "yaml",
        extensions: &["yaml", "yml"],
        group: FormatGroup::Config,
    },
    LanguageInfo {
        id: LanguageId::Toml,
        name: "toml",
        extensions: &["toml"],
        group: FormatGroup::Config,
    },
    LanguageInfo {
        id: LanguageId::Xml,
        name: "xml",
        extensions: &["xml", "svg", "xsd", "xsl", "xslt"],
        group: FormatGroup::Markup,
    },
    LanguageInfo {
        id: LanguageId::Html,
        name: "html",
        extensions: &["html", "htm"],
        group: FormatGroup::Markup,
    },
    LanguageInfo {
        id: LanguageId::Css,
        name: "css",
        extensions: &["css"],
        group: FormatGroup::Markup,
    },
];

impl LanguageId {
    pub const ALL: [Self; 18] = [
        Self::TypeScript,
        Self::Tsx,
        Self::JavaScript,
        Self::Jsx,
        Self::Python,
        Self::Rust,
        Self::Go,
        Self::Java,
        Self::C,
        Self::Bash,
        Self::Markdown,
        Self::Json,
        Self::Terraform,
        Self::Yaml,
        Self::Toml,
        Self::Xml,
        Self::Html,
        Self::Css,
    ];

    fn info(self) -> &'static LanguageInfo {
        &LANGUAGES[self as usize]
    }

    /// The serialized language name, identical to the historic string form.
    pub fn as_str(self) -> &'static str {
        self.info().name
    }

    /// Parse a serialized language name; unknown names are unsupported.
    pub fn from_name(name: &str) -> Option<Self> {
        LANGUAGES
            .iter()
            .find(|info| info.name == name)
            .map(|info| info.id)
    }

    /// The language for a file path, by case-insensitive extension.
    pub fn for_path(path: &str) -> Option<Self> {
        let file = path.rsplit(['/', '\\']).next()?;
        let (stem, extension) = file.rsplit_once('.')?;
        if stem.is_empty() {
            return None;
        }
        let extension = extension.to_ascii_lowercase();
        LANGUAGES
            .iter()
            .find(|info| info.extensions.contains(&extension.as_str()))
            .map(|info| info.id)
    }

    pub fn format_group(self) -> FormatGroup {
        self.info().group
    }

    pub fn is_code(self) -> bool {
        self.format_group() == FormatGroup::Code
    }

    /// TypeScript and JavaScript share declaration syntax handling.
    pub fn is_js_family(self) -> bool {
        matches!(
            self,
            Self::TypeScript | Self::Tsx | Self::JavaScript | Self::Jsx
        )
    }

    pub fn is_typescript(self) -> bool {
        matches!(self, Self::TypeScript | Self::Tsx)
    }

    /// The tree-sitter grammar for this language. Total over the registry:
    /// every supported language has a grammar, so callers never substitute
    /// another language's grammar for an unknown name.
    pub fn grammar(self) -> tree_sitter::Language {
        match self {
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript | Self::Jsx => tree_sitter_javascript::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
            Self::Markdown => tree_sitter_md_025::LANGUAGE.into(),
            Self::Json => tree_sitter_json::LANGUAGE.into(),
            Self::Terraform => tree_sitter_hcl::LANGUAGE.into(),
            Self::Yaml => tree_sitter_yaml::LANGUAGE.into(),
            Self::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Self::Xml => tree_sitter_xml::LANGUAGE_XML.into(),
            Self::Html => tree_sitter_html::LANGUAGE.into(),
            Self::Css => tree_sitter_css::LANGUAGE.into(),
        }
    }
}

impl fmt::Display for LanguageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for LanguageId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LanguageId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::from_name(&name)
            .ok_or_else(|| D::Error::custom(format!("unsupported language {name:?}")))
    }
}

/// The closed set of semantic declaration kinds a structure node can carry.
/// Serializes as the historic kind string.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKind {
    #[default]
    Function,
    Method,
    Constructor,
    Generator,
    Class,
    Struct,
    Union,
    Enum,
    Variant,
    Interface,
    Trait,
    Impl,
    Module,
    Type,
    Field,
    Constant,
    Variable,
    Import,
    Macro,
    Heading,
    Rule,
    Element,
    Item,
}

impl NodeKind {
    pub const ALL: [Self; 23] = [
        Self::Function,
        Self::Method,
        Self::Constructor,
        Self::Generator,
        Self::Class,
        Self::Struct,
        Self::Union,
        Self::Enum,
        Self::Variant,
        Self::Interface,
        Self::Trait,
        Self::Impl,
        Self::Module,
        Self::Type,
        Self::Field,
        Self::Constant,
        Self::Variable,
        Self::Import,
        Self::Macro,
        Self::Heading,
        Self::Rule,
        Self::Element,
        Self::Item,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Constructor => "constructor",
            Self::Generator => "generator",
            Self::Class => "class",
            Self::Struct => "struct",
            Self::Union => "union",
            Self::Enum => "enum",
            Self::Variant => "variant",
            Self::Interface => "interface",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Module => "module",
            Self::Type => "type",
            Self::Field => "field",
            Self::Constant => "constant",
            Self::Variable => "variable",
            Self::Import => "import",
            Self::Macro => "macro",
            Self::Heading => "heading",
            Self::Rule => "rule",
            Self::Element => "element",
            Self::Item => "item",
        }
    }

    /// Parse a serialized kind name; unknown names are not node kinds.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }

    /// The kinds that own a callable body. The one definition every consumer
    /// (structure, filter, map, call graph) shares.
    pub fn is_callable(self) -> bool {
        matches!(
            self,
            Self::Function | Self::Method | Self::Constructor | Self::Generator
        )
    }

    pub fn is_container(self) -> bool {
        matches!(
            self,
            Self::Class
                | Self::Struct
                | Self::Union
                | Self::Enum
                | Self::Interface
                | Self::Trait
                | Self::Impl
                | Self::Module
                | Self::Type
                | Self::Variant
        )
    }
}

impl fmt::Display for NodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for NodeKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for NodeKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::from_name(&name)
            .ok_or_else(|| D::Error::custom(format!("unsupported node kind {name:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn language_registry_round_trips_names_and_groups() {
        for language in LanguageId::ALL {
            assert_eq!(LanguageId::from_name(language.as_str()), Some(language));
            assert_eq!(
                serde_json::to_value(language).unwrap(),
                json!(language.as_str())
            );
            assert_eq!(
                serde_json::from_value::<LanguageId>(json!(language.as_str())).unwrap(),
                language
            );
            // Registry table and enum order must agree, and every declared
            // extension must dispatch back to its language and group.
            assert_eq!(language.info().id, language);
            for extension in language.info().extensions {
                let path = format!("dir/file.{extension}");
                assert_eq!(LanguageId::for_path(&path), Some(language), "{path}");
                assert_eq!(FormatGroup::for_path(&path), Some(language.format_group()));
            }
        }
        for name in ["unknown", "", "TypeScript", "cpp", "code"] {
            assert_eq!(LanguageId::from_name(name), None, "{name}");
            assert!(serde_json::from_value::<LanguageId>(json!(name)).is_err());
        }
        assert!(LanguageId::Rust.is_code());
        assert!(!LanguageId::Markdown.is_code());
        assert!(LanguageId::Tsx.is_js_family());
        assert!(LanguageId::Tsx.is_typescript());
        assert!(!LanguageId::Jsx.is_typescript());
    }

    #[test]
    fn node_kind_round_trips_and_classifies() {
        for kind in NodeKind::ALL {
            assert_eq!(NodeKind::from_name(kind.as_str()), Some(kind));
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(kind.as_str()));
            assert_eq!(
                serde_json::from_value::<NodeKind>(json!(kind.as_str())).unwrap(),
                kind
            );
        }
        for name in ["", "symbol", "symbol-description", "document", "Function"] {
            assert_eq!(NodeKind::from_name(name), None, "{name}");
            assert!(serde_json::from_value::<NodeKind>(json!(name)).is_err());
        }
        let callable: Vec<_> = NodeKind::ALL
            .into_iter()
            .filter(|k| k.is_callable())
            .collect();
        assert_eq!(
            callable,
            [
                NodeKind::Function,
                NodeKind::Method,
                NodeKind::Constructor,
                NodeKind::Generator
            ]
        );
        assert!(NodeKind::Class.is_container());
    }

    #[test]
    fn typed_fields_serialize_as_the_historic_strings() {
        let node = crate::parse::StructureNode {
            language: LanguageId::Rust,
            kind: NodeKind::Function,
            ..Default::default()
        };
        let value = serde_json::to_value(&node).unwrap();
        assert_eq!(value["language"], json!("rust"));
        assert_eq!(value["kind"], json!("function"));
        let restored: crate::parse::StructureNode = serde_json::from_value(value).unwrap();
        assert_eq!(restored, node);
        // Payloads written before the typing (plain strings) still load, and
        // names outside the vocabularies are rejected instead of coerced.
        let legacy = json!({"language": "python", "kind": "method"});
        let mut raw = serde_json::to_value(&node).unwrap();
        raw["language"] = legacy["language"].clone();
        raw["kind"] = legacy["kind"].clone();
        let restored: crate::parse::StructureNode = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(restored.language, LanguageId::Python);
        assert_eq!(restored.kind, NodeKind::Method);
        raw["kind"] = json!("symbol-description");
        assert!(serde_json::from_value::<crate::parse::StructureNode>(raw).is_err());
        assert!(NodeKind::Variant.is_container());
        assert!(!NodeKind::Function.is_container());
        assert!(!NodeKind::Heading.is_container());
    }
}
