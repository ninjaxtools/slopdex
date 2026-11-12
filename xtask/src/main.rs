//! Cross-platform development and release checks, invoked through Cargo aliases.
use std::{env, error::Error, fs, path::PathBuf, process::Command};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn main() {
    if let Err(error) = run() {
        eprintln!("xtask: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    let release = match args.as_slice() {
        [task] if task == "check" => false,
        [task] if task == "release-check" => true,
        _ => return Err("usage: cargo verify | cargo release-check".into()),
    };
    env::set_current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap())?;
    let metadata: serde_json::Value = serde_json::from_str(&capture(
        "cargo",
        &["metadata", "--locked", "--no-deps", "--format-version", "1"],
    )?)?;
    let package = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "slopdex")
        .ok_or("Cargo package slopdex is missing")?;
    let version = package["version"].as_str().ok_or("Missing Cargo version")?;
    let npm: serde_json::Value = serde_json::from_str(&fs::read_to_string("npm/package.json")?)?;
    if npm["name"] != "@ninjaxtools/slopdex" || npm["version"] != version {
        return Err(
            "npm/package.json name/version must match @ninjaxtools/slopdex and Cargo.toml".into(),
        );
    }
    for field in ["description", "license"] {
        if npm[field] != package[field] {
            return Err(format!("npm/package.json {field} must match Cargo.toml").into());
        }
    }

    // Cheap preconditions first, so they fail before the long checks.
    check_package_contents()?;
    if release {
        require_dist()?;
    }

    command("cargo", &["fmt", "--all", "--", "--check"])?;
    command(
        "cargo",
        &["check", "--locked", "--workspace", "--all-targets"],
    )?;
    command(
        "cargo",
        &[
            "clippy",
            "--locked",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    command("cargo", &["test", "--locked", "--workspace"])?;
    let actual = capture(
        "cargo",
        &[
            "run",
            "--locked",
            "--release",
            "--bin",
            "slopdex",
            "--",
            "--version",
        ],
    )?;
    if actual.trim() != format!("slopdex {version}") {
        return Err(format!("Unexpected native version: {}", actual.trim()).into());
    }
    let help = capture(
        "cargo",
        &[
            "run",
            "--locked",
            "--release",
            "--bin",
            "slopdex",
            "--",
            "--help",
        ],
    )?;
    for name in EXPECTED_COMMANDS {
        if !help.contains(name) {
            return Err(format!("Native help is missing {name}").into());
        }
    }
    if release {
        command("dist", &["generate", "--check"])?;
        command("dist", &["plan"])?;
    }
    println!(
        "{} passed",
        if release {
            "Release checks"
        } else {
            "Cargo checks and native smoke"
        }
    );
    Ok(())
}

/// cargo-dist is installed separately; a bare spawn failure does not say how.
fn require_dist() -> Result<()> {
    let install = "cargo install cargo-dist --locked --version 0.32.0";
    match Command::new("dist").arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(format!(
            "`dist --version` failed ({}); reinstall it with `{install}`",
            output.status
        )
        .into()),
        Err(error) => Err(format!(
            "release checks need cargo-dist's `dist` executable on PATH ({error}); install it with `{install}`"
        )
        .into()),
    }
}

/// The published crate carries sources, tests, and docs; never the evaluation
/// harnesses, artwork, or release plumbing excluded in Cargo.toml.
fn check_package_contents() -> Result<()> {
    let listing = capture(
        "cargo",
        &[
            "package",
            "--list",
            "--locked",
            "--allow-dirty",
            "--package",
            "slopdex",
        ],
    )?;
    let files: Vec<_> = listing
        .lines()
        .map(|line| line.replace('\\', "/"))
        .collect();
    let unwanted: Vec<_> = files
        .iter()
        .filter(|file| is_unwanted_package_file(file))
        .collect();
    if !unwanted.is_empty() {
        return Err(format!(
            "cargo package would ship excluded files (check `exclude` in Cargo.toml): {unwanted:?}"
        )
        .into());
    }
    for required in ["README.md", "LICENSE", "src/lib.rs"] {
        if !files.iter().any(|file| file == required) {
            return Err(format!("cargo package is missing {required}").into());
        }
    }
    Ok(())
}

/// Every top-level subcommand the native binary must advertise in --help.
const EXPECTED_COMMANDS: &[&str] = &[
    "search",
    "search-code",
    "search-descriptions",
    "search-docs",
    "search-symbols",
    "faq",
    "cross-search",
    "map",
    "lines",
    "refactor",
    "status",
    "metrics",
    "index",
    "generate",
    "update",
    "cache",
    "help",
    "config",
];

fn is_unwanted_package_file(file: &str) -> bool {
    ["evals/", ".github/", "npm/"]
        .iter()
        .any(|prefix| file.starts_with(prefix))
        || file.ends_with(".png")
}

fn command(program: &str, args: &[&str]) -> Result<()> {
    eprintln!("+ {program} {}", args.join(" "));
    let status = Command::new(program).args(args).status()?;
    if !status.success() {
        return Err(format!("{program} {} failed ({status})", args.join(" ")).into());
    }
    Ok(())
}

fn capture(program: &str, args: &[&str]) -> Result<String> {
    eprintln!("+ {program} {}", args.join(" "));
    let output = Command::new(program)
        .args(args)
        .stderr(std::process::Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err(format!("{program} {} failed ({})", args.join(" "), output.status).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

#[cfg(test)]
mod tests {
    use super::{EXPECTED_COMMANDS, is_unwanted_package_file};

    #[test]
    fn package_exclusions_cover_harnesses_artwork_and_release_plumbing() {
        for file in [
            "evals/run.py",
            ".github/workflows/rust.yml",
            "npm/package.json",
            "dexter.png",
            "docs/logo.png",
        ] {
            assert!(is_unwanted_package_file(file), "{file}");
        }
        for file in [
            "README.md",
            "LICENSE",
            "src/lib.rs",
            "tests/rust_integration.rs",
        ] {
            assert!(!is_unwanted_package_file(file), "{file}");
        }
    }

    #[test]
    fn expected_commands_are_unique_and_nonempty() {
        assert!(EXPECTED_COMMANDS.len() >= 17);
        let unique: std::collections::HashSet<_> = EXPECTED_COMMANDS.iter().collect();
        assert_eq!(unique.len(), EXPECTED_COMMANDS.len());
        assert!(EXPECTED_COMMANDS.iter().all(|name| !name.is_empty()));
    }
}
