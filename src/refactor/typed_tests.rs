use super::*;

#[test]
fn every_currently_indexed_statically_typed_language_is_refactorable() {
    for path in [
        "source.ts",
        "source.tsx",
        "source.rs",
        "source.go",
        "Source.java",
        "source.c",
        "source.h",
    ] {
        assert!(supported(path), "{path}");
    }
    for path in ["source.cpp", "source.cs", "source.kt", "source.py"] {
        assert!(
            !supported(path),
            "No additional language grammar was requested: {path}"
        );
    }
}

#[test]
fn rust_rename_follows_imports_aliases_qualified_uses_and_assertions() -> Result<()> {
    let f = Fixture::new(&[
        (
            "src/lib.rs",
            "pub mod utils;\npub mod caller;\n#[cfg(test)] mod tests { use crate::utils::parse; #[test] fn smoke() { assert_eq!(parse(), 1); } }\n",
        ),
        ("src/utils.rs", "pub fn parse() -> i32 { 1 }\n"),
        (
            "src/caller.rs",
            "use crate::utils::{parse, parse as read};\npub fn run() -> i32 { parse() + read() + crate::utils::parse() }\n",
        ),
    ])?;
    f.plan("src/utils.rs::parse", "parse_config")?.apply()?;
    assert!(f.read("src/utils.rs").contains("fn parse_config()"));
    assert!(f.read("src/caller.rs").contains("parse_config as read"));
    assert!(
        f.read("src/caller.rs")
            .contains("parse_config() + read() + crate::utils::parse_config()")
    );
    assert!(
        f.read("src/lib.rs")
            .contains("assert_eq!(parse_config(), 1)")
    );
    Ok(())
}

#[test]
fn rust_rename_preserves_shadowed_bindings_and_format_labels() -> Result<()> {
    let f = Fixture::new(&[(
        "src/lib.rs",
        "pub fn parse() -> i32 { 1 }\nfn run(parse: fn() -> i32) -> i32 { parse() }\nfn label() -> String { format!(\"{parse}\", parse = 1) }\n",
    )])?;
    f.plan("src/lib.rs::parse", "read_config")?.apply()?;
    let text = f.read("src/lib.rs");
    assert!(
        text.contains("fn run(parse: fn() -> i32) -> i32 { parse() }"),
        "{text}"
    );
    assert!(text.contains("parse = 1"), "{text}");
    Ok(())
}

#[test]
fn rust_file_rename_updates_module_registration_and_references() -> Result<()> {
    let f = Fixture::new(&[
        (
            "src/lib.rs",
            "//! Crate documentation.\npub mod utils;\npub mod caller;\n",
        ),
        ("src/utils.rs", "pub fn parse() -> i32 { 1 }\n"),
        (
            "src/caller.rs",
            "use crate::utils::parse;\npub fn run() -> i32 { crate::utils::parse() + parse() }\n",
        ),
    ])?;
    f.plan("src/utils.rs", "src/helpers.rs")?.apply()?;
    assert!(!f.root.join("src/utils.rs").exists());
    assert!(f.read("src/lib.rs").contains("mod helpers;"));
    assert!(!f.read("src/lib.rs").contains("mod utils;"));
    assert!(
        f.read("src/caller.rs")
            .contains("use crate::helpers::parse")
    );
    assert!(f.read("src/caller.rs").contains("crate::helpers::parse()"));
    assert!(f.read("src/lib.rs").starts_with("//! Crate documentation."));
    Ok(())
}

#[test]
fn rust_function_move_transfers_dependencies_and_updates_callers() -> Result<()> {
    let f = Fixture::new(&[
        ("src/lib.rs", "pub mod utils;\npub mod caller;\n"),
        (
            "src/utils.rs",
            "pub fn helper() -> i32 { 1 }\n/// Read configuration.\npub fn parse() -> i32 { helper() }\npub fn local() -> i32 { parse() }\n",
        ),
        (
            "src/caller.rs",
            "use crate::utils::parse;\npub fn run() -> i32 { parse() }\n",
        ),
    ])?;
    f.plan("src/utils.rs::parse", "src/parser.rs")?.apply()?;
    assert!(f.read("src/parser.rs").contains("use crate::utils::helper"));
    assert!(f.read("src/parser.rs").contains("pub fn parse()"));
    assert!(f.read("src/utils.rs").contains("use crate::parser::parse"));
    assert!(f.read("src/caller.rs").contains("use crate::parser::parse"));
    assert!(f.read("src/lib.rs").contains("mod parser;"));
    Ok(())
}

#[test]
fn rust_reexports_preserve_public_aliases() -> Result<()> {
    let f = Fixture::new(&[
        (
            "src/lib.rs",
            "pub mod a;\npub mod barrel;\npub fn run() -> i32 { barrel::parse() + barrel::stable() }\n",
        ),
        ("src/a.rs", "pub fn parse() -> i32 { 1 }\n"),
        (
            "src/barrel.rs",
            "pub use crate::a::{parse, parse as stable};\n",
        ),
    ])?;
    f.plan("src/a.rs::parse", "read_config")?.apply()?;
    assert!(f.read("src/barrel.rs").contains("read_config as stable"));
    assert!(
        f.read("src/lib.rs")
            .contains("barrel::read_config() + barrel::stable()")
    );
    Ok(())
}

#[test]
fn rust_type_rename_updates_type_positions_without_touching_fields() -> Result<()> {
    let f = Fixture::new(&[(
        "src/lib.rs",
        "pub struct User { pub value: i32 }\nimpl User { pub fn new() -> User { User { value: 1 } } }\n",
    )])?;
    f.plan("src/lib.rs::User", "Account")?.apply()?;
    let source = f.read("src/lib.rs");
    assert!(!source.contains("User"), "{source}");
    assert!(source.contains("value: 1"));
    Ok(())
}

#[test]
fn go_rename_resolves_package_names_not_directory_basename() -> Result<()> {
    let f = Fixture::new(&[
        ("go.mod", "module example.test/project\n\ngo 1.24\n"),
        ("a/a.go", "package actual\nfunc Parse() int { return 1 }\n"),
        (
            "a/local.go",
            "package actual\nfunc run() int { return Parse() }\n",
        ),
        (
            "main.go",
            "package main\nimport \"example.test/project/a\"\nfunc main() { actual.Parse() }\n",
        ),
    ])?;
    f.plan("a/a.go::Parse", "ReadConfig")?.apply()?;
    assert!(f.read("a/a.go").contains("func ReadConfig()"));
    assert!(f.read("a/local.go").contains("return ReadConfig()"));
    assert!(f.read("main.go").contains("actual.ReadConfig()"));
    Ok(())
}

#[test]
fn go_file_rename_within_package_preserves_package_references() -> Result<()> {
    let f = Fixture::new(&[
        ("a.go", "package example\nfunc Parse() int { return 1 }\n"),
        (
            "b.go",
            "package example\nfunc run() int { return Parse() }\n",
        ),
    ])?;
    f.plan("a.go", "parser.go")?.apply()?;
    assert!(f.read("parser.go").contains("func Parse()"));
    assert!(f.read("b.go").contains("return Parse()"));
    Ok(())
}

#[test]
fn go_function_move_within_package_transfers_and_prunes_imports() -> Result<()> {
    let f = Fixture::new(&[
        (
            "a.go",
            "package example\nimport \"strings\"\nfunc parse(s string) string { return strings.TrimSpace(s) }\nfunc run() string { return parse(\" x \") }\n",
        ),
        ("b.go", "package example\n"),
    ])?;
    f.plan("a.go::parse", "b.go")?.apply()?;
    assert!(!f.read("a.go").contains("import"));
    assert!(f.read("a.go").contains("return parse("));
    assert!(f.read("b.go").contains("import strings \"strings\""));
    assert!(f.read("b.go").contains("func parse("));
    Ok(())
}

#[test]
fn go_function_move_across_packages_rewrites_selectors_and_imports() -> Result<()> {
    let f = Fixture::new(&[
        ("go.mod", "module example.test/project\n\ngo 1.24\n"),
        ("a/a.go", "package a\nfunc Parse() int { return 1 }\n"),
        (
            "main.go",
            "package main\nimport \"example.test/project/a\"\nfunc main() { a.Parse() }\n",
        ),
    ])?;
    f.plan("a/a.go::Parse", "parser/parser.go")?.apply()?;
    assert!(f.read("parser/parser.go").starts_with("package parser"));
    assert!(f.read("main.go").contains("parser.Parse()"));
    assert!(!f.read("main.go").contains("project/a\""));
    assert!(f.read("main.go").contains("project/parser\""));
    Ok(())
}

#[test]
fn go_type_rename_updates_receivers_and_constructors_across_package_files() -> Result<()> {
    let f = Fixture::new(&[
        ("a.go", "package example\ntype User struct { Value int }\n"),
        (
            "b.go",
            "package example\nfunc (u User) Get() int { return u.Value }\nfunc makeUser() User { return User{Value: 1} }\n",
        ),
    ])?;
    f.plan("a.go::User", "Account")?.apply()?;
    assert!(!f.read("b.go").contains("User)"));
    assert!(f.read("b.go").contains("(u Account)"));
    assert!(f.read("b.go").contains("return Account{"));
    Ok(())
}

#[test]
fn java_public_type_rename_also_renames_constructors_and_filename() -> Result<()> {
    let f = Fixture::new(&[
        (
            "model/User.java",
            "package model;\npublic class User { public User() {} }\n",
        ),
        (
            "Main.java",
            "import model.User;\npublic class Main { User user = new User(); }\n",
        ),
    ])?;
    f.plan("model/User.java::User", "Account")?.apply()?;
    assert!(!f.root.join("model/User.java").exists());
    assert!(
        f.read("model/Account.java")
            .contains("class Account { public Account()")
    );
    assert!(f.read("Main.java").contains("import model.Account"));
    assert!(f.read("Main.java").contains("Account user = new Account()"));
    Ok(())
}

#[test]
fn java_static_method_rename_updates_static_imports_and_qualified_calls() -> Result<()> {
    let f = Fixture::new(&[
        (
            "util/Parser.java",
            "package util;\npublic class Parser { public static int parse() { return 1; } }\n",
        ),
        (
            "Main.java",
            "import static util.Parser.parse;\npublic class Main { int value = parse() + util.Parser.parse(); }\n",
        ),
    ])?;
    f.plan("util/Parser.java::Parser.parse", "readConfig")?
        .apply()?;
    assert!(
        f.read("Main.java")
            .contains("import static util.Parser.readConfig")
    );
    assert!(
        f.read("Main.java")
            .contains("readConfig() + util.Parser.readConfig()")
    );
    Ok(())
}

#[test]
fn java_file_move_updates_package_public_type_and_consumers() -> Result<()> {
    let f = Fixture::new(&[
        (
            "src/util/Parser.java",
            "package util;\npublic class Parser { public Parser() {} }\n",
        ),
        (
            "src/util/Main.java",
            "package util;\npublic class Main { Parser parser = new Parser(); }\n",
        ),
    ])?;
    f.plan("src/util/Parser.java", "src/config/ConfigParser.java")?
        .apply()?;
    let moved = f.read("src/config/ConfigParser.java");
    assert!(moved.contains("package config;"), "{moved}");
    assert!(
        moved.contains("class ConfigParser { public ConfigParser()"),
        "{moved}"
    );
    let main = f.read("src/util/Main.java");
    assert!(main.contains("import config.ConfigParser;"), "{main}");
    assert!(main.contains("ConfigParser parser = new ConfigParser()"));
    Ok(())
}

#[test]
fn java_static_method_move_updates_callers_and_promotes_access_as_needed() -> Result<()> {
    let f = Fixture::new(&[
        (
            "util/Parser.java",
            "package util;\npublic class Parser { private static int parse(int n) { return Math.abs(n); } public static int run() { return parse(-1); } }\n",
        ),
        (
            "util/Helpers.java",
            "package util;\npublic class Helpers {}\n",
        ),
    ])?;
    f.plan("util/Parser.java::Parser.parse", "util/Helpers.java")?
        .apply()?;
    let moved = f.read("util/Helpers.java");
    assert!(moved.contains("public static int parse(int n)"), "{moved}");
    assert!(
        f.read("util/Parser.java")
            .contains("return util.Helpers.parse(-1)")
    );
    Ok(())
}

#[test]
fn c_function_rename_updates_definitions_prototypes_and_explicit_callers() -> Result<()> {
    let f = Fixture::new(&[
        ("api.h", "int parse(int n);\n"),
        (
            "parse.c",
            "#include \"api.h\"\nint parse(int n) { return n + 1; }\n",
        ),
        (
            "main.c",
            "#include \"api.h\"\nint main(void) { return parse(1); }\n",
        ),
    ])?;
    f.plan("parse.c::parse", "read_config")?.apply()?;
    for path in ["api.h", "parse.c", "main.c"] {
        assert!(f.read(path).contains("read_config("), "{}", f.read(path));
    }
    Ok(())
}

#[test]
fn c_header_move_updates_includes_and_rebases_its_own_includes() -> Result<()> {
    let f = Fixture::new(&[
        ("include/api.h", "#include \"types.h\"\nint parse(void);\n"),
        ("include/types.h", "typedef int Value;\n"),
        (
            "main.c",
            "#include \"include/api.h\"\nint main(void) { return parse(); }\n",
        ),
    ])?;
    f.plan("include/api.h", "headers/parser.h")?.apply()?;
    assert!(f.read("main.c").contains("\"headers/parser.h\""));
    assert!(
        f.read("headers/parser.h")
            .contains("\"../include/types.h\"")
    );
    Ok(())
}

#[test]
fn c_function_move_keeps_a_prototype_for_remaining_source_callers() -> Result<()> {
    let f = Fixture::new(&[(
        "a.c",
        "int parse(int n) { return n + 1; }\nint run(void) { return parse(1); }\n",
    )])?;
    f.plan("a.c::parse", "parser.c")?.apply()?;
    assert!(
        f.read("parser.c")
            .contains("int parse(int n) { return n + 1; }")
    );
    assert!(f.read("a.c").contains("int parse(int n);"));
    assert!(f.read("a.c").contains("return parse(1)"));
    Ok(())
}

#[test]
fn c_rename_respects_local_shadowing_and_unrelated_static_functions() -> Result<()> {
    let f = Fixture::new(&[
        (
            "a.c",
            "static int parse(void) { return 1; }\nint run(int parse) { return parse; }\nint other(void) { return parse(); }\n",
        ),
        (
            "b.c",
            "static int parse(void) { return 2; }\nint run(void) { return parse(); }\n",
        ),
    ])?;
    f.plan("a.c::parse", "read_config")?.apply()?;
    assert!(
        f.read("a.c")
            .contains("int run(int parse) { return parse; }")
    );
    assert!(f.read("a.c").contains("return read_config()"));
    assert!(!f.read("b.c").contains("read_config"));
    Ok(())
}

#[test]
fn typed_moves_reject_inaccessible_dependencies_cycles_and_ambiguous_receivers() -> Result<()> {
    for (files, from, to) in [
        (
            vec![
                ("src/lib.rs", "mod a;"),
                ("src/a.rs", "fn helper() {} pub fn parse() { helper(); }"),
            ],
            "src/a.rs::parse",
            "src/b.rs",
        ),
        (
            vec![
                ("go.mod", "module example.test/p\n"),
                (
                    "a/a.go",
                    "package a\nfunc Helper() int { return 1 }; func Parse() int { return Helper() }; func Run() int { return Parse() }\n",
                ),
            ],
            "a/a.go::Parse",
            "b/b.go",
        ),
        (
            vec![
                (
                    "Parser.java",
                    "public class Parser { private static int VALUE = 1; public static int parse() { return VALUE; } }",
                ),
                ("Helpers.java", "public class Helpers {}"),
            ],
            "Parser.java::Parser.parse",
            "Helpers.java",
        ),
        (
            vec![(
                "a.c",
                "static int helper(void) { return 1; }\nint parse(void) { return helper(); }\n",
            )],
            "a.c::parse",
            "b.c",
        ),
    ] {
        let f = Fixture::new(&files)?;
        assert!(
            f.plan(from, to).is_err(),
            "Unexpected unsafe success: {from} -> {to}"
        );
        for (path, source) in files {
            assert_eq!(f.read(path), source);
        }
    }
    Ok(())
}

#[test]
fn native_operations_ignore_unrelated_javascript_resolution_policy() -> Result<()> {
    let f = Fixture::new(&[
        ("a.c", "int parse(void) { return 1; }\n"),
        ("a.ts", "export const unrelated = 1;\n"),
        ("tsconfig.json", "{\"extends\":\"./base.json\"}"),
    ])?;
    f.plan("a.c::parse", "read_config")?.apply()?;
    assert!(f.read("a.c").contains("read_config"));
    assert!(f.read("a.ts").contains("unrelated"));
    Ok(())
}

#[test]
fn c_typedefs_do_not_share_external_linkage_by_name() -> Result<()> {
    let f = Fixture::new(&[
        ("a.h", "typedef int Value;\n"),
        ("a.c", "#include \"a.h\"\nValue read(void) { return 1; }\n"),
        ("b.h", "typedef double Value;\n"),
        ("b.c", "#include \"b.h\"\nValue other(void) { return 2; }\n"),
    ])?;
    f.plan("a.h::Value", "Result")?.apply()?;
    assert!(f.read("a.c").contains("Result read"));
    assert!(f.read("b.h").contains("typedef double Value"));
    assert!(f.read("b.c").contains("Value other"));
    Ok(())
}

#[test]
fn rust_local_import_aliases_resolve_in_their_actual_scope() -> Result<()> {
    let f = Fixture::new(&[
        (
            "src/lib.rs",
            "pub mod a;\nfn parse() -> i32 { 0 }\nfn run() -> i32 { use crate::a::parse; parse() }\n",
        ),
        ("src/a.rs", "pub fn parse() -> i32 { 1 }\n"),
    ])?;
    f.plan("src/a.rs::parse", "read_config")?.apply()?;
    let source = f.read("src/lib.rs");
    assert!(source.contains("fn parse() -> i32 { 0 }"), "{source}");
    assert!(
        source.contains("use crate::a::read_config; read_config()"),
        "{source}"
    );
    Ok(())
}

#[test]
fn rust_file_moves_do_not_register_modules_in_unrelated_binary_entrypoints() -> Result<()> {
    let f = Fixture::new(&[
        ("src/lib.rs", "pub mod a;\n"),
        ("src/main.rs", "fn main() {}\n"),
        ("src/a.rs", "pub fn parse() {}\n"),
    ])?;
    f.plan("src/a.rs", "src/b.rs")?.apply()?;
    assert!(f.read("src/lib.rs").contains("mod b;"));
    assert_eq!(f.read("src/main.rs"), "fn main() {}\n");
    Ok(())
}

#[test]
fn shadowed_expression_macros_are_rejected_instead_of_treated_as_builtins() -> Result<()> {
    let f = Fixture::new(&[(
        "src/lib.rs",
        "macro_rules! println { ($value:expr) => { 1 }; }\nfn parse() -> i32 { 1 }\nfn run() -> i32 { println!(parse()) }\n",
    )])?;
    assert!(f.plan("src/lib.rs::parse", "read_config").is_err());
    Ok(())
}

#[test]
fn typed_declarations_without_trailing_newlines_keep_their_ast_identity() -> Result<()> {
    let f = Fixture::new(&[
        ("src/lib.rs", "pub mod a;"),
        ("src/a.rs", "pub fn parse() -> i32 { 1 }"),
    ])?;
    f.plan("src/a.rs::parse", "src/parser.rs")?.apply()?;
    assert!(f.read("src/parser.rs").contains("pub fn parse()"));
    let f = Fixture::new(&[("a.c", "int parse(void) { return 1; }")])?;
    f.plan("a.c::parse", "b.c")?.apply()?;
    assert!(f.read("b.c").contains("int parse(void)"));
    let f = Fixture::new(&[
        (
            "Parser.java",
            "public class Parser { public static int parse() { return 1; } }",
        ),
        ("Helpers.java", "public class Helpers {}"),
    ])?;
    f.plan("Parser.java::Parser.parse", "Helpers.java")?
        .apply()?;
    assert!(f.read("Helpers.java").contains("static int parse()"));
    Ok(())
}

#[test]
fn go_moves_preserve_build_roles_and_reject_initialization_changes() -> Result<()> {
    for (files, from, to) in [
        (
            vec![("a_linux.go", "package example\nfunc Parse() {}\n")],
            "a_linux.go",
            "b.go",
        ),
        (
            vec![("a_test.go", "package example\nfunc Parse() {}\n")],
            "a_test.go::Parse",
            "b.go",
        ),
        (
            vec![("a.go", "package example\nfunc init() {}\n")],
            "a.go::init",
            "b.go",
        ),
        (
            vec![
                ("go.mod", "module example.test/p\n"),
                (
                    "a/a.go",
                    "package a\nimport _ \"net/http/pprof\"\nfunc Parse() {}\n",
                ),
            ],
            "a/a.go::Parse",
            "b/b.go",
        ),
    ] {
        let f = Fixture::new(&files)?;
        assert!(
            f.plan(from, to).is_err(),
            "Unexpected unsafe success: {from} -> {to}"
        );
    }
    Ok(())
}

#[test]
fn static_c_functions_in_headers_cannot_move_away_from_includers() -> Result<()> {
    let f = Fixture::new(&[
        ("api.h", "static int parse(void) { return 1; }\n"),
        (
            "a.c",
            "#include \"api.h\"\nint run(void) { return parse(); }\n",
        ),
    ])?;
    assert!(f.plan("api.h::parse", "parser.c").is_err());
    Ok(())
}

#[test]
fn unrelated_conditional_rust_modules_do_not_block_refactors() -> Result<()> {
    let f = Fixture::new(&[
        ("src/lib.rs", "pub mod a;\n#[cfg(test)] mod test_support;\n"),
        ("src/a.rs", "pub fn parse() -> i32 { 1 }\n"),
        ("src/test_support.rs", "pub fn example() {}\n"),
    ])?;
    f.plan("src/a.rs::parse", "src/parser.rs::read_config")?
        .apply()?;
    assert!(f.read("src/parser.rs").contains("pub fn read_config()"));
    assert!(
        f.read("src/lib.rs")
            .contains("#[cfg(test)] mod test_support;")
    );
    assert!(
        f.plan("src/test_support.rs::example", "src/example.rs")
            .is_err()
    );
    Ok(())
}

#[test]
fn rust_macro_generated_symbols_require_expansion_but_standard_derives_work() -> Result<()> {
    for files in [
        vec![
            ("src/lib.rs", "include!(\"generated.rs\");\n"),
            ("src/generated.rs", "pub fn parse() {}\n"),
        ],
        vec![("src/lib.rs", "#[custom_attribute]\npub fn parse() {}\n")],
    ] {
        let f = Fixture::new(&files)?;
        let source = if f.root.join("src/generated.rs").exists() {
            "src/generated.rs::parse"
        } else {
            "src/lib.rs::parse"
        };
        assert!(f.plan(source, "read_config").is_err());
    }
    let f = Fixture::new(&[(
        "src/lib.rs",
        "#[derive(Debug, Clone)]\npub struct User { pub id: i32 }\n",
    )])?;
    f.plan("src/lib.rs::User", "Account")?.apply()?;
    assert!(f.read("src/lib.rs").contains("pub struct Account"));
    Ok(())
}

#[test]
fn c_moves_do_not_substitute_unrelated_declarations_for_private_dependencies() -> Result<()> {
    let f = Fixture::new(&[
        (
            "a.c",
            "static int helper(void) { return 1; }\nint parse(void) { return helper(); }\n",
        ),
        ("unrelated.h", "int helper(void);\n"),
    ])?;
    assert!(f.plan("a.c::parse", "parser.c").is_err());
    let f = Fixture::new(&[
        ("a.c", "int parse(void) { return 1; }\n"),
        ("b.c", "int parse(void) { return 2; }\n"),
    ])?;
    assert!(f.plan("a.c::parse", "b.c").is_err());
    let f = Fixture::new(&[(
        "a.c",
        "#include <stdbool.h>\nbool parse(void) { return true; }\n",
    )])?;
    assert!(f.plan("a.c::parse", "parser.c").is_err());
    Ok(())
}

#[test]
fn moves_reject_unresolvable_go_imports_and_attributed_rust_import_groups() -> Result<()> {
    for source in [
        "package example\nimport . \"strings\"\nfunc Parse(s string) string { return TrimSpace(s) }\n",
        "package example\nimport \"example.test/library/v2\"\nfunc Parse() { library.Run() }\n",
        "package example\nimport \"C\"\nfunc Parse() { C.run() }\n",
    ] {
        let f = Fixture::new(&[("a.go", source), ("b.go", "package example\n")])?;
        assert!(f.plan("a.go::Parse", "b.go").is_err(), "{source}");
        assert_eq!(f.read("a.go"), source);
    }
    let f = Fixture::new(&[
        ("src/lib.rs", "pub mod a;\npub mod caller;\n"),
        ("src/a.rs", "pub fn parse() {}\npub fn helper() {}\n"),
        (
            "src/caller.rs",
            "#[cfg(test)] use crate::a::{parse, helper};\n",
        ),
    ])?;
    assert!(f.plan("src/a.rs::parse", "src/parser.rs").is_err());
    Ok(())
}
