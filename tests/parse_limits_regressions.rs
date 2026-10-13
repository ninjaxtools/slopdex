//! Adversarial inputs must produce a result or a diagnostic, never a crash or
//! a hang: pathological nesting depth and heredocs full of expansions.

mod parse_support;

use parse_support::{callables, clean, named};
use slopdex::parse::parse;
use std::time::{Duration, Instant};

const DEPTH: usize = 10_000;

fn nested(prefix: &str, open: &str, inner: &str, close: &str, suffix: &str) -> String {
    format!(
        "{prefix}{}{inner}{}{suffix}",
        open.repeat(DEPTH),
        close.repeat(DEPTH)
    )
}

/// Runs on a thread with the default test stack to prove the walkers bound
/// their own recursion instead of relying on a generous main-thread stack.
fn parse_in_test_thread(path: &'static str, source: String) -> slopdex::parse::ParsedFile {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || parse(path, &source).unwrap())
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn deeply_nested_javascript_declares_surrounding_symbols_with_a_diagnostic() {
    let source = nested(
        "function before() { helper(); }\nconst x = ",
        "(",
        "1",
        ")",
        ";\nfunction after() { helper(); }\n",
    );
    let parsed = parse_in_test_thread("deep.js", source);
    let names: Vec<_> = parsed
        .structure
        .nodes
        .iter()
        .map(|node| node.qualified_name.as_str())
        .collect();
    assert_eq!(names, ["before", "x", "after"]);
    assert_eq!(parsed.callables.len(), 2);
    assert!(
        parsed
            .errors
            .iter()
            .any(|error| error.message.contains("Nesting deeper")),
        "{:?}",
        parsed.errors
    );
    assert_eq!(named(&parsed, "after").calls.len(), 1);
}

#[test]
fn deeply_nested_javascript_inside_a_callable_keeps_the_callable() {
    let source = nested("function deep() { return ", "[", "1", "]", "; }\n");
    let parsed = parse_in_test_thread("deep_body.js", source);
    assert_eq!(parsed.callables.len(), 1);
    assert_eq!(parsed.callables[0].name, "deep");
    assert!(!parsed.errors.is_empty());
}

#[test]
fn deeply_nested_python_declares_surrounding_symbols() {
    let source = nested(
        "def before():\n    pass\n\nx = ",
        "[",
        "1",
        "]",
        "\n\ndef after():\n    pass\n",
    );
    let parsed = parse_in_test_thread("deep.py", source);
    let names: Vec<_> = parsed
        .callables
        .iter()
        .map(|callable| callable.name.as_str())
        .collect();
    assert_eq!(names, ["before", "after"]);
    assert!(parsed.structure.nodes.iter().any(|node| node.name == "x"));
}

#[test]
fn deeply_nested_rust_declares_surrounding_symbols() {
    let source = nested(
        "fn before() {}\nfn deep() { let x = ",
        "(",
        "1",
        ")",
        "; }\nfn after() {}\n",
    );
    let parsed = parse_in_test_thread("deep.rs", source);
    let names: Vec<_> = parsed
        .callables
        .iter()
        .map(|callable| callable.name.as_str())
        .collect();
    assert_eq!(names, ["before", "deep", "after"]);
    assert!(
        parsed
            .errors
            .iter()
            .any(|e| e.message.contains("Nesting deeper"))
    );
}

#[test]
fn deeply_nested_json_and_yaml_keep_every_item() {
    for (path, source) in [
        ("deep.json", nested("{\"items\": ", "[", "1", "]", "}\n")),
        ("deep.yaml", nested("items: ", "[", "1", "]", "\n")),
    ] {
        let parsed = parse_in_test_thread(path, source);
        assert!(parsed.errors.is_empty(), "{path}: {:?}", parsed.errors);
        // The data walker is iterative: one `items` field plus every nested item.
        assert_eq!(parsed.structure.nodes.len(), DEPTH + 1, "{path}");
        assert_eq!(parsed.structure.nodes[0].name, "items", "{path}");
        assert!(
            parsed
                .structure
                .nodes
                .iter()
                .skip(1)
                .all(|node| node.kind == "item" && node.name == "[0]"),
            "{path}"
        );
        assert!(!parsed.chunks.is_empty(), "{path}");
    }
}

#[test]
fn heredoc_with_thousands_of_expansions_parses_in_linear_time() {
    let body = "$(x) ".repeat(3000);
    let source = format!("f() {{\ncat <<EOF\n{body}\nEOF\n}}\nafter() {{ :; }}\n");
    let started = Instant::now();
    let parsed = clean("expansions.sh", &source);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "took {:?}",
        started.elapsed()
    );
    let names: Vec<_> = parsed
        .callables
        .iter()
        .map(|callable| callable.name.as_str())
        .collect();
    assert_eq!(names, ["f", "after"]);
    let calls: Vec<_> = named(&parsed, "f")
        .calls
        .iter()
        .map(|site| site.name.as_str())
        .collect();
    assert_eq!(calls, ["cat", "x"]);
}

#[test]
fn heredoc_header_with_many_expansions_parses_in_linear_time() {
    let header = "$(x) ".repeat(3000);
    let source = format!("cat {header}<<EOF\nliteral $(y)\nEOF\nf() {{ :; }}\n");
    let started = Instant::now();
    let parsed = clean("header.sh", &source);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(parsed.callables.len(), 1);
}

#[test]
fn call_sites_keep_walk_order_and_deduplicate_per_owner() {
    let parsed = clean(
        "calls.ts",
        "function outer() {\n  third(); first();\n  function inner() { second(); first(); second(); }\n  first(); inner();\n}\nfunction lone() { first(); }\n",
    );
    let calls = |name: &str| {
        named(&parsed, name)
            .calls
            .iter()
            .map(|site| site.name.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(calls("outer"), ["third", "first", "inner"]);
    assert_eq!(calls("outer.inner"), ["second", "first"]);
    assert_eq!(calls("lone"), ["first"]);
    assert_eq!(callables(&parsed).len(), 3);
}

#[test]
fn unterminated_non_ascii_object_key_does_not_panic() {
    let parsed = parse("key.js", "const o = { \"\u{e9}\n: 1 };").unwrap();
    assert!(!parsed.errors.is_empty());
    let parsed = parse("key.ts", "class A { \"\u{e9}\n() {} }").unwrap();
    assert!(!parsed.errors.is_empty());
}
