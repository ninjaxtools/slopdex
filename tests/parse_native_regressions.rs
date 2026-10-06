mod parse_support;

use parse_support::{byte_at, callables, clean, declarations as symbols, named};
use slopdex::parse::ParsedFile;

fn assert_callable(parsed: &ParsedFile, name: &str) {
    let names: Vec<_> = callables(parsed)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(names.contains(&name), "missing callable {name}: {names:?}");
}

fn assert_callable_coverage(parsed: &ParsedFile, source: &str) {
    for callable in &parsed.callables {
        let start = byte_at(source, callable.start_line, callable.start_column);
        let end = byte_at(source, callable.end_line, callable.end_column);
        assert_eq!(&source[start..end], callable.source);
        assert!(
            parsed.structure.nodes.iter().any(|node| {
                node.qualified_name == callable.qualified_name
                    && node.start_byte <= start
                    && end <= node.end_byte
            }),
            "no structural qualified name/range covering {}: {:?}",
            callable.qualified_name,
            symbols(parsed)
        );
    }
}

#[test]
fn anonymous_callbacks_do_not_turn_ordinary_values_or_object_fields_into_symbols() {
    for (path, source, expected) in [
        (
            "values.py",
            "result = invoke(lambda: {'field': compute(), 'nested': {'value': 1}})\n",
            vec![("result", "variable")],
        ),
        (
            "values.rs",
            "fn outer() { invoke(|| { let runtime = compute(); let payload = Shape { field: runtime }; object.field = runtime; }); }\n",
            vec![("outer", "function")],
        ),
        (
            "values.go",
            "package p\nvar result = invoke(func() int { var runtime = compute(); payload := map[string]int{\"field\": runtime}; object.field = runtime; _ = payload; return 1 })\n",
            vec![("p", "module"), ("result", "variable")],
        ),
    ] {
        let parsed = clean(path, source);
        assert_eq!(symbols(&parsed), expected, "{path}");
        assert_callable_coverage(&parsed, source);
    }
}

#[test]
fn python_named_declarations_survive_runtime_values_and_keep_real_class_fields() {
    let source = r#"class Outer:
    field: int = compute()
    def run(self):
        runtime = compute({"field": 1})
        self.field = runtime
        def nested():
            return runtime
        class Local:
            field: int = compute()
            def method(self):
                return self.field
        callback = lambda value: value
        self.callback = lambda: 1
        return nested()
"#;
    let parsed = clean("healthy.py", source);
    assert_eq!(
        symbols(&parsed),
        [
            ("Outer", "class"),
            ("Outer.field", "field"),
            ("Outer.run", "method"),
            ("Outer.run.nested", "function"),
            ("Outer.run.Local", "class"),
            ("Outer.run.Local.field", "field"),
            ("Outer.run.Local.method", "method"),
            ("Outer.run.callback", "function"),
            ("Outer.run.self.callback", "function"),
        ]
    );
    assert_eq!(named(&parsed, "Outer.field").signature, "field: int");
    assert!(
        parsed
            .structure
            .nodes
            .iter()
            .all(|node| !node.signature.contains("compute"))
    );
    assert_callable_coverage(&parsed, source);
}

#[test]
fn python_local_generic_type_alias_uses_its_identifier_and_keeps_its_type_signature() {
    let source = "def outer():\n    runtime = compute()\n    type Alias = int\n    type Generic[T] = list[T]\n";
    let parsed = clean("types.py", source);
    assert_eq!(named(&parsed, "outer.Alias").kind, "type");
    assert_eq!(named(&parsed, "outer.Alias").signature, "type Alias = int");
    assert_eq!(named(&parsed, "outer.Generic").kind, "type");
    assert_eq!(
        named(&parsed, "outer.Generic").signature,
        "type Generic[T] = list[T]"
    );
    assert!(!parsed.structure.nodes.iter().any(|n| n.name == "runtime"));
    assert_callable_coverage(&parsed, source);
}

#[test]
fn rust_named_items_survive_ordinary_block_initializers_without_runtime_bindings() {
    let source = r#"fn outer() {
    const LIMIT: i32 = 1;
    static SHARED: i32 = 2;
    let runtime = {
        fn helper() {}
        struct Local { field: i32 }
        0
    };
    let callback = || {
        const INNER: i32 = 3;
        struct Nested { field: i32 }
        fn nested() {}
        let transient = 4;
    };
}
"#;
    let parsed = clean("healthy.rs", source);
    assert_eq!(
        symbols(&parsed),
        [
            ("outer", "function"),
            ("outer.LIMIT", "constant"),
            ("outer.SHARED", "variable"),
            ("outer.helper", "function"),
            ("outer.Local", "struct"),
            ("outer.Local.field", "field"),
            ("outer.callback", "function"),
            ("outer.callback.INNER", "constant"),
            ("outer.callback.Nested", "struct"),
            ("outer.callback.Nested.field", "field"),
            ("outer.callback.nested", "function"),
        ]
    );
    assert_eq!(named(&parsed, "outer.LIMIT").signature, "const LIMIT: i32");
    assert_eq!(
        named(&parsed, "outer.SHARED").signature,
        "static SHARED: i32"
    );
    assert_callable_coverage(&parsed, source);
}

#[test]
fn go_named_local_types_and_receiver_bound_callbacks_keep_callable_scopes() {
    let source = r#"package p
type Store[T any] struct { callback func() }
func (s *Store[T]) Run() {
    runtime := map[string]int{"field": 1}
    var temporary int
    type Local struct { X, Y int }
    callback := func() {
        type Nested struct { Field int }
        transient := 1
        _ = transient
    }
    s.callback = func() { type ReceiverLocal int }
    _ = runtime
    _ = temporary
    _ = callback
}
"#;
    let parsed = clean("healthy.go", source);
    for (name, kind) in [
        ("Store[T].Run", "method"),
        ("Store[T].Run.Local", "struct"),
        ("Store[T].Run.Local.X", "field"),
        ("Store[T].Run.Local.Y", "field"),
        ("Store[T].Run.callback", "function"),
        ("Store[T].Run.callback.Nested", "struct"),
        ("Store[T].Run.callback.Nested.Field", "field"),
        ("Store[T].Run.s.callback", "function"),
        ("Store[T].Run.s.callback.ReceiverLocal", "type"),
    ] {
        assert_eq!(named(&parsed, name).kind, kind);
    }
    assert!(
        parsed
            .structure
            .nodes
            .iter()
            .all(|node| !matches!(node.name.as_str(), "runtime" | "temporary" | "transient"))
    );
    assert_callable_coverage(&parsed, source);
}

#[test]
fn python_ordinary_receiver_mutations_are_not_declarations() {
    let source = "value = compute()\nservice.field = value\nservice['item'] = value\nservice.callback = lambda: 1\n";
    let parsed = clean("targets.py", source);
    assert_eq!(
        symbols(&parsed),
        [("value", "variable"), ("service.callback", "function")]
    );
    assert_callable_coverage(&parsed, source);
}

#[test]
fn python_destructuring_does_not_declare_receiver_or_index_identifiers() {
    let parsed = clean(
        "targets.py",
        "left, service.field, items[index] = values()\nhead, *tail = values()\n",
    );
    let names: Vec<_> = parsed
        .structure
        .nodes
        .iter()
        .flat_map(|node| node.names.iter().map(String::as_str))
        .collect();
    assert_eq!(names, ["left", "head", "tail"]);
}

#[test]
fn python_chained_assignments_retain_all_declared_names() {
    let parsed = clean("bindings.py", "left = right = compute()\n");
    let names: Vec<_> = parsed
        .structure
        .nodes
        .iter()
        .flat_map(|node| node.names.iter().map(String::as_str))
        .collect();
    assert!(
        names.contains(&"left") && names.contains(&"right"),
        "{names:?}"
    );
}

#[test]
fn python_chained_lambda_assignment_covers_the_extracted_callable() {
    let source = "alias = callback = lambda: 1\n";
    let parsed = clean("bindings.py", source);
    assert_callable(&parsed, "callback");
    assert_callable_coverage(&parsed, source);
}

#[test]
fn python_ordinary_initializer_retains_a_named_callable_expression() {
    let source = "result = consume((callback := lambda: 1))\n";
    let parsed = clean("initializer.py", source);
    assert_callable(&parsed, "callback");
    assert_callable_coverage(&parsed, source);
}

#[test]
fn python_unbound_callback_retains_its_named_callable_binding() {
    let source = "def outer():\n    invoke(lambda: (callback := lambda: 1))\n";
    let parsed = clean("callbacks.py", source);
    assert_callable(&parsed, "outer.callback");
    assert_eq!(named(&parsed, "outer.callback").kind, "function");
    assert_callable_coverage(&parsed, source);
}

#[test]
fn rust_unbound_callback_retains_named_items_without_runtime_values() {
    let source = r#"fn outer() {
    invoke(|| {
        let runtime = 1;
        const LIMIT: i32 = 2;
        struct Local { field: i32 }
        fn helper() {}
    });
}
"#;
    let parsed = clean("callbacks.rs", source);
    assert_callable(&parsed, "outer.helper");
    assert_eq!(named(&parsed, "outer.LIMIT").kind, "constant");
    assert_eq!(named(&parsed, "outer.Local").kind, "struct");
    assert_eq!(named(&parsed, "outer.Local.field").kind, "field");
    assert_eq!(named(&parsed, "outer.helper").kind, "function");
    assert!(!parsed.structure.nodes.iter().any(|n| n.name == "runtime"));
    assert_callable_coverage(&parsed, source);
}

#[test]
fn rust_constant_and_static_initializers_do_not_create_callable_scopes() {
    let source = r#"fn outer() {
    const VALUE: i32 = { fn helper() {} struct Local; 0 };
    static SHARED: i32 = { fn initialize() {} 0 };
}
"#;
    let parsed = clean("initializer.rs", source);
    assert_callable_coverage(&parsed, source);
    assert_eq!(named(&parsed, "outer.Local").kind, "struct");
    assert_eq!(
        named(&parsed, "outer.Local").parent_id,
        Some(named(&parsed, "outer").id)
    );
}

#[test]
fn rust_discarded_closures_do_not_declare_a_wildcard_symbol() {
    let parsed = clean("discard.rs", "fn outer() { let _ = || 1; }\n");
    assert_eq!(symbols(&parsed), [("outer", "function")]);
    assert_eq!(parsed.callables.len(), 1);
}

#[test]
fn rust_reference_binding_patterns_use_the_declared_identifier() {
    let source = "fn outer() { let ref callback = || { fn nested() {} }; }\n";
    let parsed = clean("bindings.rs", source);
    assert_eq!(named(&parsed, "outer.callback").kind, "function");
    assert_eq!(named(&parsed, "outer.callback.nested").kind, "function");
    assert_callable(&parsed, "outer.callback");
    assert_callable_coverage(&parsed, source);
}

#[test]
fn go_local_constants_are_declarations_but_runtime_variables_are_not() {
    let source = r#"package p
func outer() {
    const (
        LIMIT = 1
        SECOND = 2
    )
    var runtime int
    transient := 3
    _ = runtime
    _ = transient
}
"#;
    let parsed = clean("constants.go", source);
    assert_eq!(named(&parsed, "outer.LIMIT").kind, "constant");
    assert_eq!(named(&parsed, "outer.SECOND").kind, "constant");
    assert!(
        parsed
            .structure
            .nodes
            .iter()
            .all(|n| !matches!(n.name.as_str(), "runtime" | "transient"))
    );
}

#[test]
fn go_mutable_package_variables_are_not_constants_based_on_capitalization() {
    let parsed = clean(
        "constants.go",
        "package p\nvar VALUE int\nvar FIRST, lower int\nconst LIMIT = 1\n",
    );
    assert_eq!(named(&parsed, "VALUE").kind, "variable");
    assert_eq!(named(&parsed, "FIRST").kind, "variable");
    assert!(named(&parsed, "FIRST").names.contains(&"lower".into()));
    assert_eq!(named(&parsed, "LIMIT").kind, "constant");
}

#[test]
fn go_unbound_callback_retains_named_types_and_bound_callbacks() {
    let source = r#"package p
func outer() {
    invoke(func() {
        type Local struct { Field int }
        callback := func() {}
        runtime := 1
        _ = callback
        _ = runtime
    })
}
"#;
    let parsed = clean("callbacks.go", source);
    assert_callable(&parsed, "outer.callback");
    assert_eq!(named(&parsed, "outer.Local").kind, "struct");
    assert_eq!(named(&parsed, "outer.Local.Field").kind, "field");
    assert_eq!(named(&parsed, "outer.callback").kind, "function");
    assert!(!parsed.structure.nodes.iter().any(|n| n.name == "runtime"));
    assert_callable_coverage(&parsed, source);
}

#[test]
fn go_ordinary_package_initializer_retains_named_types_and_bound_callbacks() {
    let source = r#"package p
var result = func() int {
    type Local struct { Field int }
    callback := func() {}
    _ = callback
    return 1
}()
"#;
    let parsed = clean("initializer.go", source);
    assert_callable(&parsed, "callback");
    assert_eq!(named(&parsed, "result").kind, "variable");
    assert_eq!(named(&parsed, "Local").kind, "struct");
    assert_eq!(named(&parsed, "Local.Field").kind, "field");
    assert_eq!(named(&parsed, "callback").kind, "function");
    assert_callable_coverage(&parsed, source);
}

#[test]
fn go_multibinding_comments_do_not_hide_named_callbacks() {
    let source = "package p\nfunc outer() { first, second := func() {}, /* separator */ func() {}; _ = first; _ = second }\n";
    let parsed = clean("bindings.go", source);
    assert_callable(&parsed, "outer.first");
    assert_callable(&parsed, "outer.second");
    assert_eq!(named(&parsed, "outer.first").kind, "function");
    assert_eq!(named(&parsed, "outer.second").kind, "function");
    assert_callable_coverage(&parsed, source);
}

#[test]
fn go_mixed_multibinding_keeps_declarations_inside_the_ordinary_initializer() {
    let source = r#"package p
func outer() {
    callback, result := func() {}, func() int {
        type Local struct { Field int }
        nested := func() {}
        _ = nested
        return 1
    }()
    _ = callback
    _ = result
}
"#;
    let parsed = clean("bindings.go", source);
    assert_eq!(named(&parsed, "outer.callback").kind, "function");
    assert_eq!(named(&parsed, "outer.Local").kind, "struct");
    assert_eq!(named(&parsed, "outer.Local.Field").kind, "field");
    assert_eq!(named(&parsed, "outer.nested").kind, "function");
    assert!(!parsed.structure.nodes.iter().any(|n| n.name == "result"));
    assert_callable(&parsed, "outer.callback");
    assert_callable(&parsed, "outer.nested");
    assert_callable_coverage(&parsed, source);
}

#[test]
fn go_discarded_and_index_assigned_functions_are_not_named_declarations() {
    let source = "package p\nfunc outer() { _ = func() {}; slots[0] = func() {}; object.callback = func() {} }\n";
    let parsed = clean("targets.go", source);
    assert_eq!(
        symbols(&parsed),
        [
            ("p", "module"),
            ("outer", "function"),
            ("outer.object.callback", "function")
        ]
    );
    assert!(
        parsed
            .callables
            .iter()
            .all(|c| !matches!(c.name.as_str(), "_" | "slots[0]"))
    );
    assert_callable_coverage(&parsed, source);
}
