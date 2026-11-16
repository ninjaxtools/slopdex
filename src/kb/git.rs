use super::*;
use std::process::{Command, Output};

fn command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args(["--literal-pathspecs", "-c", "core.quotePath=false"]);
    command
}

fn checked(output: Output) -> Result<Vec<u8>> {
    ensure!(
        output.status.success(),
        "Knowledge base Git operation failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

pub(super) fn head(root: &Path) -> Result<String> {
    let top = checked(
        command(root)
            .args(["rev-parse", "--show-toplevel"])
            .output()?,
    )?;
    ensure!(
        Path::new(std::str::from_utf8(&top)?.trim()).canonicalize()? == root.canonicalize()?,
        "Knowledge base root must be the Git repository root"
    );
    Ok(String::from_utf8(checked(
        command(root)
            .args(["rev-parse", "--verify", "HEAD"])
            .output()?,
    )?)?
    .trim()
    .to_owned())
}

pub(super) fn lock(root: &Path) -> Result<fs::File> {
    use fs2::FileExt;
    let path = checked(
        command(root)
            .args(["rev-parse", "--git-path", "slopdex-kb.lock"])
            .output()?,
    )?;
    let path = root.join(std::str::from_utf8(&path)?.trim());
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.try_lock_exclusive()
        .context("Another knowledge base generation is running")?;
    Ok(file)
}

pub(super) fn unchanged(root: &Path, commit: &str, path: &str) -> Result<bool> {
    let output = command(root)
        .args([
            "diff",
            "--quiet",
            "--no-ext-diff",
            "--no-textconv",
            commit,
            "--",
            path,
        ])
        .output()?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => {
            checked(output)?;
            Ok(false)
        }
    }
}

pub(super) fn clean_head(root: &Path) -> Result<String> {
    let head = head(root)?;
    let status = checked(
        command(root)
            .args([
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ])
            .output()?,
    )?;
    ensure!(
        status.is_empty(),
        "Knowledge base generation requires a clean repository. Commit all staged, unstaged and untracked files, then rerun."
    );
    Ok(head)
}

pub(super) fn source(root: &Path, commit: &str, path: &str) -> Result<Option<String>> {
    let output = command(root)
        .args(["cat-file", "--filters", &format!("{commit}:{path}")])
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8(output.stdout).ok())
}

pub(super) fn baseline(root: &Path, path: &str) -> Result<Option<String>> {
    let output = checked(
        command(root)
            .args(["log", "-1", "--format=%H", "HEAD", "--", path])
            .output()?,
    )?;
    let commit = String::from_utf8(output)?.trim().to_owned();
    if !commit.is_empty() {
        let shallow = checked(
            command(root)
                .args(["rev-parse", "--git-path", "shallow"])
                .output()?,
        )?;
        let shallow = root.join(std::str::from_utf8(&shallow)?.trim());
        match fs::read_to_string(shallow) {
            Ok(boundaries) if boundaries.lines().any(|line| line == commit) => return Ok(None),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok((!commit.is_empty()).then_some(commit))
}

pub(super) fn commit(
    root: &Path,
    baseline: &str,
    paths: &[String],
    message: &str,
) -> Result<String> {
    ensure!(
        head(root)? == baseline,
        "Repository HEAD changed; generated articles were left uncommitted"
    );
    // A temporary index prevents unrelated user staging from entering the commit.
    let temp = tempfile::tempdir()?;
    let index = temp.path().join("index");
    let run = |args: &[&str]| -> Result<Vec<u8>> {
        checked(
            command(root)
                .env("GIT_INDEX_FILE", &index)
                .args(args)
                .output()?,
        )
    };
    run(&["read-tree", baseline])?;
    let mut args = vec!["add", "--"];
    args.extend(paths.iter().map(String::as_str));
    run(&args)?;
    let changes = checked(
        command(root)
            .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
            .output()?,
    )?;
    for entry in changes.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
        ensure!(
            entry.len() > 3 && paths.iter().any(|p| p.as_bytes() == &entry[3..]),
            "Repository changed during KB publication; articles were left uncommitted"
        );
    }
    ensure!(
        head(root)? == baseline,
        "Repository HEAD changed during KB publication"
    );
    let tree = String::from_utf8(run(&["write-tree"])?)?.trim().to_owned();
    // Plumbing avoids hooks changing the staged tree. The compare-and-swap
    // prevents a simultaneous source commit from becoming the article baseline.
    let commit = String::from_utf8(run(&["commit-tree", &tree, "-p", baseline, "-m", message])?)?
        .trim()
        .to_owned();
    run(&["update-ref", "-m", message, "HEAD", &commit, baseline])?;
    // Update only article entries in the real index; unrelated staging survives.
    let mut args = vec!["reset", "--quiet", &commit, "--"];
    args.extend(paths.iter().map(String::as_str));
    checked(command(root).args(args).output()?)?;
    Ok(commit)
}
