use super::*;
use crate::parse;
use tempfile::TempDir;

#[path = "typed_tests.rs"]
mod typed;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        for (path, source) in files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, source)?;
        }
        Ok(Self { _temp: temp, root })
    }

    fn plan(&self, from: &str, to: &str) -> Result<Plan> {
        self.plan_at(&self.root, from, to)
    }

    fn plan_at(&self, cwd: &Path, from: &str, to: &str) -> Result<Plan> {
        let mut files = BTreeMap::new();
        for entry in ignore::WalkBuilder::new(&self.root).hidden(false).build() {
            let entry = entry?;
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                let path = entry
                    .path()
                    .strip_prefix(&self.root)?
                    .to_str()
                    .unwrap()
                    .replace('\\', "/");
                if supported(&path) {
                    let source = fs::read_to_string(entry.path())?;
                    let parsed = parse::parse(&path, &source)?;
                    files.insert(path, (source, parsed.structure));
                }
            }
        }
        plan(&self.root, cwd, files, from, to)
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap()
    }
}

#[test]
fn rename_file_updates_all_module_forms_without_renaming_symbols() -> Result<()> {
    let f = Fixture::new(&[
        ("src/utils.ts", "export function parse() { return 1; }\n"),
        (
            "src/main.ts",
            "import { parse } from './utils.js';\nexport { parse as publicParse } from './utils';\nexport * from './utils.ts';\nimport * as utils from './utils';\nimport './utils';\nconst lazy = import('./utils');\nconst required = require('./utils');\nconst text = './utils';\nparse();\n",
        ),
    ])?;
    let plan = f.plan("src/utils.ts", "src/helpers.ts")?;
    assert_eq!(
        f.read("src/utils.ts"),
        "export function parse() { return 1; }\n"
    );
    let mut diff = Vec::new();
    plan.print_diff(&mut diff)?;
    assert!(String::from_utf8(diff)?.contains("+import { parse } from './helpers.js'"));
    plan.apply()?;
    assert!(!f.root.join("src/utils.ts").exists());
    assert!(f.read("src/helpers.ts").contains("function parse"));
    let source = f.read("src/main.ts");
    for expected in [
        "'./helpers.js'",
        "'./helpers.ts'",
        "import('./helpers')",
        "require('./helpers')",
        "const text = './utils'",
        "parse();",
    ] {
        assert!(source.contains(expected), "{source}");
    }
    Ok(())
}

#[test]
fn move_file_rebases_its_own_imports_and_directory_index_references() -> Result<()> {
    let f = Fixture::new(&[
        (
            "src/utils/index.ts",
            "import { value } from '../value.js';\nexport function parse() { return value; }\n",
        ),
        ("src/value.ts", "export const value = 1;\n"),
        (
            "src/main.ts",
            "import { parse } from './utils';\nparse();\n",
        ),
    ])?;
    f.plan("src/utils/index.ts", "lib/parser.ts")?.apply()?;
    assert!(f.read("lib/parser.ts").contains("'../src/value.js'"));
    assert!(f.read("src/main.ts").contains("'../lib/parser'"));
    Ok(())
}

#[test]
fn runtime_module_extensions_follow_the_destination_family() -> Result<()> {
    let f = Fixture::new(&[
        ("a.ts", "export function parse() {}\n"),
        ("main.ts", "import { parse } from './a.js';\nparse();\n"),
    ])?;
    f.plan("a.ts", "lib/parser.mts")?.apply()?;
    assert!(f.read("main.ts").contains("'./lib/parser.mjs'"));
    Ok(())
}

#[test]
fn moving_into_a_dependency_module_reuses_its_local_declaration() -> Result<()> {
    let f = Fixture::new(&[
        (
            "a.ts",
            "import { helper } from './b.js';\nexport function parse() { return helper(); }\n",
        ),
        ("b.ts", "export function helper() { return 1; }\n"),
    ])?;
    f.plan("a.ts::parse", "b.ts")?.apply()?;
    let b = f.read("b.ts");
    assert!(!b.contains("import"), "{b}");
    assert!(b.contains("function parse() { return helper(); }"));
    Ok(())
}

#[test]
fn new_imports_preserve_runtime_extensions_and_type_only_dependencies() -> Result<()> {
    let f = Fixture::new(&[
        (
            "a.ts",
            "export interface Options { value: number }\nexport function parse(options: Options) { return options.value; }\nparse({ value: 1 });\n",
        ),
        ("main.ts", "import { parse } from './a.js';\n"),
    ])?;
    f.plan("a.ts::parse", "lib/b.ts")?.apply()?;
    assert!(f.read("a.ts").contains("from \"./lib/b.js\""));
    assert!(
        f.read("lib/b.ts")
            .contains("import type { Options } from \"../a.js\"")
    );
    Ok(())
}

#[test]
fn rename_symbol_respects_lexical_binding_aliases_and_property_names() -> Result<()> {
    let f = Fixture::new(&[
        (
            "a.ts",
            "// parse stays in comments\nexport function parse() { return 1; }\nconst direct = parse;\nconst obj = { parse, parseKey: parse };\nconst literal = 'parse';\nconst template = `value ${parse()}`;\nfunction other(parse: () => number) { return parse(); }\n",
        ),
        (
            "b.ts",
            "import { parse } from './a';\nimport { parse as aliased } from './a';\nparse(); aliased();\nfunction other(parse: () => number) { return parse(); }\n",
        ),
        ("c.ts", "export function parse() { return 2; }\nparse();\n"),
    ])?;
    f.plan("a.ts::parse", "parseConfig")?.apply()?;
    let a = f.read("a.ts");
    for expected in [
        "function parseConfig()",
        "direct = parseConfig",
        "{ parse: parseConfig, parseKey: parseConfig }",
        "'parse'",
        "${parseConfig()}",
        "other(parse: () => number) { return parse(); }",
        "// parse stays",
    ] {
        assert!(a.contains(expected), "{a}");
    }
    let b = f.read("b.ts");
    for expected in [
        "{ parseConfig }",
        "{ parseConfig as aliased }",
        "parseConfig(); aliased();",
        "other(parse: () => number) { return parse(); }",
    ] {
        assert!(b.contains(expected), "{b}");
    }
    assert!(f.read("c.ts").contains("function parse()"));
    Ok(())
}

#[test]
fn rename_preserves_crlf_and_utf8_offsets() -> Result<()> {
    let original =
        "// 🐈 café\r\nexport function parse() { return 'parse'; }\r\nconst value = parse();\r\n";
    let f = Fixture::new(&[("a.ts", original)])?;
    f.plan("a.ts::parse", "readConfig")?.apply()?;
    assert_eq!(
        f.read("a.ts"),
        original
            .replace("function parse", "function readConfig")
            .replace("= parse()", "= readConfig()")
    );
    Ok(())
}

#[test]
fn renames_cover_top_level_types_enums_constants_and_variables() -> Result<()> {
    for (declaration, usage) in [
        ("export interface User { id: string }", "type Alias = User;"),
        ("export type User = { id: string };", "type Alias = User;"),
        ("export enum User { One }", "const value = User.One;"),
        ("export const User = 1;", "const value = User;"),
        ("export let User = 1;", "const value = User;"),
    ] {
        let consumer = format!("import {{ User }} from './a';\n{usage}\n");
        let f = Fixture::new(&[("a.ts", declaration), ("b.ts", &consumer)])?;
        f.plan("a.ts::User", "Account")?.apply()?;
        assert!(!f.read("a.ts").contains("User"));
        assert_eq!(f.read("b.ts"), consumer.replace("User", "Account"));
    }
    Ok(())
}

#[test]
fn rename_propagates_reexports_without_changing_public_aliases() -> Result<()> {
    let f = Fixture::new(&[
        (
            "a.ts",
            "function parse() { return 1; }\nexport { parse, parse as publicParse };\n",
        ),
        ("barrel.ts", "export { parse } from './a';\n"),
        ("star.ts", "export * from './barrel';\n"),
        ("alias.ts", "export { parse as stable } from './a';\n"),
        (
            "main.ts",
            "import { parse } from './star';\nimport { stable } from './alias';\nparse(); stable();\n",
        ),
    ])?;
    f.plan("a.ts::parse", "readConfig")?.apply()?;
    assert!(
        f.read("a.ts")
            .contains("export { readConfig, readConfig as publicParse }")
    );
    assert!(f.read("barrel.ts").contains("{ readConfig }"));
    assert!(f.read("alias.ts").contains("{ readConfig as stable }"));
    assert!(f.read("main.ts").contains("readConfig(); stable();"));
    Ok(())
}

#[test]
fn explicit_barrel_exports_override_unrelated_star_exports() -> Result<()> {
    let f = Fixture::new(&[
        ("a.ts", "export function parse() {}\n"),
        ("other.ts", "export function parse() {}\n"),
        (
            "barrel.ts",
            "export * from './a';\nexport { parse } from './other';\n",
        ),
        ("main.ts", "import { parse } from './barrel';\nparse();\n"),
    ])?;
    f.plan("a.ts::parse", "renamed")?.apply()?;
    assert_eq!(
        f.read("main.ts"),
        "import { parse } from './barrel';\nparse();\n"
    );
    assert!(f.read("barrel.ts").contains("{ parse }"));
    Ok(())
}

#[test]
fn moving_into_a_caller_removes_its_old_import() -> Result<()> {
    let f = Fixture::new(&[
        ("a.ts", "export function parse() { return 1; }\n"),
        (
            "b.ts",
            "import { parse } from './a';\nconst result = parse();\n",
        ),
    ])?;
    f.plan("a.ts::parse", "b.ts")?.apply()?;
    let b = f.read("b.ts");
    assert!(!b.contains("from './a'"), "{b}");
    assert!(b.contains("function parse()"), "{b}");
    assert!(b.contains("result = parse()"), "{b}");
    Ok(())
}

#[test]
fn rename_namespace_uses_and_jsx_and_type_references() -> Result<()> {
    let f = Fixture::new(&[
        (
            "a.tsx",
            "export class User {}\nexport function View() { return <div />; }\n",
        ),
        (
            "b.tsx",
            "import { User, View } from './a';\nimport * as api from './a';\nconst user: User = new api.User();\nconst view = <View />;\n",
        ),
    ])?;
    f.plan("a.tsx::User", "Account")?.apply()?;
    let b = f.read("b.tsx");
    assert!(b.contains("{ Account, View }"), "{b}");
    assert!(b.contains("user: Account = new api.Account()"), "{b}");
    f.plan("a.tsx::View", "Panel")?.apply()?;
    assert!(f.read("b.tsx").contains("<Panel />"));
    Ok(())
}

#[test]
fn local_require_functions_and_shadowed_namespaces_are_not_module_references() -> Result<()> {
    let f = Fixture::new(&[
        ("a.ts", "export function parse() {}\n"),
        (
            "main.ts",
            "import * as api from './a';\napi.parse();\nfunction local(api: {parse(): void}, require: (s: string) => void) { api.parse(); require('./a'); }\n",
        ),
    ])?;
    f.plan("a.ts::parse", "readConfig")?.apply()?;
    let main = f.read("main.ts");
    assert!(main.contains("api.readConfig()"), "{main}");
    assert!(main.contains("{ api.parse(); require('./a'); }"), "{main}");
    f.plan("a.ts", "b.ts")?.apply()?;
    let main = f.read("main.ts");
    assert!(main.contains("from './b'"), "{main}");
    assert!(main.contains("require('./a')"), "{main}");
    Ok(())
}

#[test]
fn renames_reject_export_collisions_and_file_moves_reject_new_ambiguity() -> Result<()> {
    let f = Fixture::new(&[
        ("a.ts", "export function parse() {}\n"),
        (
            "barrel.ts",
            "export { parse } from './a';\nexport { something as renamed } from 'external';\n",
        ),
    ])?;
    assert!(f.plan("a.ts::parse", "renamed").is_err());
    let f = Fixture::new(&[
        ("a.ts", "export function parse() {}\n"),
        ("existing.ts", "export function unrelated() {}\n"),
        ("main.ts", "import { parse } from './a';\n"),
    ])?;
    assert!(f.plan("a.ts", "existing/index.ts").is_err());
    assert!(f.root.join("a.ts").exists());
    Ok(())
}

#[test]
fn moving_function_transfers_imports_and_rewires_mixed_imports_and_callers() -> Result<()> {
    let f = Fixture::new(&[
        ("src/dep.ts", "export const value = 3;\n"),
        (
            "src/a.ts",
            "import { value as v } from './dep.js';\nexport const other = 2;\n/** Parse configuration. */\nexport function parse(n = 1) { return v + n; }\nexport function caller() { return parse(); }\n",
        ),
        (
            "src/main.ts",
            "import { parse as read, other } from './a.js';\nread();\n",
        ),
        ("src/barrel.ts", "export { parse, other } from './a.js';\n"),
        ("src/all.ts", "export * from './a.js';\n"),
    ])?;
    let plan = f.plan("src/a.ts::parse", "lib/parser.ts")?;
    assert!(!f.root.join("lib").exists());
    plan.apply()?;
    let moved = f.read("lib/parser.ts");
    assert!(
        moved.contains("import { value as v } from \"../src/dep.js\""),
        "{moved}"
    );
    assert!(
        moved.contains("/** Parse configuration. */\nexport function parse"),
        "{moved}"
    );
    let original = f.read("src/a.ts");
    assert!(!original.contains("function parse"), "{original}");
    assert!(
        original.contains("import { parse } from \"../lib/parser.js\""),
        "{original}"
    );
    let main = f.read("src/main.ts");
    assert!(main.contains("{ other } from './a.js'"), "{main}");
    assert!(
        main.contains("{ parse as read } from \"../lib/parser.js\""),
        "{main}"
    );
    assert!(
        f.read("src/barrel.ts")
            .contains("{ parse } from \"../lib/parser.js\"")
    );
    assert!(
        f.read("src/all.ts")
            .contains("export { parse } from \"../lib/parser.js\"")
    );
    Ok(())
}

#[test]
fn move_and_rename_recursive_arrow_function() -> Result<()> {
    let f = Fixture::new(&[
        (
            "a.ts",
            "export const parse = (n: number): number => n ? parse(n - 1) : 0;\n",
        ),
        ("b.ts", "import { parse } from './a';\nparse(2);\n"),
    ])?;
    f.plan("a.ts::parse", "lib/parser.ts::readConfig")?
        .apply()?;
    assert!(
        f.read("lib/parser.ts")
            .contains("export const readConfig = (n: number): number => n ? readConfig(n - 1) : 0")
    );
    assert!(f.read("b.ts").contains("readConfig(2)"));
    assert!(f.read("b.ts").contains("from \"./lib/parser\""));
    Ok(())
}

#[test]
fn move_function_with_exported_dependency_and_local_callers() -> Result<()> {
    let f = Fixture::new(&[(
        "a.ts",
        "export function helper() { return 1; }\nfunction parse() { return helper(); }\nconst value = parse();\n",
    )])?;
    f.plan("a.ts::parse", "b.ts")?.apply()?;
    assert!(f.read("b.ts").contains("import { helper } from \"./a\""));
    assert!(f.read("b.ts").contains("export function parse"));
    assert!(f.read("a.ts").contains("import { parse } from \"./b\""));
    Ok(())
}

#[test]
fn unsafe_operations_fail_without_mutation() -> Result<()> {
    let cases = [
        (
            "export function parse() {}\nconst renamed = 1;",
            "",
            "a.ts::parse",
            "renamed",
        ),
        (
            "export function parse() {}",
            "import { parse } from './a'; function use(renamed: number) { parse(); }",
            "a.ts::parse",
            "renamed",
        ),
        (
            "export function parse() {}",
            "import * as api from './a'; api['parse']();",
            "a.ts::parse",
            "renamed",
        ),
        (
            "export function parse() {}",
            "const api = require('./a'); api.parse();",
            "a.ts::parse",
            "renamed",
        ),
        (
            "function helper() {}\nexport function parse() { helper(); }",
            "",
            "a.ts::parse",
            "lib/b.ts",
        ),
        (
            "export default function parse() {}",
            "",
            "a.ts::parse",
            "lib/b.ts",
        ),
        ("export function parse() {}", "", "a.ts::parse", "class"),
        ("export function parse() {}", "", "a.ts", "../outside.ts"),
        ("export function parse() {}", "", "a.ts", "b.ts"),
        ("export function parse() {}", "", "a.ts::missing", "renamed"),
    ];
    for (a, b, from, to) in cases {
        let f = Fixture::new(&[("a.ts", a), ("b.ts", b)])?;
        assert!(
            f.plan(from, to).is_err(),
            "Unexpected success: {from} -> {to}"
        );
        assert_eq!(f.read("a.ts"), a);
        assert_eq!(f.read("b.ts"), b);
        assert!(!f.root.join("lib").exists());
    }
    Ok(())
}

#[test]
fn stale_inputs_and_destination_races_are_rejected() -> Result<()> {
    let f = Fixture::new(&[
        ("a.ts", "export function parse() {}\n"),
        ("main.ts", "import { parse } from './a';\nparse();\n"),
    ])?;
    let plan = f.plan("a.ts::parse", "renamed")?;
    fs::write(f.root.join("main.ts"), "// concurrent edit\n")?;
    assert!(plan.apply().is_err());
    assert!(f.read("a.ts").contains("function parse"));
    let plan = f.plan("a.ts", "b.ts")?;
    fs::write(f.root.join("b.ts"), "// do not overwrite\n")?;
    assert!(plan.apply().is_err());
    assert_eq!(f.read("b.ts"), "// do not overwrite\n");
    Ok(())
}

#[test]
fn failed_publication_restores_completed_writes() -> Result<()> {
    let f = Fixture::new(&[("a.ts", "original\n")])?;
    let plan = Plan {
        root: f.root.clone(),
        inputs: BTreeMap::from([("a.ts".into(), "original\n".into())]),
        permissions: BTreeMap::new(),
        changes: vec![
            FileChange {
                path: "a.ts".into(),
                before: Some("original\n".into()),
                after: Some("changed\n".into()),
            },
            // A deliberately invalid deletion exercises failure after publication.
            FileChange {
                path: "missing.ts".into(),
                before: Some("missing\n".into()),
                after: None,
            },
        ],
    };
    assert!(plan.apply().is_err());
    assert_eq!(f.read("a.ts"), "original\n");
    Ok(())
}

#[test]
fn paths_resolve_from_cwd_not_from_source_or_root() -> Result<()> {
    let f = Fixture::new(&[("src/a.ts", "export function parse() {}\n")])?;
    f.plan_at(&f.root.join("src"), "a.ts", "../lib/b.ts")?
        .apply()?;
    assert!(f.root.join("lib/b.ts").exists());
    assert!(!f.root.join("src/a.ts").exists());
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_paths_accept_verbatim_drive_and_mixed_separator_spellings() -> Result<()> {
    for spelling in 0..3 {
        let f = Fixture::new(&[("src/a.ts", "export function parse() {}\n")])?;
        let source = f.root.join("src/a.ts").to_str().unwrap().to_owned();
        let destination = f.root.join("lib/b.ts").to_str().unwrap().to_owned();
        let convert = |path: String| match spelling {
            0 => path,
            1 => path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned(),
            _ => path
                .strip_prefix(r"\\?\")
                .unwrap_or(&path)
                .replace("\\src\\", "/src/")
                .replace("\\lib\\", "/lib/"),
        };
        f.plan(&convert(source), &convert(destination))?.apply()?;
        assert!(f.root.join("lib/b.ts").exists());
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn file_moves_preserve_executable_permissions() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(&[("a.js", "#!/usr/bin/env node\nconsole.log('hello');\n")])?;
    match fs::set_permissions(f.root.join("a.js"), fs::Permissions::from_mode(0o755)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    if fs::metadata(f.root.join("a.js"))?.permissions().mode() & 0o777 != 0o755 {
        return Ok(());
    }
    f.plan("a.js", "bin/b.js")?.apply()?;
    assert_eq!(
        fs::metadata(f.root.join("bin/b.js"))?.permissions().mode() & 0o777,
        0o755
    );
    Ok(())
}

#[test]
fn custom_module_resolution_and_nonliteral_loads_are_rejected() -> Result<()> {
    for policy in [
        "{\"compilerOptions\":{\"paths\":{\"@/*\":[\"src/*\"]}}}",
        "{\"extends\":\"./base.json\"}",
    ] {
        let f = Fixture::new(&[
            ("a.ts", "export function parse() {}\n"),
            ("tsconfig.json", policy),
        ])?;
        assert!(f.plan("a.ts", "b.ts").is_err());
    }
    let f = Fixture::new(&[(
        "a.ts",
        "export function parse() {}\nconst loaded = import(moduleName);\n",
    )])?;
    assert!(f.plan("a.ts::parse", "read").is_err());
    let f = Fixture::new(&[(
        "a.ts",
        "export function parse() {}\nnamespace N { export function parse() {} }\n",
    )])?;
    assert!(f.plan("a.ts::parse", "read").is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
fn paths_support_symlinked_cwd_but_never_write_through_workspace_symlinks() -> Result<()> {
    use std::os::unix::fs::symlink;
    let f = Fixture::new(&[("src/a.ts", "export function parse() {}\n")])?;
    match symlink(f.root.join("src"), f.root.join("alias")) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    }
    f.plan_at(&f.root.join("alias"), "a.ts::parse", "renamed")?
        .apply()?;
    assert!(f.read("src/a.ts").contains("function renamed"));
    assert!(f.plan("src/a.ts", "alias/b.ts").is_err());
    assert!(f.plan("alias/../src/a.ts", "b.ts").is_err());
    Ok(())
}

#[test]
fn diff_preserves_missing_newline_and_empty_file_ranges() -> Result<()> {
    let f = Fixture::new(&[("a.ts", "const value = 1;")])?;
    let mut diff = Vec::new();
    f.plan("a.ts", "b.ts")?.print_diff(&mut diff)?;
    let diff = String::from_utf8(diff)?;
    assert!(diff.contains("@@ -1,1 +0,0 @@"), "{diff}");
    assert!(diff.contains("\\ No newline at end of file"), "{diff}");
    Ok(())
}
