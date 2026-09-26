use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde_json::{Value, json};

use crate::{
    hash, parse,
    providers::Providers,
    storage::{Database, File, Item},
    vectors::VectorIndex,
};

const EXCLUDED: &[&str] = &[
    ".git",
    ".slopdex",
    "node_modules",
    "dist",
    "build",
    "coverage",
    "vendor",
    "generated",
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    "target",
];

pub struct Engine {
    root: PathBuf,
    db: Database,
    config: Value,
    providers: Providers,
    enabled: bool,
    items: Vec<Item>,
    files: HashMap<String, File>,
    vectors: HashMap<String, Vec<f32>>,
    indexes: HashMap<String, VectorIndex>,
    complete_descriptions: bool,
    // Held for the engine's lifetime: SQLite transactions and multi-file USearch
    // publication must not interleave with another command using this index.
    _lock: fs::File,
}

impl Engine {
    pub fn open(root: &Path, index: &Path, mut config: Value) -> Result<Self> {
        let root = root
            .canonicalize()
            .context("Repository root does not exist")?;
        ensure!(root.is_dir(), "Repository root is not a directory");
        if let Some(parent) = index.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let index = if index.exists() {
            index.canonicalize()?
        } else {
            index
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .canonicalize()?
                .join(index.file_name().context("Index must name a file")?)
        };
        let mut lock_path = index.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(PathBuf::from(lock_path))?;
        lock.try_lock_exclusive()
            .context("Index is in use by another slopdex command; retry when it completes")?;
        let providers = Providers::new(&config)?;
        let db = Database::open(
            &index,
            &root,
            &providers.vector().profile(),
            flag(&config, "forceReindex"),
        )?;
        // Persisted description settings also apply when an index is moved to a
        // caller without a config file. Explicit settings still take precedence.
        if config.get("descriptionProvider").is_none()
            && config.get("descriptionModel").is_none()
            && let Some(profile) = db.meta("description_profile")?
        {
            let profile: Value = serde_json::from_str(&profile)?;
            config["descriptionProvider"] = profile["provider"].clone();
            config["descriptionModel"] = profile["model"].clone();
        }
        let providers = Providers::new(&config)?;
        let enabled = config["descriptionsEnabled"]
            .as_bool()
            .unwrap_or(db.meta("descriptions_enabled")?.as_deref() == Some("true"));
        let mut engine = Self {
            root,
            db,
            config,
            providers,
            enabled,
            items: Vec::new(),
            files: HashMap::new(),
            vectors: HashMap::new(),
            indexes: HashMap::new(),
            complete_descriptions: false,
            _lock: lock,
        };
        engine.load()?;
        Ok(engine)
    }

    pub fn refresh(&mut self) -> Result<Value> {
        if flag(&self.config, "noReindex") {
            return Ok(json!({"skipped":true,"generation":self.db.generation()?}));
        }
        let checkpoint = self.git_text(&["rev-parse", "--verify", "HEAD"]);
        let dirty = self.dirty_paths()?;
        let include = globs(&self.config["include"])?;
        let exclude = globs(&self.config["exclude"])?;
        let max_size = self.config["maxFileSize"].as_u64().unwrap_or(1_048_576);
        ensure!(max_size > 0, "maxFileSize must be positive");
        let mut walker = ignore::WalkBuilder::new(&self.root);
        walker
            .hidden(false)
            .require_git(false)
            .git_ignore(true)
            .git_exclude(true)
            .git_global(true)
            .filter_entry(|entry| {
                !entry.file_type().is_some_and(|t| t.is_dir())
                    || !EXCLUDED.contains(&entry.file_name().to_string_lossy().as_ref())
            });
        let mut paths = Vec::new();
        for entry in walker.build() {
            let entry = entry.context("Cannot walk repository")?;
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let relative = relative(&self.root, entry.path())?;
            if parse::language_for_path(&relative).is_none()
                || exclude.is_match(&relative)
                || (!include.is_empty() && !include.is_match(&relative))
            {
                continue;
            }
            paths.push(relative);
        }
        paths.sort();
        let paths_set: HashSet<_> = paths.iter().cloned().collect();
        let removed: Vec<_> = self
            .files
            .keys()
            .filter(|p| !paths_set.contains(*p))
            .cloned()
            .collect();
        let mut changed = Vec::new();
        let missing_descriptions: HashSet<_> = self
            .items
            .iter()
            .filter(|i| i.kind == "function" && i.description_embedding.is_none())
            .map(|i| i.path.as_str())
            .collect();
        for path in paths {
            let source_mode = if checkpoint.is_none() || dirty.contains(&path) {
                "working-tree"
            } else {
                "git"
            };
            let source_path = self.root.join(&path);
            let read = (|| -> Result<String> {
                ensure!(
                    fs::metadata(&source_path)?.len() <= max_size,
                    "File exceeds maxFileSize ({max_size} bytes)"
                );
                fs::read_to_string(&source_path).context("Cannot read UTF-8 source")
            })();
            let source = match read {
                Ok(source) => source,
                Err(error) => {
                    let file = File {
                        path: path.clone(),
                        hash: String::new(),
                        source: String::new(),
                        language: parse::language_for_path(&path).unwrap().into(),
                        source_mode: source_mode.into(),
                        description: None,
                        description_hash: None,
                        description_embedding: None,
                        errors: vec![
                            json!({"path":path,"code":"read-error","message":error.to_string(),"sourceMode":source_mode}),
                        ],
                    };
                    changed.push((file, Vec::new()));
                    continue;
                }
            };
            let source_hash = hash(&source);
            let previous = self.files.get(&path);
            let needs_descriptions = self.enabled
                && parse::language_for_path(&path) != Some("markdown")
                && (previous.is_none_or(|f| f.description.is_none())
                    || missing_descriptions.contains(path.as_str()));
            if previous.is_some_and(|f| {
                f.hash == source_hash && f.source_mode == source_mode && f.errors.is_empty()
            }) && !needs_descriptions
            {
                continue;
            }
            let prepared = self.prepare(&path, &source, source_mode, false, false)?;
            changed.push(prepared);
        }
        // Never publish a mixture of snapshots if files changed while remote
        // providers were running. Their completed artifacts remain reusable.
        ensure!(
            self.git_text(&["rev-parse", "--verify", "HEAD"]) == checkpoint,
            "Git HEAD changed during indexing; rerun"
        );
        for (file, _) in &changed {
            if !file.hash.is_empty() {
                ensure!(
                    hash(fs::read(self.root.join(&file.path))?) == file.hash,
                    "Source changed during indexing: {}; rerun",
                    file.path
                );
            }
        }
        let dirty = self.db.apply(&changed, &removed, checkpoint.as_deref())?;
        self.db.set_meta(
            "descriptions_enabled",
            if self.enabled { "true" } else { "false" },
        )?;
        if self.enabled {
            self.db.set_meta(
                "description_profile",
                &self.providers.llm().profile().to_string(),
            )?;
        }
        if dirty || self.enabled != self.complete_descriptions {
            self.load()?;
        }
        Ok(
            json!({"filesUpdated":changed.len(),"filesDeleted":removed.len(),"functionCount":self.items.iter().filter(|i|i.kind=="function").count(),"checkpoint":checkpoint,"generation":self.db.generation()?}),
        )
    }

    fn prepare(
        &self,
        path: &str,
        source: &str,
        source_mode: &str,
        regenerate_file: bool,
        regenerate_callables: bool,
    ) -> Result<(File, Vec<Item>)> {
        let source_hash = hash(source);
        let parse_key = hash(json!(["rust-parser-v1", path, source_hash]).to_string());
        let parsed: parse::ParsedFile = if let Some(cached) = self.db.cache("parse", &parse_key)? {
            serde_json::from_str(&cached)?
        } else {
            let parsed = parse::parse(path, source)?;
            self.db
                .cache_put("parse", &parse_key, &serde_json::to_string(&parsed)?)?;
            parsed
        };
        let previous = self.files.get(path);
        let mut file=File{path:path.into(),hash:source_hash.clone(),source:source.into(),language:parse::language_for_path(path).unwrap_or("unknown").into(),source_mode:source_mode.into(),description:previous.and_then(|f|f.description.clone()),description_hash:previous.and_then(|f|f.description_hash.clone()),description_embedding:previous.and_then(|f|f.description_embedding.clone()),errors:parsed.errors.iter().map(|e|json!({"path":path,"code":"parse-error","message":e.message,"startLine":e.start_line,"endLine":e.end_line,"sourceMode":source_mode})).collect()};
        if self.enabled
            && (file.description.is_none() || regenerate_file)
            && file.language != "markdown"
        {
            let key = hash(
                json!([self.providers.llm().profile(), "file", path, source_hash]).to_string(),
            );
            let description=self.description(&key,&format!("Describe the purpose, responsibilities, and important relationships of this existing source file. Do not propose changes.\nFile: {path}\n\n{source}"))?;
            file.description_embedding = Some(self.embed_one(&description, false)?);
            file.description = Some(description);
            file.description_hash = Some(source_hash.clone());
        }
        let mut items = Vec::new();
        let mut description_jobs = BTreeMap::new();
        let mut description_assignments = Vec::new();
        let mut occurrences = HashMap::<String, usize>::new();
        let inputs: Vec<_> = parsed
            .callables
            .iter()
            .map(|c| c.embedding_input.clone())
            .chain(parsed.chunks.iter().map(|c| c.embedding_input.clone()))
            .collect();
        self.ensure_embeddings(&inputs, false)?;
        for callable in parsed.callables {
            let occurrence = occurrences
                .entry(callable.qualified_name.clone())
                .or_default();
            let identity = hash(
                json!([path, callable.qualified_name, callable.kind, *occurrence]).to_string(),
            );
            *occurrence += 1;
            let embedding = Database::embedding_key(
                &self.providers.vector().profile(),
                false,
                &callable.embedding_input,
            );
            let mut data = serde_json::to_value(&callable)?;
            data["path"] = json!(path);
            data["sourceMode"] = json!(source_mode);
            let old = self.items.iter().find(|i| i.identity == identity);
            let mut description_embedding = old.and_then(|i| i.description_embedding.clone());
            data["description"] = old
                .map(|i| i.data["description"].clone())
                .unwrap_or(Value::Null);
            let unchanged = old.is_some_and(|i| {
                i.data["sourceHash"] == data["sourceHash"] && i.description_embedding.is_some()
            });
            if !unchanged {
                description_embedding = None;
                data["description"] = Value::Null;
            }
            if self.enabled
                && (regenerate_callables || (!unchanged && description_embedding.is_none()))
            {
                let key = hash(
                    json!([
                        self.providers.llm().profile(),
                        "function",
                        path,
                        callable.qualified_name,
                        callable.source_hash,
                        if regenerate_callables {
                            Some(source_hash.as_str())
                        } else {
                            None
                        }
                    ])
                    .to_string(),
                );
                let prompt = format!(
                    "Describe what this existing callable does, its inputs, outputs and side effects. Be concise; do not propose changes.\nFile: {path}\nFile context: {}\nSymbol: {}\n\n{}",
                    file.description.as_deref().unwrap_or(""),
                    callable.qualified_name,
                    callable.source
                );
                if self.db.cache("description", &key)?.is_none() {
                    description_jobs.insert(key.clone(), prompt);
                }
                description_assignments.push((items.len(), key));
            } else if !self.enabled
                && old.is_some_and(|i| i.data["sourceHash"] != data["sourceHash"])
            {
                description_embedding = None;
                data["description"] = Value::Null;
            }
            items.push(Item {
                id: 0,
                path: path.into(),
                identity,
                kind: "function".into(),
                data,
                embedding,
                description_embedding,
            });
        }
        let jobs: Vec<_> = description_jobs.into_iter().collect();
        let llm = self.providers.llm();
        run_jobs(
            &jobs,
            self.parallelism()?,
            |(_, prompt)| {
                llm.describe("You explain existing source code accurately and concisely. Return plain text, without a preamble.",prompt)
            },
            |(key, _), description| self.db.cache_put("description", key, &description),
        )?;
        let descriptions: Vec<_> = description_assignments
            .iter()
            .map(|(_, key)| {
                self.db
                    .cache("description", key)?
                    .context("Missing generated description")
            })
            .collect::<Result<_>>()?;
        self.ensure_embeddings(&descriptions, false)?;
        for ((index, _), description) in description_assignments.into_iter().zip(descriptions) {
            items[index].description_embedding = Some(Database::embedding_key(
                &self.providers.vector().profile(),
                false,
                &description,
            ));
            items[index].data["description"] = json!(description);
        }
        for (ordinal, chunk) in parsed.chunks.into_iter().enumerate() {
            let embedding = Database::embedding_key(
                &self.providers.vector().profile(),
                false,
                &chunk.embedding_input,
            );
            let identity = hash(json!([path, "markdown", ordinal]).to_string());
            let mut data = serde_json::to_value(chunk)?;
            data["path"] = json!(path);
            data["sourceMode"] = json!(source_mode);
            items.push(Item {
                id: 0,
                path: path.into(),
                identity,
                kind: "markdown".into(),
                data,
                embedding,
                description_embedding: None,
            });
        }
        Ok((file, items))
    }

    fn description(&self, key: &str, prompt: &str) -> Result<String> {
        if let Some(cached) = self.db.cache("description", key)? {
            return Ok(cached);
        }
        let description=self.providers.llm().describe("You explain existing source code accurately and concisely. Return plain text, without a preamble.",prompt)?;
        self.db.cache_put("description", key, &description)?;
        Ok(description)
    }

    fn ensure_embeddings(&self, inputs: &[String], query: bool) -> Result<()> {
        let vector = self.providers.vector();
        let profile = vector.profile();
        let mut pending = BTreeMap::new();
        for input in inputs {
            let key = Database::embedding_key(&profile, query, input);
            if self.db.embedding(&key)?.is_none() {
                pending.insert(key, input.clone());
            }
        }
        let pending: Vec<_> = pending.into_iter().collect();
        let batch = vector.batch_limit();
        let batches: Vec<_> = pending.chunks(batch).collect();
        run_jobs(
            &batches,
            self.parallelism()?,
            |entries| {
                let texts: Vec<_> = entries.iter().map(|(_, text)| text.clone()).collect();
                vector.embed(&texts, query)
            },
            |entries, vectors| {
                ensure!(
                    vectors.len() == entries.len(),
                    "Provider returned the wrong number of embeddings"
                );
                for ((key, _), vector) in entries.iter().zip(vectors) {
                    self.db.put_embedding(key, &vector)?;
                }
                Ok(())
            },
        )
    }

    fn parallelism(&self) -> Result<usize> {
        let parallelism = self.config["parallelism"].as_u64().unwrap_or(10) as usize;
        ensure!(parallelism > 0, "parallelism must be positive");
        Ok(parallelism)
    }

    fn embed_one(&self, input: &str, query: bool) -> Result<String> {
        self.ensure_embeddings(&[input.into()], query)?;
        Ok(Database::embedding_key(
            &self.providers.vector().profile(),
            query,
            input,
        ))
    }

    fn load(&mut self) -> Result<()> {
        self.items = self.db.items()?;
        self.files = self
            .db
            .files()?
            .into_iter()
            .map(|f| (f.path.clone(), f))
            .collect();
        self.vectors.clear();
        let keys: HashSet<_> = self
            .items
            .iter()
            .flat_map(|i| {
                std::iter::once(i.embedding.clone()).chain(i.description_embedding.clone())
            })
            .chain(
                self.files
                    .values()
                    .filter_map(|f| f.description_embedding.clone()),
            )
            .collect();
        for key in keys {
            self.vectors.insert(
                key.clone(),
                self.db
                    .embedding(&key)?
                    .context("Missing indexed embedding")?,
            );
        }
        self.complete_descriptions = self.enabled
            && self.items.iter().filter(|i| i.kind == "function").all(|i| {
                i.description_embedding.is_some()
                    && self
                        .files
                        .get(&i.path)
                        .is_some_and(|f| f.description_embedding.is_some())
            });
        for item in &mut self.items {
            item.data["id"] = json!(item.id);
        }
        self.indexes.clear();
        let dimensions = self.providers.vector().dimensions();
        for (kind, parts) in [
            ("code", 1),
            ("markdown", 1),
            ("descriptions", 2),
            ("combined", 3),
        ] {
            if parts > 1 && !self.complete_descriptions {
                continue;
            }
            let vectors: Vec<_> = self
                .items
                .iter()
                .filter(|i| {
                    if kind == "markdown" {
                        i.kind == "markdown"
                    } else {
                        i.kind == "function"
                    }
                })
                .map(|i| Ok((i.id, self.item_vector(i, kind)?)))
                .collect::<Result<_>>()?;
            let mut path = self.db.path.as_os_str().to_os_string();
            path.push(format!(".{kind}.usearch"));
            let index = VectorIndex::open(
                &PathBuf::from(path),
                dimensions * parts,
                self.db.generation()?,
                &vectors,
            )?;
            self.indexes.insert(kind.into(), index);
        }
        Ok(())
    }

    fn item_vector(&self, item: &Item, kind: &str) -> Result<Vec<f32>> {
        let mut parts = Vec::new();
        if kind != "descriptions" {
            parts.push(
                self.vectors
                    .get(&item.embedding)
                    .context("Missing code vector")?
                    .as_slice(),
            );
        }
        if kind == "descriptions" || kind == "combined" {
            parts.push(
                self.vectors
                    .get(
                        item.description_embedding
                            .as_ref()
                            .context("Missing description")?,
                    )
                    .context("Missing description vector")?
                    .as_slice(),
            );
            let file = self.files.get(&item.path).context("Missing indexed file")?;
            parts.push(
                self.vectors
                    .get(
                        file.description_embedding
                            .as_ref()
                            .context("Missing file description")?,
                    )
                    .context("Missing file vector")?
                    .as_slice(),
            );
        }
        concatenate(&parts)
    }

    pub fn search(&self, query: &str, kind: &str, options: &Value) -> Result<Vec<Value>> {
        let key = hash(
            json!([
                "query",
                self.db.generation()?,
                self.enabled,
                self.config,
                kind,
                query,
                options
            ])
            .to_string(),
        );
        if let Some(results) = self.db.search_cache(&key)? {
            return Ok(results);
        }
        let embedding_key = self.embed_one(query, true)?;
        let vector = self
            .db
            .embedding(&embedding_key)?
            .context("Missing query vector")?;
        let regex = options["regexp"].as_str().map(Regex::new).transpose()?;
        let explicit = ["code", "descriptions", "md"]
            .iter()
            .any(|k| flag(options, k));
        let code =
            kind == "search-code" || (kind == "search" && (!explicit || flag(options, "code")));
        let descriptions = kind == "search-descriptions"
            || (kind == "search"
                && ((!explicit && self.complete_descriptions) || flag(options, "descriptions")));
        let markdown =
            kind == "search-md" || (kind == "search" && (!explicit || flag(options, "md")));
        ensure!(
            !descriptions || self.complete_descriptions,
            "Descriptions are not enabled/complete; run slopdex descriptions enable"
        );
        let rerank = flag(&self.config, "rerankingEnabled");
        let limit = options["limit"].as_u64().map(|v| v as usize);
        let candidate_limit = if rerank {
            if let Some(maximum) = self.providers.reranker()?.candidate_limit() {
                Some(
                    limit
                        .unwrap_or(maximum)
                        .max(self.config["rerankerCandidates"].as_u64().unwrap_or(10) as usize)
                        .min(maximum),
                )
            } else {
                limit.map(|l| l.saturating_mul(5))
            }
        } else {
            limit
        };
        let mut results = Vec::new();
        if code || descriptions {
            let index_kind = if code && descriptions {
                "combined"
            } else if descriptions {
                "descriptions"
            } else {
                "code"
            };
            let repeats = if index_kind == "combined" {
                3
            } else if index_kind == "descriptions" {
                2
            } else {
                1
            };
            let query_vector = concatenate(&vec![vector.as_slice(); repeats])?;
            let allowed = self
                .items
                .iter()
                .filter(|i| {
                    i.kind == "function"
                        && regex.as_ref().is_none_or(|r| {
                            r.is_match(i.data["qualifiedName"].as_str().unwrap_or(""))
                        })
                })
                .map(|i| i.id)
                .collect();
            for (id, similarity) in self.neighbors(
                index_kind,
                &query_vector,
                &allowed,
                candidate_limit,
                options,
            )? {
                let item = self.item(id)?;
                let mut result =
                    json!({"type":"function","function":item.data,"similarity":similarity});
                self.add_scores(&mut result, item, &vector, None, index_kind)?;
                results.push(result);
            }
        }
        if markdown {
            let allowed = self
                .items
                .iter()
                .filter(|i| i.kind == "markdown")
                .map(|i| i.id)
                .collect();
            for (id, similarity) in
                self.neighbors("markdown", &vector, &allowed, candidate_limit, options)?
            {
                results.push(
                    json!({"type":"markdown","chunk":self.item(id)?.data,"similarity":similarity}),
                );
            }
        }
        sort_scores(&mut results, "similarity");
        if let Some(limit) = candidate_limit {
            results.truncate(limit);
        }
        if rerank && !results.is_empty() {
            let documents: Vec<_> = results.iter().map(Value::to_string).collect();
            let ranking = self.providers.reranker()?.rerank(query, &documents)?;
            results = ranking
                .into_iter()
                .map(|(index, score)| {
                    let mut row = results[index].clone();
                    row["rerankScore"] = json!(score);
                    row
                })
                .collect();
        }
        if let Some(limit) = limit {
            results.truncate(limit);
        }
        self.db.put_search_cache(&key, &results)?;
        Ok(results)
    }

    fn neighbors(
        &self,
        kind: &str,
        query: &[f32],
        allowed: &HashSet<u64>,
        limit: Option<usize>,
        options: &Value,
    ) -> Result<Vec<(u64, f64)>> {
        self.filtered_neighbors(
            kind,
            query,
            allowed.len(),
            |key| allowed.contains(&key),
            limit,
            options,
        )
    }

    fn filtered_neighbors(
        &self,
        kind: &str,
        query: &[f32],
        eligible_count: usize,
        allowed: impl Fn(u64) -> bool,
        limit: Option<usize>,
        options: &Value,
    ) -> Result<Vec<(u64, f64)>> {
        let index = self.indexes.get(kind).context("Search index unavailable")?;
        let wanted = limit.unwrap_or(eligible_count).min(eligible_count);
        if wanted == 0 {
            return Ok(Vec::new());
        }
        let min = options["minSimilarity"].as_f64().unwrap_or(0.3);
        let max = options["maxSimilarity"].as_f64().unwrap_or(f64::INFINITY);
        let mut width = wanted;
        loop {
            let found = index.search_filtered(query, width, &allowed)?;
            let total = found.len();
            let below_floor = found.last().is_some_and(|(_, score)| *score < min);
            let mut selected: Vec<_> = found
                .into_iter()
                .filter(|(_, s)| *s >= min && *s < max)
                .collect();
            if selected.len() >= wanted || width >= eligible_count || total < width || below_floor {
                selected.truncate(wanted);
                return Ok(selected);
            }
            width = width.saturating_mul(2).min(eligible_count);
        }
    }

    fn item(&self, id: u64) -> Result<&Item> {
        self.items
            .binary_search_by_key(&id, |i| i.id)
            .map(|index| &self.items[index])
            .map_err(|_| anyhow::anyhow!("USearch returned an unknown key {id}"))
    }

    fn add_scores(
        &self,
        result: &mut Value,
        item: &Item,
        code: &[f32],
        other: Option<(&[f32], &[f32])>,
        kind: &str,
    ) -> Result<()> {
        if kind != "descriptions" {
            result["codeSimilarity"] = json!(cosine(code, &self.vectors[&item.embedding]));
        }
        if kind == "combined" || kind == "descriptions" {
            let (description, file) = other.unwrap_or((code, code));
            result["descriptionSimilarity"] = json!(cosine(
                description,
                &self.vectors[item.description_embedding.as_ref().unwrap()]
            ));
            result["fileDescriptionSimilarity"] = json!(cosine(
                file,
                &self.vectors[self.files[&item.path]
                    .description_embedding
                    .as_ref()
                    .unwrap()]
            ));
        }
        Ok(())
    }

    pub fn cross_search(&self, target: Option<&Engine>, options: &Value) -> Result<Vec<Value>> {
        let target = target.unwrap_or(self);
        ensure!(
            self.providers.vector().profile() == target.providers.vector().profile(),
            "Cross-search requires identical embedding profiles"
        );
        let same = self.db.path.canonicalize()? == target.db.path.canonicalize()?;
        let combined = self.complete_descriptions
            && target.complete_descriptions
            && self.providers.llm().profile() == target.providers.llm().profile();
        let kind = if combined { "combined" } else { "code" };
        let base = options["changedSince"]
            .as_str()
            .map(|reference| self.resolve_base(reference))
            .transpose()?;
        let key = hash(
            json!([
                "cross",
                self.db.generation()?,
                target.db.path.canonicalize()?,
                target.db.generation()?,
                kind,
                base,
                self.db.meta("checkpoint")?,
                options
            ])
            .to_string(),
        );
        if let Some(results) = self.db.search_cache(&key)? {
            return Ok(results);
        }
        let min_lines = options["minLines"].as_u64().unwrap_or(2);
        let regex = options["regexp"].as_str().map(Regex::new).transpose()?;
        let source_path = options["sourcePath"]
            .as_str()
            .map(|path| self.normalize_path(path))
            .transpose()?;
        let changed = base
            .as_deref()
            .map(|reference| self.changed_identities(reference))
            .transpose()?;
        let mut seen = HashSet::new();
        let mut results = Vec::new();
        let eligible: HashMap<_, _> = target
            .items
            .iter()
            .filter(|i| {
                i.kind == "function" && i.data["lineCount"].as_u64().unwrap_or(0) >= min_lines
            })
            .map(|i| (i.id, target.root.join(&i.path)))
            .collect();
        for source in &self.items {
            if source.kind != "function"
                || source.data["lineCount"].as_u64().unwrap_or(0) < min_lines
                || regex.as_ref().is_some_and(|r| {
                    !r.is_match(source.data["qualifiedName"].as_str().unwrap_or(""))
                })
                || source_path
                    .as_ref()
                    .is_some_and(|p| !under(&source.path, p))
                || (flag(options, "uncommitted") && source.data["sourceMode"] != "working-tree")
                || changed
                    .as_ref()
                    .is_some_and(|ids| !ids.contains(&source.id))
            {
                continue;
            }
            let source_file = self.root.join(&source.path);
            let cross_file = flag(options, "crossFileOnly");
            let allowed = |id| {
                eligible.get(&id).is_some_and(|path| {
                    !(same && id == source.id || cross_file && *path == source_file)
                })
            };
            let query = self.item_vector(source, kind)?;
            let neighbors = target.filtered_neighbors(
                kind,
                &query,
                eligible.len(),
                allowed,
                Some(options["matches"].as_u64().unwrap_or(5) as usize),
                options,
            )?;
            let mut matches = Vec::new();
            for (id, similarity) in neighbors {
                if same
                    && !flag(options, "includeSymmetricDuplicates")
                    && !seen.insert((source.id.min(id), source.id.max(id)))
                {
                    continue;
                }
                let item = target.item(id)?;
                let mut row = json!({"function":item.data,"similarity":similarity});
                let other = if combined {
                    Some((
                        self.vectors[source.description_embedding.as_ref().unwrap()].as_slice(),
                        self.vectors[self.files[&source.path]
                            .description_embedding
                            .as_ref()
                            .unwrap()]
                        .as_slice(),
                    ))
                } else {
                    None
                };
                target.add_scores(
                    &mut row,
                    item,
                    &self.vectors[&source.embedding],
                    other,
                    kind,
                )?;
                if flag(options, "cohesion") {
                    row["physicalDistance"] = json!(distance(
                        &self.root.join(&source.path),
                        &target.root.join(&item.path)
                    ));
                }
                matches.push(row);
            }
            if flag(options, "cohesion") {
                matches.sort_by(|a, b| {
                    b["physicalDistance"]
                        .as_u64()
                        .cmp(&a["physicalDistance"].as_u64())
                        .then_with(|| score(b, "similarity").total_cmp(&score(a, "similarity")))
                });
            }
            if !matches.is_empty() {
                results.push(json!({"source":source.data,"matches":matches,"scoring":{"similarityMode":if combined {"code-description-file-average"}else{"code"},"similarityWeights":if combined {json!({"code":1.0/3.0,"description":1.0/3.0,"fileDescription":1.0/3.0})}else{json!({"code":1,"description":0,"fileDescription":0})}}}));
            }
        }
        self.db.put_search_cache(&key, &results)?;
        Ok(results)
    }

    pub fn describe(&self, query: &str, options: &Value) -> Result<Value> {
        let matches = self.search(query, "search", options)?;
        let threshold = options["describeFullFileThreshold"].as_f64().unwrap_or(0.8);
        let mut files = BTreeMap::<String, Value>::new();
        for item in &matches {
            let data = if item["type"] == "markdown" {
                &item["chunk"]
            } else {
                &item["function"]
            };
            if let Some(path) = data["path"].as_str() {
                let file = &self.files[path];
                let entry = files.entry(path.into()).or_insert_with(
                    || json!({"path":path,"description":file.description,"similarity":0.0}),
                );
                let similarity = score(item, "similarity");
                if similarity > score(entry, "similarity") {
                    entry["similarity"] = json!(similarity);
                }
                if similarity > threshold {
                    entry["content"] = json!(file.source);
                }
            }
        }
        let context =
            json!({"query":query,"files":files.values().collect::<Vec<_>>(),"matches":matches});
        let description=self.providers.llm().describe("Explain the existing code and documentation relevant to the user's task using only the supplied search context. Cite paths and symbols. Do not propose an implementation. If there is insufficient context, say so.",&context.to_string())?;
        let files: Vec<_> = files
            .into_values()
            .map(|mut f| {
                f.as_object_mut().unwrap().remove("content");
                f
            })
            .collect();
        let functions: Vec<_> = matches
            .into_iter()
            .filter(|r| r["type"] == "function")
            .map(|r| {
                let mut f = r["function"].clone();
                f.as_object_mut().unwrap().remove("source");
                f.as_object_mut().unwrap().remove("embeddingInput");
                f["similarity"] = r["similarity"].clone();
                f
            })
            .collect();
        Ok(json!({"query":query,"description":description,"files":files,"functions":functions}))
    }

    pub fn set_descriptions(&mut self, enabled: bool) -> Result<Value> {
        self.enabled = enabled;
        self.config["descriptionsEnabled"] = json!(enabled);
        self.db.set_meta(
            "descriptions_enabled",
            if enabled { "true" } else { "false" },
        )?;
        self.refresh()?;
        self.status()
    }

    pub fn reindex_files(&mut self, callables: bool) -> Result<Value> {
        ensure!(self.enabled, "Enable descriptions first");
        let mut changed = Vec::new();
        for file in self.files.values() {
            if file.language != "markdown" && file.description_hash.as_deref() != Some(&file.hash) {
                changed.push(self.prepare(
                    &file.path,
                    &file.source,
                    &file.source_mode,
                    true,
                    callables,
                )?);
            }
        }
        let checkpoint = self.db.meta("checkpoint")?;
        self.db.apply(&changed, &[], checkpoint.as_deref())?;
        self.load()?;
        Ok(json!({"filesReindexed":changed.len(),"descriptionsEnabled":self.enabled}))
    }

    pub fn status(&self) -> Result<Value> {
        let errors = self.errors()?;
        Ok(
            json!({"rootDir":self.root,"indexPath":self.db.path,"generation":self.db.generation()?,"gitCheckpoint":self.db.meta("checkpoint")?,"fileCount":self.files.len(),"functionCount":self.items.iter().filter(|i|i.kind=="function").count(),"markdownChunkCount":self.items.iter().filter(|i|i.kind=="markdown").count(),"embeddingProfile":self.providers.vector().profile(),"descriptionProfile":if self.enabled {self.providers.llm().profile()}else{Value::Null},"descriptionsEnabled":self.enabled,"descriptionCount":self.items.iter().filter(|i|i.description_embedding.is_some()).count(),"fileDescriptionCount":self.files.values().filter(|f|f.description.is_some()).count(),"staleFileDescriptionCount":self.files.values().filter(|f|f.description.is_some() && f.description_hash.as_deref()!=Some(&f.hash)).count(),"indexingErrorCount":errors.len(),"failedFileCount":self.files.values().filter(|f|!f.errors.is_empty()).count(),"vectorBackend":"usearch","storageBackend":"sqlite"}),
        )
    }

    pub fn errors(&self) -> Result<Vec<Value>> {
        Ok(self.files.values().flat_map(|f| f.errors.clone()).collect())
    }

    fn git_text(&self, args: &[&str]) -> Option<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn git_bytes(&self, args: &[&str]) -> Result<Vec<u8>> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()?;
        ensure!(
            output.status.success(),
            "Git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(output.stdout)
    }

    fn dirty_paths(&self) -> Result<HashSet<String>> {
        if self.git_text(&["rev-parse", "--verify", "HEAD"]).is_none() {
            return Ok(HashSet::new());
        }
        let mut bytes =
            self.git_bytes(&["diff", "--relative", "--name-only", "-z", "HEAD", "--", "."])?;
        bytes.extend(self.git_bytes(&["ls-files", "--others", "--exclude-standard", "-z"])?);
        bytes
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|b| Ok(String::from_utf8(b.to_vec())?))
            .collect()
    }

    fn resolve_base(&self, reference: &str) -> Result<String> {
        let resolved = self
            .git_text(&[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{reference}^{{commit}}"),
            ])
            .context("Cannot resolve --changed-since commit")?;
        let head = self
            .db
            .meta("checkpoint")?
            .context("--changed-since requires a Git checkpoint")?;
        self.git_bytes(&["merge-base", "--is-ancestor", &resolved, &head])
            .context("--changed-since must be an ancestor of the indexed commit")?;
        Ok(resolved)
    }

    fn changed_identities(&self, resolved: &str) -> Result<HashSet<u64>> {
        let prefix = self
            .git_text(&["rev-parse", "--show-prefix"])
            .unwrap_or_default();
        let mut base = HashMap::<String, HashSet<(String, String)>>::new();
        let mut changed = HashSet::new();
        for item in self.items.iter().filter(|i| i.kind == "function") {
            if !base.contains_key(&item.path) {
                let source = self
                    .git_bytes(&["show", &format!("{resolved}:{prefix}{}", item.path)])
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok());
                let symbols = source
                    .and_then(|s| parse::parse(&item.path, &s).ok())
                    .map(|p| {
                        p.callables
                            .into_iter()
                            .map(|c| (c.qualified_name, c.source_hash))
                            .collect()
                    })
                    .unwrap_or_default();
                base.insert(item.path.clone(), symbols);
            }
            if !base[&item.path].contains(&(
                item.data["qualifiedName"].as_str().unwrap_or("").into(),
                item.data["sourceHash"].as_str().unwrap_or("").into(),
            )) {
                changed.insert(item.id);
            }
        }
        Ok(changed)
    }

    fn normalize_path(&self, path: &str) -> Result<String> {
        let path = Path::new(path);
        if path.is_absolute() {
            return relative(&self.root, path);
        }
        ensure!(
            !path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "Source path must remain inside the repository"
        );
        Ok(path
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_owned())
    }
}

fn flag(value: &Value, key: &str) -> bool {
    value[key].as_bool().unwrap_or(false)
}
fn score(value: &Value, key: &str) -> f64 {
    value[key].as_f64().unwrap_or(0.0)
}
fn sort_scores(values: &mut [Value], key: &str) {
    values.sort_by(|a, b| score(b, key).total_cmp(&score(a, key)));
}
fn under(path: &str, parent: &str) -> bool {
    parent.is_empty()
        || path == parent
        || path
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('/'))
}
fn relative(root: &Path, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(root)
        .context("Path is outside repository")?
        .to_str()
        .context("Non-UTF-8 source path")?
        .replace('\\', "/"))
}
fn globs(value: &Value) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    if let Some(patterns) = value.as_array() {
        for pattern in patterns {
            builder.add(Glob::new(
                pattern.as_str().context("Glob must be a string")?,
            )?);
        }
    }
    Ok(builder.build()?)
}
fn concatenate(parts: &[&[f32]]) -> Result<Vec<f32>> {
    let mut values = Vec::new();
    for part in parts {
        let norm = part.iter().map(|v| (*v as f64).powi(2)).sum::<f64>().sqrt();
        ensure!(
            norm.is_finite() && norm > 0.0,
            "Cannot index a zero or non-finite vector"
        );
        values.extend(part.iter().map(|v| (*v as f64 / norm) as f32));
    }
    Ok(values)
}
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(a, b)| *a as f64 * *b as f64).sum();
    let norm = |v: &[f32]| v.iter().map(|v| (*v as f64).powi(2)).sum::<f64>().sqrt();
    (dot / (norm(a) * norm(b))).clamp(-1.0, 1.0)
}
fn distance(left: &Path, right: &Path) -> usize {
    if left == right {
        return 0;
    }
    let a: Vec<_> = left.parent().unwrap_or(left).components().collect();
    let b: Vec<_> = right.parent().unwrap_or(right).components().collect();
    let common = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    1 + a.len() + b.len() - 2 * common
}

/// Keep SQLite writes on the caller thread. Persist each successful HTTP result
/// as it arrives, even if another in-flight request fails. Never launch more
/// requests after a failed window, and join every worker before returning.
fn run_jobs<T: Sync, R: Send>(
    jobs: &[T],
    parallelism: usize,
    work: impl Fn(&T) -> Result<R> + Sync,
    mut persist: impl FnMut(&T, R) -> Result<()>,
) -> Result<()> {
    for window in jobs.chunks(parallelism) {
        std::thread::scope(|scope| -> Result<()> {
            let (sender, receiver) = std::sync::mpsc::channel();
            for job in window {
                let sender = sender.clone();
                let work = &work;
                scope.spawn(move || {
                    let _ = sender.send((job, work(job)));
                });
            }
            drop(sender);
            let mut failure = None;
            for (job, result) in receiver {
                if let Err(error) = result.and_then(|value| persist(job, value)) {
                    failure.get_or_insert(error);
                }
            }
            if let Some(error) = failure {
                Err(error)
            } else {
                Ok(())
            }
        })?;
    }
    Ok(())
}
