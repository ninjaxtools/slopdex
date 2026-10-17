//! Syntax-aware byte exclusions for source-line counting.
//!
//! Spans include only the excluded syntax (and attached test documentation),
//! not entire physical lines. The caller can therefore retain mixed lines and
//! ordinary blank lines while removing blank lines inside a multiline span.

use super::syntax::{children, text, unwrap_value};
use anyhow::{Context, Result};
use std::ops::Range;
use tree_sitter::{Language, Node, Parser};

/// Conventional test paths in the supported source languages. Both separator
/// spellings work independently of the host platform; this never accesses disk.
pub(crate) fn is_test_file(path: &str) -> bool {
    let Some(language) = super::language_for_path(path) else {
        return false;
    };
    let parts: Vec<_> = path.split(['/', '\\']).collect();
    let Some(file) = parts.last() else {
        return false;
    };
    let Some((stem, _)) = file.rsplit_once('.') else {
        return false;
    };
    let lower = stem.to_ascii_lowercase();
    let in_directory = |names: &[&str]| {
        parts[..parts.len() - 1]
            .iter()
            .any(|part| names.iter().any(|name| part.eq_ignore_ascii_case(name)))
    };
    let test_directory = in_directory(&["test", "tests", "__tests__"]);
    match language {
        "javascript" | "jsx" | "typescript" | "tsx" => {
            test_directory
                || matches!(lower.as_str(), "test" | "spec")
                || [".test", ".spec", "_test", "_spec"]
                    .iter()
                    .any(|suffix| lower.ends_with(suffix))
        }
        "python" => {
            test_directory
                || matches!(lower.as_str(), "test" | "tests")
                || lower.starts_with("test_")
                || lower.ends_with("_test")
        }
        "go" => lower.ends_with("_test"),
        "rust" => {
            in_directory(&["tests"])
                || lower == "tests"
                || lower.ends_with("_test")
                || lower.ends_with("_tests")
        }
        "java" => {
            test_directory
                || in_directory(&["integrationTest", "integrationTests"])
                || java_test_name(stem)
        }
        "c" | "bash" => {
            test_directory
                || matches!(lower.as_str(), "test" | "tests")
                || lower.starts_with("test_")
                || lower.starts_with("tests_")
                || lower.ends_with("_test")
                || lower.ends_with("_tests")
        }
        _ => false,
    }
}

/// Independent, sorted, disjoint, half-open UTF-8 byte ranges. Comment spans
/// include comments and Python docstrings inside tests; test spans include
/// attached documentation and only the outermost recognized tests.
#[derive(Default)]
pub(crate) struct Exclusions {
    pub comments: Vec<Range<usize>>,
    pub tests: Vec<Range<usize>>,
}

/// Collect both exclusion categories from the same parse. The categories may
/// overlap; merge their union when excluding comments and tests together.
pub(crate) fn all_ranges(path: &str, source: &str) -> Result<Exclusions> {
    collect_ranges(path, source, true, true)
}

/// Return sorted, disjoint, half-open UTF-8 byte ranges. Unknown file types are
/// unchanged. Recoverable syntax errors retain tree-sitter's recognized spans;
/// only parser initialization/failure is an error. Test recognition uses
/// conventional syntax, without import resolution or macro expansion.
pub(crate) fn excluded_ranges(
    path: &str,
    source: &str,
    exclude_comments: bool,
    exclude_tests: bool,
) -> Result<Vec<Range<usize>>> {
    if source.is_empty() || !(exclude_comments || exclude_tests) {
        return Ok(Vec::new());
    }
    if exclude_tests && is_test_file(path) {
        return Ok(std::iter::once(0..source.len()).collect());
    }
    let mut exclusions = collect_ranges(path, source, exclude_comments, exclude_tests)?;
    if exclusions.comments.is_empty() {
        return Ok(exclusions.tests);
    }
    if exclusions.tests.is_empty() {
        return Ok(exclusions.comments);
    }
    exclusions.comments.extend(exclusions.tests);
    Ok(merge(exclusions.comments))
}

fn collect_ranges(
    path: &str,
    source: &str,
    exclude_comments: bool,
    exclude_tests: bool,
) -> Result<Exclusions> {
    let mut exclusions = Exclusions::default();
    if source.is_empty() || !(exclude_comments || exclude_tests) {
        return Ok(exclusions);
    }
    let whole_test_file = exclude_tests && is_test_file(path);
    if whole_test_file && !exclude_comments {
        exclusions.tests.push(0..source.len());
        return Ok(exclusions);
    }
    let Some(language) = super::language_for_path(path) else {
        return Ok(exclusions);
    };
    if !matches!(
        language,
        "javascript"
            | "jsx"
            | "typescript"
            | "tsx"
            | "python"
            | "rust"
            | "go"
            | "java"
            | "c"
            | "bash"
    ) {
        if !exclude_comments {
            return Ok(exclusions);
        }
        // Reuse the data/document parsers' literal protection, Markdown inline
        // grammar, container markers, and byte-preserving grammar recovery.
        let mut descriptions = super::descriptions::SourceDescriptions::default();
        if language == "markdown" {
            super::markdown::parse(source, &mut descriptions)?;
        } else {
            super::data::parse(language, path, source, &mut descriptions)?;
        }
        exclusions.comments = merge(descriptions.comments);
        return Ok(exclusions);
    }
    let grammar: Language = match language {
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "c" => tree_sitter_c::LANGUAGE.into(),
        "bash" => tree_sitter_bash::LANGUAGE.into(),
        _ => tree_sitter_javascript::LANGUAGE.into(),
    };
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .with_context(|| format!("Cannot initialize {language} parser for {path}"))?;
    let mut tree = parser
        .parse(source, None)
        .with_context(|| format!("Cannot parse {path}: tree-sitter returned no tree"))?;
    if language == "bash" {
        tree = super::shell::recover(&mut parser, tree, source);
    }
    let root = tree.root_node();
    let whole_test_file = whole_test_file
        || (exclude_tests && language == "rust" && rust_inner_test_cfg(root, source));
    if whole_test_file {
        exclusions.tests.push(0..source.len());
        if !exclude_comments {
            return Ok(exclusions);
        }
    }
    let mut stack = vec![(root, whole_test_file)];
    while let Some((node, mut inside_test)) = stack.pop() {
        if comment(node) {
            // A shebang selects an interpreter; it is not a prose comment.
            if exclude_comments && !(node.start_byte() == 0 && text(source, node).starts_with("#!"))
            {
                exclusions.comments.push(node.byte_range());
            }
            continue;
        }
        if exclude_tests
            && !inside_test
            && let Some(range) = test_range(node, source, language)
        {
            exclusions.tests.push(range);
            if !exclude_comments {
                continue;
            }
            // Keep tests outermost, but still collect their nested comments
            // and docstrings independently for comment-only line totals.
            inside_test = true;
        }
        if exclude_comments && language == "python" {
            let body = match node.kind() {
                "module" => Some(node),
                "function_definition" | "class_definition" => node.child_by_field_name("body"),
                _ => None,
            };
            if let Some(value) =
                body.and_then(|body| super::code::python_docstring_node(source, body))
            {
                // Include parentheses around constant docstrings, but never an
                // assignment or the semicolon-separated statement beside one.
                let mut statement = value;
                while let Some(parent) = statement.parent() {
                    if !matches!(
                        parent.kind(),
                        "parenthesized_expression" | "expression_statement"
                    ) {
                        break;
                    }
                    statement = parent;
                }
                let end = statement
                    .next_sibling()
                    .filter(|next| next.kind() == ";")
                    .map_or(statement.end_byte(), |semicolon| semicolon.end_byte());
                exclusions.comments.push(statement.start_byte()..end);
            }
        }
        // An explicit stack makes deeply nested input independent of Rust's
        // call-stack limit. Literal contents never undergo textual scanning.
        let mut cursor = node.walk();
        stack.extend(
            node.named_children(&mut cursor)
                .map(|child| (child, inside_test)),
        );
    }
    exclusions.comments = merge(exclusions.comments);
    exclusions.tests = merge(exclusions.tests);
    Ok(exclusions)
}

fn comment(node: Node<'_>) -> bool {
    matches!(node.kind(), "comment" | "line_comment" | "block_comment")
}

fn merge(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.retain(|range| range.start < range.end);
    ranges.sort_unstable_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

fn test_range(node: Node<'_>, source: &str, language: &str) -> Option<Range<usize>> {
    let mut anchor = node;
    let is_test = match language {
        "rust"
            if (node.kind().ends_with("_item") && !node.kind().contains("attribute"))
                || node.kind().ends_with("_declaration")
                || matches!(
                    node.kind(),
                    "macro_definition" | "macro_invocation" | "expression_statement"
                ) =>
        {
            let mut previous = node.prev_named_sibling();
            let mut test = node
                .child_by_field_name("body")
                .is_some_and(|body| rust_inner_test_cfg(body, source));
            while let Some(sibling) = previous {
                if sibling.kind() == "attribute_item" {
                    test |= rust_test_attribute(sibling, source);
                    anchor = sibling;
                } else if !comment(sibling) {
                    break;
                }
                previous = sibling.prev_named_sibling();
            }
            test
        }
        "python" if matches!(node.kind(), "function_definition" | "class_definition") => {
            if let Some(parent) = node.parent().filter(|n| n.kind() == "decorated_definition") {
                anchor = parent;
            }
            let name = node
                .child_by_field_name("name")
                .map(|name| text(source, name))
                .unwrap_or("");
            if node.kind() == "function_definition" {
                name == "test" || name.starts_with("test_")
            } else {
                name.starts_with("Test")
                    || node
                        .child_by_field_name("superclasses")
                        .is_some_and(|bases| {
                            children(bases)
                                .into_iter()
                                .any(|base| qualified_tail(base, source) == Some("TestCase"))
                        })
            }
        }
        "java"
            if matches!(
                node.kind(),
                "method_declaration" | "class_declaration" | "interface_declaration"
            ) =>
        {
            let named_class = node.kind() != "method_declaration"
                && node
                    .child_by_field_name("name")
                    .is_some_and(|name| java_test_name(text(source, name)));
            named_class || java_test_annotations(node, source)
        }
        "javascript" | "jsx" | "typescript" | "tsx" if node.kind() == "call_expression" => {
            if !node
                .child_by_field_name("function")
                .is_some_and(|callee| js_test_callee(callee, source))
            {
                return None;
            }
            // A registration statement includes its semicolon. A nested call
            // excludes only its own bytes, not adjacent production expressions.
            while let Some(parent) = anchor.parent() {
                if !matches!(
                    parent.kind(),
                    "parenthesized_expression" | "await_expression" | "expression_statement"
                ) {
                    break;
                }
                anchor = parent;
            }
            true
        }
        _ => false,
    };
    is_test.then(|| attached_start(anchor, source)..anchor.end_byte().max(node.end_byte()))
}

fn attached_start(mut node: Node<'_>, source: &str) -> usize {
    let mut start = node.start_byte();
    while let Some(previous) = node.prev_named_sibling() {
        if !comment(previous) {
            break;
        }
        let end = previous.start_byte() + text(source, previous).trim_end().len();
        let prefix = previous.start_byte() - previous.start_position().column;
        let gap = &source[end..start];
        if !source[prefix..previous.start_byte()].trim().is_empty()
            || !gap.chars().all(char::is_whitespace)
            || gap.bytes().filter(|byte| *byte == b'\n').count() > 1
        {
            break;
        }
        start = previous.start_byte();
        node = previous;
    }
    start
}

fn java_test_name(name: &str) -> bool {
    name.starts_with("Test")
        || ["Test", "Tests", "TestCase", "IT", "ITCase"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

fn java_test_annotations(node: Node<'_>, source: &str) -> bool {
    let mut annotations = children(node);
    if let Some(modifiers) = annotations.iter().find(|n| n.kind() == "modifiers") {
        annotations.extend(children(*modifiers));
    }
    annotations.into_iter().any(|annotation| {
        matches!(annotation.kind(), "annotation" | "marker_annotation")
            && annotation.child_by_field_name("name").is_some_and(|name| {
                matches!(
                    text(source, name).rsplit('.').next().unwrap_or(""),
                    "Test"
                        | "ParameterizedTest"
                        | "RepeatedTest"
                        | "TestFactory"
                        | "TestTemplate"
                        | "Theory"
                        | "Nested"
                        | "Before"
                        | "After"
                        | "BeforeEach"
                        | "AfterEach"
                        | "BeforeAll"
                        | "AfterAll"
                        | "BeforeClass"
                        | "AfterClass"
                        | "BeforeMethod"
                        | "AfterMethod"
                        | "BeforeSuite"
                        | "AfterSuite"
                )
            })
    })
}

fn qualified_tail<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    match node.kind() {
        "identifier" => Some(text(source, node)),
        "attribute" => node
            .child_by_field_name("attribute")
            .map(|name| text(source, name)),
        _ => None,
    }
}

fn js_test_callee(mut node: Node<'_>, source: &str) -> bool {
    // Traverse only syntactic callees, never callback bodies or literal text.
    loop {
        node = unwrap_value(node);
        match node.kind() {
            "identifier" => {
                return matches!(
                    text(source, node),
                    "describe"
                        | "it"
                        | "test"
                        | "suite"
                        | "context"
                        | "specify"
                        | "xdescribe"
                        | "xit"
                        | "xtest"
                        | "fdescribe"
                        | "fit"
                        | "beforeEach"
                        | "afterEach"
                        | "beforeAll"
                        | "afterAll"
                );
            }
            "member_expression" => {
                let Some(property) = node.child_by_field_name("property") else {
                    return false;
                };
                let Some(object) = node.child_by_field_name("object") else {
                    return false;
                };
                if text(source, property) == "test"
                    && object.kind() == "identifier"
                    && matches!(text(source, object), "Deno" | "Bun" | "QUnit")
                {
                    return true;
                }
                if !matches!(
                    text(source, property),
                    "only"
                        | "skip"
                        | "todo"
                        | "each"
                        | "concurrent"
                        | "serial"
                        | "parallel"
                        | "failing"
                        | "fails"
                        | "runIf"
                        | "skipIf"
                        | "sequential"
                ) {
                    return false;
                }
                node = object;
            }
            "call_expression" => {
                let Some(callee) = node.child_by_field_name("function") else {
                    return false;
                };
                node = callee;
            }
            _ => return false,
        }
    }
}

fn rust_inner_test_cfg(node: Node<'_>, source: &str) -> bool {
    children(node)
        .into_iter()
        .any(|child| child.kind() == "inner_attribute_item" && rust_test_attribute(child, source))
}

fn rust_test_attribute(node: Node<'_>, source: &str) -> bool {
    let Some(attribute) = children(node).into_iter().find(|n| n.kind() == "attribute") else {
        return false;
    };
    let Some(path) = children(attribute)
        .into_iter()
        .find(|n| matches!(n.kind(), "identifier" | "scoped_identifier"))
    else {
        return false;
    };
    let name = text(source, path);
    if matches!(
        name.rsplit("::").next().unwrap_or(""),
        "test" | "bench" | "rstest" | "test_case" | "quickcheck"
    ) {
        return true;
    }
    if name != "cfg" {
        return false;
    }
    let Some(arguments) = attribute.child_by_field_name("arguments") else {
        return false;
    };
    let expressions = cfg_expressions(arguments);
    // Exclude only conditions that cannot enable production code. In
    // particular, cfg(any(test, unix)) and cfg(not(test)) must remain.
    expressions.len() == 1
        && cfg_value(&expressions[0], source, false, 0) == Some(false)
        && cfg_value(&expressions[0], source, true, 0) != Some(false)
}

fn cfg_expressions(node: Node<'_>) -> Vec<Vec<Node<'_>>> {
    let mut cursor = node.walk();
    let mut expressions = vec![Vec::new()];
    for token in node.children(&mut cursor) {
        match token.kind() {
            "(" | ")" | "[" | "]" | "{" | "}" | "line_comment" | "block_comment" => {}
            "," => expressions.push(Vec::new()),
            _ => expressions.last_mut().unwrap().push(token),
        }
    }
    expressions.retain(|expression| !expression.is_empty());
    expressions
}

/// Evaluate with the supplied `test` predicate; all other predicates are
/// unknown. Bound metadata recursion separately from source-tree traversal.
fn cfg_value(expression: &[Node<'_>], source: &str, test: bool, depth: usize) -> Option<bool> {
    if depth >= 128 {
        return None;
    }
    match expression {
        [name] if name.kind() == "identifier" && text(source, *name) == "test" => Some(test),
        [name, arguments] if name.kind() == "identifier" && arguments.kind() == "token_tree" => {
            let expressions = cfg_expressions(*arguments);
            let values: Vec<_> = expressions
                .iter()
                .map(|e| cfg_value(e, source, test, depth + 1))
                .collect();
            match text(source, *name) {
                "all" if values.contains(&Some(false)) => Some(false),
                "all" if values.iter().all(|value| *value == Some(true)) => Some(true),
                "any" if values.contains(&Some(true)) => Some(true),
                "any" if values.iter().all(|value| *value == Some(false)) => Some(false),
                "not" if values.len() == 1 => values[0].map(|value| !value),
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_all_modes(path: &str, source: &str) -> Exclusions {
        let exclusions = all_ranges(path, source).unwrap();
        for ranges in [&exclusions.comments, &exclusions.tests] {
            for (index, range) in ranges.iter().enumerate() {
                assert!(range.start < range.end && range.end <= source.len());
                assert!(source.is_char_boundary(range.start) && source.is_char_boundary(range.end));
                if index > 0 {
                    assert!(ranges[index - 1].end < range.start);
                }
            }
        }
        for (comments, tests) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut expected = Vec::new();
            if comments {
                expected.extend_from_slice(&exclusions.comments);
            }
            if tests {
                expected.extend_from_slice(&exclusions.tests);
            }
            assert_eq!(
                excluded_ranges(path, source, comments, tests).unwrap(),
                merge(expected),
                "{path}: comments={comments}, tests={tests}"
            );
        }
        exclusions
    }

    fn remaining(path: &str, source: &str, comments: bool, tests: bool) -> String {
        let ranges = excluded_ranges(path, source, comments, tests).unwrap();
        for (index, range) in ranges.iter().enumerate() {
            assert!(
                range.start < range.end && range.end <= source.len(),
                "{path}: {range:?}"
            );
            assert!(source.is_char_boundary(range.start) && source.is_char_boundary(range.end));
            if index > 0 {
                assert!(ranges[index - 1].end < range.start);
            }
        }
        source
            .char_indices()
            .map(|(offset, character)| {
                if character != '\n' && ranges.iter().any(|range| range.contains(&offset)) {
                    ' '
                } else {
                    character
                }
            })
            .collect()
    }

    #[test]
    fn conventional_test_paths_are_language_specific_and_portable() {
        for path in [
            "src/widget.test.ts",
            "src/widget.spec.tsx",
            "src/widget_test.mjs",
            "src/test.ts",
            "src/spec.jsx",
            "src/__tests__/widget.jsx",
            "test_widget.py",
            "test.py",
            "src/tests.py",
            "src/widget_test.pyw",
            "tests/helpers.py",
            "src/widget_test.go",
            "tests/integration.rs",
            "src/parser_tests.rs",
            "src/parser_test.rs",
            "src/tests.rs",
            "src/test/java/Helper.java",
            "src/WidgetTest.java",
            "src/WidgetTests.java",
            "src/TestWidget.java",
            "src/WidgetIT.java",
            "test_widget.c",
            "src/widget_test.h",
            "tests/helper.c",
            "tests/check.sh",
            "C:\\project\\__tests__\\widget.TS",
            "C:\\project\\tests\\helper.rs",
        ] {
            assert!(is_test_file(path), "{path}");
            assert_eq!(
                excluded_ranges(path, "\ncode\n\n", false, true).unwrap(),
                std::iter::once(0..7).collect::<Vec<_>>()
            );
        }
        for path in [
            "src/testable.ts",
            "src/contest.py",
            "src/latest.rs",
            "src/testing.go",
            "tests/README.md",
            "tests/config.json",
            "tests/data.yaml",
            "test.rs",
            "src/Contest.java",
            "test.cpp",
            "tests/file.txt",
            "src/test_helpers.go",
        ] {
            assert!(!is_test_file(path), "{path}");
        }
    }

    #[test]
    fn every_code_language_uses_grammar_comment_nodes_not_literal_delimiters() {
        for (path, source, literal) in [
            (
                "main.ts",
                "// remove\nconst keep = '// literal'; /* remove */\n",
                "'// literal'",
            ),
            (
                "main.tsx",
                "/* remove */\nconst keep = <div>{'// literal'}</div>;\n",
                "'// literal'",
            ),
            (
                "main.js",
                "// remove\nconst keep = /[/][*]literal/; /* remove */\n",
                "/[/][*]literal/",
            ),
            (
                "main.jsx",
                "/* remove */\nconst keep = <div>{'/* literal */'}</div>;\n",
                "'/* literal */'",
            ),
            (
                "main.py",
                "# remove\nkeep = '# literal' # remove\n",
                "'# literal'",
            ),
            (
                "main.rs",
                "// remove\nconst KEEP: &str = r#\"/* literal */\"#; /* outer /* remove */ remove */\n",
                "r#\"/* literal */\"#",
            ),
            (
                "main.go",
                "package main\n// remove\nvar keep = `// literal` /* remove */\n",
                "`// literal`",
            ),
            (
                "Main.java",
                "// remove\nclass Main { String keep = \"/* literal */\"; /* remove */ }\n",
                "\"/* literal */\"",
            ),
            (
                "main.c",
                "/* remove */\nchar *keep = \"// literal\"; // remove\n",
                "\"// literal\"",
            ),
            (
                "main.sh",
                "# remove\nkeep='# literal' # remove\ncat <<'END'\n# literal heredoc\nEND\n",
                "# literal heredoc",
            ),
        ] {
            assert_all_modes(path, source);
            let kept = remaining(path, source, true, false);
            assert!(!kept.contains("remove"), "{path}: {kept}");
            assert!(kept.contains(literal), "{path}: {kept}");
            assert!(
                kept.contains("keep") || kept.contains("KEEP"),
                "{path}: {kept}"
            );
            assert!(
                excluded_ranges(path, source, false, false)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn data_markup_and_document_comments_respect_literals() {
        for (path, source, literal) in [
            (
                "config.json",
                "// remove\n{\"keep\": \"// literal /* literal */ # literal\"} /* remove */\n",
                "// literal /* literal */ # literal",
            ),
            (
                "config.yaml",
                "# remove\nkeep: '# literal' # remove\nblock: |\n  # literal block\n",
                "# literal block",
            ),
            (
                "config.toml",
                "# remove\nkeep = '# literal' # remove\n",
                "'# literal'",
            ),
            (
                "config.tf",
                "# remove\n// remove\n/* remove */\nkeep = \"// literal\"\n",
                "\"// literal\"",
            ),
            (
                "main.html",
                "<!-- remove -->\n<p title='<!-- literal -->'>keep</p>\n<script>const keep = '<!-- literal -->';</script>\n",
                "'<!-- literal -->'",
            ),
            (
                "main.xml",
                "<!-- remove -->\n<keep><![CDATA[<!-- literal -->]]></keep>\n",
                "<![CDATA[<!-- literal -->]]>",
            ),
            (
                "main.css",
                "/* remove */\np { content: '/* literal */'; }\n",
                "'/* literal */'",
            ),
            (
                "README.md",
                "<!-- remove -->\nkeep <!-- remove --> text\n`<!-- literal -->`\n\n```html\n<!-- literal fence -->\n```\n",
                "<!-- literal fence -->",
            ),
        ] {
            assert_all_modes(path, source);
            let kept = remaining(path, source, true, false);
            assert!(!kept.contains("remove"), "{path}: {kept}");
            assert!(kept.contains(literal), "{path}: {kept}");
            assert!(
                excluded_ranges(path, source, false, true)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn comment_spans_preserve_mixed_lines_unicode_crlf_and_external_blanks() {
        let source = "\r\n/* α\r\n\r\nend */ let keep = 1; // β\r\n\r\n";
        let ranges = excluded_ranges("main.ts", source, true, false).unwrap();
        assert_eq!(ranges.len(), 2);
        assert_eq!(&source[ranges[0].clone()], "/* α\r\n\r\nend */");
        assert_eq!(&source[ranges[1].clone()], "// β");
        assert!(!ranges.iter().any(|range| range.contains(&0)));
        assert!(
            !ranges
                .iter()
                .any(|range| range.contains(&(source.len() - 1)))
        );
        assert!(remaining("main.ts", source, true, false).contains("let keep = 1;"));
    }

    #[test]
    fn python_docstrings_are_only_leading_constant_text() {
        let source = "\"\"\"remove module\"\"\"\nvalue = '# keep'\nclass Main:\n    (r\"remove class\")\n    def run(self):\n        # remove\n        (\"remove \" \"function\")\n        value = \"keep assigned\"\n        \"keep nonleading\"\n        return value\ndef raw():\n    b'keep bytes'\ndef formatted():\n    f'keep formatted'\nif value:\n    'keep conditional'\n";
        let kept = remaining("main.py", source, true, false);
        assert!(!kept.contains("remove"), "{kept}");
        for literal in [
            "# keep",
            "keep assigned",
            "keep nonleading",
            "keep bytes",
            "keep formatted",
            "keep conditional",
        ] {
            assert!(kept.contains(literal), "{kept}");
        }
        assert!(!kept.contains("(r"));
    }

    #[test]
    fn rust_tests_include_attributes_docs_nested_items_and_test_only_cfgs() {
        let source = r##"// production header

fn keep() { let value = "#[test] fn fake() {}"; }
// remove attached docs
#[allow(dead_code)]
#[cfg(all(test, feature = "extra"))]
mod checks {

    #[test]
    fn remove_nested() {}
}
/// remove async docs
#[tokio::test(flavor = "multi_thread")]
async fn remove_async() {}
#[bench]
fn remove_bench() {}
#[cfg(any(test, unix))]
fn keep_any() {}
#[cfg(not(test))]
fn keep_not() {}
#[cfg(feature = "test")]
fn keep_feature() {}
#[cfg(any())]
fn keep_disabled() {}
#[cfg(test)]
use crate::remove_import;
#[cfg(not(not(test)))]
fn remove_double_negation() {}
mod production {
    #[test]
    fn remove_inline() {}
    fn keep_nested() {}
}
mod remove_inner {
    #![cfg(test)]
    fn remove_helper() {}
}
"##;
        let kept = remaining("src/main.rs", source, false, true);
        assert!(!kept.contains("remove"), "{kept}");
        for literal in [
            "production header",
            "#[test] fn fake() {}",
            "keep_any",
            "keep_not",
            "keep_feature",
            "keep_disabled",
            "keep_nested",
        ] {
            assert!(kept.contains(literal), "{kept}");
        }
        assert!(!kept.contains("allow(dead_code)"));
        let inner = "//! documentation\n#![cfg(test)]\n\nfn helper() {}\n";
        assert_eq!(
            excluded_ranges("src/main.rs", inner, false, true).unwrap(),
            std::iter::once(0..inner.len()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_attachment_does_not_consume_trailing_or_detached_comments() {
        let source = "fn keep() {} // keep trailing\n#[test]\nfn remove_one() {}\n// keep detached\n\n/// remove docs\n#[test]\nfn remove_two() {}\n";
        let kept = remaining("main.rs", source, false, true);
        assert!(!kept.contains("remove"), "{kept}");
        assert!(kept.contains("keep trailing"));
        assert!(kept.contains("keep detached"));
    }

    #[test]
    fn overlapping_comment_and_test_spans_are_unioned_without_external_blanks() {
        let source = "\n// ordinary comment\n\n/// test docs\n#[test]\nfn case() {\n    /* inside\n\n    comment */\n}\n\n";
        let exclusions = assert_all_modes("main.rs", source);
        assert_eq!(exclusions.comments.len(), 3);
        assert_eq!(exclusions.tests.len(), 1);
        let ranges = excluded_ranges("main.rs", source, true, true).unwrap();
        assert_eq!(ranges.len(), 2);
        assert!(source[ranges[0].clone()].starts_with("// ordinary"));
        assert!(source[ranges[1].clone()].starts_with("/// test docs"));
        assert!(source[ranges[1].clone()].ends_with('}'));
        assert_eq!(source[ranges[0].end..ranges[1].start].trim(), "");
        assert!(ranges[0].start > 0 && ranges[1].end < source.len() - 1);
    }

    #[test]
    fn independent_comments_descend_into_outermost_tests_and_preserve_attached_docs() {
        for (path, source, expected_comments, following) in [
            (
                "main.rs",
                "// ordinary\n\n/// attached\n#[cfg(test)]\nmod checks {\n    // inside module\n    #[test]\n    fn nested() { /* nested comment */ }\n}\nfn keep() {}\n",
                &[
                    "// ordinary",
                    "/// attached",
                    "// inside module",
                    "/* nested comment */",
                ][..],
                "\nfn keep",
            ),
            (
                "main.py",
                "# ordinary\n\n# attached\nclass TestOuter:\n    \"\"\"class docs\"\"\"\n    # inside class\n    def test_nested(self):\n        (\"function docs\");\n        # inside function\n        return '# literal'\n\ndef keep():\n    return 1\n",
                &[
                    "# ordinary",
                    "# attached",
                    "\"\"\"class docs\"\"\"",
                    "# inside class",
                    "(\"function docs\");",
                    "# inside function",
                ][..],
                "\n\ndef keep",
            ),
            (
                "main.ts",
                "// ordinary\n\n// attached\ndescribe('outer', () => {\n    /* inside suite */\n    it('nested', () => {\n        // inside case\n        use('/* literal */');\n    });\n});\nconst keep = 1;\n",
                &[
                    "// ordinary",
                    "// attached",
                    "/* inside suite */",
                    "// inside case",
                ][..],
                "\nconst keep",
            ),
            (
                "Main.java",
                "// ordinary\n\n/** attached */\nclass WidgetTests {\n    // inside class\n    @Test void nested() {\n        /* inside method */\n    }\n}\nclass Main {}\n",
                &[
                    "// ordinary",
                    "/** attached */",
                    "// inside class",
                    "/* inside method */",
                ][..],
                "\nclass Main",
            ),
        ] {
            let exclusions = assert_all_modes(path, source);
            assert_eq!(
                exclusions
                    .comments
                    .iter()
                    .map(|range| source[range.clone()].trim_end())
                    .collect::<Vec<_>>(),
                expected_comments,
                "{path}"
            );
            assert_eq!(
                exclusions.tests,
                std::iter::once(
                    source.find(expected_comments[1]).unwrap()..source.find(following).unwrap()
                )
                .collect::<Vec<_>>(),
                "{path}"
            );
        }
    }

    #[test]
    fn whole_test_files_still_collect_independent_comments_and_docstrings() {
        for (path, source, expected_comments) in [
            (
                "src/widget.test.ts",
                "// comment\ntest('case', () => { /* nested */ });\n",
                &["// comment", "/* nested */"][..],
            ),
            (
                "tests/helper.py",
                "#!/usr/bin/env python3\n\"\"\"module docs\"\"\"\n# comment\n\ndef helper():\n    \"\"\"helper docs\"\"\"\n    return '# literal'\n",
                &[
                    "\"\"\"module docs\"\"\"",
                    "# comment",
                    "\"\"\"helper docs\"\"\"",
                ][..],
            ),
            (
                "tests/helper.rs",
                "// comment\nfn helper() { let value = \"// literal\"; }\n",
                &["// comment"][..],
            ),
            (
                "src/main.rs",
                "//! module docs\n#![cfg(test)]\n\nfn helper() {\n    // comment\n}\n",
                &["//! module docs", "// comment"][..],
            ),
            (
                "widget_test.go",
                "package main\n// comment\nvar value = `// literal`\n",
                &["// comment"][..],
            ),
            (
                "WidgetTest.java",
                "// comment\nclass WidgetTest { String value = \"// literal\"; }\n",
                &["// comment"][..],
            ),
            (
                "tests/helper.c",
                "/* comment */\nchar *value = \"// literal\";\n",
                &["/* comment */"][..],
            ),
            (
                "tests/check.sh",
                "#!/bin/sh\n# comment\nvalue='# literal'\n",
                &["# comment"][..],
            ),
        ] {
            let exclusions = assert_all_modes(path, source);
            assert_eq!(
                exclusions.tests,
                std::iter::once(0..source.len()).collect::<Vec<_>>(),
                "{path}"
            );
            assert_eq!(
                exclusions
                    .comments
                    .iter()
                    .map(|range| source[range.clone()].trim_end())
                    .collect::<Vec<_>>(),
                expected_comments,
                "{path}"
            );
        }
    }

    #[test]
    fn standalone_python_docstrings_include_parentheses_and_semicolons() {
        for source in ["('remove');\n", "('remove'); keep = 1\n"] {
            let kept = remaining("main.py", source, true, false);
            assert!(!kept.contains(['(', ')', ';']), "{kept}");
            assert_eq!(kept.contains("keep = 1"), source.contains("keep = 1"));
        }
        assert!(
            remaining("main.py", "def test_remove(): pass;\n", false, true)
                .trim()
                .is_empty()
        );
    }

    #[test]
    fn javascript_registrations_and_modifiers_are_syntax_not_literal_searches() {
        let source = r#"const keep = "describe('fake', () => {})";
// remove docs
describe.only('remove suite', () => {
    beforeEach(() => setup());
    it('remove case', () => {});
});
test.each([[1], [2]])('remove each', (n) => {});
test.skip('remove skipped', () => {});
test.todo('remove todo');
(test('remove parenthesized', () => {}));
await test('remove awaited', () => {});
Deno.test('remove deno', () => {});
object.test('keep method');
const keepResult = production(test('remove nested', () => {}), 1);
test.extend({ keep: true });
"#;
        for path in ["main.ts", "main.tsx", "main.js", "main.jsx"] {
            let kept = remaining(path, source, false, true);
            assert!(!kept.contains("remove"), "{path}: {kept}");
            for literal in [
                "describe('fake'",
                "keep method",
                "const keepResult = production(",
                "test.extend({ keep: true })",
            ] {
                assert!(kept.contains(literal), "{path}: {kept}");
            }
        }
    }

    #[test]
    fn python_and_java_inline_test_declarations_include_decorators_and_docs() {
        let python = "value = 'def test_keep(): pass'\n# remove docs\n@pytest.mark.parametrize('x', [1])\ndef test_remove(x):\n    assert x\nclass TestRemove:\n    def helper(self): pass\nclass Checks(unittest.TestCase):\n    def helper(self): pass\ndef keep(): return 1\n";
        let kept = remaining("main.py", python, false, true);
        assert!(
            !kept.contains("remove") && !kept.contains("TestRemove") && !kept.contains("Checks"),
            "{kept}"
        );
        assert!(kept.contains("def test_keep(): pass") && kept.contains("def keep()"));
        let java = "class Main {\n String keep = \"@Test void literal() {}\";\n /** remove docs */\n @org.junit.jupiter.api.Test\n void removeCase() {}\n @ParameterizedTest\n void removeParameters() {}\n @BeforeEach void removeSetup() {}\n void keep() {}\n class InnerTests { void removeHelper() {} }\n}\n";
        let kept = remaining("Main.java", java, false, true);
        assert!(!kept.contains("remove"), "{kept}");
        assert!(kept.contains("@Test void literal() {}") && kept.contains("void keep()"));
    }

    #[test]
    fn unsupported_sources_and_interpreter_directives_remain_unchanged() {
        for (path, source) in [("file.txt", "// keep\n"), ("main.rs", "")] {
            let exclusions = assert_all_modes(path, source);
            assert!(exclusions.comments.is_empty() && exclusions.tests.is_empty());
        }
        assert!(
            excluded_ranges("file.txt", "// keep\n", true, true)
                .unwrap()
                .is_empty()
        );
        for (path, source) in [
            ("main.py", "#!/usr/bin/env python3\n# remove\n"),
            (
                "main.sh",
                "#!/bin/sh\n# remove\n#! remove ordinary comment\n",
            ),
        ] {
            let kept = remaining(path, source, true, true);
            assert!(kept.starts_with("#!"), "{path}: {kept}");
            assert!(!kept.contains("remove"));
        }
        assert!(
            excluded_ranges("main.rs", "", true, true)
                .unwrap()
                .is_empty()
        );
    }
}
