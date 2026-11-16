use super::*;
use std::process::Command;

struct Repo {
    temp: tempfile::TempDir,
    root: PathBuf,
}

impl Repo {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let repo = Self { temp, root };
        repo.git(&["init", "--quiet"])?;
        for (key, value) in [
            ("core.autocrlf", "false"),
            ("core.eol", "lf"),
            ("user.name", "KB Fixture"),
            ("user.email", "kb@example.invalid"),
            ("commit.gpgsign", "false"),
        ] {
            repo.git(&["config", "--local", key, value])?;
        }
        // Ignore inherited hooks, including those spawned by production Git.
        fs::create_dir(repo.root.join(".git/empty-hooks"))?;
        repo.git(&[
            "config",
            "--local",
            "core.hooksPath",
            repo.root.join(".git/empty-hooks").to_str().unwrap(),
        ])?;
        repo.write("source.rs","fn example() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n}\n\nfn unrelated() {}\n")?;
        repo.save()?;
        Ok(repo)
    }
    fn git(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .current_dir(&self.root)
            .args(args)
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }
    fn write(&self, path: &str, text: &str) -> Result<()> {
        let absolute = self.root.join(path);
        fs::create_dir_all(absolute.parent().unwrap())?;
        fs::write(absolute, text)?;
        Ok(())
    }
    fn save(&self) -> Result<()> {
        self.git(&["add", "--all"])?;
        self.git(&["commit", "--quiet", "-m", "fixture"])?;
        Ok(())
    }
    fn article(&self, reference: Value) -> Result<Article> {
        let markdown = render(
            &self.root,
            "Example",
            &json!({"description":"Detailed **Markdown** explanation.","references":[reference]}),
        )?;
        self.write("docs/kb/example.md", &markdown)?;
        self.save()?;
        parse_article("docs/kb/example.md".into(), markdown)
    }
}

#[test]
fn git_freshness_tracks_only_cited_ranges_and_counts_changes() -> Result<()> {
    let repo = Repo::new()?;
    let article = repo.article(json!({"path":"source.rs","startLine":1,"endLine":6}))?;
    assert_eq!(article.result(&repo.root)?["freshness"], 1.0);
    let source = fs::read_to_string(repo.root.join("source.rs"))?;
    repo.write(
        "source.rs",
        &format!(
            "// inserted before range\n{}",
            source.replace("fn unrelated() {}", "fn other() {}")
        ),
    )?;
    let result = article.result(&repo.root)?;
    assert_eq!(result["freshness"], 1.0);
    assert_eq!(result["references"][0]["startLine"], 2);
    repo.save()?;
    repo.write("source.rs", &source.replace("let c = 3", "let c = 9"))?;
    let result = article.result(&repo.root)?;
    assert_eq!(result["freshness"], 5.0 / 6.0);
    assert_eq!(result["references"][0]["outdated"], true);
    // No index or private dependency store is involved, even after committing.
    repo.save()?;
    assert_eq!(article.result(&repo.root)?["freshness"], 5.0 / 6.0);
    fs::remove_file(repo.root.join("source.rs"))?;
    assert_eq!(article.result(&repo.root)?["freshness"], 0.0);
    Ok(())
}

#[test]
fn symbols_relocate_and_uncommitted_articles_are_untrusted() -> Result<()> {
    let repo = Repo::new()?;
    let article = repo.article(json!({"path":"source.rs","symbol":"example"}))?;
    assert_eq!(article.references[0]["startLine"], 1);
    let source = fs::read_to_string(repo.root.join("source.rs"))?;
    repo.write("source.rs", &format!("\n\n{source}"))?;
    assert_eq!(article.result(&repo.root)?["freshness"], 1.0);
    let modified = parse_article(article.path, format!("{}\nEdited\n", article.markdown))?;
    repo.write(&modified.path, &modified.markdown)?;
    assert_eq!(modified.result(&repo.root)?["freshness"], 0.0);
    let untracked = parse_article("docs/kb/new.md".into(), modified.markdown)?;
    assert_eq!(untracked.result(&repo.root)?["freshness"], 0.0);
    Ok(())
}

#[test]
fn anonymous_ranges_count_insertions_and_deletions_but_not_moves() -> Result<()> {
    let repo = Repo::new()?;
    let article = repo.article(json!({"path":"source.rs","startLine":1,"endLine":6}))?;
    let source = fs::read_to_string(repo.root.join("source.rs"))?;
    for (changed, expected) in [
        (source.replace("    let b = 2;\n", ""), 5.0 / 6.0),
        (
            source.replace("    let b = 2;\n", "    let b = 2;\n    let added = 9;\n"),
            6.0 / 7.0,
        ),
        (
            format!(
                "fn unrelated() {{}}\n\n{}\n",
                source.split("\n\n").next().unwrap()
            ),
            1.0,
        ),
    ] {
        repo.write("source.rs", &changed)?;
        assert_eq!(
            article.result(&repo.root)?["freshness"],
            expected,
            "{changed}"
        );
    }
    Ok(())
}

#[test]
fn unqualified_symbol_citations_are_saved_with_canonical_names() -> Result<()> {
    let repo = Repo::new()?;
    repo.write("source.rs", "mod nested {\n    pub fn example() {}\n}\n")?;
    repo.save()?;
    let article = repo.article(json!({"path":"source.rs","symbol":"example"}))?;
    assert_eq!(article.references[0]["symbol"], "nested.example");
    repo.write(
        "source.rs",
        "\n\nmod nested {\n    pub fn example() {}\n}\n",
    )?;
    assert_eq!(article.result(&repo.root)?["freshness"], 1.0);
    Ok(())
}

#[test]
fn git_stat_cache_flags_do_not_conceal_reference_or_article_edits() -> Result<()> {
    let repo = Repo::new()?;
    let article = repo.article(json!({"path":"source.rs","startLine":1,"endLine":6}))?;
    repo.git(&[
        "update-index",
        "--assume-unchanged",
        "source.rs",
        "docs/kb/example.md",
    ])?;
    let source = fs::read_to_string(repo.root.join("source.rs"))?;
    repo.write("source.rs", &source.replace("let c = 3", "let c = 9"))?;
    assert_eq!(article.result(&repo.root)?["freshness"], 5.0 / 6.0);
    repo.write(
        "docs/kb/example.md",
        &article.markdown.replace("Detailed", "Edited"),
    )?;
    assert_eq!(
        articles(&repo.root, &json!({}))?[0].result(&repo.root)?["freshness"],
        0.0
    );
    fs::remove_file(repo.root.join("source.rs"))?;
    assert_eq!(article.result(&repo.root)?["freshness"], 0.0);
    Ok(())
}

#[test]
fn shallow_boundaries_do_not_fabricate_article_baselines() -> Result<()> {
    let repo = Repo::new()?;
    let article = repo.article(json!({"path":"source.rs"}))?;
    let head = repo.git(&["rev-parse", "HEAD"])?;
    repo.write(".git/shallow", &format!("{head}\n"))?;
    let result = article.result(&repo.root)?;
    assert!(result["commit"].is_null());
    assert_eq!(result["freshness"], 0.0);
    fs::remove_file(repo.root.join(".git/shallow"))?;
    assert_eq!(article.result(&repo.root)?["freshness"], 1.0);
    Ok(())
}

#[test]
fn markdown_examples_cannot_inject_unverified_references() -> Result<()> {
    let repo = Repo::new()?;
    let markdown = render(
        &repo.root,
        "Example",
        &json!({
            "description":"Example:\n```md\n## References\n- [invented](/missing.rs)\n```",
            "references":[{"path":"source.rs"}]
        }),
    )?;
    let article = parse_article("docs/kb/example.md".into(), markdown)?;
    assert_eq!(article.references, vec![json!({"path":"source.rs"})]);
    Ok(())
}

#[test]
fn generation_checks_clean_tree_before_spawning_and_before_publication() -> Result<()> {
    let repo = Repo::new()?;
    for staged in [false, true] {
        repo.write(
            "source.rs",
            if staged {
                "fn staged() {}\n"
            } else {
                "fn changed() {}\n"
            },
        )?;
        if staged {
            repo.git(&["add", "source.rs"])?;
        }
        let error = generate(&repo.root, &json!({}), &json!({}), false).unwrap_err();
        assert!(error.to_string().contains("Commit all"), "{error:#}");
        repo.save()?;
    }
    repo.write("untracked.txt", "pending")?;
    assert!(
        generate(&repo.root, &json!({}), &json!({}), false)
            .unwrap_err()
            .to_string()
            .contains("Commit all")
    );
    Ok(())
}

#[test]
fn markdown_and_configuration_are_validated() -> Result<()> {
    let repo = Repo::new()?;
    for config in [
        json!({"kbPath":"../escape"}),
        json!({"kbPath":"/tmp/kb"}),
        json!({"kbPath":""}),
        json!({"kbParallelism":0}),
        json!({"kbQueryFreshness":1.1}),
        json!({"kbRefreshFreshness":null}),
        json!({"kbThreshold":2}),
    ] {
        assert!(validate(&config).is_err(), "{config}");
    }
    assert_eq!(
        fraction(&json!({}), &json!({}), "kbQueryFreshness", 0.8)?,
        0.8
    );
    for reference in [
        json!({"path":"missing.rs"}),
        json!({"path":"source.rs","symbol":"missing"}),
        json!({"path":"source.rs","startLine":0,"endLine":2}),
        json!({"path":"source.rs","startLine":2}),
        json!({"path":"../escape"}),
    ] {
        assert!(
            render(
                &repo.root,
                "Example",
                &json!({"description":"Explanation","references":[reference]})
            )
            .is_err()
        );
    }
    repo.write("a space (é).txt", "hello\n")?;
    repo.save()?;
    let markdown = render(
        &repo.root,
        "Example",
        &json!({"description":"## Detail\nExplanation","references":[{"path":"a space (é).txt"}]}),
    )?;
    let article = parse_article("docs/kb/example.md".into(), markdown)?;
    assert_eq!(article.references[0]["path"], "a space (é).txt");
    assert_eq!(repo.temp.path().canonicalize()?, repo.root);
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlinked_knowledge_base_paths_are_rejected() -> Result<()> {
    let repo = Repo::new()?;
    let outside = tempfile::tempdir()?;
    std::os::unix::fs::symlink(outside.path(), repo.root.join("docs"))?;
    assert!(articles(&repo.root, &json!({})).is_err());
    Ok(())
}
