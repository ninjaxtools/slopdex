//! Public-parser regressions for accepted JS/JSX/TS/TSX declaration syntax.
mod parse_support;

use parse_support::{byte_at, callables, clean, declarations, named};
use slopdex::parse::ParsedFile;
use tree_sitter::{Language, Parser};

const SCRIPT_PATHS: [&str; 4] = ["fixture.js", "fixture.jsx", "fixture.ts", "fixture.tsx"];
const TYPE_PATHS: [&str; 2] = ["fixture.ts", "fixture.tsx"];
const JSX_PATHS: [&str; 2] = ["fixture.jsx", "fixture.tsx"];

fn accepted(path: &str, source: &str) -> ParsedFile {
    let grammar: Language = if path.ends_with(".tsx") {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else if path.ends_with(".ts") {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    } else {
        tree_sitter_javascript::LANGUAGE.into()
    };
    let mut parser = Parser::new();
    parser.set_language(&grammar).unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "fixture is not accepted by {path}'s grammar: {}",
        tree.root_node().to_sexp()
    );
    clean(path, source)
}

fn assert_ranges_and_names(parsed: &ParsedFile, source: &str) {
    for (id, node) in parsed.structure.nodes.iter().enumerate() {
        assert_eq!(node.id, id);
        assert_eq!(
            node.start_byte,
            byte_at(source, node.start_line, node.start_column)
        );
        assert_eq!(
            node.end_byte,
            byte_at(source, node.end_line, node.end_column)
        );
        assert!(source.get(node.start_byte..node.end_byte).is_some());
        assert!(node.names.contains(&node.name), "{node:?}");
        if let Some(parent_id) = node.parent_id {
            assert!(parent_id < node.id, "{node:?}");
            let parent = &parsed.structure.nodes[parent_id];
            assert!(parent.start_byte <= node.start_byte && node.end_byte <= parent.end_byte);
            assert_eq!(
                node.qualified_name,
                format!("{}.{}", parent.qualified_name, node.name)
            );
        } else {
            assert_eq!(node.qualified_name, node.name);
        }
    }
    for callable in &parsed.callables {
        let start = byte_at(source, callable.start_line, callable.start_column);
        let end = byte_at(source, callable.end_line, callable.end_column);
        assert_eq!(source.get(start..end), Some(callable.source.as_str()));
        assert_eq!(
            callable.line_count,
            callable.end_line - callable.start_line + 1
        );
        assert!(!callable.signature.as_deref().unwrap_or("").is_empty());
        assert!(
            parsed.structure.nodes.iter().any(|node| {
                node.qualified_name == callable.qualified_name
                    && node.name == callable.name
                    && node.kind == callable.kind
                    && node.start_byte <= start
                    && end <= node.end_byte
            }),
            "no matching declaration/name/kind/range for {}: {:?}",
            callable.qualified_name,
            declarations(parsed)
        );
    }
}

fn assert_script_callables(source: &str, expected: &[(&str, &str)]) {
    // Parse all four grammars before checking extraction, even when the first
    // grammar exposes the regression. Every failing fixture must be accepted.
    let parsed = SCRIPT_PATHS.map(|path| accepted(path, source));
    for (path, parsed) in SCRIPT_PATHS.into_iter().zip(parsed) {
        assert_eq!(callables(&parsed), expected, "{path}");
        assert_ranges_and_names(&parsed, source);
    }
}

#[test]
fn anonymous_callback_data_stays_absent_while_real_declarations_survive() {
    let source = r#"// café: columns and ranges are UTF-8 byte based.
invoke(async () => {
  const scalar = compute();
  const payload = { value: 1, nested: { id: 2 }, items: [{ id: 3 }] };
  service.state = { status: "ready", values: [1].map(value => ({ id: value })) };
  function helper() { const data = { hidden: 1 }; return data; }
  class Local { field = compute(); run() { return helper(); } }
  const tools = { nested: { run: () => helper() } };
  service.execute = () => { const label = "café 🚀"; function inside() {} };
  return { result: () => helper(), data: { hidden: 1 } };
});
"#;
    for path in SCRIPT_PATHS {
        let parsed = accepted(path, source);
        assert_eq!(
            declarations(&parsed),
            [
                ("helper", "function"),
                ("Local", "class"),
                ("Local.field", "field"),
                ("Local.run", "method"),
                ("tools", "constant"),
                ("tools.nested", "variable"),
                ("tools.nested.run", "function"),
                ("execute", "function"),
                ("execute.inside", "function"),
                ("result", "function"),
            ],
            "{path}"
        );
        assert_eq!(
            callables(&parsed),
            [
                ("helper", "function"),
                ("Local.run", "method"),
                ("tools.nested.run", "function"),
                ("execute", "function"),
                ("execute.inside", "function"),
                ("result", "function"),
            ],
            "{path}"
        );
        assert_ranges_and_names(&parsed, source);
    }
}

#[test]
fn ordinary_initializers_preserve_named_definitions_in_the_lexical_scope() {
    let fixtures = [
        (
            SCRIPT_PATHS.as_slice(),
            "const result = wrap(() => { const data = { hidden: 1 }; function Healthy() {} class Local { field = 1; run() {} } });",
            vec![
                ("result", "constant"),
                ("Healthy", "function"),
                ("Local", "class"),
                ("Local.field", "field"),
                ("Local.run", "method"),
            ],
        ),
        (
            SCRIPT_PATHS.as_slice(),
            "const entries = [function Healthy() {}, class Local { run() {} }];",
            vec![
                ("entries", "constant"),
                ("Healthy", "function"),
                ("Local", "class"),
                ("Local.run", "method"),
            ],
        ),
        (
            SCRIPT_PATHS.as_slice(),
            "const config = { task: wrap(() => { const data = { hidden: 1 }; function Healthy() {} }) };",
            vec![
                ("config", "constant"),
                ("config.task", "field"),
                ("config.Healthy", "function"),
            ],
        ),
        (
            SCRIPT_PATHS.as_slice(),
            "class Owner { field = wrap(() => { const data = { hidden: 1 }; function Healthy() {} }); }",
            vec![
                ("Owner", "class"),
                ("Owner.field", "field"),
                ("Owner.Healthy", "function"),
            ],
        ),
        (
            JSX_PATHS.as_slice(),
            "const view = <Panel render={() => { const data = { hidden: 1 }; function Healthy() {} return <div />; }} />;",
            vec![("view", "constant"), ("Healthy", "function")],
        ),
    ];
    let mut failures = Vec::new();
    for (paths, source, expected) in fixtures {
        let expected_callables: Vec<_> = expected
            .iter()
            .copied()
            .filter(|(_, kind)| {
                matches!(*kind, "function" | "method" | "constructor" | "generator")
            })
            .collect();
        for path in paths {
            let parsed = accepted(path, source);
            let actual = declarations(&parsed);
            let actual_callables = callables(&parsed);
            if actual != expected || actual_callables != expected_callables {
                failures.push(format!(
                    "{path}: {source}\nexpected declarations: {expected:?}\nactual declarations: {actual:?}\nexpected callables: {expected_callables:?}\nactual callables: {actual_callables:?}"
                ));
            } else {
                assert_ranges_and_names(&parsed, source);
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn parenthesized_object_and_anonymous_class_bindings_keep_the_binding_scope() {
    let source = r#"const api = ({ nested: ({ run() {} }) });
const Store = (class { field = 1; run() {} });
service.tasks = ({ run: () => 1 });
const config = { Type: (class { run() {} }) };
"#;
    assert_script_callables(
        source,
        &[
            ("api.nested.run", "method"),
            ("Store.run", "method"),
            ("tasks.run", "function"),
            ("config.Type.run", "method"),
        ],
    );
}

#[test]
fn comments_inside_parentheses_do_not_erase_or_rename_bound_callables() {
    let source = r#"const arrow = (/* explanation */ () => { function nested() {} });
const regular = (/* explanation */ function internal() { function nested() {} });
const generator = (/* explanation */ function* internal() { yield 1; });
service.assigned = (/* explanation */ () => 1);
const api = { task: (/* explanation */ () => 1) };
class Owner { #task = (/* explanation */ () => 1); }
"#;
    assert_script_callables(
        source,
        &[
            ("arrow", "function"),
            ("arrow.nested", "function"),
            ("regular", "function"),
            ("regular.nested", "function"),
            ("generator", "generator"),
            ("assigned", "function"),
            ("api.task", "function"),
            ("Owner.task", "method"),
        ],
    );
}

#[test]
fn field_bound_objects_and_classes_scope_methods_under_the_field() {
    let source = r#"class Owner {
  tools = { run() {} };
  Type = class { field = 1; run() {} };
}
"#;
    assert_script_callables(
        source,
        &[("Owner.tools.run", "method"), ("Owner.Type.run", "method")],
    );
}

#[test]
fn quoted_and_computed_keys_have_consistent_lossless_symbol_names() {
    let source = r#"const api = {
  "plain"() {},
  constructor() {},
  "can't": () => 1,
  ['dynamic']: () => 2,
  "nested": { "can't": () => 3 },
};
class Store { "constructor"() {} "load"() {} }
"#;
    assert_script_callables(
        source,
        &[
            ("api.plain", "method"),
            ("api.constructor", "method"),
            ("api.can't", "function"),
            ("api.['dynamic']", "function"),
            ("api.nested.can't", "function"),
            ("Store.constructor", "constructor"),
            ("Store.load", "method"),
        ],
    );
}

#[test]
fn inline_type_expressions_do_not_promote_members_out_of_their_type_context() {
    let source = r#"type Wrapped = Promise<{ inner: number; run(): void }>;
type Items = { item: number }[];
type Parenthesized = ({ wrapped: string });
type Conditional<T> = T extends string ? { yes: number } : { no: string };
type Indexed = { selected: number }["selected"];
type Direct = { nested: { leaf: number; run(): void }; call(): { result: string } };
interface Shape { nested: { leaf: number }; method(): { result: string }; }
function make(): { value: number; method(): void } { return { value: 1, method() {} }; }
invoke((): { callbackValue: number } => ({ callbackValue: 1 }));
"#;
    let parsed = TYPE_PATHS.map(|path| accepted(path, source));
    for (path, parsed) in TYPE_PATHS.into_iter().zip(parsed) {
        assert_eq!(
            declarations(&parsed),
            [
                ("Wrapped", "type"),
                ("Items", "type"),
                ("Parenthesized", "type"),
                ("Conditional", "type"),
                ("Indexed", "type"),
                ("Direct", "type"),
                ("Direct.nested", "field"),
                ("Direct.call", "method"),
                ("Shape", "interface"),
                ("Shape.nested", "field"),
                ("Shape.method", "method"),
                ("make", "function"),
                ("make.method", "method"),
            ],
            "{path}"
        );
        for (name, signature) in [
            (
                "Wrapped",
                "type Wrapped = Promise<{ inner: number; run(): void }>",
            ),
            ("Items", "type Items = { item: number }[]"),
            (
                "Parenthesized",
                "type Parenthesized = ({ wrapped: string })",
            ),
            (
                "Conditional",
                "type Conditional<T> = T extends string ? { yes: number } : { no: string }",
            ),
            (
                "Indexed",
                "type Indexed = { selected: number }[\"selected\"]",
            ),
            ("Direct.nested", "nested: { leaf: number; run(): void }"),
            ("Direct.call", "call(): { result: string }"),
            ("Shape.nested", "nested: { leaf: number }"),
            ("Shape.method", "method(): { result: string }"),
            ("make", "function make(): { value: number; method(): void }"),
        ] {
            assert_eq!(named(&parsed, name).signature, signature, "{path}: {name}");
        }
        assert_eq!(
            callables(&parsed),
            [("make", "function"), ("make.method", "method")]
        );
        assert_ranges_and_names(&parsed, source);
    }
}

#[test]
fn real_type_declarations_and_callable_signatures_remain_available() {
    let source = r#"namespace API {
  interface Callable { field: string; run<T>(value: T): T; (value: number): string; new (): Callable; [key: string]: unknown; }
  type Direct = { field: number; run(): void };
  enum State { Ready, Busy = compute() }
  abstract class Base { value: string = compute(); abstract run(value: number): void; concrete<T>(value: T): T { return value; } }
  declare function external(value: string): number;
  function overload(value: string): string;
  function overload(value: string): string { return value; }
}
"#;
    for path in TYPE_PATHS {
        let parsed = accepted(path, source);
        for (name, kind) in [
            ("API", "module"),
            ("API.Callable", "interface"),
            ("API.Callable.field", "field"),
            ("API.Callable.run", "method"),
            ("API.Callable.call", "method"),
            ("API.Callable.new", "constructor"),
            ("API.Callable.index", "field"),
            ("API.Direct.field", "field"),
            ("API.Direct.run", "method"),
            ("API.State.Ready", "variant"),
            ("API.State.Busy", "variant"),
            ("API.Base.value", "field"),
            ("API.Base.run", "method"),
            ("API.external", "function"),
        ] {
            assert_eq!(named(&parsed, name).kind, kind, "{path}");
        }
        assert_eq!(
            callables(&parsed),
            [
                ("API.Base.concrete", "method"),
                ("API.overload", "function")
            ]
        );
        assert_eq!(
            parsed.callables[0].signature.as_deref(),
            Some("concrete<T>(value: T): T")
        );
        assert_eq!(
            parsed
                .structure
                .nodes
                .iter()
                .filter(|node| node.qualified_name == "API.overload")
                .count(),
            2
        );
        assert_ranges_and_names(&parsed, source);
    }
}
