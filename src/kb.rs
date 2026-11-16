//! Repository-owned Markdown knowledge base. Git is the only freshness store.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
    thread,
};

mod freshness;
mod git;

const ARTICLE_SYSTEM: &str = "Explain the requested existing codebase topic in detail. Inspect the repository using read-only tools. Explain architecture, behavior, important edge cases and relationships; do not propose an implementation. Return only a JSON object: {\"description\":\"<detailed Markdown explanation>\",\"references\":[{\"path\":\"<repository-relative path>\",\"symbol\":\"<exact qualified symbol name>\"}]}. Omit symbol for whole files, or use startLine/endLine for a specific range. Cite all sources relied on. Do not invent references. Do not modify files or commit; slopdex writes the Markdown and commits it.";

#[derive(Clone, Debug)]
pub(crate) struct Article {
    pub path: String,
    pub title: String,
    pub markdown: String,
    pub references: Vec<Value>,
}

impl Article {
    pub fn result(&self, root: &Path) -> Result<Value> {
        let (commit, freshness, references) = freshness::check(root, self)?;
        Ok(
            json!({"path":self.path,"title":self.title,"description":self.markdown,
            "commit":commit,"freshness":freshness,"references":references}),
        )
    }
}

pub(crate) fn directory(root: &Path, config: &Value) -> Result<PathBuf> {
    let path = config
        .get("kbPath")
        .map_or(Some("docs/kb"), Value::as_str)
        .context("kbPath must be a nonempty repository-relative directory")?;
    let path = Path::new(path);
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|p| matches!(p, Component::Normal(_))),
        "kbPath must be a nonempty repository-relative directory without '.' or '..'"
    );
    safe_path(root, path)?;
    Ok(path.to_owned())
}

fn safe_path(root: &Path, path: &Path) -> Result<()> {
    ensure!(
        path.components().all(|p| matches!(p, Component::Normal(_))),
        "Invalid knowledge base path"
    );
    let mut absolute = root.to_owned();
    for component in path.components() {
        absolute.push(component);
        match fs::symlink_metadata(&absolute) {
            Ok(meta) => ensure!(
                !meta.file_type().is_symlink(),
                "Knowledge base paths must not contain symlinks: {}",
                absolute.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) fn articles(root: &Path, config: &Value) -> Result<Vec<Article>> {
    let directory = directory(root, config)?;
    let mut pending = vec![root.join(directory)];
    let mut articles = Vec::new();
    while let Some(path) = pending.pop() {
        if !path.exists() {
            continue;
        }
        safe_path(root, path.strip_prefix(root)?)?;
        if path.is_dir() {
            for entry in fs::read_dir(path)? {
                pending.push(entry?.path());
            }
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        {
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .context("KB paths must be UTF-8")?
                .replace('\\', "/");
            articles.push(parse_article(relative, fs::read_to_string(path)?)?);
        }
    }
    articles.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(articles)
}

/// References are ordinary Markdown links with repository-root-relative targets.
/// An optional symbol is only presentation; the line anchor is authoritative.
fn parse_article(path: String, markdown: String) -> Result<Article> {
    let title = markdown
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .context("Knowledge base article needs a level-one heading")?
        .trim()
        .to_owned();
    let mut references = Vec::new();
    let parsed = crate::parse::parse(&path, &markdown)?;
    let lines: Vec<_> = markdown.lines().collect();
    // Use the final real heading, not a `## References` example inside a
    // fenced code block or quoted Markdown in the explanation.
    let start = parsed
        .structure
        .nodes
        .iter()
        .filter(|node| {
            node.kind == crate::parse::NodeKind::Heading
                && lines.get(node.start_line.saturating_sub(1)) == Some(&"## References")
        })
        .map(|node| node.start_line)
        .max()
        .unwrap_or(lines.len());
    for line in lines
        .into_iter()
        .skip(start)
        .take_while(|line| !line.starts_with("## "))
    {
        let Some((label, rest)) = line
            .strip_prefix("- [")
            .and_then(|line| line.split_once("]("))
        else {
            continue;
        };
        let Some(target) = rest.strip_suffix(')') else {
            continue;
        };
        let Some(target) = target.strip_prefix('/') else {
            continue;
        };
        let (path, anchor) = target
            .split_once('#')
            .map_or((target, None), |(p, a)| (p, Some(a)));
        let path = decode_path(path)?;
        ensure!(
            Path::new(&path)
                .components()
                .all(|p| matches!(p, Component::Normal(_)))
                && !path.is_empty(),
            "Invalid KB reference path"
        );
        let mut reference = json!({"path":path});
        if let Some(anchor) = anchor {
            let (start, end) = anchor
                .strip_prefix('L')
                .and_then(|a| a.split_once("-L"))
                .context("KB reference anchors must use #Lstart-Lend")?;
            let start: u64 = start.parse()?;
            let end: u64 = end.parse()?;
            ensure!(start > 0 && end >= start, "Invalid KB reference range");
            reference["startLine"] = json!(start);
            reference["endLine"] = json!(end);
        }
        if let Some((_, symbol)) = label.split_once(" :: ") {
            reference["symbol"] = json!(symbol);
        }
        if !references.contains(&reference) {
            references.push(reference);
        }
    }
    Ok(Article {
        path,
        title,
        markdown,
        references,
    })
}

fn encode_path(path: &str) -> String {
    path.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"/-_.".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn decode_path(path: &str) -> Result<String> {
    let mut bytes = Vec::new();
    let mut input = path.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let pair = [
                input.next().context("Invalid escaped reference")?,
                input.next().context("Invalid escaped reference")?,
            ];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&pair)?, 16)?);
        } else {
            bytes.push(byte);
        }
    }
    Ok(String::from_utf8(bytes)?)
}

pub(crate) fn list(root: &Path, config: &Value) -> Result<Vec<Value>> {
    git::head(root)?;
    articles(root, config)?
        .iter()
        .map(|article| article.result(root))
        .collect()
}

pub(crate) fn fraction(config: &Value, options: &Value, key: &str, default: f64) -> Result<f64> {
    options
        .get("freshness")
        .or_else(|| config.get(key))
        .map_or(Ok(default), |value| {
            value
                .as_f64()
                .filter(|n| n.is_finite() && (0.0..=1.0).contains(n))
                .with_context(|| format!("{key}/freshness must be a finite number between 0 and 1"))
        })
}

pub(crate) fn validate(config: &Value) -> Result<()> {
    for (key, default) in [("kbQueryFreshness", 0.8), ("kbRefreshFreshness", 0.9)] {
        fraction(config, &json!({}), key, default)?;
    }
    if let Some(value) = config.get("kbThreshold") {
        ensure!(
            value
                .as_f64()
                .is_some_and(|n| n.is_finite() && (-1.0..=1.0).contains(&n)),
            "kbThreshold must be a finite number between -1 and 1"
        );
    }
    if let Some(value) = config.get("kbPath") {
        let path = value
            .as_str()
            .context("kbPath must be a nonempty repository-relative directory")?;
        ensure!(
            !path.is_empty()
                && Path::new(path)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
            "kbPath must be a nonempty repository-relative directory without '.' or '..'"
        );
    }
    if let Some(value) = config.get("kbParallelism") {
        ensure!(
            value.as_u64().is_some_and(|n| (1..=256).contains(&n)),
            "kbParallelism must be an integer between 1 and 256"
        );
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Topic {
    title: String,
    description: String,
}

fn json_response(raw: &str) -> Result<Value> {
    let raw = raw.trim();
    let raw = raw
        .strip_prefix("```json")
        .or_else(|| raw.strip_prefix("```"))
        .and_then(|s| s.trim().strip_suffix("```"))
        .unwrap_or(raw)
        .trim();
    serde_json::from_str(raw).context("Agent must return the requested JSON object")
}

/// Generate entirely in memory, then recheck the clean baseline before writing.
pub(crate) fn generate(
    root: &Path,
    config: &Value,
    options: &Value,
    refresh: bool,
) -> Result<Value> {
    validate(config)?;
    let directory = directory(root, config)?;
    let _lock = git::lock(root)?;
    let baseline = git::clean_head(root)?;
    let parallelism = options
        .get("parallelism")
        .or_else(|| config.get("kbParallelism"))
        .map_or(Some(4), Value::as_u64)
        .filter(|n| (1..=256).contains(n))
        .context("KB parallelism must be an integer between 1 and 256")?
        as usize;
    let cutoff = fraction(config, options, "kbRefreshFreshness", 0.9)?;
    let existing = articles(root, config)?;
    let mut tasks = Vec::new();
    let providers = crate::providers::Providers::new(config)?;
    if refresh {
        for article in existing {
            let result = article.result(root)?;
            if result["freshness"].as_f64().unwrap() < cutoff {
                tasks.push((article.path, article.title, format!("Review this article and its references against the current repository. Update the explanation only where needed; otherwise retain it. Return the full detailed explanation and current verified references, even if the prose is still accurate.\n\n{}\n\nReference freshness:\n{}", article.markdown, result["references"])));
            }
        }
    } else {
        ensure!(
            existing.is_empty(),
            "Knowledge base already contains articles; use `kb refresh`"
        );
        let review = providers.llm().describe_in(root,
            "Perform a full read-only review of this repository. Identify all meaningful knowledge-base topics covering its architecture, features, workflows and cross-cutting behavior. Return only JSON: {\"topics\":[{\"title\":\"<unique topic title>\",\"description\":\"<scope and what to explain>\"}]}. Do not write files or commit.",
            &format!("Review the entire repository at {} and enumerate all topics for its knowledge base. Inspect actual source and documentation; do not rely just on filenames.", root.display()))?;
        let value = json_response(&review)?;
        let topics: Vec<Topic> = serde_json::from_value(value["topics"].clone())
            .context("Invalid knowledge base topic review")?;
        ensure!(!topics.is_empty(), "Agent review returned no topics");
        let mut paths = BTreeSet::new();
        for topic in topics {
            ensure!(
                !topic.title.trim().is_empty()
                    && !topic.title.contains(['\n', '\r'])
                    && !topic.description.trim().is_empty(),
                "Invalid knowledge base topic"
            );
            let slug = slug(&topic.title);
            let path = directory
                .join(format!("{slug}.md"))
                .to_str()
                .context("KB path must be UTF-8")?
                .replace('\\', "/");
            ensure!(
                paths.insert(path.clone()) && !root.join(&path).exists(),
                "Duplicate or existing KB article: {path}"
            );
            tasks.push((path, topic.title, topic.description));
        }
    }
    let mut generated = Vec::new();
    // Join every worker before errors propagate. Each call starts a new agent.
    for batch in tasks.chunks(parallelism) {
        let results = thread::scope(|scope| {
            let workers: Vec<_> = batch
                .iter()
                .map(|(path, title, task)| {
                    let llm = providers.llm();
                    scope.spawn(move || -> Result<(String, String)> {
                        let raw = llm.describe_in(
                            root,
                            ARTICLE_SYSTEM,
                            &format!("Topic: {title}\n\n{task}\n\nRepository: {}", root.display()),
                        )?;
                        let markdown = render(root, title, &json_response(&raw)?)?;
                        ensure!(
                            markdown.len() as u64
                                <= config
                                    .get("maxFileSize")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(crate::config::DEFAULT_MAX_FILE_SIZE),
                            "KB article exceeds maxFileSize and would not be indexed: {path}"
                        );
                        Ok((path.clone(), markdown))
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| {
                    worker
                        .join()
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("KB agent worker panicked")))
                })
                .collect::<Vec<_>>()
        });
        generated.extend(results.into_iter().collect::<Result<Vec<_>>>()?);
    }
    ensure!(
        git::clean_head(root)? == baseline,
        "Repository HEAD changed during KB generation; rerun against a clean commit"
    );
    if generated.is_empty() {
        return Ok(json!({"articles":[],"commit":null}));
    }
    let paths: Vec<_> = generated.iter().map(|(path, _)| path.clone()).collect();
    for (path, markdown) in &generated {
        safe_path(root, Path::new(path))?;
        let absolute = root.join(path);
        fs::create_dir_all(absolute.parent().unwrap())?;
        use std::io::Write;
        let mut staged = tempfile::NamedTempFile::new_in(absolute.parent().unwrap())?;
        staged.write_all(markdown.as_bytes())?;
        if refresh {
            staged.persist(&absolute)?;
        } else {
            // Never overwrite a file created after the clean-tree check, even
            // on a case-insensitive filesystem with colliding topic slugs.
            staged.persist_noclobber(&absolute)?;
        }
    }
    let commit = git::commit(
        root,
        &baseline,
        &paths,
        if refresh {
            "docs: refresh knowledge base"
        } else {
            "docs: initialize knowledge base"
        },
    )?;
    Ok(json!({"articles":paths,"commit":commit}))
}

fn slug(title: &str) -> String {
    let slug = title
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        crate::hash(title)[..12].to_owned()
    } else {
        slug
    }
}

fn render(root: &Path, title: &str, response: &Value) -> Result<String> {
    let description = response["description"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .context("Agent article is missing its explanation")?;
    let citations = response["references"]
        .as_array()
        .filter(|r| !r.is_empty())
        .context("Agent article must cite at least one reference")?;
    // Human-readable provenance also ensures a review with unchanged prose is
    // an article change in Git, so its next baseline is the review commit.
    let revision = git::head(root)?;
    let mut markdown = format!(
        "# {title}\n\n{}\n\n<!-- Reviewed against Git commit {revision}. -->\n\n## References\n",
        description.trim()
    );
    let mut references = BTreeSet::new();
    for citation in citations {
        let path = citation["path"]
            .as_str()
            .context("Reference needs a path")?;
        safe_path(root, Path::new(path))?;
        let source =
            git::source(root, "HEAD", path)?.context("Reference must be a committed UTF-8 file")?;
        let disk = fs::read_to_string(root.join(path))?;
        ensure!(
            freshness::same_text(&source, &disk) && git::unchanged(root, "HEAD", path)?,
            "Reference {path} has uncommitted changes; commit all files before generating knowledge-base articles"
        );
        let mut reference = json!({"path":path});
        let mut symbol = citation["symbol"].as_str().map(str::to_owned);
        if let Some(symbol) = &mut symbol {
            let parsed = crate::parse::parse(path, &source)?;
            let found = parsed
                .structure
                .nodes
                .iter()
                .filter(|n| n.qualified_name == *symbol || n.name == *symbol)
                .collect::<Vec<_>>();
            ensure!(
                found.len() == 1,
                "Missing or ambiguous reference symbol {path}::{symbol}"
            );
            reference["startLine"] = json!(found[0].start_line);
            reference["endLine"] = json!(found[0].end_line);
            *symbol = found[0].qualified_name.clone();
        } else {
            match (citation.get("startLine"), citation.get("endLine")) {
                (None, None) => {}
                (Some(start), Some(end)) => {
                    reference["startLine"] = start.clone();
                    reference["endLine"] = end.clone();
                }
                _ => anyhow::bail!("Reference requires both startLine and endLine"),
            }
        }
        ensure!(
            freshness::reference_source(&source, &reference).is_some(),
            "Invalid reference line range for {path}"
        );
        let label = symbol.map_or(path.to_owned(), |symbol| format!("{path} :: {symbol}"));
        ensure!(
            !label.contains(['[', ']', '\n', '\r']),
            "Reference labels contain unsupported Markdown characters"
        );
        let anchor = match (
            reference["startLine"].as_u64(),
            reference["endLine"].as_u64(),
        ) {
            (Some(start), Some(end)) => format!("#L{start}-L{end}"),
            _ => String::new(),
        };
        let line = format!("- [{label}](/{}{anchor})\n", encode_path(path));
        if references.insert(line.clone()) {
            markdown.push_str(&line);
        }
    }
    Ok(markdown)
}

#[cfg(test)]
mod tests;
