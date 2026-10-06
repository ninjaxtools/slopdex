use super::*;

fn structure(path: &str, source: &str) -> FileStructure {
    let parsed = crate::parse::parse(path, source).unwrap();
    for node in &parsed.structure.nodes {
        assert!(
            node.start_byte <= node.end_byte && node.end_byte <= source.len(),
            "{node:?}"
        );
        assert!(source.is_char_boundary(node.start_byte) && source.is_char_boundary(node.end_byte));
        assert_eq!(node.id, parsed.structure.nodes[node.id].id);
        if let Some(parent) = node.parent_id {
            assert!(parent < node.id);
            assert!(node.qualified_name.starts_with(&format!(
                "{}.",
                parsed.structure.nodes[parent].qualified_name
            )));
        }
    }
    parsed.structure
}

fn named<'a>(s: &'a FileStructure, name: &str) -> &'a StructureNode {
    s.nodes
        .iter()
        .find(|n| n.qualified_name == name)
        .unwrap_or_else(|| panic!("missing {name}: {:#?}", s.nodes))
}

#[test]
fn rust_bodyless_items_variants_attributes_nested_scopes_and_imports() {
    let source = "use std::{io::{self, Read as R}, fmt::*};\n#[derive(Clone)]\npub struct S { pub x: i32, y: String }\nmod inner { pub trait T { type Item; const X: i32 = hidden(); fn run(&self) -> Self::Item; } }\nenum E { A, B { value: i32 }, C(u8, String) }\nimpl S { fn go(&self) { fn nested() { hidden(); } hidden(); } }\nmacro_rules! m { () => { hidden!() } }";
    let s = structure("x.rs", source);
    assert_eq!(named(&s, "S").signature, "pub struct S");
    assert_eq!(named(&s, "S").attributes, ["#[derive(Clone)]"]);
    assert_eq!(named(&s, "S.x").signature, "pub x: i32");
    assert_eq!(
        named(&s, "inner.T.run").signature,
        "fn run(&self) -> Self::Item"
    );
    assert_eq!(named(&s, "inner.T.X").signature, "const X: i32");
    assert_eq!(named(&s, "E.B.value").kind, "field");
    assert_eq!(named(&s, "E.C.0").signature, "0: u8");
    assert_eq!(named(&s, "E.C.1").signature, "1: String");
    assert_eq!(named(&s, "S.go.nested").kind, "function");
    assert!(
        s.nodes.iter().all(|n| !n.signature.contains("hidden")),
        "{s:#?}"
    );
    let imports = &s.nodes[0].imports;
    assert!(
        imports
            .iter()
            .any(|i| i.path == "std::io::Read" && i.alias.as_deref() == Some("R"))
    );
    assert!(
        imports
            .iter()
            .any(|i| i.path == "std::fmt::*" && i.wildcard)
    );
}

#[test]
fn typescript_overloads_fields_namespaces_type_members_and_initializers() {
    let source = "import Default, {Thing as Local, type Other} from 'pkg';\nexport namespace API { export interface Shape { field: string; call<T>(x: T): T; } export type Obj = { item: number; run(): void }; export enum E { A, B = hidden() } export abstract class Base { value: string = hidden(); abstract go(x: string): void; method(x = hidden()): void { hidden(); } } export function f(x: string): string; export function f(x: string) { return hidden(); } export const arrow = (x: number): number => hidden(); }";
    let s = structure("x.ts", source);
    for name in [
        "API",
        "API.Shape",
        "API.Shape.field",
        "API.Shape.call",
        "API.Obj",
        "API.Obj.item",
        "API.Obj.run",
        "API.E.A",
        "API.E.B",
        "API.Base",
        "API.Base.value",
        "API.Base.go",
        "API.Base.method",
        "API.arrow",
    ] {
        named(&s, name);
    }
    assert_eq!(
        s.nodes
            .iter()
            .filter(|n| n.qualified_name == "API.f")
            .count(),
        2
    );
    assert_eq!(named(&s, "API.Base.value").signature, "value: string");
    assert!(
        s.nodes.iter().all(|n| !n.signature.contains("hidden")),
        "{s:#?}"
    );
    assert!(
        s.nodes[0]
            .imports
            .iter()
            .any(|i| i.name.as_deref() == Some("Thing") && i.alias.as_deref() == Some("Local"))
    );
    assert!(
        s.nodes[0]
            .imports
            .iter()
            .any(|i| i.name.as_deref() == Some("default") && i.alias.as_deref() == Some("Default"))
    );
}

#[test]
fn typescript_union_object_branches_remain_in_type_signatures() {
    let source = "export type Result =\n  | { compacted: false; events: Event[] }\n  | { compacted: true; summary: string };\nexport type Event = Metadata & (\n  | { kind: 'tool'; call: TypedToolCall<Tools> }\n  | { kind: 'text'; text: string }\n);\nexport type Box = { value: { left: number } | { right: string } };\nexport type Combined = { left: number } & { right: string };";
    for path in ["x.ts", "x.tsx"] {
        let s = structure(path, source);
        assert_eq!(
            named(&s, "Result").signature,
            "type Result = | { compacted: false; events: Event[] } | { compacted: true; summary: string }"
        );
        assert_eq!(
            named(&s, "Event").signature,
            "type Event = Metadata & ( | { kind: 'tool'; call: TypedToolCall<Tools> } | { kind: 'text'; text: string } )"
        );
        assert_eq!(named(&s, "Box").signature, "type Box =");
        assert_eq!(
            named(&s, "Box.value").signature,
            "value: { left: number } | { right: string }"
        );
        assert_eq!(
            named(&s, "Combined").signature,
            "type Combined = { left: number } & { right: string }"
        );
        assert!(s.nodes.iter().all(|node| {
            node.parent_id != Some(named(&s, "Result").id)
                && node.parent_id != Some(named(&s, "Event").id)
                && node.parent_id != Some(named(&s, "Combined").id)
        }));
    }
}

#[test]
fn javascript_jsx_tsx_and_bound_functions() {
    for path in ["x.js", "x.jsx", "x.tsx"] {
        let s = structure(
            path,
            "export class View { render() { return <div/>; } field = hidden(); } const render = () => <View/>; function outer() { class Inner { run() {} } }",
        );
        assert_eq!(named(&s, "View.render").signature, "render()");
        assert_eq!(named(&s, "render").kind, "function");
        assert_eq!(named(&s, "outer.Inner.run").kind, "method");
        assert!(s.nodes.iter().all(|n| !n.signature.contains("hidden")
            && !n.signature.contains("<div")
            && !n.signature.contains("<View")));
    }
}

#[test]
fn python_decorated_hierarchy_annotations_and_imports() {
    let source = "from ..pkg import Thing as Alias, Other\nimport os.path as path\n@decorate(hidden())\nclass Outer(Base):\n    value: int = hidden()\n    @classmethod\n    def run(cls, value: int = hidden()) -> str:\n        def inner():\n            return hidden()\n        return hidden()\n    class Nested:\n        pass\nCOUNT: int = hidden()\n";
    let s = structure("x.py", source);
    assert_eq!(named(&s, "Outer").attributes, ["@decorate(hidden())"]);
    assert_eq!(named(&s, "Outer.run").attributes, ["@classmethod"]);
    assert_eq!(named(&s, "Outer.value").signature, "value: int");
    assert_eq!(
        named(&s, "Outer.run").signature,
        "def run(cls, value: int ) -> str:"
    );
    named(&s, "Outer.run.inner");
    named(&s, "Outer.Nested");
    assert!(
        s.nodes.iter().all(|n| !n.signature.contains("hidden")),
        "{s:#?}"
    );
    assert!(
        s.nodes[0]
            .imports
            .iter()
            .any(|i| i.path == "..pkg.Thing" && i.alias.as_deref() == Some("Alias"))
    );
    assert_eq!(s.nodes[1].imports[0].path, "os.path");
}

#[test]
fn go_types_fields_bodyless_interface_methods_and_imports() {
    let source = "package p\nimport ( f \"fmt\"; \"io\" )\ntype S struct { X, Y int; io.Reader }\ntype I interface { Run(x int) error; io.Reader }\nconst C int = hidden()\nvar V string = hidden()\nfunc (s *S) Run(x int) error { return hidden() }\nfunc F() { hidden() }";
    let s = structure("x.go", source);
    for name in [
        "p",
        "S",
        "S.X",
        "S.Y",
        "S.io.Reader",
        "I",
        "I.Run",
        "I.io.Reader",
        "C",
        "V",
        "S.Run",
        "F",
    ] {
        named(&s, name);
    }
    assert!(
        s.nodes.iter().all(|n| !n.signature.contains("hidden")),
        "{s:#?}"
    );
    assert!(
        s.nodes
            .iter()
            .flat_map(|n| &n.imports)
            .any(|i| i.path == "fmt" && i.alias.as_deref() == Some("f"))
    );
}

#[test]
fn java_nested_types_fields_enum_constants_and_bodyless_methods() {
    let source = "package demo; import java.util.List; import static java.util.Collections.*; @Deprecated public class Outer { public int x = hidden(), y = hidden(); public interface I { String run(int x); } public enum E { A(hidden()), B; int field = hidden(); } public Outer() { hidden(); } public String run(int x) { return hidden(); } }";
    let s = structure("x.java", source);
    for name in [
        "demo",
        "Outer",
        "Outer.x",
        "Outer.y",
        "Outer.I",
        "Outer.I.run",
        "Outer.E",
        "Outer.E.A",
        "Outer.E.B",
        "Outer.E.field",
        "Outer.Outer",
        "Outer.run",
    ] {
        named(&s, name);
    }
    assert_eq!(named(&s, "Outer.x").signature, "public int x");
    assert_eq!(named(&s, "Outer.y").signature, "public int y");
    assert!(
        named(&s, "Outer")
            .attributes
            .contains(&"@Deprecated".to_owned())
    );
    assert!(
        s.nodes.iter().all(|n| !n.signature.contains("hidden")),
        "{s:#?}"
    );
    assert!(
        s.nodes
            .iter()
            .flat_map(|n| &n.imports)
            .any(|i| i.path == "java.util.Collections.*")
    );
}

#[test]
fn c_prototypes_function_pointers_tags_fields_and_macros() {
    let source = "#include <stdio.h>\n#define ANSWER hidden()\n#define CALL(x) hidden(x)\nstruct S { int x, y; int (*callback)(int); };\ntypedef struct S S;\nenum E { A = hidden(), B };\nextern int f(const char *x);\nint (*callback)(int) = hidden;\nint global = hidden();\nint f(const char *x) { return hidden(); }";
    let s = structure("x.c", source);
    for name in [
        "ANSWER",
        "CALL",
        "S",
        "S.x",
        "S.y",
        "S.callback",
        "E",
        "E.A",
        "E.B",
        "f",
        "callback",
        "global",
    ] {
        named(&s, name);
    }
    assert_eq!(named(&s, "callback").kind, "variable");
    assert_eq!(
        s.nodes
            .iter()
            .filter(|n| n.name == "f" && n.kind == "function")
            .count(),
        2
    );
    assert!(
        s.nodes.iter().all(|n| !n.signature.contains("hidden")),
        "{s:#?}"
    );
    assert_eq!(s.nodes[0].imports[0].path, "stdio.h");
}

#[test]
fn markdown_ranges_hierarchy_setext_and_no_chunk_truncation() {
    let long = "項".repeat(3000);
    let source = format!(
        "# Root\r\nbody\r\n## {long}\r\n```\r\n# fake\r\n```\r\n<!--\r\n# fake\r\n-->\r\nPeer\r\n----\r\nend\r\n# Next\r\n"
    );
    let s = structure("x.md", &source);
    assert_eq!(s.nodes.len(), 4);
    assert_eq!(s.nodes[1].name, long);
    assert_eq!(s.nodes[1].parent_id, Some(0));
    assert_eq!(named(&s, "Root.Peer").start_line, 10);
    assert_eq!(named(&s, "Root").end_byte, source.find("# Next").unwrap());
    assert_eq!(named(&s, "Next").end_byte, source.len());
}

#[test]
fn complete_nodes_unicode_byte_ranges_and_backwards_serde() {
    let fields = (0..100)
        .map(|n| format!("field_{n}: {}", "LongType".repeat(50)))
        .collect::<Vec<_>>()
        .join(",\n");
    let source = format!("// 項\r\nstruct Many {{\n{fields}\n}}\n");
    let s = structure("x.rs", &source);
    assert_eq!(s.nodes.len(), 101);
    assert_eq!(s.nodes[0].start_byte, "// 項\r\n".len());
    assert_eq!(s.nodes[0].start_line, 2);
    assert!(named(&s, "Many.field_99").signature.len() > 400);
    let encoded = serde_json::to_string(&s).unwrap();
    assert_eq!(s, serde_json::from_str(&encoded).unwrap());
    let old: crate::parse::ParsedFile =
        serde_json::from_str(r#"{"callables":[],"chunks":[],"errors":[]}"#).unwrap();
    assert!(old.structure.nodes.is_empty());
}

#[test]
fn defaults_nested_objects_binding_names_and_reexports() {
    let s = structure(
        "x.ts",
        "import NS = require('pkg'); import Other = NS.Inner; export {Thing as Alias} from 'other'; export * from 'all'; const {a = hidden(), key: renamed, nested: {deep}, ...rest} = hidden(); const obj = { x: hidden(), nested: { y: hidden(), run(a = hidden()) { hidden(); } } }; function f(x = () => hidden()) { const inner = () => hidden(); }",
    );
    let destructured = s
        .nodes
        .iter()
        .find(|n| n.names.contains(&"renamed".into()))
        .unwrap();
    assert_eq!(destructured.names, ["a", "renamed", "deep", "rest"]);
    for name in [
        "obj.x",
        "obj.nested",
        "obj.nested.y",
        "obj.nested.run",
        "f.inner",
    ] {
        named(&s, name);
    }
    assert!(
        s.nodes.iter().all(|n| !n.signature.contains("hidden")),
        "{s:#?}"
    );
    let imports: Vec<_> = s.nodes.iter().flat_map(|n| &n.imports).collect();
    assert!(
        imports
            .iter()
            .any(|i| i.path == "pkg" && i.alias.as_deref() == Some("NS"))
    );
    assert!(
        imports
            .iter()
            .any(|i| i.path == "NS.Inner" && i.alias.as_deref() == Some("Other"))
    );
    assert!(
        imports
            .iter()
            .any(|i| i.path == "other.Thing" && i.alias.as_deref() == Some("Alias"))
    );
    assert!(imports.iter().any(|i| i.path == "all.*" && i.wildcard));
}

#[test]
fn native_bound_functions_records_and_trait_impl_identity() {
    let s = structure(
        "x.rs",
        "struct Tuple(pub u8, String); trait T { fn f(); } impl T for Tuple { fn f() { let inner = |x: i32| hidden(x); } }",
    );
    assert_eq!(named(&s, "Tuple.0").signature, "pub 0: u8");
    named(&s, "<Tuple as T>.f.inner");
    assert!(
        s.nodes.iter().all(|n| !n.signature.contains("hidden")),
        "{s:#?}"
    );
    let s = structure(
        "x.go",
        "package p\nvar F = func(x int) int { return hidden() }\nfunc Outer() { inner := func() { hidden() }; _ = inner }",
    );
    assert_eq!(named(&s, "F").kind, "function");
    named(&s, "Outer.inner");
    assert!(
        s.nodes.iter().all(|n| !n.signature.contains("hidden")),
        "{s:#?}"
    );
    let s = structure(
        "x.java",
        "record Point(int x, String y) { } @interface Setting { int value() default hidden(); }",
    );
    named(&s, "Point.x");
    named(&s, "Point.y");
    assert_eq!(named(&s, "Setting.value").signature, "int value()");
}

#[test]
fn recovered_typescript_tree_uses_original_ranges_and_signatures() {
    let source = "import data from './data.json' with { type: 'json' };\nexport function load(value: import('./types').Value): import('./types').Result { return hidden(); }";
    let parsed = crate::parse::parse("x.ts", source).unwrap();
    let function = named(&parsed.structure, "load");
    assert_eq!(parsed.callables.len(), 1);
    assert!(
        function.signature.contains("import('./types').Value"),
        "{function:#?}"
    );
    assert!(function.signature.contains("import('./types').Result"));
    assert!(!function.signature.contains("hidden"));
    assert_eq!(function.start_byte, source.find("export").unwrap());
    assert_eq!(function.end_byte, source.len());
}

#[test]
fn c_forward_declarations_and_anonymous_typedef_members() {
    let s = structure(
        "x.h",
        "struct Forward; typedef struct { int value; union { long a; char b; } inner; } Record; typedef int (*Callback)(int); extern struct Forward *make(void);",
    );
    assert_eq!(named(&s, "Forward").signature, "struct Forward");
    assert_eq!(named(&s, "Record").signature, "typedef struct Record");
    assert_eq!(named(&s, "Record.value").kind, "field");
    assert_eq!(named(&s, "Record.inner.a").kind, "field");
    assert_eq!(named(&s, "Record.inner.b").kind, "field");
    assert_eq!(named(&s, "Callback").kind, "type");
    assert!(named(&s, "Callback").signature.starts_with("typedef int"));
    assert_eq!(named(&s, "make").kind, "function");
}

#[test]
fn literal_types_and_parenthesized_c_declarators_are_preserved() {
    let s = structure(
        "x.ts",
        "type Literal = 'two  words' | `a  b`; class S { #private() {} } ",
    );
    assert_eq!(
        named(&s, "Literal").signature,
        "type Literal = 'two  words' | `a  b`"
    );
    named(&s, "S.private");
    let s = structure(
        "x.h",
        "int (f)(int); int (*callback)(int); int (*factory(void))(int); int *plain(void);",
    );
    for name in ["f", "factory", "plain"] {
        assert_eq!(named(&s, name).kind, "function");
    }
    assert_eq!(named(&s, "callback").kind, "variable");
}

fn assert_callable_scopes(path: &str, source: &str) -> crate::parse::ParsedFile {
    let parsed = crate::parse::parse(path, source).unwrap();
    assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
    for callable in &parsed.callables {
        assert!(
            parsed.structure.nodes.iter().any(|node| {
                super::callable(&node.kind)
                    && node.qualified_name == callable.qualified_name
                    && node.start_line <= callable.start_line
                    && node.end_line >= callable.end_line
            }),
            "missing callable {} in {:#?}",
            callable.qualified_name,
            parsed.structure.nodes
        );
    }
    parsed
}

#[test]
fn javascript_callbacks_and_payloads_do_not_create_data_symbols() {
    let source = r#"
it("should drop snapshot overlap before changing the visible error or transcript", async () => {
    const f = fixture();
    f.client.restore = async () => restoreConversation([{
      branch: {
        branchId: "main", chatId: "chat", sequence: 3, parent: null,
        kind: "jonin", status: "active", createdAt: "2026-09-27", updatedAt: "2026-09-27",
      },
      events: [{
        branchId: "main", chatId: "chat", eventId: 2, sequence: 2, createdAt: "2026-09-27",
        data: { type: "text", messageId: "message", stepIndex: 1, id: "answer", text: "Recovered" },
      }],
    }]);
    const error = { type: "error", message: "failed", branchId: "main", sequence: 3 };
    const delta = { type: "text", id: "answer", partId: "part", stepIndex: 1, text: "next", sequence: 4, branchId: "main" };
});
"#;
    for path in ["x.js", "x.jsx", "x.ts", "x.tsx"] {
        let parsed = assert_callable_scopes(path, source);
        assert_eq!(
            parsed.structure.nodes.len(),
            1,
            "{path}: {:#?}",
            parsed.structure
        );
        assert_eq!(parsed.structure.nodes[0].name, "restore");
        assert_eq!(parsed.structure.nodes[0].kind, "function");
        assert_eq!(
            crate::map::render_nodes(&parsed.structure.nodes, None),
            "@@ 4-13 @@\nf.client.restore = async () =>\n"
        );
    }
}

#[test]
fn javascript_executable_objects_preserve_declarations_without_data_fields() {
    let body = r#"
  const scalar = 1;
  const payload = { nested: { value: 1 }, items: [{ id: 2 }] };
  service.state = { nested: { value: 1 } };
  const local = {
    value: 1,
    nested: { run() { const data = { value: 1 }; } },
    arrow: () => ({ nested: { value: 1 } }),
    Type: class { field = 1; run() {} },
  };
  const helper = () => { const data = { value: 1 }; };
  class Inner { field = 1; run() {} }
  consume({ nested: { value: 1 }, items: [{ id: 2 }] });
  return { nested: { value: 1 }, result: () => ({ value: 1 }) };
"#;
    for path in ["x.js", "x.jsx", "x.ts", "x.tsx"] {
        for (prefix, suffix, scope) in [
            ("function outer() {", "}", "outer."),
            ("invoke(async () => {", "});", ""),
            ("invoke(function () {", "});", ""),
            ("invoke(function* () {", "});", ""),
            ("(() => {", "})();", ""),
        ] {
            let source = format!("{prefix}{body}{suffix}");
            let parsed = assert_callable_scopes(path, &source);
            let names: Vec<_> = parsed
                .structure
                .nodes
                .iter()
                .map(|node| node.qualified_name.as_str())
                .collect();
            let mut expected = Vec::new();
            if !scope.is_empty() {
                expected.push("outer".to_owned());
            }
            expected.extend(
                [
                    "local",
                    "local.nested",
                    "local.nested.run",
                    "local.arrow",
                    "local.Type",
                    "local.Type.field",
                    "local.Type.run",
                    "helper",
                    "Inner",
                    "Inner.field",
                    "Inner.run",
                    "result",
                ]
                .map(|name| format!("{scope}{name}")),
            );
            assert_eq!(names, expected, "{path}: {prefix}");
        }
    }
}

#[test]
fn javascript_local_object_spreads_and_wrappers_preserve_callable_scopes() {
    let source = r#"
function outer() {
  const spread = { ...({ run() {} }) };
  const wrapped = { task: wrap(() => { function nested() {} }) };
  const list = { items: [{ run() {} }] };
}
invoke((callback = function fallback() { const data = { value: 1 }; }) => {});
"#;
    for path in ["x.js", "x.jsx", "x.ts", "x.tsx"] {
        let parsed = assert_callable_scopes(path, source);
        for name in [
            "outer.spread.run",
            "outer.wrapped.nested",
            "outer.list.run",
            "fallback",
        ] {
            named(&parsed.structure, name);
        }
        assert!(
            !parsed
                .structure
                .nodes
                .iter()
                .any(|node| node.name == "data" || node.name == "value")
        );
    }
}

#[test]
fn javascript_payload_value_callbacks_do_not_create_object_symbols() {
    let source = r#"
it("example", () => {
  const error = { message: (() => "failed")() };
  const payload = {
    values: [1, 2].map(value => ({ id: value })),
    nested: { message: (function () { return "failed"; })() },
    items: consume(function* () { yield 1; }),
  };
});
"#;
    for path in ["x.js", "x.jsx", "x.ts", "x.tsx"] {
        let parsed = assert_callable_scopes(path, source);
        assert!(
            parsed.structure.nodes.is_empty(),
            "{path}: {:#?}",
            parsed.structure
        );
        assert!(parsed.callables.is_empty());
    }
}

#[test]
fn javascript_assignment_object_and_class_expression_scopes_match_callables() {
    for path in ["x.js", "x.ts", "x.tsx"] {
        let source = r#"
const api = { get(id) { return id; }, put: function internal(value) { return value; }, nested: { 'run': () => hidden() } };
module.exports.remove = (id) => hidden(id);
const Store = class { #load = () => hidden(); save = function internal() { hidden(); }; *values() { yield 1; } };
const Alias = class Named { run() { hidden(); } };
invoke(function named() {});
function outer() {
  const local = { nested: { run() { hidden(); } }, arrow: () => hidden() };
  service.tasks = { run: () => hidden() };
  service.execute = () => { function nested() { hidden(); } };
  return { result: () => hidden() };
}
const bound = (x = function ignored() { hidden(); }) => hidden();
"#;
        let parsed = assert_callable_scopes(path, source);
        let s = &parsed.structure;
        for name in [
            "api.nested.run",
            "remove",
            "Store.load",
            "Store.save",
            "Named.run",
            "outer.local.nested.run",
            "outer.tasks.run",
            "outer.execute.nested",
            "outer.result",
        ] {
            named(s, name);
        }
        assert!(named(s, "Named").names.contains(&"Alias".into()));
        assert!(!s.nodes.iter().any(|n| n.name == "ignored"));
        assert!(
            s.nodes.iter().all(|n| !n.signature.contains("hidden")),
            "{s:#?}"
        );
    }
}

#[test]
fn java_bound_lambdas_and_anonymous_classes_match_callable_scopes() {
    let source = r#"class Demo {
        Runnable task = () -> { class Nested { void run() { hidden(); } } };
        Object object = new Object() { void member() { hidden(); } };
        void outer() {
            Runnable local = (() -> hidden());
            Object value = new Object() { void member() { hidden(); } };
            invoke(() -> hidden());
        }
    }"#;
    let parsed = assert_callable_scopes("x.java", source);
    for name in ["Demo.task", "Demo.task.Nested.run", "Demo.outer.local"] {
        named(&parsed.structure, name);
    }
    assert!(
        parsed
            .structure
            .nodes
            .iter()
            .all(|n| !n.signature.contains("hidden")),
        "{:#?}",
        parsed.structure
    );
    assert!(
        named(&parsed.structure, "Demo.task")
            .signature
            .contains("Runnable task = () ->")
    );
}

#[test]
fn default_closures_variants_and_macro_replacements_do_not_leak() {
    for (path, source) in [
        (
            "x.ts",
            "function f(x = (() => hidden()), y: () => void = function named() { hidden(); }) { hidden(); } const arrow = (x = () => hidden()) => hidden(); enum E { A = (() => hidden())() }",
        ),
        ("x.py", "def f(x=lambda: hidden()):\n    return hidden()\n"),
        (
            "x.rs",
            "enum E { A = { hidden() } } impl<T> Trait< T > for Type< T > { fn f() { hidden(); } }",
        ),
        (
            "x.c",
            "#define VALUE hidden()\n#define CALL(x) \\\n    hidden(x)\nenum E { A = hidden() };\n",
        ),
    ] {
        let parsed = crate::parse::parse(path, source).unwrap();
        assert!(
            parsed
                .structure
                .nodes
                .iter()
                .all(|n| !n.signature.contains("hidden")),
            "{path}: {:#?}",
            parsed.structure
        );
        if path == "x.rs" {
            assert_callable_scopes(path, source);
        }
    }
}

#[test]
fn go_one_to_one_bindings_match_callable_scopes() {
    let source = "package p\ntype S struct {}\nvar One, Two = func() { hidden() }, func() { hidden() }\nfunc (s *S) Run() { left, right := func() { hidden() }, func() { hidden() }; s.callback = func() { hidden() }; _ = left; _ = right }\n";
    let parsed = assert_callable_scopes("x.go", source);
    for name in [
        "One",
        "Two",
        "S.Run.left",
        "S.Run.right",
        "S.Run.s.callback",
    ] {
        named(&parsed.structure, name);
    }
    assert!(
        parsed
            .structure
            .nodes
            .iter()
            .all(|n| !n.signature.contains("hidden"))
    );
}

#[test]
fn rust_lifetimes_compact_layout_without_changing_literal_contents() {
    let s = structure(
        "x.rs",
        "fn borrow<'a, 'b>(\n    x: &'a str,\n    y: &'b str,\n) -> &'a str { hidden(); x }\n",
    );
    assert_eq!(
        named(&s, "borrow").signature,
        "fn borrow<'a, 'b>( x: &'a str, y: &'b str, ) -> &'a str"
    );
    let raw = "r###\"one \"  two\"###";
    assert_eq!(compact(raw, "rust"), raw);
    assert_eq!(compact("'two  words'", "typescript"), "'two  words'");
}
