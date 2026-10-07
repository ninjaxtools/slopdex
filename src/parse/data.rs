//! Tree-sitter structure for configuration and markup, with bounded text search units.

use super::{Diagnostic, FileStructure, ParsedFile, StructureNode};
use anyhow::{Context, Result};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    ops::Range,
    sync::OnceLock,
};
use tree_sitter::{Language, Node, Parser};

#[path = "document.rs"]
pub(super) mod document;
#[path = "data_recovery.rs"]
mod recovery;
#[path = "data_strings.rs"]
mod strings;

use document::{Positions, children, text};

pub(super) fn parse(language: &'static str, path: &str, source: &str) -> Result<ParsedFile> {
    let grammar: Language = match language {
        "json" => tree_sitter_json::LANGUAGE.into(),
        "terraform" => tree_sitter_hcl::LANGUAGE.into(),
        "yaml" => tree_sitter_yaml::LANGUAGE.into(),
        "toml" => tree_sitter_toml_ng::LANGUAGE.into(),
        "xml" => tree_sitter_xml::LANGUAGE_XML.into(),
        "html" => tree_sitter_html::LANGUAGE.into(),
        "css" => tree_sitter_css::LANGUAGE.into(),
        _ => unreachable!("only data languages are dispatched here"),
    };
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .with_context(|| format!("Cannot initialize {language} parser for {path}"))?;
    let mut tree = parser
        .parse(source, None)
        .with_context(|| format!("Cannot parse {path}: tree-sitter returned no tree"))?;
    let input = if language == "css" && tree.root_node().has_error() {
        css_compatibility(source, tree.root_node())
    } else {
        Cow::Borrowed(source)
    };
    if matches!(input, Cow::Owned(_)) {
        tree = parser
            .parse(input.as_bytes(), None)
            .with_context(|| format!("Cannot parse {path}: tree-sitter returned no tree"))?;
    }
    let mut errors = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            errors.push(Diagnostic {
                message: if node.is_missing() {
                    format!("Tree-sitter expected {}.", node.kind())
                } else {
                    "Tree-sitter could not parse this source region.".into()
                },
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
            });
        } else if node.has_error() {
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
    }
    let tree = recovery::recover(&mut parser, tree, &input, language);
    let mut collector = Collector {
        language,
        source,
        positions: Positions::new(source),
        nodes: Vec::new(),
        tables: HashMap::new(),
        array_tables: HashSet::new(),
        array_counts: HashMap::new(),
    };
    collector.walk(tree.root_node(), None);
    let filtered =
        document::without_spans(source, &mut comments(tree.root_node(), language, source));
    // A comment-only configuration file has no search content. HTML/XML text
    // without elements is still useful as a search unit.
    let chunks = if collector.nodes.is_empty() && !matches!(language, "xml" | "html") {
        Vec::new()
    } else {
        super::markdown::parse_plain(&filtered)
    };
    Ok(ParsedFile {
        chunks,
        errors,
        structure: FileStructure {
            nodes: collector.nodes,
        },
        ..ParsedFile::default()
    })
}

fn css_compatibility<'a>(source: &'a str, root: Node<'_>) -> Cow<'a, str> {
    const TRIVIA: &str = r"(?:[ \t\r\n\x0c]|/\*[^*]*\*+(?:[^/*][^*]*\*+)*/)";
    static SOURCE_PATH: OnceLock<regex::Regex> = OnceLock::new();
    static CONTAINER_NAME: OnceLock<regex::Regex> = OnceLock::new();
    let mut replacements = Vec::new();
    let mut stack = vec![(root, false)];
    while let Some((node, in_block)) = stack.pop() {
        if matches!(node.kind(), "string_value" | "comment" | "js_comment") {
            continue;
        }
        if node.kind() == "at_keyword" {
            match text(source, node) {
                "@source" if !in_block => {
                    let pattern = SOURCE_PATH.get_or_init(|| {
                        regex::Regex::new(&format!(
                            r#"^{TRIVIA}*(?:"(?:[^"\\\r\n]|\\[^\r\n])*"|'(?:[^'\\\r\n]|\\[^\r\n])*'){TRIVIA}*;"#,
                        ))
                        .unwrap()
                    });
                    if pattern.is_match(&source[node.end_byte()..]) {
                        replacements.push((node.byte_range(), Some("@import")));
                    }
                }
                "@container" => {
                    let pattern = CONTAINER_NAME.get_or_init(|| {
                        regex::Regex::new(&format!(
                            r"^{TRIVIA}*(?P<name>(?:--|-?[_a-zA-Z\x{{80}}-\x{{10ffff}}])[-_a-zA-Z0-9\x{{80}}-\x{{10ffff}}]*){TRIVIA}+(?:not{TRIVIA}+)?\("
                        ))
                        .unwrap()
                    });
                    if node.parent().is_some_and(|parent| {
                        parent.kind() == "at_rule"
                            && children(parent).iter().any(|child| child.kind() == "block")
                    }) && let Some(captures) = pattern.captures(&source[node.end_byte()..])
                        && let Some(name) = captures.name("name")
                        && !matches!(
                            name.as_str().to_ascii_lowercase().as_str(),
                            "none"
                                | "and"
                                | "or"
                                | "not"
                                | "default"
                                | "initial"
                                | "inherit"
                                | "unset"
                                | "revert"
                                | "revert-layer"
                        )
                    {
                        replacements.push((node.byte_range(), Some("@media    ")));
                        replacements.push((
                            node.end_byte() + name.start()..node.end_byte() + name.end(),
                            None,
                        ));
                    }
                }
                _ => {}
            }
        }
        let in_block = in_block || matches!(node.kind(), "block" | "keyframe_block_list");
        stack.extend(children(node).into_iter().map(|child| (child, in_block)));
    }
    if replacements.is_empty() {
        return Cow::Borrowed(source);
    }
    let mut input = source.as_bytes().to_vec();
    for (range, replacement) in replacements {
        if let Some(replacement) = replacement {
            input[range].copy_from_slice(replacement.as_bytes());
        } else {
            input[range].fill(b' ');
        }
    }
    Cow::Owned(String::from_utf8(input).expect("CSS compatibility edits preserve UTF-8"))
}

fn literal(language: &str, kind: &str) -> bool {
    matches!(
        (language, kind),
        ("json", "string")
            | (
                "yaml",
                "block_scalar"
                    | "plain_scalar"
                    | "single_quote_scalar"
                    | "double_quote_scalar"
                    | "alias"
                    | "anchor"
                    | "tag"
            )
            | ("toml", "string" | "quoted_key")
            | (
                "terraform",
                "string_lit" | "quoted_template" | "heredoc_template"
            )
            | ("css", "string_value")
            | ("xml", "CDSect" | "PI")
            | ("html", "quoted_attribute_value" | "attribute_value")
    )
}

fn opening_tag(node: Node<'_>) -> Option<Node<'_>> {
    children(node).into_iter().find(|child| {
        matches!(
            child.kind(),
            "STag" | "EmptyElemTag" | "start_tag" | "self_closing_tag"
        )
    })
}

fn tag_name(tag: Node<'_>) -> Option<Node<'_>> {
    children(tag)
        .into_iter()
        .find(|child| matches!(child.kind(), "Name" | "tag_name"))
}

pub(super) fn html_text_element(node: Node<'_>, source: &str) -> bool {
    opening_tag(node).and_then(tag_name).is_some_and(|name| {
        matches!(
            text(source, name).to_ascii_lowercase().as_str(),
            "title" | "textarea" | "script" | "style" | "iframe" | "noembed" | "noframes" | "xmp"
        )
    })
}

fn comments(root: Node<'_>, language: &str, source: &str) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "comment" | "Comment" | "js_comment") {
            result.push(node.byte_range());
        } else if !(literal(language, node.kind())
            || language == "html" && html_text_element(node, source))
        {
            stack.extend(children(node));
        }
    }
    result
}

struct Collector<'a> {
    language: &'static str,
    source: &'a str,
    positions: Positions,
    nodes: Vec<StructureNode>,
    tables: HashMap<Vec<String>, usize>,
    array_tables: HashSet<usize>,
    array_counts: HashMap<(Option<usize>, Vec<String>), usize>,
}

impl Collector<'_> {
    fn push(
        &mut self,
        node: Node<'_>,
        parent: Option<usize>,
        kind: &str,
        name: String,
        signature: String,
    ) -> usize {
        let id = self.nodes.len();
        let qualified_name = parent.map_or_else(
            || name.clone(),
            |parent| format!("{}.{}", self.nodes[parent].qualified_name, name),
        );
        let start = node.start_position();
        let end = node.end_position();
        self.nodes.push(StructureNode {
            id,
            parent_id: parent,
            language: self.language.into(),
            kind: kind.into(),
            name: name.clone(),
            names: vec![name],
            qualified_name,
            signature,
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            start_line: start.row + 1,
            start_column: start.column + 1,
            end_line: end.row + 1,
            end_column: end.column + 1,
            ..StructureNode::default()
        });
        id
    }

    fn walk(&mut self, node: Node<'_>, parent: Option<usize>) {
        if node.is_missing()
            || literal(self.language, node.kind())
            || matches!(node.kind(), "comment" | "Comment" | "js_comment")
        {
            return;
        }
        let syntax = node.kind();
        if self.language == "toml" && matches!(syntax, "table" | "table_array_element") {
            self.toml_table(node);
            return;
        }
        if matches!(
            (self.language, syntax),
            ("json" | "toml", "array")
                | ("yaml", "block_sequence" | "flow_sequence")
                | ("terraform", "tuple")
        ) {
            for (index, child) in children(node)
                .into_iter()
                .filter(|child| {
                    !matches!(child.kind(), "comment" | "tuple_start" | "tuple_end")
                        && !child.is_missing()
                })
                .enumerate()
            {
                let name = format!("[{index}]");
                let id = self.push(child, parent, "item", name.clone(), name);
                self.walk(child, Some(id));
            }
            return;
        }
        let source = self.source;
        let mut descendants = None;
        let declaration = if node.is_error() {
            None
        } else {
            match (self.language, syntax) {
                ("json", "pair") => {
                    let key = node.child_by_field_name("key");
                    let value = node.child_by_field_name("value");
                    key.filter(|key| !key.has_error()).map(|key| {
                        descendants = value;
                        let name = strings::decode("json", text(source, key));
                        let signature = match value {
                            Some(value) if matches!(value.kind(), "object" | "array") => format!(
                                "{}: {}",
                                text(source, key),
                                if value.kind() == "object" { "{" } else { "[" }
                            ),
                            Some(value) => {
                                format!("{}: {}", text(source, key), text(source, value))
                            }
                            None => text(source, node).to_owned(),
                        };
                        ("field", name, signature)
                    })
                }
                ("yaml", "block_mapping_pair" | "flow_pair") => {
                    let key = node.child_by_field_name("key");
                    key.filter(|key| !key.has_error()).map(|key| {
                        descendants = node.child_by_field_name("value");
                        (
                            "field",
                            strings::yaml_key(source, key),
                            text(source, node)
                                .lines()
                                .next()
                                .unwrap_or("")
                                .trim()
                                .to_owned(),
                        )
                    })
                }
                ("toml", "pair") => children(node)
                    .into_iter()
                    .find(|child| matches!(child.kind(), "bare_key" | "quoted_key" | "dotted_key"))
                    .filter(|key| !key.has_error())
                    .map(|key| {
                        descendants = children(node)
                            .into_iter()
                            .find(|child| child.id() != key.id() && child.kind() != "comment");
                        (
                            "field",
                            strings::toml_key(source, key).join("."),
                            text(source, node).trim().to_owned(),
                        )
                    }),
                ("terraform", "block") => {
                    let start = children(node)
                        .into_iter()
                        .find(|child| child.kind() == "block_start");
                    let name = children(node)
                        .into_iter()
                        .take_while(|child| child.kind() != "block_start")
                        .filter(|child| matches!(child.kind(), "identifier" | "string_lit"))
                        .filter(|child| !child.has_error())
                        .map(|child| strings::decode("terraform", text(source, child)))
                        .collect::<Vec<_>>()
                        .join(".");
                    start.filter(|_| !name.is_empty()).map(|start| {
                        (
                            "module",
                            name,
                            source[node.start_byte()..start.start_byte()]
                                .trim()
                                .to_owned(),
                        )
                    })
                }
                ("terraform", "attribute") => children(node)
                    .into_iter()
                    .find(|child| child.kind() == "identifier")
                    .filter(|key| !key.has_error())
                    .map(|key| {
                        descendants = children(node)
                            .into_iter()
                            .find(|child| child.kind() == "expression");
                        (
                            "field",
                            text(source, key).to_owned(),
                            text(source, node)
                                .lines()
                                .next()
                                .unwrap_or("")
                                .trim()
                                .to_owned(),
                        )
                    }),
                ("terraform", "object_elem") => node
                    .child_by_field_name("key")
                    .filter(|key| !key.has_error())
                    .map(|key| {
                        descendants = node.child_by_field_name("val");
                        (
                            "field",
                            strings::hcl_key(source, key),
                            text(source, node).trim().to_owned(),
                        )
                    }),
                ("css", "rule_set") => children(node)
                    .into_iter()
                    .find(|child| child.kind() == "selectors")
                    .filter(|selector| !selector.has_error())
                    .map(|selector| {
                        (
                            "rule",
                            text(source, selector).trim().to_owned(),
                            text(source, selector).trim().to_owned(),
                        )
                    }),
                ("css", "keyframes_statement") => children(node)
                    .into_iter()
                    .find(|child| child.kind() == "keyframes_name")
                    .filter(|name| !name.has_error())
                    .map(|name| {
                        (
                            "module",
                            text(source, name).to_owned(),
                            source[node.start_byte()..name.end_byte()].trim().to_owned(),
                        )
                    }),
                ("css", "keyframe_block") => children(node)
                    .into_iter()
                    .find(|child| child.kind() == "block")
                    .map(|block| {
                        let name = source[node.start_byte()..block.start_byte()]
                            .trim()
                            .to_owned();
                        ("rule", name.clone(), name)
                    }),
                ("css", "declaration") => children(node)
                    .into_iter()
                    .find(|child| child.kind() == "property_name")
                    .filter(|key| !key.has_error())
                    .map(|key| {
                        (
                            "field",
                            text(source, key).to_owned(),
                            text(source, node).trim().trim_end_matches(';').to_owned(),
                        )
                    }),
                ("xml" | "html", "element" | "script_element" | "style_element") => {
                    let tag = opening_tag(node).filter(|tag| !tag.has_error());
                    tag.and_then(|tag| {
                        tag_name(tag).map(|name| {
                            (
                                "element",
                                text(source, name).to_owned(),
                                text(source, tag).to_owned(),
                            )
                        })
                    })
                }
                _ => None,
            }
        };
        let parent = declaration.map_or(parent, |(kind, name, signature)| {
            let id = self.push(node, parent, kind, name, signature);
            if self.language == "css"
                && syntax == "rule_set"
                && let Some(selectors) = children(node)
                    .into_iter()
                    .find(|child| child.kind() == "selectors")
            {
                for selector in children(selectors)
                    .into_iter()
                    .filter(|child| child.kind() != "comment")
                {
                    let name = text(source, selector).trim().to_owned();
                    if !self.nodes[id].names.contains(&name) {
                        self.nodes[id].names.push(name);
                    }
                }
            }
            Some(id)
        });
        if self.language == "html" && html_text_element(node, source) {
            return;
        }
        if let Some(value) = descendants {
            self.walk(value, parent);
        } else {
            for child in children(node) {
                self.walk(child, parent);
            }
        }
    }

    fn toml_table(&mut self, node: Node<'_>) {
        let header = children(node)
            .into_iter()
            .find(|child| matches!(child.kind(), "bare_key" | "quoted_key" | "dotted_key"));
        let Some(header) = header.filter(|header| !header.has_error()) else {
            for child in children(node) {
                self.walk(child, None);
            }
            return;
        };
        let path = strings::toml_key(self.source, header);
        let parent = (1..path.len())
            .rev()
            .find_map(|length| self.tables.get(&path[..length]).map(|id| (length, *id)));
        let parent_id = parent.map(|(_, id)| id);
        let mut name = path[parent.map_or(0, |(length, _)| length)..].join(".");
        if node.kind() == "table_array_element" {
            // A new outer array item invalidates its previous subtables. Nested
            // counts follow array instances, not explicitly declared ordinary
            // supertables: declaring [a] must not reset the count for [[a.b]].
            let enclosing_array = (1..path.len()).rev().find_map(|length| {
                self.tables
                    .get(&path[..length])
                    .copied()
                    .filter(|id| self.array_tables.contains(id))
            });
            self.tables.retain(|key, _| !key.starts_with(&path));
            let index = self
                .array_counts
                .entry((enclosing_array, path.clone()))
                .or_default();
            name.push_str(&format!(".[{index}]"));
            *index += 1;
        }
        let signature = text(self.source, node)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_owned();
        let id = self.push(node, parent_id, "module", name, signature);
        if node.kind() == "table_array_element" {
            self.array_tables.insert(id);
        }
        self.tables.insert(path, id);
        for child in children(node)
            .into_iter()
            .filter(|child| child.id() != header.id())
        {
            self.walk(child, Some(id));
        }
        let mut ancestor = parent_id;
        while let Some(parent) = ancestor {
            if self.nodes[parent].end_byte < node.end_byte() {
                let end = self.positions.point(node.end_byte());
                self.nodes[parent].end_byte = node.end_byte();
                self.nodes[parent].end_line = end.row + 1;
                self.nodes[parent].end_column = end.column + 1;
            }
            ancestor = self.nodes[parent].parent_id;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::parse::{language_for_path, parse};

    #[test]
    fn css_compatibility_only_rewrites_supported_at_rules() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_css::LANGUAGE.into())
            .unwrap();
        for source in [
            "@media all { @source \"./*.tsx\"; }",
            ".panel { @source \"./*.tsx\"; color: @; }",
            ".panel { content: '@source \"literal\";'; color: @; }",
            "/* @source \"comment\"; */ .panel { color: @; }",
            "@source-map \"./*.tsx\";",
            "@container none (width: 1px) { .panel { color: red; } }",
        ] {
            let tree = parser.parse(source, None).unwrap();
            assert!(
                matches!(
                    super::css_compatibility(source, tree.root_node()),
                    std::borrow::Cow::Borrowed(_)
                ),
                "unexpected normalization: {source}"
            );
        }
    }

    #[test]
    fn supported_formats_produce_nested_structure_and_search_units() {
        for (path, source, names) in [
            (
                "config.json",
                "{\"server\": {\"port\": 8080}}",
                vec!["server", "server.port"],
            ),
            (
                "main.tf",
                "resource \"aws_instance\" \"web\" {\n  ami = \"abc\"\n}\n",
                vec!["resource.aws_instance.web", "resource.aws_instance.web.ami"],
            ),
            (
                "config.yaml",
                "server:\n  port: 8080\n",
                vec!["server", "server.port"],
            ),
            (
                "Cargo.toml",
                "[package]\nname = \"sample\"\n",
                vec!["package", "package.name"],
            ),
            (
                "page.xml",
                "<site><title>Hello</title></site>",
                vec!["site", "site.title"],
            ),
            (
                "page.html",
                "<main><h1>Hello</h1></main>",
                vec!["main", "main.h1"],
            ),
            (
                "style.css",
                "main { color: red; }",
                vec!["main", "main.color"],
            ),
        ] {
            let parsed = parse(path, source).unwrap();
            let found: Vec<_> = parsed
                .structure
                .nodes
                .iter()
                .map(|node| node.qualified_name.as_str())
                .collect();
            assert_eq!(found, names, "{path}: {:#?}", parsed.structure.nodes);
            assert_eq!(parsed.chunks.len(), 1, "{path}");
            assert!(!parsed.chunks[0].content.is_empty());
            for (id, node) in parsed.structure.nodes.iter().enumerate() {
                assert_eq!(node.id, id);
                assert_eq!(node.language, language_for_path(path).unwrap());
                assert!(node.end_byte <= source.len());
            }
        }
    }

    #[test]
    fn extension_aliases_and_comment_only_files() {
        for (path, language) in [
            ("file.YML", "yaml"),
            ("file.tfvars", "terraform"),
            ("file.hcl", "terraform"),
            ("file.SVG", "xml"),
            ("file.xsd", "xml"),
            ("file.xsl", "xml"),
            ("file.xslt", "xml"),
            ("file.htm", "html"),
            ("file.CSS", "css"),
        ] {
            assert_eq!(language_for_path(path), Some(language));
        }
        for (path, source) in [
            ("x.yaml", "# just a comment"),
            ("x.tf", "# just a comment"),
            ("x.toml", "# just a comment"),
            ("x.json", " "),
        ] {
            let parsed = parse(path, source).unwrap();
            assert!(parsed.chunks.is_empty(), "{path}");
            assert!(parsed.structure.nodes.is_empty(), "{path}");
        }
    }

    #[test]
    fn malformed_json_does_not_hide_healthy_siblings() {
        let parsed = parse("config.json", "{\"good\": 1, \"bad\": , \"last\": 2}").unwrap();
        let names: Vec<_> = parsed
            .structure
            .nodes
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert!(names.contains(&"good"), "{names:?}");
        assert!(names.contains(&"last"), "{names:?}");
        assert!(!parsed.errors.is_empty());
    }

    #[test]
    fn array_items_keep_their_own_scopes() {
        for (path, source) in [
            (
                "config.json",
                "{\"servers\": [{\"host\": \"a\"}, {\"host\": \"b\"}]}",
            ),
            ("config.yaml", "servers:\n  - host: a\n  - host: b\n"),
        ] {
            let parsed = parse(path, source).unwrap();
            assert!(parsed.errors.is_empty(), "{path}: {:?}", parsed.errors);
            let names: Vec<_> = parsed
                .structure
                .nodes
                .iter()
                .map(|node| node.qualified_name.as_str())
                .collect();
            assert!(names.contains(&"servers.[0].host"), "{path}: {names:?}");
            assert!(names.contains(&"servers.[1].host"), "{path}: {names:?}");
        }
    }
}
