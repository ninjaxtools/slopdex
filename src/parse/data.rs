//! Tree-sitter structure for configuration and markup, with bounded text search units.

use super::{Diagnostic, FileStructure, ParsedFile, StructureNode};
use anyhow::{Context, Result};
use tree_sitter::{Language, Node, Parser};

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
    let tree = parser
        .parse(source, None)
        .with_context(|| format!("Cannot parse {path}: tree-sitter returned no tree"))?;
    let mut collector = Collector {
        language,
        source,
        nodes: Vec::new(),
    };
    collector.walk(tree.root_node(), None);
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
            stack.extend(children(node));
        }
    }
    // A comment-only configuration file has no search content. HTML/XML text
    // without elements is still useful as a search unit.
    let chunks = if collector.nodes.is_empty() && !matches!(language, "xml" | "html") {
        Vec::new()
    } else {
        super::markdown::parse_plain(source)
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

fn text<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn unquote(name: &str) -> String {
    let name = name.trim();
    serde_json::from_str::<String>(name)
        .unwrap_or_else(|_| name.trim_matches(['\'', '"']).to_owned())
}

struct Collector<'a> {
    language: &'static str,
    source: &'a str,
    nodes: Vec<StructureNode>,
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
        if node.has_error() && (node.is_error() || node.is_missing()) {
            return;
        }
        let syntax = node.kind();
        if !node.has_error()
            && matches!(
                (self.language, syntax),
                ("json", "array") | ("yaml", "block_sequence" | "flow_sequence")
            )
        {
            for (index, child) in children(node).into_iter().enumerate() {
                let name = format!("[{index}]");
                let id = self.push(child, parent, "item", name.clone(), name);
                self.walk(child, Some(id));
            }
            return;
        }
        let source = self.source;
        let mut descendants = None;
        let declaration = if node.has_error() {
            None
        } else {
            match (self.language, syntax) {
                ("json", "pair") => {
                    let key = node.child_by_field_name("key");
                    let value = node.child_by_field_name("value");
                    key.map(|key| {
                        descendants = value;
                        let name = unquote(text(source, key));
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
                    key.map(|key| {
                        descendants = node.child_by_field_name("value");
                        (
                            "field",
                            unquote(text(source, key)),
                            text(source, node)
                                .lines()
                                .next()
                                .unwrap_or("")
                                .trim()
                                .to_owned(),
                        )
                    })
                }
                ("toml", "table" | "table_array_element") => {
                    let header = children(node).into_iter().find(|child| {
                        matches!(child.kind(), "bare_key" | "quoted_key" | "dotted_key")
                    });
                    header.map(|header| {
                        let name = unquote(text(source, header));
                        (
                            "module",
                            name,
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
                    .map(|key| {
                        (
                            "field",
                            unquote(text(source, key)),
                            text(source, node).trim().to_owned(),
                        )
                    }),
                ("terraform", "block") => {
                    let name = children(node)
                        .into_iter()
                        .take_while(|child| child.kind() != "block_start")
                        .filter(|child| matches!(child.kind(), "identifier" | "string_lit"))
                        .map(|child| unquote(text(source, child)))
                        .collect::<Vec<_>>()
                        .join(".");
                    (!name.is_empty()).then(|| {
                        (
                            "module",
                            name,
                            text(source, node)
                                .split('{')
                                .next()
                                .unwrap_or("")
                                .trim()
                                .to_owned(),
                        )
                    })
                }
                ("terraform", "attribute") => children(node)
                    .into_iter()
                    .find(|child| child.kind() == "identifier")
                    .map(|key| {
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
                ("css", "rule_set") => children(node)
                    .into_iter()
                    .find(|child| child.kind() == "selectors")
                    .map(|selector| {
                        (
                            "rule",
                            text(source, selector).trim().to_owned(),
                            text(source, selector).trim().to_owned(),
                        )
                    }),
                ("css", "declaration") => children(node)
                    .into_iter()
                    .find(|child| child.kind() == "property_name")
                    .map(|key| {
                        (
                            "field",
                            text(source, key).to_owned(),
                            text(source, node).trim().trim_end_matches(';').to_owned(),
                        )
                    }),
                ("xml" | "html", "element" | "script_element" | "style_element") => {
                    let tag = children(node).into_iter().find(|child| {
                        matches!(
                            child.kind(),
                            "STag" | "EmptyElemTag" | "start_tag" | "self_closing_tag"
                        )
                    });
                    tag.and_then(|tag| {
                        children(tag)
                            .into_iter()
                            .find(|child| matches!(child.kind(), "Name" | "tag_name"))
                            .map(|name| {
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
            Some(self.push(node, parent, kind, name, signature))
        });
        if let Some(value) = descendants {
            self.walk(value, parent);
        } else {
            for child in children(node) {
                self.walk(child, parent);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::parse::{language_for_path, parse};

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
