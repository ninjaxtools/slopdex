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
    for name in ["search", "cross-search", "describe"] {
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
