use super::*;

/// Compiler checks supplement (never replace) unconditional parser/CLI tests.
/// Compilers are optional tools, not new runtime dependencies of refactor.
fn compiler(name: &str) -> Result<Option<PathBuf>> {
    let version = Command::new(name)
        .arg(match name {
            "go" => "version",
            "javac" => "-version",
            _ => "--version",
        })
        .env("GOTOOLCHAIN", "local")
        .output();
    match version {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(output) => ensure!(
            output.status.success(),
            "Cannot run installed compiler {name}: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    }
    if name == "rustc" {
        // Bypass the rustup proxy when HOME is isolated in the fixture child.
        let root = Command::new(name).args(["--print", "sysroot"]).output()?;
        ensure!(
            root.status.success(),
            "Cannot resolve installed Rust compiler"
        );
        return Ok(Some(
            PathBuf::from(String::from_utf8(root.stdout)?.trim())
                .join("bin")
                .join(format!("rustc{}", std::env::consts::EXE_SUFFIX)),
        ));
    }
    Ok(Some(PathBuf::from(name)))
}

fn compiled(output: Output) -> Result<()> {
    ensure!(
        output.status.success(),
        "Compiler rejected refactored source:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn typed_language_cli_coverage_does_not_require_installed_compilers() -> Result<()> {
    for (files, from, to, changed, expected) in [
        (
            vec![("src/lib.rs", "pub fn parse() -> i32 { 1 }\n")],
            "src/lib.rs::parse",
            "read_config",
            "src/lib.rs",
            "fn read_config()",
        ),
        (
            vec![("a.go", "package example\nfunc Parse() int { return 1 }\n")],
            "a.go::Parse",
            "ReadConfig",
            "a.go",
            "func ReadConfig()",
        ),
        (
            vec![(
                "Parser.java",
                "public class Parser { public static int parse() { return 1; } }\n",
            )],
            "Parser.java::Parser.parse",
            "readConfig",
            "Parser.java",
            "int readConfig()",
        ),
        (
            vec![("a.c", "int parse(void) { return 1; }\n")],
            "a.c::parse",
            "read_config",
            "a.c",
            "int read_config(",
        ),
    ] {
        let repo = Repo::new()?;
        for (path, source) in &files {
            repo.write(path, source)?;
        }
        let preview = repo.cli_json(&["refactor", from, to, "--dry-run"])?;
        assert_eq!(preview["dryRun"], true);
        for (path, source) in &files {
            assert_eq!(fs::read_to_string(repo.root.join(path))?, *source);
        }
        let applied = repo.cli_json(&["refactor", from, to])?;
        assert_eq!(applied["dryRun"], false);
        assert!(fs::read_to_string(repo.root.join(changed))?.contains(expected));
    }
    Ok(())
}

#[test]
fn typed_rust_cli_refactors_compile_with_module_and_import_updates() -> Result<()> {
    let Some(rustc) = compiler("rustc")? else {
        return Ok(());
    };
    let repo = Repo::new()?;
    repo.write(
        "src/lib.rs",
        "//! Library documentation.\npub mod utils;\npub mod caller;\n",
    )?;
    repo.write(
        "src/utils.rs",
        "pub fn helper() -> i32 { 1 }\npub fn parse() -> i32 { helper() }\n",
    )?;
    repo.write("src/caller.rs", "use crate::utils::parse;\npub fn run() -> i32 { assert_eq!(crate::utils::parse(), 1); parse() }\n")?;
    repo.cli_json(&["refactor", "src/utils.rs", "src/helpers.rs"])?;
    repo.cli_json(&[
        "refactor",
        "src/helpers.rs::parse",
        "src/parser.rs::read_config",
    ])?;
    compiled(
        repo.child(rustc)
            .args(["--edition=2024", "--crate-type=lib", "src/lib.rs", "-o"])
            .arg(repo.home.join("fixture.rlib"))
            .output()?,
    )?;
    Ok(())
}

#[test]
fn rust_path_file_moves_update_test_crates_offline_without_changing_module_names() -> Result<()> {
    let repo = Repo::new()?;
    let driver = "#[path = \"support.rs\"]\nmod suite;\nfn expected() -> i32 { 1 }\n#[test] fn smoke() { assert_eq!(suite::parse(), 1); }\n";
    let other = "#[cfg(test)]\n#[path = r#\"support.rs\"#]\nmod shared;\nfn expected() -> i32 { 1 }\n#[test] fn smoke() { assert_eq!(shared::parse(), 1); }\n";
    let support = "pub fn parse() -> i32 { super::expected() }\n";
    repo.write("tests/driver.rs", driver)?;
    repo.write("tests/other.rs", other)?;
    repo.write("tests/support.rs", support)?;
    let args = ["refactor", "tests/support.rs", "tests/nested/lifecycle.rs"];
    let mut preview_args = args.to_vec();
    preview_args.push("--dry-run");
    let preview = repo.cli_json(&preview_args)?;
    assert_eq!(preview["dryRun"], true);
    assert_eq!(
        fs::read_to_string(repo.root.join("tests/driver.rs"))?,
        driver
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("tests/support.rs"))?,
        support
    );
    assert!(!repo.root.join("tests/nested/lifecycle.rs").exists());
    repo.cli_json(&args)?;
    assert!(!repo.root.join("tests/support.rs").exists());
    assert_eq!(
        fs::read_to_string(repo.root.join("tests/nested/lifecycle.rs"))?,
        support
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("tests/driver.rs"))?,
        driver.replace("support.rs", "nested/lifecycle.rs")
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("tests/other.rs"))?,
        other.replace("r#\"support.rs\"#", "\"nested/lifecycle.rs\"")
    );
    if let Some(rustc) = compiler("rustc")? {
        for entry in ["driver", "other"] {
            compiled(
                repo.child(&rustc)
                    .args(["--edition=2024", "--test"])
                    .arg(format!("tests/{entry}.rs"))
                    .arg("-o")
                    .arg(
                        repo.home
                            .join(format!("{entry}{}", std::env::consts::EXE_SUFFIX)),
                    )
                    .output()?,
            )?;
        }
    }
    Ok(())
}

#[test]
fn typed_go_cli_refactors_compile_without_unused_imports_or_package_cycles() -> Result<()> {
    let Some(go) = compiler("go")? else {
        return Ok(());
    };
    let repo = Repo::new()?;
    repo.write("go.mod", "module example.test/project\n\ngo 1.18\n")?;
    repo.write("a/a.go", "package a\nimport \"strings\"\nfunc Parse(s string) string { return strings.TrimSpace(s) }\n")?;
    repo.write(
        "main.go",
        "package main\nimport \"example.test/project/a\"\nfunc main() { a.Parse(\" x \") }\n",
    )?;
    repo.cli_json(&["refactor", "a/a.go::Parse", "parser/parser.go::ReadConfig"])?;
    compiled(
        repo.child(go)
            .env("GOTOOLCHAIN", "local")
            .env("GOPROXY", "off")
            .env("GOCACHE", repo.home.join("go-cache"))
            .env("GOPATH", repo.home.join("go-path"))
            .args(["test", "-timeout=2m", "./..."])
            .output()?,
    )?;
    Ok(())
}

#[test]
fn typed_c_cli_refactors_compile_and_link_with_updated_prototypes_and_includes() -> Result<()> {
    let Some(cc) = compiler("cc")? else {
        return Ok(());
    };
    let repo = Repo::new()?;
    repo.write("api.h", "int parse(int n);\n")?;
    repo.write("a.c", "#include \"api.h\"\nint parse(int n) { return n + 1; }\nint run(void) { return parse(1); }\n")?;
    repo.write(
        "main.c",
        "#include \"api.h\"\nint main(void) { return parse(1); }\n",
    )?;
    repo.cli_json(&["refactor", "api.h", "include/parser.h"])?;
    repo.cli_json(&["refactor", "a.c::parse", "parser.c::read_config"])?;
    compiled(
        repo.child(cc)
            .args([
                "-std=c11", "-Wall", "-Werror", "a.c", "parser.c", "main.c", "-o",
            ])
            .arg(
                repo.home
                    .join(format!("fixture{}", std::env::consts::EXE_SUFFIX)),
            )
            .output()?,
    )?;
    Ok(())
}

#[test]
fn typed_java_cli_refactors_compile_with_public_type_and_static_method_updates() -> Result<()> {
    let Some(javac) = compiler("javac")? else {
        return Ok(());
    };
    let repo = Repo::new()?;
    repo.write("model/User.java", "package model;\npublic class User { public User() {} public static int parse(int n) { return Math.abs(n); } }\n")?;
    repo.write(
        "domain/Helpers.java",
        "package domain;\npublic class Helpers {}\n",
    )?;
    repo.write("Main.java", "import model.User;\npublic class Main { User user = new User(); int value = User.parse(-1); }\n")?;
    repo.cli_json(&["refactor", "model/User.java", "domain/Person.java"])?;
    repo.cli_json(&[
        "refactor",
        "domain/Person.java::Person.parse",
        "domain/Helpers.java::readConfig",
    ])?;
    let classes = repo.home.join("classes");
    fs::create_dir(&classes)?;
    compiled(
        repo.child(javac)
            .arg("-d")
            .arg(classes)
            .args(["domain/Person.java", "domain/Helpers.java", "Main.java"])
            .output()?,
    )?;
    Ok(())
}

#[test]
fn engine_refactor_requires_live_bytes_matching_its_structure_snapshot() -> Result<()> {
    let repo = Repo::new()?;
    repo.write("a.ts", "export function parse() {}\n")?;
    let mut engine = repo.open_map(&json!({}))?;
    engine.refresh_structure()?;
    let plan = engine.plan_refactor("a.ts::parse", "readConfig", &repo.root)?;
    repo.write("a.ts", "export function parse() { return 2; }\n")?;
    assert!(plan.apply().is_err());
    assert!(
        engine
            .plan_refactor("a.ts::parse", "readConfig", &repo.root)
            .is_err()
    );
    engine.refresh_structure()?;
    engine
        .plan_refactor("a.ts::parse", "readConfig", &repo.root)?
        .apply()?;
    assert!(fs::read_to_string(repo.root.join("a.ts"))?.contains("function readConfig()"));
    Ok(())
}

#[test]
fn cli_refactor_dry_run_apply_and_index_refresh_are_offline() -> Result<()> {
    let repo = Repo::new()?;
    repo.init_git()?;
    repo.write("src/a.ts", "export function parse() { return 1; }\n")?;
    repo.write("src/main.ts", "import { parse } from './a.js';\nparse();\n")?;
    let preview = repo.cli_json(&["refactor", "src/a.ts::parse", "parseConfig", "--dry-run"])?;
    assert_eq!(preview["dryRun"], true);
    assert_eq!(preview["changes"].as_array().unwrap().len(), 2);
    assert!(fs::read_to_string(repo.root.join("src/a.ts"))?.contains("function parse()"));
    let applied = repo.cli_json(&["refactor", "src/a.ts::parse", "lib/parser.ts::parseConfig"])?;
    assert_eq!(applied["dryRun"], false);
    assert!(!fs::read_to_string(repo.root.join("src/a.ts"))?.contains("function parse"));
    assert!(fs::read_to_string(repo.root.join("lib/parser.ts"))?.contains("function parseConfig"));
    let mapped = repo.cli_json(&["map", "--no-reindex", "--private"])?;
    let mapped = mapped.to_string();
    assert!(mapped.contains("parseConfig"), "{mapped}");
    assert!(mapped.contains("lib/parser.ts"), "{mapped}");
    Ok(())
}

#[test]
fn cli_refactor_file_move_and_text_diff_leave_sources_untouched_during_preview() -> Result<()> {
    let repo = Repo::new()?;
    repo.write("src/a.ts", "export const VALUE = 1;\n")?;
    repo.write("main.ts", "import { VALUE } from './src/a';\n")?;
    let preview = repo.cli_format(&["refactor", "src/a.ts", "lib/b.ts", "--dry-run"], "text")?;
    ensure!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let text = String::from_utf8(preview.stdout)?;
    assert!(text.contains("--- a/src/a.ts"), "{text}");
    assert!(text.contains("+++ b/lib/b.ts"), "{text}");
    assert!(text.contains("Would change 3 file(s)"), "{text}");
    assert!(!repo.root.join("lib").exists());
    repo.cli_json(&["refactor", "src/a.ts", "lib/b.ts"])?;
    assert!(!repo.root.join("src/a.ts").exists());
    assert!(fs::read_to_string(repo.root.join("main.ts"))?.contains("'./lib/b'"));
    Ok(())
}

#[test]
fn cli_refactor_markdown_moves_update_links_offline_and_refresh_structure() -> Result<()> {
    let repo = Repo::new()?;
    repo.init_git()?;
    repo.write("docs/old.md", "# Guide\n\n[Home](../README.md)\n")?;
    repo.write("README.md", "# Home\n\n[Guide](docs/old.md#guide)\n")?;
    repo.write(
        "nested/index.markdown",
        "[Guide][guide]\n\n[guide]: ../docs/old.md\n",
    )?;
    repo.write("ignored.md", "[Guide](docs/old.md)\n")?;
    repo.write(".gitignore", "ignored.md\n")?;
    let preview = repo.cli_json(&["refactor", "docs/old.md", "guides/deep/new.md", "--dry-run"])?;
    assert_eq!(preview["dryRun"], true);
    assert_eq!(preview["changes"].as_array().unwrap().len(), 4);
    assert!(repo.root.join("docs/old.md").exists());
    assert!(!repo.root.join("guides").exists());

    let report = repo.cli_json(&["refactor", "docs/old.md", "guides/deep/new.md"])?;
    assert_eq!(report["dryRun"], false);
    assert!(!repo.root.join("docs/old.md").exists());
    assert_eq!(
        fs::read_to_string(repo.root.join("guides/deep/new.md"))?,
        "# Guide\n\n[Home](../../README.md)\n"
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("README.md"))?,
        "# Home\n\n[Guide](guides/deep/new.md#guide)\n"
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("nested/index.markdown"))?,
        "[Guide][guide]\n\n[guide]: ../guides/deep/new.md\n"
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("ignored.md"))?,
        "[Guide](docs/old.md)\n"
    );
    let mapped = repo.cli_json(&["map", "--no-reindex", "--private"])?;
    let mapped = mapped.to_string();
    assert!(mapped.contains("guides/deep/new.md"), "{mapped}");
    assert!(!mapped.contains("docs/old.md"), "{mapped}");
    Ok(())
}

#[test]
fn cli_refactor_rejects_saved_snapshots_and_conflicts() -> Result<()> {
    let repo = Repo::new()?;
    let original = "export function parse() {}\nconst collision = 1;\n";
    repo.write("a.ts", original)?;
    let output = repo.cli(&["refactor", "a.ts::parse", "collision", "--no-reindex"])?;
    assert_eq!(output.status.code(), Some(2));
    assert!(!repo.index.exists());
    let output = repo.cli(&["refactor", "a.ts::parse", "collision"])?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(fs::read_to_string(repo.root.join("a.ts"))?, original);
    Ok(())
}

#[cfg(unix)]
#[test]
fn cli_refactor_paths_are_cwd_relative_even_through_a_symlink() -> Result<()> {
    let repo = Repo::new()?;
    repo.init_git()?;
    repo.write("src/a.ts", "export function parse() {}\n")?;
    let alias = repo.home.join("source-alias");
    match std::os::unix::fs::symlink(repo.root.join("src"), &alias) {
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
    let output = repo
        .child(env!("CARGO_BIN_EXE_slopdex"))
        .current_dir(&alias)
        .arg("--index")
        .arg(&repo.index)
        .args(["refactor", "a.ts", "../lib/b.ts", "--output", "json"])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        report["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["path"] == "lib/b.ts")
    );
    assert!(repo.root.join("lib/b.ts").exists());
    Ok(())
}
