use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde_json::{Value, json};

use crate::{
    cache::{Artifacts, DescriptionArtifact, DescriptionGeneration},
    filter::Selection,
    git, hash, parse,
    providers::Providers,
    storage::{Database, File, Item, STRUCTURE_PARSER_VERSION},
    ui,
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

const FILE_DESCRIPTION_SYSTEM: &str = "Describe existing source code accurately. Return exactly one concise paragraph about the file's purpose, responsibilities, and important relationships; use plain text without a heading, bullets, or a preamble.";
const CALLABLE_DESCRIPTION_SYSTEM: &str = "Describe existing source code accurately. Return exactly one sentence describing what the callable does, including relevant inputs, outputs, or side effects; use plain text without a heading, bullets, or a preamble.";
// A byte cap is deliberately conservative even for providers with different tokenizers.
// Leave room for the task, expanded search context, and the model's response.
const DESCRIBE_PROMPT_BYTES: usize = 128 * 1024;

pub struct Engine {
    root: PathBuf,
    db: Database,
    artifacts: Artifacts,
    config: Value,
    providers: Providers,
    enabled: bool,
    items: Vec<Item>,
    files: HashMap<String, File>,
    vectors: HashMap<String, Vec<f32>>,
    indexes: HashMap<String, VectorIndex>,
    complete_descriptions: bool,
    complete_code: bool,
    structural_only: bool,
    // Readers share a lock; writers hold it across SQLite and USearch publication.
    _lock: fs::File,
    readonly: bool,
}

#[derive(Debug)]
pub(crate) struct NeedsWrite;

impl std::fmt::Display for NeedsWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Index needs an exclusive refresh")
    }
}

impl std::error::Error for NeedsWrite {}

fn lock_index(lock: &fs::File, readonly: bool) -> Result<()> {
    lock_index_with_timeout(lock, readonly, Duration::from_secs(10))
}

fn lock_index_with_timeout(lock: &fs::File, readonly: bool, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        let result = if readonly {
            FileExt::try_lock_shared(lock)
        } else {
            FileExt::try_lock_exclusive(lock)
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if start.elapsed() >= timeout {
                    anyhow::bail!(
                        "Index is in use by another slopdex command (waited {:.2} seconds)",
                        timeout.as_secs_f64()
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error).context("Lock index"),
        }
    }
}

struct PrepareInput {
    path: String,
    source: String,
    source_mode: String,
    regenerate_file: bool,
    regenerate_callables: bool,
}

struct PreparedCallable {
    identity: String,
    data: Value,
    description_embedding: Option<String>,
}

struct PreparedFile {
    file: File,
    parsed: parse::ParsedFile,
    regenerate_file: bool,
    regenerate_callables: bool,
    callables: Vec<PreparedCallable>,
}

impl Engine {
    pub fn open(root: &Path, index: &Path, mut config: Value) -> Result<Self> {
        Self::open_internal(root, index, config.take(), false, false)
    }

    pub fn open_map(root: &Path, index: &Path, config: Value) -> Result<Self> {
        Self::open_internal(root, index, config, true, false)
    }

    pub fn open_map_readonly(root: &Path, index: &Path, config: Value) -> Result<Self> {
        Self::open_internal(root, index, config, true, true)
    }

    pub fn open_readonly(root: &Path, index: &Path, config: Value) -> Result<Self> {
        Self::open_internal(root, index, config, false, true)
    }

    fn open_internal(
        root: &Path,
        index: &Path,
        mut config: Value,
        structural_only: bool,
        readonly: bool,
    ) -> Result<Self> {
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
        lock_index(&lock, readonly)?;
        let db = if readonly {
            Database::open_readonly(&index, &root)?
        } else {
            Database::open(&index, &root, &Value::Null, flag(&config, "forceReindex"))?
        };
        let artifacts = Artifacts::open(&config);
        if !readonly && artifacts.import_workspace(&db).is_err() {
            ui::warning("Could not import existing artifacts into the shared cache");
        }
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
        // Construction makes no requests. Map does not validate or select a model profile.
        let map_config = json!({});
        let providers = Providers::new(if structural_only {
            &map_config
        } else {
            &config
        })?;
        if !structural_only && !readonly {
            db.set_projection_profile(&providers.vector().profile())?;
        }
        let enabled = config["descriptionsEnabled"]
            .as_bool()
            .unwrap_or(db.meta("descriptions_enabled")?.as_deref() == Some("true"));
        let mut engine = Self {
            root,
            db,
            artifacts,
            config,
            providers,
            enabled,
            items: Vec::new(),
            files: HashMap::new(),
            vectors: HashMap::new(),
            indexes: HashMap::new(),
            complete_descriptions: false,
            complete_code: false,
            structural_only,
            _lock: lock,
            readonly,
        };
        engine.load()?;
        Ok(engine)
    }

    pub fn refresh(&mut self) -> Result<Value> {
        let structure = self.refresh_structure()?;
        if flag(&self.config, "noReindex") {
            return Ok(structure);
        }
        ensure!(
            !self.structural_only,
            "Semantic preparation requires a search engine"
        );
        let mut paths = HashSet::new();
        for item in &self.items {
            if item.embedding.is_empty()
                || (self.enabled && item.kind == "function" && item.description_embedding.is_none())
            {
                paths.insert(item.path.clone());
            }
        }
        if self.enabled {
            for file in self.files.values() {
                if file.language != "markdown"
                    && file.description_embedding.is_none()
                    && !file.hash.is_empty()
                {
                    paths.insert(file.path.clone());
                }
            }
        }
        let mut paths: Vec<_> = paths.into_iter().collect();
        paths.sort();
        let profile = self.providers.vector().profile().to_string();
        let prepared = paths.is_empty()
            && self.db.meta("active_embedding_profile")?.as_deref() == Some(&profile)
            && self.db.meta("descriptions_enabled")?.as_deref()
                == Some(if self.enabled { "true" } else { "false" })
            && (!self.enabled
                || self.db.meta("description_profile")?.as_deref()
                    == Some(self.providers.llm().profile().to_string().as_str()));
        if self.readonly && !prepared {
            return Err(NeedsWrite.into());
        }
        if prepared {
            return Ok(
                json!({"filesUpdated":structure["filesUpdated"],"filesDeleted":structure["filesDeleted"],
                "filesPrepared":0,"functionCount":self.items.iter().filter(|i|i.kind=="function").count(),
                "checkpoint":self.db.meta("checkpoint")?,"generation":self.db.generation()?}),
            );
        }
        let mut inputs = Vec::new();
        for path in paths {
            let file = &self.files[&path];
            inputs.push(PrepareInput {
                path,
                source: file.source.clone(),
                source_mode: file.source_mode.clone(),
                regenerate_file: false,
                regenerate_callables: false,
            });
        }
        let changed = self.prepare_all(inputs)?;
        let checkpoint = self.db.meta("checkpoint")?;
        ensure!(
            git::head(&self.root) == checkpoint,
            "Git HEAD changed during indexing; rerun"
        );
        for (file, _) in &changed {
            ensure!(
                hash(fs::read(self.root.join(&file.path))?) == file.hash,
                "Source changed during indexing: {}; rerun",
                file.path
            );
        }
        self.db.apply(&changed, &[], checkpoint.as_deref())?;
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
        self.load()?;
        self.artifacts.backfill_remote(&self.db);
        Ok(
            json!({"filesUpdated":structure["filesUpdated"],"filesDeleted":structure["filesDeleted"],
            "filesPrepared":changed.len(),"functionCount":self.items.iter().filter(|i|i.kind=="function").count(),
            "checkpoint":checkpoint,"generation":self.db.generation()?}),
        )
    }

    pub fn refresh_structure(&mut self) -> Result<Value> {
        if flag(&self.config, "noReindex") {
            return Ok(json!({"skipped":true,"generation":self.db.generation()?}));
        }
        ui::progress("Scanning repository files");
        let checkpoint = git::head(&self.root);
        let dirty = git::dirty_paths(&self.root)?;
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
            if index_artifact(entry.path(), &self.db.path) {
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
        if self.readonly && (!removed.is_empty() || self.db.meta("checkpoint")? != checkpoint) {
            return Err(NeedsWrite.into());
        }
        let mut changed = Vec::new();
        let indexing = ui::counted("Indexing files", paths.len());
        for path in paths {
            ui::progress(&path);
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
                    changed.push((file, parse::ParsedFile::default()));
                    indexing.inc(1);
                    continue;
                }
            };
            let source_hash = hash(&source);
            let previous = self.files.get(&path);
            if previous.is_some_and(|f| {
                f.hash == source_hash && f.source_mode == source_mode && f.errors.is_empty()
            }) && self
                .db
                .structure_current(&path, &source_hash, STRUCTURE_PARSER_VERSION)?
            {
                indexing.inc(1);
                continue;
            }
            if self.readonly {
                return Err(NeedsWrite.into());
            }
            let parsed = self.parsed(&path, &source)?;
            let file = File {
                path: path.clone(),
                hash: source_hash,
                source,
                language: parse::language_for_path(&path).unwrap().into(),
                source_mode: source_mode.into(),
                description: previous.and_then(|f| f.description.clone()),
                description_hash: previous.and_then(|f| f.description_hash.clone()),
                description_embedding: previous.and_then(|f| f.description_embedding.clone()),
                errors: Vec::new(),
            };
            changed.push((file, parsed));
            indexing.inc(1);
        }
        indexing.finish();
        if changed.is_empty() && removed.is_empty() && self.db.meta("checkpoint")? == checkpoint {
            return Ok(
                json!({"filesUpdated":0,"filesDeleted":0,"functionCount":self.items.iter().filter(|i|i.kind=="function").count(),"checkpoint":checkpoint,"generation":self.db.generation()?}),
            );
        }
        // Never publish a mixture of snapshots if files changed while remote
        // providers were running. Their completed artifacts remain reusable.
        ensure!(
            git::head(&self.root) == checkpoint,
            "Git HEAD changed during indexing; rerun"
        );
        let verifying = ui::counted("Verifying indexed files", changed.len());
        for (file, _) in &changed {
            if !file.hash.is_empty() {
                ensure!(
                    hash(fs::read(self.root.join(&file.path))?) == file.hash,
                    "Source changed during indexing: {}; rerun",
                    file.path
                );
            }
            verifying.inc(1);
        }
        verifying.finish();
        ui::progress("Saving index snapshot");
        let dirty = self
            .db
            .apply_structure(&changed, &removed, checkpoint.as_deref())?;
        if dirty {
            self.load()?;
        }
        Ok(
            json!({"filesUpdated":changed.len(),"filesDeleted":removed.len(),"functionCount":self.items.iter().filter(|i|i.kind=="function").count(),"checkpoint":checkpoint,"generation":self.db.generation()?}),
        )
    }

    pub fn map(&self, options: &Value) -> Result<Vec<Value>> {
        let selection = Selection::compile(options)?;
        let paths = options["paths"]
            .as_array()
            .map(|paths| {
                paths
                    .iter()
                    .map(|path| {
                        self.normalize_path(path.as_str().context("Map paths must be strings")?)
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        let callers = options["callers"]
            .as_u64()
            .unwrap_or(0)
            .max(options["expandCallers"].as_u64().unwrap_or(0)) as usize;
        let callees = options["callees"]
            .as_u64()
            .unwrap_or(0)
            .max(options["expandCallees"].as_u64().unwrap_or(0)) as usize;
        let mut selected = BTreeMap::<String, Vec<parse::StructureNode>>::new();
        for path in self.db.paths()? {
            if !selection.path_matches(&path)
                || (!paths.is_empty() && !paths.iter().any(|p| under(&path, p)))
            {
                continue;
            }
            let file = &self.files[&path];
            ensure!(
                self.db
                    .structure_current(&path, &file.hash, STRUCTURE_PARSER_VERSION)?,
                "Structure for {path} requires refresh; run map without --no-reindex"
            );
            let nodes = selection.select_structure(&self.db.structure(&path)?);
            if !nodes.is_empty() {
                selected.insert(path, nodes);
            }
        }
        let expansion = if callers > 0 || callees > 0 {
            let graph = self.call_graph()?;
            let expand_callers = options["expandCallers"].as_u64().unwrap_or(0) as usize;
            let expand_callees = options["expandCallees"].as_u64().unwrap_or(0) as usize;
            let seeds = selected.iter().flat_map(|(path, nodes)| {
                nodes
                    .iter()
                    .filter(|node| {
                        matches!(
                            node.kind.as_str(),
                            "function" | "method" | "constructor" | "generator"
                        )
                    })
                    .map(|node| crate::callgraph::Key {
                        path: path.clone(),
                        id: node.id,
                    })
            });
            let expanded = graph.expand(seeds, callers, callees);
            let code_keys = graph.code_keys(&expanded, expand_callers, expand_callees);
            for key in expanded.depths.keys() {
                if let Some(structure) = graph.files.get(&key.path)
                    && let Some(node) = graph.node(key)
                {
                    let nodes = selected.entry(key.path.clone()).or_default();
                    for ancestor in crate::map::ancestors(structure, node) {
                        if !nodes.iter().any(|existing| existing.id == ancestor.id) {
                            nodes.push(ancestor);
                        }
                    }
                }
            }
            Some((expanded, code_keys))
        } else {
            None
        };
        let mut result = Vec::new();
        for (path, mut nodes) in selected {
            nodes.sort_by_key(|node| (node.start_byte, node.id));
            let mut values = Vec::new();
            for node in nodes {
                let key = crate::callgraph::Key {
                    path: path.clone(),
                    id: node.id,
                };
                let mut value = serde_json::to_value(&node)?;
                value.as_object_mut().unwrap().remove("calls");
                if let Some((expansion, code_keys)) = &expansion {
                    if let Some(depth) = expansion.depths.get(&key) {
                        value["callDepth"] = json!(depth);
                    }
                    if let Some(calls) = expansion.comments.get(&key) {
                        value["callees"] = json!(calls);
                    }
                    if code_keys.contains(&key) {
                        value["expandedCode"] = json!(true);
                        if let Some(code) = self
                            .presentation_source(&path)
                            .and_then(|source| source.get(node.start_byte..node.end_byte))
                        {
                            value["source"] = json!(code);
                        }
                    }
                }
                values.push(value);
            }
            result.push(json!({"path":path,"nodes":values}));
        }
        Ok(result)
    }

    /// Search presentation uses the same indexed declarations as map. Older
    /// offline indexes can still return hits without current structure; callers
    /// can fall back to the search unit's declaration metadata in that case.
    pub fn presentation_structure(&self, path: &str) -> Result<Option<parse::FileStructure>> {
        let Some(file) = self.files.get(path) else {
            return Ok(None);
        };
        if !self
            .db
            .structure_current(path, &file.hash, STRUCTURE_PARSER_VERSION)?
        {
            return Ok(None);
        }
        Ok(Some(self.db.structure(path)?))
    }

    pub fn call_graph(&self) -> Result<crate::callgraph::CallGraph> {
        crate::callgraph::CallGraph::load(&self.db)
    }

    pub fn presentation_source(&self, path: &str) -> Option<&str> {
        self.files.get(path).map(|file| file.source.as_str())
    }

    pub fn presentation_file_description(&self, path: &str) -> Option<&str> {
        self.files
            .get(path)
            .and_then(|file| file.description.as_deref())
    }

    pub fn presentation_symbol_descriptions(&self, path: &str) -> Result<HashMap<usize, String>> {
        self.db.symbol_descriptions(path)
    }

    fn parsed(&self, path: &str, source: &str) -> Result<parse::ParsedFile> {
        let key = hash(json!([STRUCTURE_PARSER_VERSION, path, hash(source)]).to_string());
        if let Some(cached) = self.db.cache("parse", &key)? {
            return Ok(serde_json::from_str(&cached)?);
        }
        let parsed = parse::parse(path, source)?;
        self.db
            .cache_put("parse", &key, &serde_json::to_string(&parsed)?)?;
        Ok(parsed)
    }

    fn prepare_all(&self, inputs: Vec<PrepareInput>) -> Result<Vec<(File, Vec<Item>)>> {
        let mut prepared = Vec::with_capacity(inputs.len());
        for input in inputs {
            let source_hash = hash(&input.source);
            let parsed = self.parsed(&input.path, &input.source)?;
            let previous = self.files.get(&input.path);
            let file = File {
                path: input.path.clone(),
                hash: source_hash,
                source: input.source,
                language: parse::language_for_path(&input.path)
                    .unwrap_or("unknown")
                    .into(),
                source_mode: input.source_mode.clone(),
                description: previous.and_then(|f| f.description.clone()),
                description_hash: previous.and_then(|f| f.description_hash.clone()),
                description_embedding: previous.and_then(|f| f.description_embedding.clone()),
                errors: parsed
                    .errors
                    .iter()
                    .map(|e| {
                        json!({"path":input.path,"code":"parse-error","message":e.message,"startLine":e.start_line,"endLine":e.end_line,"sourceMode":input.source_mode})
                    })
                    .collect(),
            };
            prepared.push(PreparedFile {
                file,
                parsed,
                regenerate_file: input.regenerate_file,
                regenerate_callables: input.regenerate_callables,
                callables: Vec::new(),
            });
        }

        let mut file_jobs = BTreeMap::new();
        let mut file_assignments = Vec::new();
        for (index, prepared) in prepared.iter().enumerate() {
            if self.enabled
                && (prepared.file.description.is_none() || prepared.regenerate_file)
                && prepared.file.language != "markdown"
            {
                let key = hash(json!([prepared.file.hash, FILE_DESCRIPTION_SYSTEM]).to_string());
                let prompt = format!(
                    "Describe the purpose, responsibilities, and important relationships of this existing source file in one paragraph. Do not propose changes.\nFile: {}\n\n{}",
                    prepared.file.path, prepared.file.source
                );
                file_jobs
                    .entry(key.clone())
                    .or_insert_with(|| DescriptionGeneration {
                        scope: "file".into(),
                        path: prepared.file.path.clone(),
                        symbol: None,
                        source_hash: prepared.file.hash.clone(),
                        file_hash: prepared.file.hash.clone(),
                        file_description: None,
                        profile: self.providers.llm().profile(),
                        settings: description_settings(&self.config),
                        system: FILE_DESCRIPTION_SYSTEM.into(),
                        prompt,
                        regenerate: prepared.regenerate_file,
                    });
                file_assignments.push((index, key));
            }
        }
        self.artifacts
            .hydrate_descriptions(&self.db, &file_jobs.keys().cloned().collect::<Vec<_>>())?;
        let jobs = missing_jobs(&self.db, "description", file_jobs)?;
        let llm = self.providers.llm();
        run_jobs(
            "Generating file descriptions",
            &jobs,
            self.parallelism()?,
            |_| 1,
            |(_, generation)| llm.describe(&generation.system, &generation.prompt),
            |(key, generation), description| {
                let (artifact, contents) = DescriptionArtifact::new(description, generation)?;
                let encoded = serde_json::to_string(&artifact)?;
                self.db.put_description_artifact(key, &encoded, &contents)?;
                self.artifacts
                    .put_description(&self.db, key, &encoded, &contents);
                Ok(())
            },
        )?;
        for (index, key) in file_assignments {
            let artifact = self
                .db
                .cache("description", &key)?
                .context("Missing generated file description")?;
            prepared[index].file.description = Some(DescriptionArtifact::decode(&artifact)?.text);
            prepared[index].file.description_hash = Some(prepared[index].file.hash.clone());
        }

        let mut description_jobs = BTreeMap::new();
        let mut description_assignments = Vec::new();
        for (file_index, prepared) in prepared.iter_mut().enumerate() {
            let mut occurrences = HashMap::<String, usize>::new();
            for callable in &prepared.parsed.callables {
                let occurrence = occurrences
                    .entry(callable.qualified_name.clone())
                    .or_default();
                let identity = hash(
                    json!([
                        prepared.file.path,
                        callable.qualified_name,
                        callable.kind,
                        *occurrence
                    ])
                    .to_string(),
                );
                *occurrence += 1;
                let mut data = serde_json::to_value(callable)?;
                data["path"] = json!(prepared.file.path);
                data["sourceMode"] = json!(prepared.file.source_mode);
                let old = self.items.iter().find(|item| item.identity == identity);
                let mut description_embedding =
                    old.and_then(|item| item.description_embedding.clone());
                data["description"] = old
                    .map(|item| item.data["description"].clone())
                    .unwrap_or(Value::Null);
                let unchanged = old.is_some_and(|item| {
                    item.data["sourceHash"] == data["sourceHash"]
                        && item.data["description"].is_string()
                });
                if !unchanged {
                    description_embedding = None;
                    data["description"] = Value::Null;
                }
                if self.enabled
                    && (prepared.regenerate_callables
                        || (!unchanged && description_embedding.is_none()))
                {
                    let key = hash(
                        json!([
                            callable.qualified_name,
                            callable.source_hash,
                            prepared.file.description.as_deref().unwrap_or(""),
                            CALLABLE_DESCRIPTION_SYSTEM,
                        ])
                        .to_string(),
                    );
                    let prompt = format!(
                        "Describe what this existing callable does in one sentence, covering relevant inputs, outputs and side effects. Do not propose changes.\nFile: {}\nFile context: {}\nSymbol: {}\n\n{}",
                        prepared.file.path,
                        prepared.file.description.as_deref().unwrap_or(""),
                        callable.qualified_name,
                        callable.source
                    );
                    description_jobs
                        .entry(key.clone())
                        .or_insert_with(|| DescriptionGeneration {
                            scope: "callable".into(),
                            path: prepared.file.path.clone(),
                            symbol: Some(callable.qualified_name.clone()),
                            source_hash: callable.source_hash.clone(),
                            file_hash: prepared.file.hash.clone(),
                            file_description: prepared.file.description.clone(),
                            profile: self.providers.llm().profile(),
                            settings: description_settings(&self.config),
                            system: CALLABLE_DESCRIPTION_SYSTEM.into(),
                            prompt,
                            regenerate: prepared.regenerate_callables,
                        });
                    description_assignments.push((file_index, prepared.callables.len(), key));
                } else if !self.enabled
                    && old.is_some_and(|item| item.data["sourceHash"] != data["sourceHash"])
                {
                    description_embedding = None;
                    data["description"] = Value::Null;
                }
                prepared.callables.push(PreparedCallable {
                    identity,
                    data,
                    description_embedding,
                });
            }
        }
        self.artifacts.hydrate_descriptions(
            &self.db,
            &description_jobs.keys().cloned().collect::<Vec<_>>(),
        )?;
        let jobs = missing_jobs(&self.db, "description", description_jobs)?;
        run_jobs(
            "Generating callable descriptions",
            &jobs,
            self.parallelism()?,
            |_| 1,
            |(_, generation)| llm.describe(&generation.system, &generation.prompt),
            |(key, generation), description| {
                let (artifact, contents) = DescriptionArtifact::new(description, generation)?;
                let encoded = serde_json::to_string(&artifact)?;
                self.db.put_description_artifact(key, &encoded, &contents)?;
                self.artifacts
                    .put_description(&self.db, key, &encoded, &contents);
                Ok(())
            },
        )?;
        for (file_index, callable_index, key) in description_assignments {
            let artifact = self
                .db
                .cache("description", &key)?
                .context("Missing generated callable description")?;
            prepared[file_index].callables[callable_index].data["description"] =
                json!(DescriptionArtifact::decode(&artifact)?.text);
        }

        let mut embedding_groups = Vec::with_capacity(prepared.len());
        for prepared in &prepared {
            let mut inputs = Vec::new();
            inputs.extend(
                prepared
                    .parsed
                    .callables
                    .iter()
                    .map(|callable| callable.embedding_input.clone()),
            );
            inputs.extend(
                prepared
                    .parsed
                    .chunks
                    .iter()
                    .map(|chunk| chunk.embedding_input.clone()),
            );
            if self.enabled {
                inputs.extend(prepared.file.description.clone());
                inputs.extend(prepared.callables.iter().filter_map(|callable| {
                    callable.data["description"].as_str().map(str::to_owned)
                }));
            }
            embedding_groups.push(inputs);
        }
        self.ensure_embedding_groups(embedding_groups, false)?;

        let vector_profile = self.providers.vector().profile();
        let mut result = Vec::with_capacity(prepared.len());
        for mut prepared in prepared {
            if self.enabled
                && let Some(description) = &prepared.file.description
            {
                prepared.file.description_embedding =
                    Some(Database::embedding_key(&vector_profile, false, description));
            }
            let mut items = Vec::new();
            for (callable, mut prepared_callable) in prepared
                .parsed
                .callables
                .into_iter()
                .zip(prepared.callables)
            {
                if self.enabled
                    && let Some(description) = prepared_callable.data["description"].as_str()
                {
                    prepared_callable.description_embedding =
                        Some(Database::embedding_key(&vector_profile, false, description));
                }
                items.push(Item {
                    id: 0,
                    path: prepared.file.path.clone(),
                    identity: prepared_callable.identity,
                    kind: "function".into(),
                    data: prepared_callable.data,
                    embedding: Database::embedding_key(
                        &vector_profile,
                        false,
                        &callable.embedding_input,
                    ),
                    description_embedding: prepared_callable.description_embedding,
                });
            }
            for (ordinal, chunk) in prepared.parsed.chunks.into_iter().enumerate() {
                let embedding =
                    Database::embedding_key(&vector_profile, false, &chunk.embedding_input);
                let kind = if prepared.file.language == "markdown" {
                    "markdown"
                } else {
                    "document"
                };
                let identity = hash(json!([prepared.file.path, kind, ordinal]).to_string());
                let mut data = serde_json::to_value(chunk)?;
                data["path"] = json!(prepared.file.path);
                data["sourceMode"] = json!(prepared.file.source_mode);
                items.push(Item {
                    id: 0,
                    path: prepared.file.path.clone(),
                    identity,
                    kind: kind.into(),
                    data,
                    embedding,
                    description_embedding: None,
                });
            }
            result.push((prepared.file, items));
        }
        Ok(result)
    }

    fn ensure_embeddings(&self, inputs: &[String], query: bool) -> Result<()> {
        self.ensure_embedding_groups(vec![inputs.to_vec()], query)
    }

    fn ensure_embedding_groups(&self, groups: Vec<Vec<String>>, query: bool) -> Result<()> {
        let vector = self.providers.vector();
        let profile = vector.profile();
        let dimensions = vector.dimensions();
        let batch = vector.batch_limit();
        let mut seen = HashSet::new();
        let mut batches = Vec::new();
        let keys: Vec<_> = groups
            .iter()
            .flatten()
            .map(|input| (Database::embedding_key(&profile, query, input), dimensions))
            .collect();
        self.artifacts.hydrate_embeddings(&self.db, &keys)?;
        for inputs in groups {
            let mut pending = BTreeMap::new();
            for input in inputs {
                let key = Database::embedding_key(&profile, query, &input);
                if seen.insert(key.clone()) && self.db.embedding(&key)?.is_none() {
                    pending.insert(key, input);
                }
            }
            let pending: Vec<_> = pending.into_iter().collect();
            batches.extend(pending.chunks(batch).map(<[_]>::to_vec));
        }
        run_jobs(
            "Generating embeddings",
            &batches,
            self.parallelism()?,
            |entries| entries.len(),
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
                    self.artifacts.put_embedding(&self.db, key, &vector);
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
        ui::progress("Loading index records");
        let profile = self.providers.vector().profile();
        self.items = if self.structural_only {
            Vec::new()
        } else {
            self.db.items_for_profile(&profile)?
        };
        let files = if self.structural_only {
            self.db.files()?
        } else {
            self.db.files_for_profile(&profile)?
        };
        self.files = files.into_iter().map(|f| (f.path.clone(), f)).collect();
        self.vectors.clear();
        self.indexes.clear();
        self.complete_code =
            !self.structural_only && self.items.iter().all(|i| !i.embedding.is_empty());
        self.complete_descriptions = self.enabled
            && self.complete_code
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
        if !self.complete_code {
            return Ok(());
        }
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
        let loading = ui::counted("Loading embeddings", keys.len());
        for key in keys {
            self.vectors.insert(
                key.clone(),
                self.db
                    .embedding(&key)?
                    .context("Missing indexed embedding")?,
            );
            loading.inc(1);
        }
        loading.finish();
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
        let loading = ui::counted(
            "Loading vector indexes",
            if self.complete_descriptions { 4 } else { 2 },
        );
        for (kind, parts) in [
            ("code", 1),
            ("markdown", 1),
            ("descriptions", 2),
            ("combined", 3),
        ] {
            if parts > 1 && !self.complete_descriptions {
                continue;
            }
            let items: Vec<_> = self
                .items
                .iter()
                .filter(|i| {
                    if kind == "markdown" {
                        matches!(i.kind.as_str(), "markdown" | "document")
                    } else {
                        i.kind == "function"
                    }
                })
                .collect();
            let preparing = ui::counted(format_args!("Preparing {kind} vectors"), items.len());
            let mut vectors = Vec::with_capacity(items.len());
            for item in items {
                vectors.push((item.id, self.item_vector(item, kind)?));
                preparing.inc(1);
            }
            preparing.finish();
            let mut path = self.db.path.as_os_str().to_os_string();
            path.push(format!(".{kind}.usearch"));
            ui::progress(format_args!("Loading {kind} vector index"));
            let index = if self.readonly {
                VectorIndex::open_readonly(
                    &PathBuf::from(path),
                    dimensions * parts,
                    self.db.generation()?,
                    &vectors,
                )?
            } else {
                VectorIndex::open(
                    &PathBuf::from(path),
                    dimensions * parts,
                    self.db.generation()?,
                    &vectors,
                )?
            };
            self.indexes.insert(kind.into(), index);
            loading.inc(1);
        }
        loading.finish();
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
        ensure!(
            self.complete_code && (!self.enabled || self.complete_descriptions),
            "Semantic index is incomplete for the configured profiles; run without --no-reindex to prepare it"
        );
        let selection = Selection::compile(options)?;
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
        if self.readonly {
            return Err(NeedsWrite.into());
        }
        let embedding_key = self.embed_one(query, true)?;
        let vector = self
            .db
            .embedding(&embedding_key)?
            .context("Missing query vector")?;
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
        let searching = ui::counted(
            "Searching vector indexes",
            usize::from(code || descriptions) + usize::from(markdown),
        );
        if code || descriptions {
            let index_kind = if code && descriptions {
                "combined"
            } else if descriptions {
                "descriptions"
            } else {
                "code"
            };
            ui::progress(format_args!("Searching {index_kind} index"));
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
                        && selection.path_matches(&i.path)
                        && selection.name_matches(i.data["qualifiedName"].as_str().unwrap_or(""))
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
            searching.inc(1);
        }
        if markdown {
            ui::progress("Searching markdown index");
            let allowed = self
                .items
                .iter()
                .filter(|i| {
                    (i.kind == "markdown" || kind == "search" && !explicit && i.kind == "document")
                        && selection.path_matches(&i.path)
                        && selection.name_matches(&heading_name(&i.data))
                })
                .map(|i| i.id)
                .collect();
            for (id, similarity) in
                self.neighbors("markdown", &vector, &allowed, candidate_limit, options)?
            {
                let item = self.item(id)?;
                results.push(json!({"type":item.kind,"chunk":item.data,"similarity":similarity}));
            }
            searching.inc(1);
        }
        searching.finish();
        sort_scores(&mut results, "similarity");
        if let Some(limit) = candidate_limit {
            results.truncate(limit);
        }
        if rerank && !results.is_empty() {
            let documents: Vec<_> = results.iter().map(Value::to_string).collect();
            let reranking = ui::counted("Reranking candidates", documents.len());
            let ranking_key = hash(
                json!([
                    "rerank-v1",
                    self.config["rerankerProvider"],
                    self.config["rerankerModel"],
                    self.config["rerankerBaseUrl"],
                    query,
                    documents
                ])
                .to_string(),
            );
            self.artifacts
                .hydrate_text(&self.db, "rerank", std::slice::from_ref(&ranking_key))?;
            let cached: Option<Vec<(usize, f64)>> = self
                .db
                .cache("rerank", &ranking_key)?
                .and_then(|text| serde_json::from_str(&text).ok())
                .filter(|rank: &Vec<(usize, f64)>| {
                    rank.len() == results.len()
                        && rank
                            .iter()
                            .all(|(id, score)| *id < results.len() && score.is_finite())
                        && rank.iter().map(|(id, _)| id).collect::<HashSet<_>>().len() == rank.len()
                });
            let ranking: Vec<(usize, f64)> = if let Some(cached) = cached {
                cached
            } else {
                let ranking = self.providers.reranker()?.rerank(query, &documents)?;
                let text = serde_json::to_string(&ranking)?;
                self.db.cache_put("rerank", &ranking_key, &text)?;
                self.artifacts
                    .put_text(&self.db, "rerank", &ranking_key, &text);
                ranking
            };
            results = ranking
                .into_iter()
                .map(|(index, score)| {
                    let mut row = results[index].clone();
                    row["rerankScore"] = json!(score);
                    row
                })
                .collect();
            reranking.inc(documents.len());
            reranking.finish();
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
        // Cross-search needs a more selective floor than natural-language
        // queries: weak edges can otherwise join every connected component.
        // Include the effective default in the cache key as well as filtering.
        let mut options = options.clone();
        if options["minSimilarity"].as_f64().is_none() {
            options["minSimilarity"] = json!(0.8);
        }
        let options = &options;
        let target = target.unwrap_or(self);
        ensure!(
            self.complete_code
                && target.complete_code
                && (!self.enabled || self.complete_descriptions)
                && (!target.enabled || target.complete_descriptions),
            "Semantic index is incomplete for the configured profiles; run without --no-reindex to prepare it"
        );
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
            .map(|reference| {
                git::resolve_base(
                    &self.root,
                    reference,
                    self.db.meta("checkpoint")?.as_deref(),
                )
            })
            .transpose()?;
        let key = hash(
            json!([
                "cross",
                self.db.generation()?,
                target.db.path.canonicalize()?,
                target.db.generation()?,
                self.providers.vector().profile(),
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
        let max_lines = options["maxLines"].as_u64();
        let lines_match = |item: &Item| {
            let lines = item.data["lineCount"].as_u64().unwrap_or(0);
            lines >= min_lines && max_lines.is_none_or(|max| lines < max)
        };
        let selection = Selection::compile(options)?;
        let source_path = options["sourcePath"]
            .as_str()
            .map(|path| self.normalize_path(path))
            .transpose()?;
        let changed = base
            .as_deref()
            .map(|reference| git::changed_identities(&self.root, reference, &self.items))
            .transpose()?;
        let mut seen = HashSet::new();
        let mut results = Vec::new();
        let eligible: HashMap<_, _> = target
            .items
            .iter()
            .filter(|i| i.kind == "function" && lines_match(i))
            .map(|i| (i.id, target.root.join(&i.path)))
            .collect();
        let sources: Vec<_> = self
            .items
            .iter()
            .filter(|source| {
                source.kind == "function"
                    && lines_match(source)
                    && selection.path_matches(&source.path)
                    && selection.name_matches(source.data["qualifiedName"].as_str().unwrap_or(""))
                    && source_path.as_ref().is_none_or(|p| under(&source.path, p))
                    && (!flag(options, "uncommitted")
                        || source.data["sourceMode"] == "working-tree")
                    && changed.as_ref().is_none_or(|ids| ids.contains(&source.id))
            })
            .collect();
        let searching = ui::counted("Searching source functions", sources.len());
        for source in sources {
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
            searching.inc(1);
        }
        searching.finish();
        self.db.put_search_cache(&key, &results)?;
        Ok(results)
    }

    pub fn describe(&self, query: &str, options: &Value) -> Result<Value> {
        let matches = self.search(query, "search", options)?;
        let mut files = BTreeMap::<String, Value>::new();
        for item in &matches {
            let data = if item["type"] == "markdown" || item["type"] == "document" {
                &item["chunk"]
            } else {
                &item["function"]
            };
            if let Some(path) = data["path"].as_str() {
                let file = &self.files[path];
                let similarity = score(item, "similarity");
                let entry = files.entry(path.into()).or_insert_with(
                    || json!({"path":path,"description":file.description,"similarity":similarity}),
                );
                if similarity > score(entry, "similarity") {
                    entry["similarity"] = json!(similarity);
                }
            }
        }
        let mut prompt = format!("Task: {query}\n\n");
        let marker = "\n[Search results truncated to fit the prompt size limit]\n";
        ensure!(
            prompt.len() + marker.len() <= DESCRIBE_PROMPT_BYTES,
            "Describe query exceeds the prompt size limit"
        );
        let search = crate::cli::describe_search_context(
            self,
            &matches,
            (options["callers"]
                .as_u64()
                .unwrap_or(0)
                .max(options["expandCallers"].as_u64().unwrap_or(2))) as usize,
            (options["callees"]
                .as_u64()
                .unwrap_or(0)
                .max(options["expandCallees"].as_u64().unwrap_or(2))) as usize,
            options["expandCallers"].as_u64().unwrap_or(2) as usize,
            options["expandCallees"].as_u64().unwrap_or(2) as usize,
            options["expandCodeThreshold"].as_f64().unwrap_or(0.9),
        )?;
        let search_budget = DESCRIBE_PROMPT_BYTES - prompt.len();
        if search.len() <= search_budget {
            prompt.push_str(&search);
        } else {
            let mut end = search_budget.saturating_sub(marker.len());
            while !search.is_char_boundary(end) {
                end -= 1;
            }
            prompt.push_str(&search[..end]);
            prompt.push_str(marker);
        }
        ui::progress("Generating explanation from search results");
        let system = "Explain the existing code and documentation relevant to the user's task using only the supplied search context. Cite paths and symbols. Do not propose an implementation. If there is insufficient context, say so.";
        let key = hash(
            json!([
                "explanation-v2",
                self.providers.llm().profile(),
                self.config["descriptionBaseUrl"],
                self.config["descriptionFallbackModel"],
                self.config["fallbackModel"],
                system,
                prompt
            ])
            .to_string(),
        );
        self.artifacts
            .hydrate_text(&self.db, "explanation", std::slice::from_ref(&key))?;
        let description = if let Some(cached) = self.db.cache("explanation", &key)? {
            cached
        } else {
            let description = self.providers.llm().describe(system, &prompt)?;
            self.db.cache_put("explanation", &key, &description)?;
            self.artifacts
                .put_text(&self.db, "explanation", &key, &description);
            description
        };
        let files: Vec<_> = files.into_values().collect();
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
        self.load()?;
        self.status()
    }

    pub fn reindex_files(&mut self, callables: bool) -> Result<Value> {
        ensure!(self.enabled, "Enable descriptions first");
        let inputs: Vec<_> = self
            .files
            .values()
            .filter(|file| {
                file.language != "markdown" && file.description_hash.as_deref() != Some(&file.hash)
            })
            .map(|file| PrepareInput {
                path: file.path.clone(),
                source: file.source.clone(),
                source_mode: file.source_mode.clone(),
                regenerate_file: true,
                regenerate_callables: callables,
            })
            .collect();
        let changed = self.prepare_all(inputs)?;
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

    fn normalize_path(&self, path: &str) -> Result<String> {
        let path = Path::new(path);
        if path.is_absolute() {
            // Engine::open canonicalizes the root; resolve aliases here too
            // before checking that an absolute source path is inside it.
            let path = path.canonicalize().context("Cannot resolve source path")?;
            return relative(&self.root, &path);
        }
        ensure!(
            !path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "Source path must remain inside the repository"
        );
        Ok(path
            .components()
            .filter(|c| !matches!(c, std::path::Component::CurDir))
            .collect::<PathBuf>()
            .to_string_lossy()
            .replace('\\', "/"))
    }
}

/// The index may live inside the scanned root; its vector manifests are JSON.
fn index_artifact(path: &Path, index: &Path) -> bool {
    if path == index {
        return true;
    }
    if path.parent() != index.parent() {
        return false;
    }
    let Some(suffix) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(index.file_name()?.to_str()?))
    else {
        return false;
    };
    suffix == ".lock"
        || ["code", "markdown", "descriptions", "combined"]
            .iter()
            .any(|role| {
                suffix == format!(".{role}.usearch.manifest.json")
                    || suffix == format!(".{role}.usearch")
            })
}

fn flag(value: &Value, key: &str) -> bool {
    value[key].as_bool().unwrap_or(false)
}

fn description_settings(config: &Value) -> Value {
    json!({
        "descriptionProvider": config["descriptionProvider"],
        "descriptionModel": config["descriptionModel"],
        "descriptionFallbackModel": config["descriptionFallbackModel"],
        "fallbackModel": config["fallbackModel"],
        "descriptionBaseUrl": config["descriptionBaseUrl"],
        "providerTimeoutMs": config["providerTimeoutMs"],
        "providerMaxRetries": config["providerMaxRetries"],
        "retryDelayMs": config["retryDelayMs"],
    })
}
fn heading_name(data: &Value) -> String {
    data["headingPath"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(".")
        })
        .unwrap_or_default()
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

fn missing_jobs<T>(
    db: &Database,
    kind: &str,
    jobs: BTreeMap<String, T>,
) -> Result<Vec<(String, T)>> {
    jobs.into_iter()
        .filter_map(|(key, job)| match db.cache(kind, &key) {
            Ok(Some(_)) => None,
            other => Some(other.map(|_| (key, job))),
        })
        .collect()
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
    label: &str,
    jobs: &[T],
    parallelism: usize,
    units: impl Fn(&T) -> usize,
    work: impl Fn(&T) -> Result<R> + Sync,
    mut persist: impl FnMut(&T, R) -> Result<()>,
) -> Result<()> {
    if jobs.is_empty() {
        return Ok(());
    }
    let progress = ui::counted(label, jobs.iter().map(&units).sum());
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
                } else {
                    progress.inc(units(job));
                }
            }
            if let Some(error) = failure {
                Err(error)
            } else {
                Ok(())
            }
        })?;
    }
    progress.finish();
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn contended_writer_times_out_but_readers_can_share() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("index.lock");
        let first = fs::File::create(&path)?;
        let second = fs::File::open(&path)?;
        lock_index(&first, true)?;
        lock_index(&second, true)?;
        let error = lock_index_with_timeout(&second, false, Duration::from_millis(60))
            .expect_err("writer must wait for other readers");
        assert!(error.to_string().contains("Index is in use"));
        drop(first);
        drop(second);
        let writer = fs::File::open(&path)?;
        lock_index(&writer, false)?;
        Ok(())
    }

    #[test]
    fn normalize_absolute_source_paths_through_symlinked_root() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        let alias = temp.path().join("alias");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("src/source.rs"), "fn source() {}")?;
        fs::create_dir(&outside)?;
        symlink(&root, &alias)?;
        symlink(&outside, root.join("escape"))?;
        let engine = Engine::open(&alias, &temp.path().join("index.sqlite"), json!({}))?;

        // Both spellings must match the canonical root stored by Engine::open,
        // including when a system temp directory itself is a symlink on macOS.
        for base in [alias, root.canonicalize()?] {
            for suffix in ["", "src", "src/source.rs"] {
                assert_eq!(
                    engine.normalize_path(base.join(suffix).to_str().unwrap())?,
                    suffix
                );
            }
        }
        for path in [outside, root.join("escape"), root.join("../outside")] {
            let error = engine.normalize_path(path.to_str().unwrap()).unwrap_err();
            assert_eq!(error.to_string(), "Path is outside repository");
        }
        assert_eq!(engine.normalize_path("./src/source.rs")?, "src/source.rs");
        assert!(engine.normalize_path("../outside").is_err());
        Ok(())
    }
}
