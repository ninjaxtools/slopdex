//! Concrete document-parser regressions exercised exclusively through the public API.

use slopdex::parse::{ParsedFile, StructureNode, parse};

fn accepted(path: &str, source: &str) -> ParsedFile {
    let parsed = parse(path, source).unwrap();
    assert!(
        parsed.errors.is_empty(),
        "{path}: accepted-source fixture has diagnostics: {:?}\n{source}",
        parsed.errors
    );
    assert!(parsed.callables.is_empty(), "{path}: {parsed:#?}");
    parsed
}

fn qualified(parsed: &ParsedFile) -> Vec<&str> {
    parsed
        .structure
        .nodes
        .iter()
        .map(|node| node.qualified_name.as_str())
        .collect()
}

fn named<'a>(parsed: &'a ParsedFile, name: &str) -> &'a StructureNode {
    parsed
        .structure
        .nodes
        .iter()
        .find(|node| node.qualified_name == name)
        .unwrap_or_else(|| {
            panic!(
                "missing {name:?}; actual nodes: {:#?}",
                parsed.structure.nodes
            )
        })
}

fn point(source: &str, byte: usize) -> (usize, usize) {
    let prefix = &source[..byte];
    (
        prefix.bytes().filter(|byte| *byte == b'\n').count() + 1,
        prefix.rsplit('\n').next().unwrap().len() + 1,
    )
}

fn ranges(parsed: &ParsedFile, source: &str) {
    for (id, node) in parsed.structure.nodes.iter().enumerate() {
        assert_eq!(node.id, id);
        assert!(node.start_byte < node.end_byte && node.end_byte <= source.len());
        assert!(source.is_char_boundary(node.start_byte));
        assert!(source.is_char_boundary(node.end_byte));
        assert_eq!(
            (node.start_line, node.start_column),
            point(source, node.start_byte),
            "start of {node:#?}"
        );
        assert_eq!(
            (node.end_line, node.end_column),
            point(source, node.end_byte),
            "end of {node:#?}"
        );
        if let Some(parent) = node.parent_id {
            assert!(parent < id);
            let parent = &parsed.structure.nodes[parent];
            assert!(parent.start_byte <= node.start_byte && node.end_byte <= parent.end_byte);
        }
    }
    for chunk in &parsed.chunks {
        assert!(chunk.start_line > 0 && chunk.start_line <= chunk.end_line);
        assert!(chunk.end_line <= source.split('\n').count());
        assert!(chunk.content.len() <= 8192);
        assert_eq!(chunk.content, chunk.embedding_input);
    }
}

#[test]
fn json_nested_arrays_and_literal_lookalikes_keep_scopes() {
    let source = "{\r\n  \"servers\": [[{\"host\": \"é🚀\"}], [{\"host\": \"b\"}]],\r\n  \"literal\": \"{\\\"fake\\\": 1}\",\r\n  \"\\u006eame\": true\r\n}";
    let parsed = accepted("nested.json", source);
    assert_eq!(
        qualified(&parsed),
        [
            "servers",
            "servers.[0]",
            "servers.[0].[0]",
            "servers.[0].[0].host",
            "servers.[1]",
            "servers.[1].[0]",
            "servers.[1].[0].host",
            "literal",
            "name"
        ]
    );
    ranges(&parsed, source);
}

#[test]
fn json_malformed_array_keeps_healthy_items_under_the_array_field() {
    let source = r#"{"servers": [{"host": "a"}, {"broken": }, {"host": "b"}], "tail": true}"#;
    let parsed = parse("recovery.json", source).unwrap();
    assert!(!parsed.errors.is_empty());
    assert!(qualified(&parsed).contains(&"tail"));
    let hosts: Vec<_> = parsed
        .structure
        .nodes
        .iter()
        .filter(|node| node.name == "host")
        .collect();
    assert_eq!(hosts.len(), 2, "{:#?}", parsed.structure.nodes);
    for host in hosts {
        let item = host.parent_id.map(|id| &parsed.structure.nodes[id]);
        assert!(
            item.is_some_and(|item| item.kind == "item"
                && item
                    .parent_id
                    .is_some_and(|id| parsed.structure.nodes[id].name == "servers")),
            "healthy host lost its array scope: {host:#?}; actual: {:#?}",
            parsed.structure.nodes
        );
    }
    ranges(&parsed, source);
}

#[test]
fn hcl_static_object_keys_are_symbols_under_their_attributes() {
    let source = "locals {\n  config = { host = \"a\", nested = { port = 8080 } }\n}\n";
    let parsed = accepted("objects.hcl", source);
    for name in [
        "locals",
        "locals.config",
        "locals.config.host",
        "locals.config.nested",
        "locals.config.nested.port",
    ] {
        named(&parsed, name);
    }
    ranges(&parsed, source);
}

#[test]
fn hcl_tuple_objects_have_distinct_item_scopes() {
    let source = "servers = [{ host = \"a\" }, { host = \"b\" }]\n";
    let parsed = accepted("servers.tfvars", source);
    named(&parsed, "servers.[0].host");
    named(&parsed, "servers.[1].host");
    ranges(&parsed, source);
}

#[test]
fn hcl_comments_strings_heredocs_and_block_labels_are_not_declarations() {
    let source = "# fake { ghost = 1 }\r\nresource \"example\" \"café\" {\r\n  text = \"fake { ghost = 2 }\"\r\n  script = <<EOF\r\nfake { ghost = 3 }\r\nEOF\r\n  /* fake { ghost = 4 } */\r\n  enabled = true\r\n}\r\n";
    let parsed = accepted("literal.tf", source);
    assert_eq!(
        qualified(&parsed),
        [
            "resource.example.café",
            "resource.example.café.text",
            "resource.example.café.script",
            "resource.example.café.enabled"
        ]
    );
    ranges(&parsed, source);
}

#[test]
fn hcl_block_signature_does_not_stop_at_a_brace_inside_a_label() {
    let source = "thing \"a{b\" {\n  enabled = true\n}\n";
    let parsed = accepted("labels.hcl", source);
    assert_eq!(named(&parsed, "thing.a{b").signature, "thing \"a{b\"");
}

#[test]
fn hcl_block_labels_decode_hcl_unicode_escapes() {
    let source = "thing \"\\U0001F680\" {\n  enabled = true\n}\n";
    let parsed = accepted("labels.hcl", source);
    assert_eq!(qualified(&parsed), ["thing.🚀", "thing.🚀.enabled"]);
}

#[test]
fn yaml_sequence_comments_do_not_become_items_or_shift_indices() {
    let source = "servers:\n  # before\n  - host: a\n  # between\n  - host: b\n";
    let parsed = accepted("comments.yaml", source);
    assert_eq!(
        qualified(&parsed),
        [
            "servers",
            "servers.[0]",
            "servers.[0].host",
            "servers.[1]",
            "servers.[1].host"
        ]
    );
    ranges(&parsed, source);
}

#[test]
fn yaml_flow_sequence_comments_do_not_become_items_or_shift_indices() {
    let source = "servers: [\n  # before\n  {host: a},\n  # between\n  {host: b}\n]\n";
    let parsed = accepted("flow.yaml", source);
    assert_eq!(
        qualified(&parsed),
        [
            "servers",
            "servers.[0]",
            "servers.[0].host",
            "servers.[1]",
            "servers.[1].host"
        ]
    );
    ranges(&parsed, source);
}

#[test]
fn yaml_quoted_keys_use_yaml_escape_rules() {
    let source = "'it''s': 1\n\"\\x41\": 2\n\"\\U0001F680\": 3\n";
    let parsed = accepted("keys.yaml", source);
    assert_eq!(qualified(&parsed), ["it's", "A", "🚀"]);
}

#[test]
fn yaml_key_tags_and_anchors_do_not_become_part_of_the_key_name() {
    let source = "!!str host: a\n&key port: 8080\n";
    let parsed = accepted("keys.yaml", source);
    assert_eq!(qualified(&parsed), ["host", "port"]);
}

#[test]
fn yaml_nested_sequences_block_scalars_aliases_and_comments_are_literal_safe() {
    let source = "servers:\r\n  - - host: é🚀\r\n  - - host: b\r\ntext: |\r\n  ghost: nope\r\n  - fake: nope\r\nfolded: >\r\n  hidden: nope\r\nvalue: &base {port: 8080}\r\nalias: *base\r\n# phantom: nope\r\n";
    let parsed = accepted("literal.yml", source);
    assert_eq!(
        qualified(&parsed),
        [
            "servers",
            "servers.[0]",
            "servers.[0].[0]",
            "servers.[0].[0].host",
            "servers.[1]",
            "servers.[1].[0]",
            "servers.[1].[0].host",
            "text",
            "folded",
            "value",
            "value.port",
            "alias"
        ]
    );
    ranges(&parsed, source);
}

#[test]
fn toml_inline_table_arrays_keep_each_items_scope() {
    let source = "servers = [[{ host = \"a\" }], [{ host = \"b\" }]]\n";
    let parsed = accepted("arrays.toml", source);
    named(&parsed, "servers.[0].[0].host");
    named(&parsed, "servers.[1].[0].host");
    ranges(&parsed, source);
}

#[test]
fn toml_array_of_tables_children_attach_to_the_current_item() {
    let source = "[[servers]]\nhost = \"a\"\n[servers.tls]\nenabled = true\n[[servers]]\nhost = \"b\"\n[servers.tls]\nenabled = false\n";
    let parsed = accepted("tables.toml", source);
    let tables: Vec<_> = parsed
        .structure
        .nodes
        .iter()
        .filter(|node| node.signature == "[servers.tls]")
        .collect();
    assert_eq!(tables.len(), 2);
    for (index, table) in tables.into_iter().enumerate() {
        assert!(
            table.parent_id.is_some(),
            "array-item subtable {index} is a root: {table:#?}"
        );
        let parent = &parsed.structure.nodes[table.parent_id.unwrap()];
        assert_eq!(parent.signature, "[[servers]]");
        assert!(parent.start_byte < table.start_byte);
        assert!(
            table.qualified_name.contains(&format!("[{index}]")),
            "{table:#?}"
        );
    }
}

#[test]
fn toml_dotted_keys_decode_each_component_without_stripping_inner_quotes() {
    let source = "\"service\".\"host\" = 1\n";
    let parsed = accepted("dotted.toml", source);
    assert_eq!(qualified(&parsed), ["service.host"]);
}

#[test]
fn toml_unicode_escapes_in_keys_are_decoded() {
    let source = "\"\\U0001F680\" = 1\n";
    let parsed = accepted("keys.toml", source);
    assert_eq!(qualified(&parsed), ["🚀"]);
}

#[test]
fn toml_multiline_literals_and_comments_do_not_create_keys() {
    let source = "# [fake]\r\n[package]\r\nname = \"é🚀\"\r\ntext = '''\r\n[ghost]\r\nhidden = 1\r\n'''\r\nquoted = \"\"\"\r\n[phantom]\r\nnope = 2\r\n\"\"\"\r\nconfig = { port = 8080 }\r\n";
    let parsed = accepted("literal.toml", source);
    assert_eq!(
        qualified(&parsed),
        [
            "package",
            "package.name",
            "package.text",
            "package.quoted",
            "package.config",
            "package.config.port"
        ]
    );
    ranges(&parsed, source);
}

#[test]
fn xml_svg_empty_elements_comments_and_cdata() {
    let source = "<?xml version=\"1.0\"?>\r\n<svg xmlns=\"http://www.w3.org/2000/svg\">\r\n<!-- <fake/> -->\r\n<g id=\"é🚀\"><path d=\"M0 0\"/><text><![CDATA[<ghost/>]]></text></g>\r\n</svg>\r\n";
    let parsed = accepted("drawing.SVG", source);
    assert_eq!(
        qualified(&parsed),
        ["svg", "svg.g", "svg.g.path", "svg.g.text"]
    );
    ranges(&parsed, source);
}

#[test]
fn xml_dtd_entities_and_references_keep_element_scopes() {
    let source =
        "<!DOCTYPE root [\n<!ENTITY sample \"literal\">\n]>\n<root><child>&sample;</child></root>";
    let parsed = accepted("entities.xml", source);
    assert_eq!(qualified(&parsed), ["root", "root.child"]);
    ranges(&parsed, source);
}

#[test]
fn html_void_elements_raw_text_and_comments_have_correct_scopes() {
    let source = "<!doctype html>\r\n<main>\r\n<!-- <fake/> -->\r\n<img alt=\"é🚀\"><br><input name=\"port\">\r\n<script>const x = '<ghost></ghost>';</script>\r\n<style>.fake { content: '<phantom/>'; }</style>\r\n<textarea><hidden></hidden></textarea>\r\n<p>Body</p>\r\n</main>";
    let parsed = accepted("page.html", source);
    assert_eq!(
        qualified(&parsed),
        [
            "main",
            "main.img",
            "main.br",
            "main.input",
            "main.script",
            "main.style",
            "main.textarea",
            "main.p"
        ]
    );
    ranges(&parsed, source);
}

#[test]
fn html_comment_only_files_have_no_search_content() {
    let parsed = accepted("comments.htm", "<!-- <fake/> -->\r\n");
    assert!(parsed.structure.nodes.is_empty());
    assert!(
        parsed.chunks.is_empty(),
        "comment indexed as text: {:#?}",
        parsed.chunks
    );
}

#[test]
fn css_keyframes_are_named_scopes_for_frame_declarations() {
    let source = "@keyframes fade {\n  from { opacity: 0; }\n  to { opacity: 1; }\n}\n";
    let parsed = accepted("animation.css", source);
    let animation = parsed
        .structure
        .nodes
        .iter()
        .find(|node| node.name == "fade");
    assert!(
        animation.is_some(),
        "missing named animation: {:#?}",
        parsed.structure.nodes
    );
    let animation = animation.unwrap();
    for declaration in parsed
        .structure
        .nodes
        .iter()
        .filter(|node| node.name == "opacity")
    {
        let frame = declaration.parent_id.map(|id| &parsed.structure.nodes[id]);
        assert!(
            frame.is_some_and(|frame| frame.parent_id == Some(animation.id)),
            "{declaration:#?}"
        );
    }
}

#[test]
fn css_selector_lists_expose_each_selector_for_symbol_filtering() {
    let source = "#app, .panel:hover, main > p { color: red; }";
    let parsed = accepted("selectors.css", source);
    let rule = &parsed.structure.nodes[0];
    assert_eq!(rule.name, "#app, .panel:hover, main > p");
    for selector in ["#app", ".panel:hover", "main > p"] {
        assert!(
            rule.names.iter().any(|name| name == selector),
            "missing selector {selector:?}: {rule:#?}"
        );
    }
}

#[test]
fn css_comments_literal_values_custom_properties_and_nested_rules() {
    let source = "/* .ghost { fake: 1; } */\r\n.panel {\r\n  --label: \"é🚀\";\r\n  content: \"<fake/>\";\r\n  & > .child { color: red; }\r\n}\r\n";
    let parsed = accepted("literal.css", source);
    assert_eq!(
        qualified(&parsed),
        [
            ".panel",
            ".panel.--label",
            ".panel.content",
            ".panel.& > .child",
            ".panel.& > .child.color"
        ]
    );
    ranges(&parsed, source);
}

#[test]
fn markdown_indented_headings_report_real_byte_columns() {
    let source = "   # Café 🚀\r\nbody\r\n  # Next\r\nlast";
    let parsed = accepted("indented.md", source);
    assert_eq!(qualified(&parsed), ["Café 🚀", "Next"]);
    ranges(&parsed, source);
}

#[test]
fn markdown_blockquote_headings_report_real_byte_columns() {
    let source = "> # Quoted\r\n> body\r\n\r\n# Next\r\nlast";
    let parsed = accepted("quoted.md", source);
    assert_eq!(qualified(&parsed), ["Quoted", "Next"]);
    ranges(&parsed, source);
}

#[test]
fn markdown_blockquote_heading_keeps_its_body_and_exact_signature() {
    let source = "> # Quoted\n> body\n\n# Next\nlast";
    let parsed = accepted("quoted.md", source);
    assert!(
        parsed
            .chunks
            .iter()
            .any(|chunk| chunk.heading_path == ["Quoted"] && chunk.content.contains("body")),
        "heading consumed the first body line: {parsed:#?}"
    );
    assert_eq!(named(&parsed, "Quoted").signature, "# Quoted");
}

#[test]
fn markdown_list_heading_keeps_its_body_and_exact_signature() {
    let source = "- # Listed\n  body\n\n# Next\nlast";
    let parsed = accepted("listed.md", source);
    assert!(
        parsed
            .chunks
            .iter()
            .any(|chunk| chunk.heading_path == ["Listed"] && chunk.content.contains("body")),
        "heading consumed the first body line: {parsed:#?}"
    );
    assert_eq!(named(&parsed, "Listed").signature, "# Listed");
    ranges(&parsed, source);
}

#[test]
fn markdown_inline_comments_are_excluded_without_removing_neighboring_prose() {
    let source = "# Real\nBefore <!-- secret --> after.\n";
    let parsed = accepted("inline.md", source);
    assert_eq!(parsed.chunks.len(), 1);
    assert!(parsed.chunks[0].content.contains("Before "));
    assert!(parsed.chunks[0].content.contains(" after."));
    assert!(
        !parsed.chunks[0].content.contains("secret"),
        "{:#?}",
        parsed.chunks
    );
}

#[test]
fn markdown_fence_closer_in_quote_requires_at_most_three_content_spaces() {
    let source = "> ```\n> example\n>     ```\n> # Hidden\n> ```\n\n# Real\nbody";
    let parsed = accepted("quote-fence.md", source);
    assert_eq!(qualified(&parsed), ["Real"]);
}

#[test]
fn markdown_fence_closer_in_list_requires_at_most_three_content_spaces() {
    let source = "- ```\n  example\n      ```\n  # Hidden\n  ```\n\n# Real\nbody";
    let parsed = accepted("list-fence.md", source);
    assert_eq!(qualified(&parsed), ["Real"]);
}

#[test]
fn json_repeated_and_dotted_keys_retain_distinct_nodes_and_parent_links() {
    let source = r#"{"same": 1, "same": 2, "a.b": 3, "a": {"b": 4}, "[0]": [{"x": 5}]}"#;
    let parsed = accepted("keys.json", source);
    let same: Vec<_> = parsed
        .structure
        .nodes
        .iter()
        .filter(|node| node.name == "same")
        .collect();
    assert_eq!(same.len(), 2);
    assert_ne!(same[0].id, same[1].id);
    let dotted = parsed
        .structure
        .nodes
        .iter()
        .find(|node| node.name == "a.b")
        .unwrap();
    let nested = parsed
        .structure
        .nodes
        .iter()
        .find(|node| node.name == "b")
        .unwrap();
    assert_eq!(dotted.parent_id, None);
    assert_eq!(nested.parent_id, Some(named(&parsed, "a").id));
    assert_ne!(dotted.id, nested.id);
    ranges(&parsed, source);
}

#[test]
fn xml_processing_instruction_text_does_not_create_elements() {
    let source = "<?note literal?>\n<root><child/></root>";
    let parsed = accepted("processing.xml", source);
    assert_eq!(qualified(&parsed), ["root", "root.child"]);
    ranges(&parsed, source);
}

#[test]
fn html_title_and_textarea_rcdata_are_not_nested_elements() {
    for tag in ["title", "textarea"] {
        let source = format!("<{tag}><fake></fake>&lt;literal&gt;</{tag}>");
        let parsed = accepted("rcdata.html", &source);
        assert_eq!(qualified(&parsed), [tag], "{tag}");
        ranges(&parsed, &source);
    }
}

#[test]
fn css_repeated_selectors_keep_distinct_nodes_and_their_own_declarations() {
    let source =
        "@media screen { .panel { color: red; } }\n@media print { .panel { color: black; } }";
    let parsed = accepted("media.css", source);
    let rules: Vec<_> = parsed
        .structure
        .nodes
        .iter()
        .filter(|node| node.kind == "rule")
        .collect();
    assert_eq!(rules.len(), 2);
    assert_ne!(rules[0].id, rules[1].id);
    for (rule, signature) in rules.iter().zip(["color: red", "color: black"]) {
        let declaration = parsed
            .structure
            .nodes
            .iter()
            .find(|node| node.signature == signature)
            .unwrap();
        assert_eq!(declaration.parent_id, Some(rule.id));
    }
    ranges(&parsed, source);
}

#[test]
fn yaml_repeated_keys_in_separate_documents_keep_distinct_nodes_and_children() {
    let source = "---\nserver:\n  host: a\n---\nserver:\n  host: b\n";
    let parsed = accepted("documents.yaml", source);
    let servers: Vec<_> = parsed
        .structure
        .nodes
        .iter()
        .filter(|node| node.name == "server")
        .collect();
    assert_eq!(servers.len(), 2);
    assert_ne!(servers[0].id, servers[1].id);
    for (server, signature) in servers.iter().zip(["host: a", "host: b"]) {
        let host = parsed
            .structure
            .nodes
            .iter()
            .find(|node| node.signature == signature)
            .unwrap();
        assert_eq!(host.parent_id, Some(server.id));
    }
    ranges(&parsed, source);
}

#[test]
fn markdown_block_comment_removal_preserves_same_line_visible_text() {
    let source = "<!-- hidden -->visible text\n\n# Real\nbody\n";
    let parsed = accepted("comments.md", source);
    assert!(
        parsed
            .chunks
            .iter()
            .any(|chunk| chunk.content.contains("visible text")),
        "visible text discarded with comment: {:#?}",
        parsed.chunks
    );
    assert!(
        parsed
            .chunks
            .iter()
            .all(|chunk| !chunk.content.contains("hidden"))
    );
}

#[test]
fn markdown_comment_with_container_prefix_is_excluded_from_search() {
    let source = "> <!-- secret\n> # Fake\n> -->\n\n# Real\nbody\n";
    let parsed = accepted("quoted-comments.md", source);
    assert_eq!(qualified(&parsed), ["Real"]);
    assert!(
        parsed
            .chunks
            .iter()
            .all(|chunk| !chunk.content.contains("secret")),
        "container comment leaked: {:#?}",
        parsed.chunks
    );
}

#[test]
fn markdown_fenced_indented_and_html_literal_headings_are_excluded() {
    let source = "# Real\r\n```md\r\n# Fenced\r\n```\r\n\r\n    # Indented\r\n\r\n<div>\r\n# Html\r\n</div>\r\n\r\nChild\r\n-----\r\n正文 🚀\r\n\r\n## Peer\r\nbody\r\n";
    let parsed = accepted("literal.markdown", source);
    assert_eq!(qualified(&parsed), ["Real", "Real.Child", "Real.Peer"]);
    assert!(
        parsed
            .chunks
            .iter()
            .any(|chunk| chunk.content.contains("# Fenced"))
    );
    ranges(&parsed, source);
}

fn recovery(path: &str, source: &str, healthy: &str) {
    let parsed = parse(path, source).unwrap();
    assert!(
        !parsed.errors.is_empty(),
        "{path}: recovery fixture did not produce a diagnostic"
    );
    assert!(
        parsed
            .structure
            .nodes
            .iter()
            .any(|node| node.qualified_name == healthy),
        "{path}: healthy sibling {healthy:?} was lost: {parsed:#?}"
    );
    ranges(&parsed, source);
}

#[test]
fn json_malformed_field_keeps_healthy_siblings() {
    recovery(
        "x.json",
        "{\"first\": 1, \"broken\": , \"last\": 2}",
        "last",
    );
}

#[test]
fn hcl_malformed_attribute_keeps_healthy_siblings() {
    recovery("x.hcl", "first = 1\nbroken = )\nlast = 2\n", "last");
}

#[test]
fn yaml_malformed_value_keeps_healthy_siblings() {
    recovery("x.yaml", "first: 1\nbroken: [a, }]\nlast: 2\n", "last");
}

#[test]
fn toml_malformed_value_keeps_healthy_siblings() {
    recovery("x.toml", "first = 1\nbroken = ?\nlast = 2\n", "last");
}

#[test]
fn xml_malformed_tag_keeps_healthy_siblings() {
    recovery("x.xml", "<root><first/><bad @/><last/></root>", "root.last");
}

#[test]
fn html_malformed_tag_keeps_healthy_siblings() {
    recovery(
        "x.html",
        "<main><p>first</p></><span>last</span></main>",
        "main.span",
    );
}

#[test]
fn css_malformed_declaration_keeps_healthy_siblings() {
    recovery(
        "x.css",
        ".first { color: red; }\n.bad { color: @; }\n.last { color: blue; }",
        ".last",
    );
}

#[test]
fn css_malformed_declaration_keeps_healthy_properties_in_their_rule_scope() {
    recovery(
        "x.css",
        ".panel { color: red; broken: @; margin: 0; }",
        ".panel.color",
    );
}

#[test]
fn data_format_comments_are_excluded_from_search_chunks() {
    let mut leaked = Vec::new();
    for (path, source) in [
        ("x.hcl", "# secret-comment\nport = 8080\n"),
        ("x.yaml", "# secret-comment\nport: 8080\n"),
        ("x.toml", "# secret-comment\nport = 8080\n"),
        ("x.xml", "<root><!-- secret-comment --><child/></root>"),
        ("x.html", "<main><!-- secret-comment --><p>body</p></main>"),
        ("x.css", "/* secret-comment */\n.panel { color: red; }"),
    ] {
        let parsed = accepted(path, source);
        assert!(!parsed.structure.nodes.is_empty(), "{path}");
        if parsed
            .chunks
            .iter()
            .any(|chunk| chunk.content.contains("secret-comment"))
        {
            leaked.push((path, parsed.chunks));
        }
    }
    assert!(
        leaked.is_empty(),
        "AST comments were indexed as document text: {leaked:#?}"
    );
}

#[test]
fn data_format_large_unicode_literals_have_lossless_bounded_search_chunks() {
    let body = "é🚀正文".repeat(2000);
    for (path, source) in [
        ("x.json", format!(r#"{{"body":"{body}"}}"#)),
        ("x.hcl", format!("body = \"{body}\"")),
        ("x.yaml", format!("body: \"{body}\"")),
        ("x.toml", format!("body = \"{body}\"")),
        ("x.xml", format!("<root>{body}</root>")),
        ("x.html", format!("<main>{body}</main>")),
        ("x.css", format!(".panel {{ content: \"{body}\"; }}")),
    ] {
        let parsed = accepted(path, &source);
        assert!(parsed.chunks.len() > 1, "{path}");
        assert_eq!(
            parsed
                .chunks
                .iter()
                .map(|chunk| chunk.content.as_str())
                .collect::<String>(),
            source,
            "{path}: chunk splitting lost content"
        );
        ranges(&parsed, &source);
    }
}

#[test]
fn recovery_preserves_original_unicode_crlf_ranges_and_signatures() {
    for (path, source, qualified, declaration, signature) in [
        (
            "x.hcl",
            "locals {\r\n  first = \"é🚀\"\r\n  broken = )\r\n  last = 2\r\n}\r\n",
            "locals.last",
            "last = 2",
            "last = 2",
        ),
        (
            "x.toml",
            "[server]\r\nfirst = \"é🚀\"\r\nbroken = ?\r\nlast = 2\r\n",
            "server.last",
            "last = 2",
            "last = 2",
        ),
        (
            "x.yaml",
            "server:\r\n  first: é🚀\r\n  broken: [a, }]\r\n  last: 2\r\n",
            "server.last",
            "last: 2",
            "last: 2",
        ),
        (
            "x.css",
            ".panel {\r\n  content: \"é🚀\";\r\n  broken: @;\r\n  margin: 0;\r\n}\r\n",
            ".panel.margin",
            "margin: 0;",
            "margin: 0",
        ),
    ] {
        let parsed = parse(path, source).unwrap();
        assert!(!parsed.errors.is_empty(), "{path}");
        let node = named(&parsed, qualified);
        assert_eq!(node.start_byte, source.find(declaration).unwrap(), "{path}");
        assert_eq!(
            source[node.start_byte..node.end_byte].trim_end_matches(['\r', '\n']),
            declaration,
            "{path}"
        );
        assert_eq!(node.signature, signature, "{path}");
        ranges(&parsed, source);
    }
}

#[test]
fn comment_filtering_keeps_literal_lookalikes_in_all_data_formats() {
    for (path, source) in [
        (
            "x.json",
            r#"{"body": "<!-- literal-comment -->", "tail": 2}"#,
        ),
        (
            "x.hcl",
            "body = \"# literal-comment\"\n# actual-comment\ntail = 2\n",
        ),
        (
            "x.yaml",
            "body: |\n  # literal-comment\n# actual-comment\ntail: 2\n",
        ),
        (
            "x.toml",
            "body = '''\n# literal-comment\n'''\n# actual-comment\ntail = 2\n",
        ),
        (
            "x.xml",
            "<root><![CDATA[<!-- literal-comment -->]]><!-- actual-comment --><tail/></root>",
        ),
        (
            "x.html",
            "<main><textarea><!-- literal-comment --></textarea><!-- actual-comment --><p>tail</p></main>",
        ),
        (
            "x.css",
            ".panel { content: \"/* literal-comment */\"; /* actual-comment */ color: red; }",
        ),
    ] {
        let parsed = accepted(path, source);
        let content = parsed
            .chunks
            .iter()
            .map(|chunk| chunk.content.as_str())
            .collect::<String>();
        assert!(content.contains("literal-comment"), "{path}: {content}");
        assert!(!content.contains("actual-comment"), "{path}: {content}");
        ranges(&parsed, source);
    }
}

#[test]
fn recovery_keeps_heredoc_multiline_string_and_block_scalar_examples_literal() {
    for (path, source, literal) in [
        (
            "x.hcl",
            "body = <<EOF\nghost = )\n# literal-comment\nEOF\nbroken = )\ntail = 2\n",
            "ghost = )",
        ),
        (
            "x.toml",
            "body = '''\nghost = ?\n# literal-comment\n'''\nbroken = ?\ntail = 2\n",
            "ghost = ?",
        ),
        (
            "x.yaml",
            "body: |\n  ghost: [a, }]\n  # literal-comment\nbroken: [a, }]\ntail: 2\n",
            "ghost: [a, }]",
        ),
    ] {
        let parsed = parse(path, source).unwrap();
        assert!(!parsed.errors.is_empty(), "{path}");
        named(&parsed, "tail");
        assert!(
            parsed
                .structure
                .nodes
                .iter()
                .all(|node| node.name != "ghost"),
            "{path}: {parsed:#?}"
        );
        let content = parsed
            .chunks
            .iter()
            .map(|chunk| chunk.content.as_str())
            .collect::<String>();
        assert!(
            content.contains(literal) && content.contains("literal-comment"),
            "{path}: {content}"
        );
        ranges(&parsed, source);
    }
}

#[test]
fn toml_nested_array_table_instances_keep_ancestry_and_containing_ranges() {
    let source = "[[\"clusters\"]]\nname = \"é🚀\"\n[[clusters.\"nodes\"]]\nhost = \"a\"\n[clusters.nodes.tls]\nenabled = true\n[[clusters.nodes]]\nhost = \"b\"\n[[clusters]]\nname = \"next\"\n[[clusters.nodes]]\nhost = \"c\"\n[clusters.nodes.tls]\nenabled = false\n";
    let parsed = accepted("nested.toml", source);
    for path in [
        "clusters.[0].nodes.[0].host",
        "clusters.[0].nodes.[0].tls.enabled",
        "clusters.[0].nodes.[1].host",
        "clusters.[1].nodes.[0].host",
        "clusters.[1].nodes.[0].tls.enabled",
    ] {
        named(&parsed, path);
    }
    ranges(&parsed, source);
}

#[test]
fn markdown_comments_keep_code_spans_fences_and_visible_container_neighbors() {
    let source = "# Real\r\nBefore `<!-- inline-literal -->` <!-- actual-comment --> after.\r\n\r\n~~~\r\n<!-- fenced-literal -->\r\n~~~\r\n\r\n    <!-- indented-literal -->\r\n\r\n> <!-- another-comment -->visible text\r\n";
    let parsed = accepted("literal.md", source);
    let content = parsed
        .chunks
        .iter()
        .map(|chunk| chunk.content.as_str())
        .collect::<String>();
    for literal in [
        "inline-literal",
        "fenced-literal",
        "indented-literal",
        "> visible text",
    ] {
        assert!(content.contains(literal), "{content}");
    }
    assert!(
        !content.contains("actual-comment") && !content.contains("another-comment"),
        "{content}"
    );
    ranges(&parsed, source);
    for source in ["> <!-- only a comment -->\n", "- <!-- only a comment -->\n"] {
        assert!(
            accepted("comments.md", source).chunks.is_empty(),
            "{source}"
        );
    }
}

#[test]
fn markdown_quoted_setext_headings_keep_signatures_ranges_and_body_lines() {
    let source = "> Café 🚀\r\n> -----\r\n> first body\r\n> second body\r\n\r\n# Next\r\nlast";
    let parsed = accepted("quoted.md", source);
    assert_eq!(qualified(&parsed), ["Café 🚀", "Next"]);
    assert_eq!(named(&parsed, "Café 🚀").signature, "Café 🚀\n-----");
    assert!(
        parsed
            .chunks
            .iter()
            .any(|chunk| chunk.heading_path == ["Café 🚀"]
                && chunk.content.contains("first body")
                && chunk.content.contains("second body")),
        "{parsed:#?}"
    );
    ranges(&parsed, source);
}

#[test]
fn html_quoted_attribute_comments_are_literal_search_content() {
    for source in [
        "<div title=\"<!-- literal -->\">body</div>",
        "<div title='<!-- literal -->'><!-- actual -->body</div>",
        "<div title=\"é🚀 <!-- literal -->\" data-value='<!-- second-literal -->'>body</div>",
    ] {
        let parsed = accepted("x.html", source);
        let content = &parsed.chunks[0].content;
        assert!(content.contains("<!-- literal -->"), "{content}");
        assert!(!content.contains("<!-- actual -->"), "{content}");
        assert_eq!(qualified(&parsed), ["div"]);
        ranges(&parsed, source);
    }
}

#[test]
fn html_raw_text_elements_preserve_literal_comments_and_markup() {
    for tag in ["iframe", "noembed", "noframes", "xmp"] {
        let source =
            format!("<{tag}><!-- literal --><fake></fake></{tag}><!-- actual --><p>body</p>");
        let parsed = accepted("x.html", &source);
        assert_eq!(qualified(&parsed), [tag, "p"]);
        let content = &parsed.chunks[0].content;
        assert!(
            content.contains("<!-- literal --><fake></fake>"),
            "{tag}: {content}"
        );
        assert!(!content.contains("<!-- actual -->"), "{tag}: {content}");
        ranges(&parsed, &source);
    }
}

#[test]
fn markdown_quoted_html_attribute_comments_are_opaque() {
    for quote in ['\'', '"'] {
        for prefix in ["", "Before ", "# Heading "] {
            let source = format!(
                "{prefix}<span title={quote}<!-- literal -->{quote}>body</span> <!-- actual --> after.\n"
            );
            let parsed = accepted("x.md", &source);
            let content = parsed
                .chunks
                .iter()
                .map(|chunk| chunk.content.as_str())
                .collect::<String>();
            // A heading needs a body before it can produce a chunk.
            let text = if prefix.starts_with('#') {
                &parsed.structure.nodes[0].signature
            } else {
                &content
            };
            assert!(text.contains("<!-- literal -->"), "{source}: {parsed:#?}");
            assert!(!text.contains("<!-- actual -->"), "{source}: {parsed:#?}");
            ranges(&parsed, &source);
        }
    }
}

#[test]
fn markdown_cdata_and_processing_instructions_protect_nested_comment_text() {
    for literal in ["<![CDATA[<!-- literal -->]]>", "<?pi <!-- literal --> ?>"] {
        for prefix in ["", "Before ", "> ", "`code` "] {
            let source =
                format!("{prefix}{literal} after. <!-- actual -->\r\n\r\n# Real\r\nbody\r\n");
            let parsed = accepted("x.md", &source);
            let content = parsed
                .chunks
                .iter()
                .map(|chunk| chunk.content.as_str())
                .collect::<String>();
            assert!(content.contains(literal), "{source}: {content}");
            assert!(!content.contains("<!-- actual -->"), "{source}: {content}");
            assert_eq!(qualified(&parsed), ["Real"]);
            ranges(&parsed, &source);
        }
    }
}

#[test]
fn toml_array_ordinals_survive_explicit_declarations_of_ordinary_supertables() {
    for source in [
        "[[a.b]]\n[a]\n[[a.b]]\n",
        "[[\"a\".\"b\"]]\n[a]\n[[a.b]]\n[[a.b]]\n",
    ] {
        let parsed = accepted("x.toml", source);
        let items: Vec<_> = parsed
            .structure
            .nodes
            .iter()
            .filter(|node| node.signature.starts_with("[["))
            .collect();
        for (index, item) in items.iter().enumerate() {
            assert_eq!(item.qualified_name, format!("a.b.[{index}]"), "{parsed:#?}");
        }
        ranges(&parsed, source);
    }
}

#[test]
fn toml_nested_array_ordinals_ignore_ordinary_parents_and_reset_for_outer_items() {
    let source = "[[outer]]\n[[outer.a.b]]\n[outer.a]\n[[outer.a.b]]\n[[outer]]\n[[outer.a.b]]\n[outer.a]\n[[outer.a.b]]\n";
    let parsed = accepted("x.toml", source);
    let items: Vec<_> = parsed
        .structure
        .nodes
        .iter()
        .filter(|node| node.signature == "[[outer.a.b]]")
        .collect();
    assert_eq!(
        items
            .iter()
            .map(|node| node.qualified_name.as_str())
            .collect::<Vec<_>>(),
        [
            "outer.[0].a.b.[0]",
            "outer.[0].a.b.[1]",
            "outer.[1].a.b.[0]",
            "outer.[1].a.b.[1]"
        ]
    );
    ranges(&parsed, source);
}

#[test]
fn yaml_folding_does_not_trim_whitespace_produced_by_escapes() {
    for (key, expected) in [
        ("a\\x20\n  b", "a  b"),
        ("a\\x20 \n  b", "a  b"),
        ("a\\t\n  b", "a\t b"),
        ("a\\u0020\r\n  b", "a  b"),
        ("a\\x20\n\n  b", "a \nb"),
        ("a\\x20\\\n  b", "a b"),
        ("a \n  b", "a b"),
    ] {
        let source = format!("? \"{key}\"\n: 1\n");
        let parsed = accepted("x.yaml", &source);
        assert_eq!(qualified(&parsed), [expected], "{source}");
        ranges(&parsed, &source);
    }
}

#[test]
fn markdown_atx_closing_hashes_are_decided_before_comment_removal() {
    for (heading, title) in [
        ("# A <!--x-->#", "A #"),
        ("# A ###<!--x-->", "A ###"),
        ("# A <!--x--> ###", "A"),
        ("# A <!--x--># ###", "A #"),
        ("# A ### <!--x-->", "A ###"),
        ("# A <!--x-->###", "A ###"),
    ] {
        let source = format!("{heading}\r\nbody\r\n");
        let parsed = accepted("x.md", &source);
        assert_eq!(qualified(&parsed), [title], "{source}");
        assert_eq!(parsed.chunks[0].heading_path, [title], "{source}");
        assert!(!parsed.chunks[0].content.contains("<!--x-->"));
        ranges(&parsed, &source);
    }
}

#[test]
fn markdown_indented_unclosed_comments_are_excluded_without_hiding_literals() {
    for prefix in [" ", "  ", "   ", ">  ", "-  "] {
        let source = format!("{prefix}<!-- secret\r\n");
        let parsed = accepted("x.md", &source);
        assert!(parsed.chunks.is_empty(), "{source}: {parsed:#?}");
        let source = format!("# Real\r\nBefore é🚀.\r\n{prefix}<!-- secret\r\n");
        let parsed = accepted("x.md", &source);
        assert_eq!(parsed.chunks[0].content, "# Real\n\nBefore é🚀.");
        assert_eq!(
            (parsed.chunks[0].start_line, parsed.chunks[0].end_line),
            (1, 2)
        );
        ranges(&parsed, &source);
    }
    for source in [
        "    <!-- literal\n",
        "` <!-- literal`\n",
        "~~~\n <!-- literal\n~~~\n",
    ] {
        let parsed = accepted("x.md", source);
        assert!(
            parsed.chunks[0].content.contains("<!-- literal"),
            "{source}"
        );
        ranges(&parsed, source);
    }
}

#[test]
fn markdown_html_attribute_protection_requires_a_real_unescaped_tag() {
    for source in [
        "Before \"<!-- actual -->\" after.",
        "`<span` title=\"<!-- actual -->\"> after.",
        "Before \\<span title=\"<!-- actual -->\"> after.",
    ] {
        let parsed = accepted("x.md", source);
        assert!(
            !parsed.chunks[0].content.contains("<!-- actual -->"),
            "{source}: {parsed:#?}"
        );
        assert!(parsed.chunks[0].content.contains("after."));
        ranges(&parsed, source);
    }
}

#[test]
fn markdown_opaque_html_spans_end_at_their_own_literal_boundaries() {
    for source in [
        "Before <span title=\"<?pi <!-- literal -->\">body</span> <!-- actual --> ?> after.",
        "Before <span title=\"<![CDATA[<!-- literal -->\">body</span> <!-- actual --> ]]> after.",
        "Before <?pi <iframe><!-- literal --> ?> <!-- actual --></iframe> after.",
        "Before <![CDATA[<iframe><!-- literal -->]]> <!-- actual --></iframe> after.",
    ] {
        let parsed = accepted("x.md", source);
        let content = &parsed.chunks[0].content;
        assert!(content.contains("<!-- literal -->"), "{source}: {content}");
        assert!(!content.contains("<!-- actual -->"), "{source}: {content}");
        assert!(content.contains("after."));
        ranges(&parsed, source);
    }
}
