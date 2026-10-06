mod parse_support;

use parse_support::{callables, clean, declarations, named};
use slopdex::parse::{ParsedFile, parse};

#[test]
fn java_unbound_lambdas_preserve_named_local_classes() {
    let parsed = clean(
        "Demo.java",
        "class Demo { void outer() { consume(() -> { class Healthy { void run() {} } int data = 1; }); } }",
    );
    assert_eq!(
        callables(&parsed),
        [
            ("Demo.outer", "method"),
            ("Demo.outer.Healthy.run", "method")
        ]
    );
    assert_eq!(
        declarations(&parsed),
        [
            ("Demo", "class"),
            ("Demo.outer", "method"),
            ("Demo.outer.Healthy", "class"),
            ("Demo.outer.Healthy.run", "method"),
        ]
    );
}

#[test]
fn java_wrapped_lambda_initializers_preserve_named_local_classes() {
    let parsed = clean(
        "Demo.java",
        "class Demo { Runnable task = wrap(() -> { class Healthy { void run() {} } int data = 1; }); }",
    );
    assert_eq!(callables(&parsed), [("Demo.Healthy.run", "method")]);
    assert_eq!(
        declarations(&parsed),
        [
            ("Demo", "class"),
            ("Demo.task", "field"),
            ("Demo.Healthy", "class"),
            ("Demo.Healthy.run", "method"),
        ]
    );
}

#[test]
fn java_comments_in_parenthesized_lambda_bindings_are_transparent() {
    let parsed = clean(
        "Demo.java",
        "class Demo { void outer() { Runnable task = (/* explanation */ () -> { class Healthy { void run() {} } }); } }",
    );
    assert_eq!(
        callables(&parsed),
        [
            ("Demo.outer", "method"),
            ("Demo.outer.task", "function"),
            ("Demo.outer.task.Healthy.run", "method"),
        ]
    );
    named(&parsed, "Demo.outer.task.Healthy");
}

#[test]
fn java_method_named_constructor_is_an_ordinary_method() {
    let parsed = clean("Demo.java", "class Demo { void constructor() {} }");
    assert_eq!(callables(&parsed), [("Demo.constructor", "method")]);
    assert_eq!(named(&parsed, "Demo.constructor").kind, "method");
}

#[test]
fn java_multiple_field_lambdas_own_only_their_calls() {
    let parsed = clean(
        "Demo.java",
        "class Demo { Runnable left = () -> leftCall(), right = () -> rightCall(); }",
    );
    assert_eq!(
        callables(&parsed),
        [("Demo.left", "function"), ("Demo.right", "function")]
    );
    for (name, call) in [("Demo.left", "leftCall"), ("Demo.right", "rightCall")] {
        let node = named(&parsed, name);
        assert_eq!(
            node.calls
                .iter()
                .map(|site| site.name.as_str())
                .collect::<Vec<_>>(),
            [call],
            "{name}: {node:?}"
        );
    }
}

#[test]
fn c_type_references_do_not_declare_new_tags() {
    let parsed = clean(
        "types.c",
        "struct Data { int value; }; struct Data *get(struct Data *input) { return (struct Data *)input; }",
    );
    assert_eq!(callables(&parsed), [("get", "function")]);
    assert_eq!(
        declarations(&parsed),
        [
            ("Data", "struct"),
            ("Data.value", "field"),
            ("get", "function")
        ]
    );
}

#[test]
fn c_named_tag_definitions_in_initializers_are_preserved() {
    let parsed = clean(
        "initializers.c",
        "int size = sizeof(struct Healthy { int field; });",
    );
    assert_eq!(named(&parsed, "Healthy").kind, "struct");
    assert_eq!(named(&parsed, "Healthy.field").kind, "field");
    assert_eq!(named(&parsed, "Healthy").signature, "struct Healthy");
}

#[test]
fn c_local_initializer_data_does_not_hide_named_tag_definitions() {
    let parsed = clean(
        "initializers.c",
        "void outer(void) { int size = sizeof(struct Healthy { int field; }); }",
    );
    assert_eq!(named(&parsed, "outer.Healthy").kind, "struct");
    assert_eq!(named(&parsed, "outer.Healthy.field").kind, "field");
    assert!(
        !parsed
            .structure
            .nodes
            .iter()
            .any(|node| node.name == "size")
    );
}

#[test]
fn c_prototype_parameters_preserve_actual_tag_definitions() {
    let parsed = clean("api.h", "int use(struct Healthy { int field; } *input);");
    assert!(parsed.callables.is_empty());
    assert_eq!(named(&parsed, "use").kind, "function");
    assert!(
        parsed
            .structure
            .nodes
            .iter()
            .any(|node| node.name == "Healthy" && node.kind == "struct"),
        "{:?}",
        declarations(&parsed)
    );
    assert!(
        parsed
            .structure
            .nodes
            .iter()
            .any(|node| node.name == "field" && node.kind == "field")
    );
}

#[test]
fn c_comments_in_parenthesized_declarators_are_transparent() {
    for source in [
        "int (/* explanation */ healthy)(void) { return 1; }",
        "int (/* explanation */ healthy)(void);",
    ] {
        let parsed = clean("declarators.c", source);
        assert_eq!(declarations(&parsed), [("healthy", "function")]);
        if source.ends_with('}') {
            assert_eq!(callables(&parsed), [("healthy", "function")]);
        } else {
            assert!(parsed.callables.is_empty());
        }
    }
}

#[test]
fn c_return_type_tag_definition_belongs_to_the_enclosing_scope() {
    let parsed = clean(
        "types.c",
        "struct Healthy { int field; } make(void) { return (struct Healthy){1}; }",
    );
    assert_eq!(named(&parsed, "Healthy").kind, "struct");
    assert_eq!(named(&parsed, "Healthy.field").kind, "field");
    assert_eq!(named(&parsed, "make").kind, "function");
    assert!(
        !parsed
            .structure
            .nodes
            .iter()
            .any(|node| node.qualified_name == "make.Healthy")
    );
}

#[test]
fn c_attributes_on_declarator_spines_preserve_named_functions() {
    let parsed = clean(
        "attributes.c",
        "int healthy [[gnu::noinline]] (void) { return 1; }",
    );
    assert_eq!(callables(&parsed), [("healthy", "function")]);
    assert_eq!(declarations(&parsed), [("healthy", "function")]);
}

#[test]
fn c_attributes_on_declarator_spines_preserve_types_variables_and_fields() {
    let parsed = clean(
        "attributes.c",
        "typedef int Alias [[gnu::deprecated]]; int (*callback [[gnu::deprecated]])(int); struct Record { int field [[gnu::deprecated]]; };",
    );
    assert!(parsed.callables.is_empty());
    assert_eq!(
        declarations(&parsed),
        [
            ("Alias", "type"),
            ("callback", "variable"),
            ("Record", "struct"),
            ("Record.field", "field"),
        ]
    );
}

#[test]
fn c_named_nested_functions_in_statement_expression_initializers_are_preserved() {
    // GNU C supports both statement expressions and named nested functions.
    let parsed = clean(
        "initializers.c",
        "void outer(void) { int data = ({ int healthy(void) { return 1; } healthy(); }); }",
    );
    let expected = [("outer", "function"), ("outer.healthy", "function")];
    assert_eq!(callables(&parsed), expected);
    assert_eq!(declarations(&parsed), expected);
}

#[test]
fn c_local_data_bindings_do_not_hide_nested_named_tag_definitions() {
    let parsed = clean(
        "local.c",
        "void outer(void) { struct { struct Healthy { int field; } member; } payload; }",
    );
    assert!(
        parsed
            .structure
            .nodes
            .iter()
            .any(|node| node.name == "Healthy" && node.kind == "struct")
    );
    assert!(
        !parsed
            .structure
            .nodes
            .iter()
            .any(|node| node.name == "payload")
    );
}

#[test]
fn c_compound_literal_initializers_preserve_named_tags_without_data_members() {
    let parsed = clean(
        "literal.c",
        "void outer(void) { void *data = &(struct Healthy { int field; }){ .field = 1 }; }",
    );
    assert_eq!(named(&parsed, "outer.Healthy").kind, "struct");
    assert_eq!(named(&parsed, "outer.Healthy.field").kind, "field");
    assert!(
        !parsed
            .structure
            .nodes
            .iter()
            .any(|node| node.name == "data")
    );
    assert_eq!(
        parsed
            .structure
            .nodes
            .iter()
            .filter(|node| node.name == "field")
            .count(),
        1
    );
}

#[test]
fn bash_comments_strings_and_quoted_heredocs_are_data() {
    let parsed = clean(
        "data.sh",
        r#"#!/bin/bash
# commented() { :; }
text='literal() { :; }'
printf '%s\n' "string() { :; }"
cat <<'DATA'
heredoc() { :; }
$(quoted_substitution() { :; })
DATA
healthy() { :; }
"#,
    );
    assert_eq!(callables(&parsed), [("healthy", "function")]);
    assert_eq!(declarations(&parsed), [("healthy", "function")]);
}

#[test]
fn bash_executable_nested_scopes_preserve_named_functions() {
    let parsed = clean(
        "scopes.bash",
        r#"outer() (
    if true; then nested() { :; }; fi
    text=$(substitution() { :; }; substitution)
    cat <<DATA
$(heredoc_substitution() { :; }; heredoc_substitution)
heredoc_data() { :; }
DATA
)
function after { :; }
"#,
    );
    let expected = [
        ("outer", "function"),
        ("outer.nested", "function"),
        ("outer.substitution", "function"),
        ("outer.heredoc_substitution", "function"),
        ("after", "function"),
    ];
    assert_eq!(callables(&parsed), expected);
    assert_eq!(declarations(&parsed), expected);
    assert_eq!(named(&parsed, "outer").signature, "outer()");
}

#[test]
fn bash_recovery_unterminated_heredocs_keep_literal_data_out_of_symbols() {
    for source in [
        "f() { :; } <<EOF\n  EOF\nfake() { :; }\n",
        "f() { :; } <<-EOF\n EOF\nfake() { :; }\n",
        "f() { :; } <<EO\\F\n EOF\n$(fake() { :; })\n",
    ] {
        let parsed = parse("unfinished.sh", source).unwrap();
        assert!(!parsed.errors.is_empty(), "{source:?}");
        assert!(parsed.callables.is_empty(), "{:?}", callables(&parsed));
        assert_eq!(declarations(&parsed), [("f", "function")]);
        assert_eq!(named(&parsed, "f").end_byte, source.len());
    }
}

#[test]
fn bash_recovery_unterminated_heredocs_keep_real_executable_islands() {
    let source = "f() { :; } <<EOF\n  EOF\nfake() { :; }\n$(inside() { :; })\n";
    let parsed = parse("unfinished.sh", source).unwrap();
    assert!(!parsed.errors.is_empty());
    assert_eq!(callables(&parsed), [("f.inside", "function")]);
    assert_eq!(
        declarations(&parsed),
        [("f", "function"), ("f.inside", "function")]
    );
    assert_eq!(named(&parsed, "f").end_byte, source.len());
}

#[test]
fn bash_recovery_descriptor_heredocs_keep_function_scope_and_source() {
    for source in [
        "f() { :; } 3<<EOF\n$(inside() { :; })\nEOF\n",
        "f() { :; } 3<<-EOF\n\t$(inside() { :; })\n\tEOF\n",
        "f() { :; } 12<<EOF >out\n$(inside() { :; })\nEOF\n",
    ] {
        let parsed = clean("descriptors.sh", source);
        let expected = [("f", "function"), ("f.inside", "function")];
        assert_eq!(callables(&parsed), expected, "{source:?}");
        assert_eq!(declarations(&parsed), expected, "{source:?}");
        assert_eq!(parsed.callables[0].source, source.trim_end_matches('\n'));
        assert_eq!(named(&parsed, "f").end_byte, source.len() - 1);
    }
}

#[test]
fn bash_recovery_malformed_siblings_do_not_detach_healthy_heredoc_scopes() {
    for operator in ["&&", "||", "|"] {
        let source =
            format!("f() {{ :; }} <<EOF {operator} broken() {{\n$(inside() {{ :; }})\nEOF\n");
        let parsed = parse("malformed_header.sh", &source).unwrap();
        assert!(!parsed.errors.is_empty(), "{operator}");
        assert_eq!(
            callables(&parsed),
            [("f", "function"), ("f.inside", "function")],
            "{operator}: {:?}",
            parsed.errors
        );
        assert_eq!(parsed.callables[0].source, source.trim_end_matches('\n'));
        assert_eq!(named(&parsed, "f").end_byte, source.len() - 1);
        assert_eq!(
            named(&parsed, "f.inside").parent_id,
            Some(named(&parsed, "f").id)
        );
    }
}

#[test]
fn bash_partially_escaped_heredoc_delimiters_disable_expansions() {
    let parsed = clean(
        "data.sh",
        "cat <<EO\\F\n$(fake() { :; })\nEOF\nhealthy() { :; }\n",
    );
    assert_eq!(callables(&parsed), [("healthy", "function")]);
    assert_eq!(declarations(&parsed), [("healthy", "function")]);
}

#[test]
fn bash_heredoc_terminators_do_not_allow_space_indentation() {
    for redirect in ["<<EOF", "<<-EOF"] {
        let source = format!("cat {redirect}\n  EOF\nfake() {{ :; }}\nEOF\nhealthy() {{ :; }}\n");
        let parsed = clean("data.sh", &source);
        assert_eq!(callables(&parsed), [("healthy", "function")], "{redirect}");
        assert_eq!(
            declarations(&parsed),
            [("healthy", "function")],
            "{redirect}"
        );
    }
}

#[test]
fn bash_backtick_heredoc_substitutions_are_executable_scopes() {
    let parsed = clean("data.sh", "cat <<EOF\n`healthy() { :; }; healthy`\nEOF\n");
    assert_eq!(callables(&parsed), [("healthy", "function")]);
    assert_eq!(declarations(&parsed), [("healthy", "function")]);
}

#[test]
fn bash_function_redirections_preserve_named_declarations() {
    let parsed = clean(
        "redirect.sh",
        "outer() { :; } >\"$(healthy() { :; }; healthy)\"\n",
    );
    assert_eq!(
        callables(&parsed),
        [("outer", "function"), ("outer.healthy", "function")]
    );
    assert_eq!(
        declarations(&parsed),
        [("outer", "function"), ("outer.healthy", "function")]
    );
}

#[test]
fn bash_function_heredoc_redirections_preserve_function_scope() {
    let parsed = clean(
        "redirect.sh",
        "outer() { :; } <<EOF\n$(healthy() { :; }; healthy)\nEOF\n",
    );
    let expected = [("outer", "function"), ("outer.healthy", "function")];
    assert_eq!(callables(&parsed), expected);
    assert_eq!(declarations(&parsed), expected);
}

#[test]
fn bash_heredoc_header_siblings_keep_their_lexical_scope_and_call_ownership() {
    for operator in ["&&", "||", "|"] {
        for (prefix, suffix, scope) in [("", "", ""), ("outer() {\n", "}\n", "outer.")] {
            let source = format!(
                "{prefix}f() {{ :; }} <<EOF {operator} sibling() {{ sibling_call; }}\n$(inside() {{ inside_call; }}; inside)\nEOF\n{suffix}"
            );
            let parsed = clean("siblings.sh", &source);
            let names: Vec<_> = parsed
                .callables
                .iter()
                .map(|callable| callable.qualified_name.as_str())
                .collect();
            let mut expected = Vec::new();
            if !scope.is_empty() {
                expected.push("outer".to_owned());
            }
            expected.extend(["f", "sibling", "f.inside"].map(|name| format!("{scope}{name}")));
            assert_eq!(names, expected, "{operator}: {scope}");
            assert_callable_structure_agreement(&parsed);
            let f = named(&parsed, &format!("{scope}f"));
            assert_eq!(
                f.calls
                    .iter()
                    .map(|site| site.name.as_str())
                    .collect::<Vec<_>>(),
                ["inside"]
            );
            let sibling = named(&parsed, &format!("{scope}sibling"));
            assert_eq!(
                sibling
                    .calls
                    .iter()
                    .map(|site| site.name.as_str())
                    .collect::<Vec<_>>(),
                ["sibling_call"]
            );
        }
        let source = format!("f() {{ :; }} <<EOF {operator} sibling_call\n$(inside_call)\nEOF\n");
        let parsed = clean("calls.sh", &source);
        assert_eq!(
            named(&parsed, "f")
                .calls
                .iter()
                .map(|site| site.name.as_str())
                .collect::<Vec<_>>(),
            ["inside_call"]
        );
    }
}

#[test]
fn bash_function_signatures_exclude_executable_redirection_data() {
    let parsed = clean(
        "redirect.sh",
        "healthy() { :; } >\"$(printf runtime_data)\"\n",
    );
    assert_eq!(named(&parsed, "healthy").signature, "healthy()");
}

#[test]
fn bash_line_continuations_preserve_function_identity() {
    // Bash accepts this declaration and `declare -F` reports `foobar`.
    let parsed = clean("names.sh", "foo\\\nbar() { :; }\n");
    assert_eq!(callables(&parsed), [("foobar", "function")]);
    assert_eq!(declarations(&parsed), [("foobar", "function")]);
}

#[test]
fn bash_loop_function_bodies_are_healthy_named_declarations() {
    // These are valid Bash compound-command bodies, without brace wrappers.
    for source in [
        "healthy() while false; do :; done\n",
        "healthy() for item in a; do :; done\n",
        "healthy() case value in *) :;; esac\n",
    ] {
        let parsed = clean("bodies.sh", source);
        assert_eq!(callables(&parsed), [("healthy", "function")]);
        assert_eq!(declarations(&parsed), [("healthy", "function")]);
    }
}

#[test]
fn anonymous_initializer_classes_preserve_members_and_suppress_local_data() {
    let parsed = clean(
        "Demo.java",
        "class Demo { void outer() { Object data = wrap(new Object() { int field = 1; void healthy() { int local = 2; } }); } }",
    );
    let anonymous = parsed
        .structure
        .nodes
        .iter()
        .find(|node| node.name.starts_with("<anonymous@"))
        .unwrap();
    assert_eq!(anonymous.kind, "class");
    assert_eq!(anonymous.parent_id, Some(named(&parsed, "Demo.outer").id));
    assert_eq!(
        named(&parsed, &format!("{}.field", anonymous.qualified_name)).kind,
        "field"
    );
    assert_eq!(parsed.callables.len(), 2);
    assert_eq!(
        parsed.callables[1].qualified_name,
        format!("{}.healthy", anonymous.qualified_name)
    );
    assert!(
        !parsed
            .structure
            .nodes
            .iter()
            .any(|node| matches!(node.name.as_str(), "data" | "local"))
    );
    assert_callable_structure_agreement(&parsed);
}

#[test]
fn c_data_initializers_and_pointer_declarators_do_not_create_callable_symbols() {
    let parsed = clean(
        "data.c",
        r#"struct Record { int value; int (*callback)(int); };
struct Record global = { .value = 1, .callback = 0 };
int (*callbacks[2])(int) = {0, 0};
int (*factory(void))[2] { return 0; }
void outer(void) {
    struct Record payload = { .value = 2, .callback = 0 };
    int (*local)(int) = 0;
    int prototype(int);
    const char *text = "int fake(void) { return 1; }";
}
"#,
    );
    assert_eq!(
        callables(&parsed),
        [("factory", "function"), ("outer", "function")]
    );
    assert_eq!(named(&parsed, "callbacks").kind, "variable");
    assert_eq!(named(&parsed, "Record.callback").kind, "field");
    assert_eq!(named(&parsed, "outer.prototype").kind, "function");
    assert_eq!(
        parsed
            .structure
            .nodes
            .iter()
            .filter(|node| node.name == "value")
            .count(),
        1
    );
    assert!(
        !parsed
            .structure
            .nodes
            .iter()
            .any(|node| matches!(node.name.as_str(), "payload" | "local" | "text" | "fake"))
    );
    assert!(
        parsed
            .structure
            .nodes
            .iter()
            .all(|node| !node.signature.contains(".value ="))
    );
    assert_callable_structure_agreement(&parsed);
}

#[test]
fn bash_non_brace_bodies_keep_nested_functions_and_signature_boundaries() {
    for source in [
        "outer() [[ $(healthy() { :; }; healthy) == ready ]]\n",
        "outer() if true; then healthy() { :; }; fi\n",
    ] {
        let parsed = clean("bodies.sh", source);
        let expected = [("outer", "function"), ("outer.healthy", "function")];
        assert_eq!(callables(&parsed), expected);
        assert_eq!(declarations(&parsed), expected);
        assert_eq!(named(&parsed, "outer").signature, "outer()");
        assert_callable_structure_agreement(&parsed);
    }
}

fn assert_callable_structure_agreement(parsed: &ParsedFile) {
    for callable in &parsed.callables {
        assert!(
            parsed.structure.nodes.iter().any(|node| {
                node.qualified_name == callable.qualified_name
                    && node.kind == callable.kind
                    && node.start_line <= callable.start_line
                    && callable.end_line <= node.end_line
            }),
            "callable {} has no corresponding declaration: {:?}",
            callable.qualified_name,
            declarations(parsed)
        );
    }
}
