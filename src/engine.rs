use std::{
    cell::{OnceCell, Ref, RefCell},
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use globset::{Glob, GlobSet, GlobSetBuilder};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

use crate::{
    cache::{Artifacts, DescriptionArtifact, DescriptionGeneration},
    filter::Selection,
    formats::FormatGroup,
    git, hash,
    limits::{self, LimitedResults},
    models::{Message, Vector},
    parse,
    providers::Providers,
    registry,
    storage::{Database, File, Item, STRUCTURE_PARSER_VERSION},
    symbols, ui,
    vectors::SharedIndex,
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

// Generated dependency resolutions, including ecosystems without a .lock suffix.
// Match at any depth, but do not exclude dependency manifests or source names.
const EXCLUDED_LOCK_FILES: &[&str] = &[
    "**/*.lock",
    "**/*.lockb",
    "**/*.lockfile",
    "**/*.locked",
    "**/*.lock.json",
    "**/*.lock.yaml",
    "**/*.lock.yml",
    "**/*-lock.json",
    "**/*-lock.yaml",
    "**/*-lock.yml",
    "**/npm-shrinkwrap.json",
    "**/shrinkwrap.yaml",
    "**/esy.lock/**",
    "**/pylock.toml",
    "**/pylock.*.toml",
    "**/go.sum",
    "**/go.work.sum",
    "**/Package.resolved",
    "**/Cartfile.resolved",
    "**/.terraform.lock.hcl",
    "**/manifest.toml",
    "**/Manifest.toml",
    "**/Manifest-v*.toml",
    "**/JuliaManifest.toml",
    "**/JuliaManifest-v*.toml",
    "**/cpanfile.snapshot",
    "**/cabal.project.freeze",
    "**/cabal.project.*.freeze",
    "**/dub.selections.json",
    "**/.meteor/versions",
];

const DESCRIPTION_SYSTEM: &str = "Describe existing source code accurately. When asked about a file, return exactly one concise paragraph about its purpose, responsibilities, and important relationships. When asked about a callable, return exactly one sentence describing what it does, including relevant inputs, outputs, or side effects. Use plain text without a heading, bullets, or a preamble. Do not propose changes.";
// A byte cap is deliberately conservative even for providers with different tokenizers.
// Leave room for the task, expanded search context, and the model's response.
const DESCRIBE_PROMPT_BYTES: usize = 128 * 1024;

pub struct Engine {
    root: PathBuf,
    db: Database,
    artifacts: Artifacts,
    config: Value,
    providers: Providers,
    items: Vec<Item>,
    files: HashMap<String, File>,
    sources: HashMap<String, OnceCell<String>>,
    vectors: RefCell<HashMap<String, Vec<f32>>>,
    indexes: RefCell<HashMap<String, SharedIndex>>,
    // Derived from committed records; cleared whenever they are reloaded.
    structures: RefCell<HashMap<String, Option<Arc<parse::FileStructure>>>>,
    symbol_index: RefCell<Option<SymbolIndex>>,
    structural_only: bool,
    // Readers share a lock; writers hold it across SQLite and USearch publication.
    _lock: fs::File,
    readonly: bool,
}

struct SymbolIndex {
    names: BTreeMap<u64, String>,
    index: SharedIndex,
}

struct SearchPlan {
    groups: Vec<FormatGroup>,
    descriptions: bool,
    symbols: bool,
}

impl SearchPlan {
    fn compile(kind: &str, options: &Value, selection: &Selection) -> Self {
        let explicit = ["code", "descriptions", "docs", "md", "symbols"]
            .iter()
            .any(|channel| flag(options, channel));
        let selected = |channel| kind == "search" && (!explicit || flag(options, channel));
        let groups = FormatGroup::ALL
            .into_iter()
            .filter(|group| {
                selection.includes_group(*group)
                    && match group {
                        FormatGroup::Code => kind == "search-code" || selected("code"),
                        FormatGroup::Docs => {
                            matches!(kind, "search-docs" | "search-md")
                                || selected("docs")
                                || selected("md")
                        }
                        FormatGroup::Config | FormatGroup::Markup => kind == "search" && !explicit,
                    }
            })
            .collect();
        Self {
            groups,
            descriptions: kind == "search-descriptions" || selected("descriptions"),
            symbols: kind == "search-symbols" || selected("symbols"),
        }
    }

    fn roles(&self) -> impl Iterator<Item = &str> {
        self.groups
            .iter()
            .map(|group| group.as_str())
            .chain(self.descriptions.then_some("descriptions"))
            .chain(self.symbols.then_some("symbols"))
    }
}

type DescriptionQueries<'a> = (Option<&'a [f32]>, Option<&'a [f32]>);

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
            // Windows reports ERROR_LOCK_VIOLATION rather than WouldBlock.
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
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
    generate_descriptions: bool,
}

struct PreparedCallable {
    identity: String,
    data: Value,
    generate_description: bool,
}

struct FileConversation {
    messages: Vec<Message>,
    session: String,
    file_ready: bool,
    next_callable: usize,
}

struct DescriptionJob {
    file_index: usize,
    callable_index: Option<usize>,
    key: String,
    generation: DescriptionGeneration,
    session: String,
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

    pub(crate) fn open_symbol_map(
        root: &Path,
        index: &Path,
        config: Value,
        readonly: bool,
    ) -> Result<Self> {
        let mut engine = Self::open_internal(root, index, config, true, readonly)?;
        crate::providers::resolve_embeddings(
            &mut engine.config,
            engine.db.meta("active_embedding_profile")?.as_deref(),
        )?;
        engine.providers = Providers::new(&engine.config)?;
        Ok(engine)
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
        limits::validate_config(&config)?;
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
            Database::open_readonly_with_config(&index, &root, &config)?
        } else {
            Database::open_with_config(&index, &root, &config, flag(&config, "forceReindex"))?
        };
        config["artifactCachePath"] = json!(db.global_path());
        let artifacts = if readonly {
            Artifacts::open_readonly(&config)?
        } else {
            Artifacts::open(&config)?
        };
        if !readonly {
            registry::register(&db, &root)?;
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
        let map_config = json!({});
        if !structural_only {
            crate::providers::resolve_embeddings(
                &mut config,
                db.meta("active_embedding_profile")?.as_deref(),
            )?;
        }
        // Provider construction is offline. Structural map does not validate or
        // resolve a semantic model selection.
        let providers = Providers::new(if structural_only {
            &map_config
        } else {
            &config
        })?;
        if !structural_only && !readonly {
            db.set_projection_profile(&providers.vector().profile())?;
        }
        let mut engine = Self {
            root,
            db,
            artifacts,
            config,
            providers,
            items: Vec::new(),
            files: HashMap::new(),
            sources: HashMap::new(),
            vectors: RefCell::new(HashMap::new()),
            indexes: RefCell::new(HashMap::new()),
            structures: RefCell::new(HashMap::new()),
            symbol_index: RefCell::new(None),
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
            if (item.kind != "symbol-description" && item.embedding.is_empty())
                || (item.data["description"].is_string() && item.description_embedding.is_none())
            {
                paths.insert(item.path.clone());
            }
        }
        for file in self.files.values() {
            if file.description.is_some() && file.description_embedding.is_none() {
                paths.insert(file.path.clone());
            }
        }
        let mut paths: Vec<_> = paths.into_iter().collect();
        paths.sort();
        let profile = self.providers.vector().profile().to_string();
        let prepared = paths.is_empty()
            && self.db.meta("active_embedding_profile")?.as_deref() == Some(&profile);
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
            let source = self.source(&path)?.to_owned();
            inputs.push(PrepareInput {
                path,
                source,
                source_mode: file.source_mode.clone(),
                regenerate_file: false,
                regenerate_callables: false,
                generate_descriptions: false,
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
        self.load()?;
        self.artifacts.backfill_remote(&self.db);
        Ok(
            json!({"filesUpdated":structure["filesUpdated"],"filesDeleted":structure["filesDeleted"],
            "filesPrepared":changed.len(),"functionCount":self.items.iter().filter(|i|i.kind=="function").count(),
            "checkpoint":checkpoint,"generation":self.db.generation()?}),
        )
    }

    pub fn refresh_structure(&mut self) -> Result<Value> {
        self.refresh_structure_with(|| Ok(()))
    }

    fn refresh_structure_with(
        &mut self,
        after_discovery: impl FnOnce() -> Result<()>,
    ) -> Result<Value> {
        if flag(&self.config, "noReindex") {
            return Ok(json!({"skipped":true,"generation":self.db.generation()?}));
        }
        ui::progress("Checking repository changes");
        // Git owns discovery of tracked/untracked changes. Previously dirty
        // paths remain candidates: restoring one makes it disappear from status.
        let previous_checkpoint = self.db.meta("checkpoint")?;
        let changes = git::changes(&self.root, previous_checkpoint.as_deref())?;
        let checkpoint = changes
            .as_ref()
            .map_or_else(|| git::head(&self.root), |c| c.head.clone());
        let mut dirty = match &changes {
            Some(changes) => changes.dirty.clone(),
            None => git::dirty_paths(&self.root)?,
        };
        let eligibility = Eligibility::new(&self.config)?;
        self.retain_relevant_dirty(&mut dirty, &eligibility);
        let previous_dirty: HashSet<String> = self
            .db
            .meta("dirty_paths")?
            .map(|value| serde_json::from_str(&value))
            .transpose()?
            .unwrap_or_default();
        let mut directories: HashSet<String> = self
            .db
            .meta("source_directories")?
            .map(|value| serde_json::from_str(&value))
            .transpose()?
            .unwrap_or_default();
        let policy = indexing_policy(
            &self.root,
            &self.config,
            self.db.meta("repository_identity")?.as_deref(),
            &directories,
        )?;
        let policy_current = self.db.meta("discovery_policy")?.as_deref() == Some(&policy);
        // Parse diagnostics are a function of content and parser version, both
        // covered below. A failed read has no content hash, so only its saved
        // metadata can show that retrying might now succeed.
        let read_failures: Vec<&str> = self
            .files
            .values()
            .filter(|file| has_read_error(file))
            .map(|file| file.path.as_str())
            .collect();
        if changes.is_some()
            && previous_checkpoint == checkpoint
            && dirty.is_empty()
            && previous_dirty.is_empty()
            && policy_current
            && self
                .db
                .missing_structure_paths(STRUCTURE_PARSER_VERSION)?
                .is_empty()
            && (read_failures.is_empty() || {
                let saved = self.saved_fingerprints()?;
                let mut unchanged = true;
                for path in &read_failures {
                    unchanged &=
                        saved.get(*path) == Some(&file_fingerprint(&self.root.join(path))?);
                }
                unchanged
            })
        {
            return Ok(
                json!({"filesUpdated":0,"filesDeleted":0,"functionCount":self.items.iter().filter(|item| item.kind=="function").count(),"checkpoint":checkpoint,"generation":self.db.generation()?}),
            );
        }
        let previous_fingerprints = self.saved_fingerprints()?;
        let mut fingerprints = previous_fingerprints.clone();
        let mut changed_policy = false;
        for path in dirty
            .iter()
            .chain(previous_dirty.iter())
            .filter(|path| policy_path(path))
        {
            let fingerprint = file_fingerprint(&self.root.join(path))?;
            changed_policy |= previous_fingerprints.get(path) != Some(&fingerprint);
            fingerprints.insert(path.clone(), fingerprint);
        }
        let full_scan = changes.is_none()
            || !policy_current
            || changed_policy
            || (previous_checkpoint != checkpoint
                && changes.as_ref().is_some_and(|c| c.policy_changed));
        let max_size = self.config["maxFileSize"].as_u64().unwrap_or(1_048_576);
        let mut candidates = changes
            .as_ref()
            .map(|c| c.changed.clone())
            .unwrap_or_default();
        candidates.extend(previous_dirty.iter().cloned());
        candidates.extend(self.db.missing_structure_paths(STRUCTURE_PARSER_VERSION)?);
        // Failed reads stay candidates so a metadata change retries them; files
        // with parse diagnostics are ordinary unchanged files.
        candidates.extend(read_failures.iter().map(|path| (*path).to_owned()));
        let paths = if full_scan {
            directories.clear();
            source_paths_subset(
                &self.root,
                &self.db.path,
                &self.config,
                None,
                Some(&mut directories),
                true,
            )?
        } else {
            let paths = if candidates.is_empty() {
                Vec::new()
            } else {
                source_paths_subset(
                    &self.root,
                    &self.db.path,
                    &self.config,
                    Some(&candidates),
                    Some(&mut directories),
                    true,
                )?
            };
            // Candidate walks only add directories. Forget deleted ones so their
            // policy files are not probed forever; absent files never contribute
            // to the policy, so this cannot change it.
            directories.retain(|directory| self.root.join(directory).is_dir());
            paths
        };
        let policy = indexing_policy(
            &self.root,
            &self.config,
            self.db.meta("repository_identity")?.as_deref(),
            &directories,
        )?;
        after_discovery()?;
        let paths_set: HashSet<_> = paths.iter().cloned().collect();
        let removed: Vec<_> = self
            .files
            .keys()
            .filter(|p| (full_scan || candidates.contains(*p)) && !paths_set.contains(*p))
            .cloned()
            .collect();
        if self.readonly
            && (!removed.is_empty()
                || self.db.meta("checkpoint")? != checkpoint
                || self.db.meta("discovery_policy")?.as_deref() != Some(&policy))
        {
            return Err(NeedsWrite.into());
        }
        for path in &removed {
            fingerprints.remove(path);
        }
        let mut changed = Vec::new();
        let indexing = ui::counted("Indexing files", paths.len());
        for path in &paths {
            let path = path.clone();
            ui::progress(&path);
            let source_mode = if checkpoint.is_none() || dirty.contains(&path) {
                "working-tree"
            } else {
                "git"
            };
            let fingerprint = file_fingerprint(&self.root.join(&path))?;
            fingerprints.insert(path.clone(), fingerprint.clone());
            let previous = self.files.get(&path);
            // Metadata and structure currency do not depend on how the file was
            // discovered, so full scans (every non-Git refresh, and any policy
            // change) reuse them too. The size limit is the one policy input
            // that changes the outcome for unchanged metadata.
            if let Some(previous) = previous
                && previous_checkpoint == checkpoint
                && previous.source_mode == source_mode
                && previous_fingerprints.get(&path) == Some(&fingerprint)
                && if has_read_error(previous) {
                    policy_current
                } else {
                    fingerprint[0].as_u64().is_some_and(|size| size <= max_size)
                }
                && self
                    .db
                    .structure_current(&path, &previous.hash, STRUCTURE_PARSER_VERSION)?
            {
                indexing.inc(1);
                continue;
            }
            let source = match read_source(&self.root, &path, max_size) {
                Ok(source) => source,
                Err(error) => {
                    let errors = vec![
                        json!({"path":path,"code":"read-error","message":error.to_string(),"sourceMode":source_mode}),
                    ];
                    // The same failure is already published: nothing to rewrite.
                    if previous.is_some_and(|f| f.source_mode == source_mode && f.errors == errors)
                        && self
                            .db
                            .structure_current(&path, &hash(""), STRUCTURE_PARSER_VERSION)?
                    {
                        indexing.inc(1);
                        continue;
                    }
                    if self.readonly {
                        return Err(NeedsWrite.into());
                    }
                    let file = File {
                        path: path.clone(),
                        hash: hash(""),
                        source: String::new(),
                        language: parse::language_for_path(&path).unwrap().into(),
                        source_mode: source_mode.into(),
                        description: None,
                        description_hash: None,
                        description_embedding: None,
                        errors,
                    };
                    changed.push((file, parse::ParsedFile::default()));
                    indexing.inc(1);
                    continue;
                }
            };
            let source_hash = hash(&source);
            // Saved parse diagnostics belong to this exact content. A failed read
            // shares the empty-content hash and must still be replaced.
            if previous.is_some_and(|f| {
                f.hash == source_hash && f.source_mode == source_mode && !has_read_error(f)
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
        // Never publish a mixture of snapshots if files changed while remote
        // providers were running. Their completed artifacts remain reusable.
        ensure!(
            git::head(&self.root) == checkpoint,
            "Git HEAD changed during indexing; rerun"
        );
        let verifying = ui::counted("Verifying indexed files", changed.len());
        for (file, _) in &changed {
            if !file
                .errors
                .iter()
                .any(|error| error["code"] == "read-error")
            {
                ensure!(
                    hash(fs::read(self.root.join(&file.path))?) == file.hash,
                    "Source changed during indexing: {}; rerun",
                    file.path
                );
            }
            verifying.inc(1);
        }
        verifying.finish();
        let mut current_dirty = if changes.is_some() {
            let current = git::changes(&self.root, previous_checkpoint.as_deref())?
                .context("Git repository changed during indexing; rerun")?;
            ensure!(
                current.head == checkpoint,
                "Git HEAD changed during indexing; rerun"
            );
            current.dirty
        } else {
            git::dirty_paths(&self.root)?
        };
        self.retain_relevant_dirty(&mut current_dirty, &eligibility);
        ensure!(
            current_dirty == dirty,
            "Git working tree changed during indexing; rerun"
        );
        ensure!(
            indexing_policy(
                &self.root,
                &self.config,
                self.db.meta("repository_identity")?.as_deref(),
                &directories
            )? == policy,
            "Ignore policy changed during indexing; rerun"
        );
        let verified_paths = source_paths_subset(
            &self.root,
            &self.db.path,
            &self.config,
            if full_scan { None } else { Some(&candidates) },
            None,
            false,
        )?;
        ensure!(
            verified_paths == paths,
            "Repository file selection changed during indexing; rerun"
        );
        for path in &paths {
            ensure!(
                fingerprints.get(path) == Some(&file_fingerprint(&self.root.join(path))?),
                "Source changed during indexing: {path}; rerun"
            );
        }
        if changed.is_empty() && removed.is_empty() && previous_checkpoint == checkpoint {
            if !self.readonly {
                self.save_refresh_state(&dirty, &fingerprints, &policy, &directories)?;
                self.db.apply_structure(&[], &[], checkpoint.as_deref())?;
            }
            return Ok(
                json!({"filesUpdated":0,"filesDeleted":0,"functionCount":self.items.iter().filter(|i|i.kind=="function").count(),"checkpoint":checkpoint,"generation":self.db.generation()?}),
            );
        }
        ui::progress("Saving index snapshot");
        // Save the union first so a crash after snapshot publication cannot lose
        // the information required to notice a later restore of a dirty path.
        let mut pending_dirty = previous_dirty;
        pending_dirty.extend(dirty.iter().cloned());
        self.db
            .set_meta("dirty_paths", &serde_json::to_string(&pending_dirty)?)?;
        self.db
            .set_meta("selection_policy", &selection_contract(&self.config))?;
        self.db
            .set_meta("source_directories", &serde_json::to_string(&directories)?)?;
        let published = self
            .db
            .apply_structure(&changed, &removed, checkpoint.as_deref())?;
        self.save_refresh_state(&dirty, &fingerprints, &policy, &directories)?;
        registry::observe_checkpoint(&self.db, &self.root, checkpoint.as_deref())?;
        if published {
            self.load()?;
        }
        Ok(
            json!({"filesUpdated":changed.len(),"filesDeleted":removed.len(),"functionCount":self.items.iter().filter(|i|i.kind=="function").count(),"checkpoint":checkpoint,"generation":self.db.generation()?}),
        )
    }

    fn save_refresh_state(
        &self,
        dirty: &HashSet<String>,
        fingerprints: &BTreeMap<String, Value>,
        policy: &str,
        directories: &HashSet<String>,
    ) -> Result<()> {
        let tx = self.db.conn.unchecked_transaction()?;
        for (key, value) in [
            ("dirty_paths", serde_json::to_string(dirty)?),
            ("file_fingerprints", serde_json::to_string(fingerprints)?),
            ("discovery_policy", policy.to_owned()),
            ("selection_policy", selection_contract(&self.config)),
            ("source_directories", serde_json::to_string(directories)?),
        ] {
            tx.execute("INSERT INTO metadata VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE value<>excluded.value", rusqlite::params![key,value])?;
        }
        tx.commit()?;
        Ok(())
    }

    fn saved_fingerprints(&self) -> Result<BTreeMap<String, Value>> {
        Ok(self
            .db
            .meta("file_fingerprints")?
            .map(|value| serde_json::from_str(&value))
            .transpose()?
            .unwrap_or_default())
    }

    /// Git status also reports paths discovery can never select (modified
    /// submodules, unsupported or excluded files). They must not keep the
    /// working tree "dirty" forever and defeat the unchanged fast path. Indexed
    /// paths and ignore-policy files always stay relevant.
    fn retain_relevant_dirty(&self, dirty: &mut HashSet<String>, eligibility: &Eligibility) {
        dirty.retain(|path| {
            !self.own_artifact(path)
                && (self.files.contains_key(path)
                    || policy_path(path)
                    || eligibility.selects_git_path(path))
        });
    }

    fn own_artifact(&self, path: &str) -> bool {
        let path = self.root.join(path);
        index_artifact(&path, &self.db.path)
            || path
                .ancestors()
                .any(|ancestor| global_artifact(ancestor, self.db.global_path()))
    }

    pub fn map(&self, options: &Value) -> Result<Vec<Value>> {
        let mut options = options.clone();
        options["limit"] = limits::resolve(&self.config, &options)?.value();
        crate::map::query(self, &self.root, &options)
    }

    pub(crate) fn map_limited(&self, options: &Value) -> Result<LimitedResults<Value>> {
        let mut options = options.clone();
        options["limit"] = limits::resolve(&self.config, &options)?.value();
        crate::map::query_limited(self, &self.root, &options)
    }

    /// Resolve name and description selectors against the current structural snapshot.
    /// SQLite's symbols and content-addressed embeddings are authoritative; the
    /// separate USearch vocabulary index is a disposable materialized view.
    pub(crate) fn selection(&self, options: &Value) -> Result<Selection> {
        let selection = Selection::compile(options)?;
        let queries = crate::filter::strings(options, "symbolQuery")?;
        if queries.is_empty() {
            return Ok(selection);
        }
        let vector = self.providers.symbol_vector()?;
        let profile = vector.profile();
        let queries: Vec<_> = queries
            .iter()
            .map(|query| symbols::normalize(query))
            .collect();
        let threshold = options["symbolThreshold"].as_f64().unwrap_or(0.5);
        // Map can resolve source or generated prose with the symbol provider,
        // even when the content indexes have never been prepared.
        let structures = self.symbol_structures()?;
        let mut descriptions = Vec::new();
        for (path, structure) in &structures {
            let stored = self.db.symbol_descriptions(path)?;
            for node in &structure.nodes {
                if let Some(text) = stored.get(&node.id).or(node.description.as_ref()) {
                    descriptions.push((path.clone(), Some(node.id), text.clone()));
                }
            }
            if let Some(text) = self
                .files
                .get(path)
                .and_then(|file| file.description.as_ref())
            {
                descriptions.push((path.clone(), None, text.clone()));
            }
        }
        descriptions.retain(|(_, _, text)| !symbols::normalize(text).is_empty());
        let cache_key = || -> Result<String> {
            Ok(hash(
                json!([
                    "symbol-selection-v2-descriptions",
                    self.db.meta("snapshot")?,
                    self.db.generation()?,
                    self.index_fingerprint("symbols"),
                    profile,
                    queries,
                    threshold,
                    descriptions
                ])
                .to_string(),
            ))
        };
        let key = cache_key()?;
        if let Some(cached) = self.db.search_cache(&key)? {
            let cached = cached
                .first()
                .context("Invalid cached semantic selection")?;
            return Ok(selection.with_semantic_matches(
                serde_json::from_value(cached["names"].clone())?,
                serde_json::from_value(cached["symbols"].clone())?,
                serde_json::from_value(cached["paths"].clone())?,
            ));
        }
        let symbols = self.symbol_index()?;
        self.ensure_embedding_groups_with(vector, vec![queries.clone()], true)?;
        self.ensure_embedding_groups_with(
            vector,
            vec![
                descriptions
                    .iter()
                    .map(|(_, _, text)| text.clone())
                    .collect(),
            ],
            false,
        )?;
        let description_vectors = descriptions
            .iter()
            .map(|(path, id, text)| {
                let embedding = self
                    .db
                    .embedding(&Database::embedding_key(&profile, false, text))?
                    .context("Missing selector description embedding")?;
                Ok((path, id, embedding))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut matched = HashSet::new();
        let mut matched_symbols = HashSet::new();
        let mut matched_paths = HashSet::new();
        for query in &queries {
            let query = self
                .db
                .embedding(&Database::embedding_key(&profile, true, query))?
                .context("Missing symbol query embedding")?;
            for (id, similarity) in
                symbols
                    .index
                    .search_filtered(&query, symbols.names.len(), |_| true)?
            {
                if similarity >= threshold {
                    matched.insert(symbols.names[&id].clone());
                }
            }
            for (path, id, description) in &description_vectors {
                if cosine(&query, description) >= threshold {
                    if let Some(id) = id {
                        matched_symbols.insert(((*path).clone(), *id));
                    } else {
                        matched_paths.insert((*path).clone());
                    }
                }
            }
        }
        if !self.readonly {
            self.db.put_search_cache(
                &cache_key()?,
                &[json!({"names":matched, "symbols":matched_symbols, "paths":matched_paths})],
            )?;
        }
        Ok(selection.with_semantic_matches(matched, matched_symbols, matched_paths))
    }

    fn symbol_structures(&self) -> Result<BTreeMap<String, Arc<parse::FileStructure>>> {
        self.db
            .paths()?
            .into_iter()
            .map(|path| {
                let structure = self.cached_structure(&path)?.with_context(|| {
                    format!("Structure for {path} requires refresh; run without --no-reindex")
                })?;
                Ok((path, structure))
            })
            .collect()
    }

    /// One vocabulary index per loaded snapshot: selectors and symbol results in
    /// the same command share it instead of reopening the shared base.
    fn symbol_index(&self) -> Result<Ref<'_, SymbolIndex>> {
        if self.symbol_index.borrow().is_none() {
            let index = self.open_symbol_index()?;
            *self.symbol_index.borrow_mut() = Some(index);
        }
        Ok(Ref::map(self.symbol_index.borrow(), |index| {
            index.as_ref().expect("initialized symbol index")
        }))
    }

    fn open_symbol_index(&self) -> Result<SymbolIndex> {
        let vector = self.providers.symbol_vector()?;
        let profile = vector.profile();
        let names: std::collections::BTreeSet<_> = self
            .symbol_structures()?
            .values()
            .flat_map(|structure| &structure.nodes)
            .flat_map(|node| std::iter::once(&node.name).chain(&node.names))
            .map(|name| symbols::normalize(name))
            .filter(|name| !name.is_empty())
            .collect();
        let names: Vec<_> = names.into_iter().collect();
        self.ensure_embedding_groups_with(vector, vec![names.clone()], false)?;
        // Name hashes keep unchanged graph entries stable when vocabulary order
        // changes. Reject collisions rather than silently dropping a name.
        let name_count = names.len();
        let names: BTreeMap<_, _> = names
            .into_iter()
            .map(|name| Ok((u64::from_str_radix(&hash(&name)[..16], 16)?, name)))
            .collect::<Result<_>>()?;
        ensure!(names.len() == name_count, "Symbol name hash collision");
        let vectors: Vec<_> = names
            .iter()
            .map(|(&id, name)| {
                let key = Database::embedding_key(&profile, false, name);
                Ok((
                    id,
                    key.clone(),
                    self.db
                        .embedding(&key)?
                        .context("Missing symbol embedding")?,
                ))
            })
            .collect::<Result<_>>()?;
        let mut path = self.db.path.as_os_str().to_os_string();
        path.push(".symbols.shared.json");
        let path = PathBuf::from(path);
        let mut profile = profile;
        profile["dimensions"] = json!(vector.dimensions());
        profile["indexChannel"] = json!("symbols");
        let index = SharedIndex::open(
            &self.global_indexes(),
            &path,
            &profile,
            &vectors,
            self.readonly,
        )?;
        if !self.readonly {
            self.db
                .set_meta("index_state_symbols", index.fingerprint())?;
        }
        Ok(SymbolIndex { names, index })
    }

    fn symbol_results(
        &self,
        query: &str,
        selection: &Selection,
        options: &Value,
    ) -> Result<Vec<Value>> {
        let query = symbols::normalize(query);
        ensure!(
            !query.is_empty(),
            "Symbol query must contain letters or numbers"
        );
        let symbols = self.symbol_index()?;
        let vector = self.providers.symbol_vector()?;
        let profile = vector.profile();
        self.ensure_embedding_groups_with(vector, vec![vec![query.clone()]], true)?;
        let query = self
            .db
            .embedding(&Database::embedding_key(&profile, true, &query))?
            .context("Missing symbol query embedding")?;
        let scores: HashMap<_, _> = symbols
            .index
            .search_filtered(&query, symbols.names.len(), |_| true)?
            .into_iter()
            .map(|(id, score)| (symbols.names[&id].clone(), score))
            .collect();
        let min = options["minSimilarity"].as_f64().unwrap_or(0.3);
        let max = options["maxSimilarity"].as_f64().unwrap_or(f64::INFINITY);
        let mut results = Vec::new();
        for (path, structure) in self.symbol_structures()? {
            if !selection.path_matches(&path) {
                continue;
            }
            let source = selection
                .has_regex()
                .then(|| self.source(&path))
                .transpose()?;
            for node in &structure.nodes {
                if !selection.kind_matches(&node.kind)
                    || !selection.symbol_matches_in(&path, node, source)
                {
                    continue;
                }
                let similarity = std::iter::once(&node.name)
                    .chain(&node.names)
                    .filter_map(|name| scores.get(&symbols::normalize(name)).copied())
                    .max_by(f64::total_cmp);
                if let Some(similarity) = similarity.filter(|score| *score >= min && *score < max) {
                    let mut symbol = serde_json::to_value(node)?;
                    symbol.as_object_mut().unwrap().remove("calls");
                    symbol["path"] = json!(path);
                    symbol["sourceMode"] = json!(self.files[&path].source_mode);
                    results.push(
                        json!({"type":"symbol", "symbol":symbol, "similarity":similarity,
                        "symbolSimilarity":similarity}),
                    );
                }
            }
        }
        Ok(results)
    }

    /// Search presentation uses the same indexed declarations as map. Older
    /// offline indexes can still return hits without current structure; callers
    /// can fall back to the search unit's declaration metadata in that case.
    pub fn presentation_structure(&self, path: &str) -> Result<Option<parse::FileStructure>> {
        Ok(self
            .cached_structure(path)?
            .map(|structure| (*structure).clone()))
    }

    /// Committed structure for one file, loaded at most once per snapshot load.
    fn cached_structure(&self, path: &str) -> Result<Option<Arc<parse::FileStructure>>> {
        if let Some(cached) = self.structures.borrow().get(path) {
            return Ok(cached.clone());
        }
        let structure = match self.files.get(path) {
            Some(file)
                if self
                    .db
                    .structure_current(path, &file.hash, STRUCTURE_PARSER_VERSION)? =>
            {
                Some(Arc::new(self.db.structure(path)?))
            }
            _ => None,
        };
        self.structures
            .borrow_mut()
            .insert(path.to_owned(), structure.clone());
        Ok(structure)
    }

    pub fn call_graph(&self) -> Result<crate::callgraph::CallGraph> {
        ensure!(
            self.db
                .missing_structure_paths(STRUCTURE_PARSER_VERSION)?
                .is_empty(),
            "Call graph requires current structure; run without --no-reindex"
        );
        let key = hash(
            json!([
                "callgraph-v2",
                STRUCTURE_PARSER_VERSION,
                self.db.meta("snapshot")?
            ])
            .to_string(),
        );
        if let Some(cached) = self.db.cache("callgraph", &key)? {
            return Ok(serde_json::from_str(&cached)?);
        }
        let graph = crate::callgraph::CallGraph::load(&self.db)?;
        if !self.readonly {
            self.db
                .cache_put("callgraph", &key, &serde_json::to_string(&graph)?)?;
        }
        Ok(graph)
    }

    pub fn presentation_source(&self, path: &str) -> Option<&str> {
        self.source(path).ok()
    }

    fn cached_line_totals(&self, path: &str) -> Result<crate::lines::Totals> {
        let file = self.files.get(path).context("Missing indexed file")?;
        let key = crate::lines::cache_key(
            &file.language,
            &file.hash,
            parse::line_exclusions::is_test_file(path),
        );
        if let Some(cached) = self.db.cache(crate::lines::CACHE_KIND, &key)? {
            return serde_json::from_str(&cached)
                .with_context(|| format!("Invalid cached line totals for {path}"));
        }
        if self.readonly {
            return Err(NeedsWrite.into());
        }
        let _lock = registry::artifact_lock(&self.db, crate::lines::CACHE_KIND, &key)?;
        if let Some(cached) = self.db.cache(crate::lines::CACHE_KIND, &key)? {
            return serde_json::from_str(&cached)
                .with_context(|| format!("Invalid cached line totals for {path}"));
        }
        let totals = crate::lines::Totals::compute(path, self.source(path)?)?;
        self.db.cache_put(
            crate::lines::CACHE_KIND,
            &key,
            &serde_json::to_string(&totals)?,
        )?;
        Ok(totals)
    }

    fn source(&self, path: &str) -> Result<&str> {
        let file = self.files.get(path).context("Missing indexed file")?;
        let source = self
            .sources
            .get(path)
            .context("Missing indexed source binding")?;
        if source.get().is_none() {
            let _ = source.set(self.db.source(&file.hash)?);
        }
        Ok(source.get().expect("initialized source").as_str())
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
        let key = hash(
            json!([
                STRUCTURE_PARSER_VERSION,
                parse::language_for_path(path),
                hash(source)
            ])
            .to_string(),
        );
        if let Some(cached) = self.db.cache("parse", &key)? {
            return Ok(serde_json::from_str(&cached)?);
        }
        let _lock = registry::artifact_lock(&self.db, "parse", &key)?;
        if let Some(cached) = self.db.cache("parse", &key)? {
            return Ok(serde_json::from_str(&cached)?);
        }
        let parsed = parse::parse(path, source)?;
        self.db
            .cache_put("parse", &key, &serde_json::to_string(&parsed)?)?;
        Ok(parsed)
    }

    fn prepare_all(&self, inputs: Vec<PrepareInput>) -> Result<Vec<(File, Vec<Item>)>> {
        let generate = inputs.iter().any(|input| input.generate_descriptions);
        let mut prepared = Vec::with_capacity(inputs.len());
        for input in inputs {
            let source_hash = hash(&input.source);
            let parsed = self.parsed(&input.path, &input.source)?;
            let previous = self.files.get(&input.path);
            let mut file = File {
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
            if let Some(description) = &parsed.description {
                file.description = Some(description.clone());
                file.description_hash = Some(format!("source:{}", hash(description)));
            } else if file
                .description_hash
                .as_deref()
                .is_some_and(|h| h.starts_with("source:"))
            {
                file.description = None;
                file.description_hash = None;
                file.description_embedding = None;
            }
            let source_description = parsed.description.is_some();
            prepared.push(PreparedFile {
                file,
                parsed,
                regenerate_file: input.regenerate_file && !source_description,
                regenerate_callables: input.regenerate_callables,
                callables: Vec::new(),
            });
        }

        // The first record wins, as a linear search over the loaded units would.
        let mut existing = HashMap::<&str, &Item>::new();
        if !prepared.is_empty() {
            existing.reserve(self.items.len());
            for item in &self.items {
                existing.entry(item.identity.as_str()).or_insert(item);
            }
        }
        for prepared in &mut prepared {
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
                let old = existing.get(identity.as_str()).copied();
                data["sourceDescription"] = json!(callable.description.is_some());
                let unchanged = old.is_some_and(|item| {
                    item.data["sourceHash"] == data["sourceHash"]
                        && item.data["description"].is_string()
                        && !flag(&item.data, "sourceDescription")
                }) && self
                    .files
                    .get(&prepared.file.path)
                    .is_some_and(|file| file.hash == prepared.file.hash);
                if callable.description.is_none() {
                    data["description"] = if unchanged {
                        old.unwrap().data["description"].clone()
                    } else {
                        Value::Null
                    };
                }
                let generate_description = generate
                    && callable.description.is_none()
                    && (prepared.regenerate_callables || !unchanged);
                prepared.callables.push(PreparedCallable {
                    identity,
                    data,
                    generate_description,
                });
            }
        }
        if generate {
            self.prepare_descriptions(&mut prepared)?;
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
            inputs.extend(prepared.file.description.clone());
            inputs.extend(
                prepared.callables.iter().filter_map(|callable| {
                    callable.data["description"].as_str().map(str::to_owned)
                }),
            );
            inputs.extend(
                prepared
                    .parsed
                    .structure
                    .nodes
                    .iter()
                    .filter_map(|node| node.description.clone()),
            );
            embedding_groups.push(inputs);
        }
        self.ensure_embedding_groups(embedding_groups, false)?;

        let vector_profile = self.providers.vector().profile();
        let mut result = Vec::with_capacity(prepared.len());
        for mut prepared in prepared {
            if let Some(description) = &prepared.file.description {
                prepared.file.description_embedding =
                    Some(Database::embedding_key(&vector_profile, false, description));
            }
            let mut items = crate::storage::parsed_items(&prepared.file, &prepared.parsed)?;
            let mut callables = HashMap::<&str, &PreparedCallable>::new();
            for callable in &prepared.callables {
                callables
                    .entry(callable.identity.as_str())
                    .or_insert(callable);
            }
            for item in &mut items {
                if let Some(callable) = callables.get(item.identity.as_str()) {
                    item.data = callable.data.clone();
                }
                if let Some(input) = item.data["embeddingInput"].as_str() {
                    item.embedding = Database::embedding_key(&vector_profile, false, input);
                }
                item.description_embedding = item.data["description"]
                    .as_str()
                    .map(|text| Database::embedding_key(&vector_profile, false, text));
            }
            result.push((prepared.file, items));
        }
        Ok(result)
    }

    fn prepare_descriptions(&self, prepared: &mut [PreparedFile]) -> Result<()> {
        let parallelism = self.parallelism()?;
        for files in prepared.chunks_mut(parallelism) {
            self.prepare_description_window(files, parallelism)?;
        }
        Ok(())
    }

    fn prepare_description_window(
        &self,
        prepared: &mut [PreparedFile],
        parallelism: usize,
    ) -> Result<()> {
        let mut conversations: Vec<_> = prepared.iter().map(|prepared| {
            let prompt = format!(
                "Describe the purpose, responsibilities, and important relationships of this existing source file in one paragraph. Do not propose changes.\nFile: {}\n\n{}",
                prepared.file.path, prepared.file.source
            );
            let file_ready = prepared.file.description.is_some() && !prepared.regenerate_file;
            let mut messages = vec![Message::user(prompt)];
            if file_ready {
                messages.push(Message::assistant(prepared.file.description.clone().unwrap()));
            }
            FileConversation {
                messages,
                session: crate::providers::session_id(),
                file_ready,
                next_callable: 0,
            }
        }).collect();
        let llm = self.providers.llm();
        loop {
            // Only the next turn of each file can run. Completing a turn makes
            // its exact request and response the prefix of the following turn.
            let mut jobs = BTreeMap::<String, Vec<DescriptionJob>>::new();
            for (file_index, (prepared, conversation)) in
                prepared.iter().zip(&mut conversations).enumerate()
            {
                if FormatGroup::for_path(&prepared.file.path) == Some(FormatGroup::Docs) {
                    continue;
                }
                let (callable_index, symbol, source_hash, regenerate, messages) = if !conversation
                    .file_ready
                {
                    (
                        None,
                        None,
                        prepared.file.hash.clone(),
                        prepared.regenerate_file,
                        conversation.messages.clone(),
                    )
                } else {
                    while conversation.next_callable < prepared.callables.len()
                        && !prepared.callables[conversation.next_callable].generate_description
                    {
                        conversation.next_callable += 1;
                    }
                    let index = conversation.next_callable;
                    let Some(callable) = prepared.parsed.callables.get(index) else {
                        continue;
                    };
                    let prompt = format!(
                        "Describe what this existing callable does in one sentence, covering relevant inputs, outputs and side effects. Use the source file already provided.\nSymbol: {}\nLines: {}-{}",
                        callable.qualified_name, callable.start_line, callable.end_line
                    );
                    let mut messages = conversation.messages.clone();
                    messages.push(Message::user(prompt));
                    (
                        Some(index),
                        Some(callable.qualified_name.clone()),
                        callable.source_hash.clone(),
                        prepared.regenerate_callables,
                        messages,
                    )
                };
                let generation = DescriptionGeneration {
                    scope: if callable_index.is_some() {
                        "callable"
                    } else {
                        "file"
                    }
                    .into(),
                    path: prepared.file.path.clone(),
                    symbol,
                    source_hash,
                    file_hash: prepared.file.hash.clone(),
                    file_description: if callable_index.is_some() {
                        prepared.file.description.clone()
                    } else {
                        None
                    },
                    profile: llm.profile(),
                    settings: description_settings(&self.config),
                    system: DESCRIPTION_SYSTEM.into(),
                    prompt: messages.last().unwrap().content.clone(),
                    messages,
                    regenerate,
                };
                let key = hash(
                    json!([
                        "description-request-v1",
                        generation.profile,
                        generation.settings,
                        generation.system,
                        generation
                            .messages
                            .iter()
                            .map(|message| (&message.role, &message.content))
                            .collect::<Vec<_>>()
                    ])
                    .to_string(),
                );
                jobs.entry(key.clone()).or_default().push(DescriptionJob {
                    file_index,
                    callable_index,
                    key,
                    generation,
                    session: conversation.session.clone(),
                });
            }
            if jobs.is_empty() {
                return Ok(());
            }
            self.artifacts
                .hydrate_descriptions(&self.db, &jobs.keys().cloned().collect::<Vec<_>>())?;
            let mut pending = Vec::new();
            for (_, jobs) in jobs {
                if let Some(cached) = self.db.cache("description", &jobs[0].key)? {
                    let text = DescriptionArtifact::decode(&cached)?.text;
                    for job in jobs {
                        apply_description(prepared, &mut conversations, &job, text.clone());
                    }
                } else {
                    pending.push(jobs);
                }
            }
            let global_path = self.db.global_path();
            run_jobs(
                "Generating descriptions",
                &pending,
                parallelism,
                |_| 1,
                |jobs| {
                    let job = &jobs[0];
                    let lock = registry::lock_path(global_path, "description", &job.key)?;
                    let conn = crate::cache::open_store(global_path, true)?;
                    let cached: Option<String> = conn
                        .query_row(
                            "SELECT value FROM cache WHERE kind='description' AND key=?",
                            [&job.key],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if let Some(cached) = cached {
                        return Ok((DescriptionArtifact::decode(&cached)?.text, lock));
                    }
                    let text = llm.describe_conversation(
                        DESCRIPTION_SYSTEM,
                        &job.generation.messages,
                        &job.session,
                    )?;
                    Ok((text, lock))
                },
                |jobs, (text, _lock)| {
                    let job = &jobs[0];
                    let (artifact, contents) =
                        DescriptionArtifact::new(text.clone(), &job.generation)?;
                    let encoded = serde_json::to_string(&artifact)?;
                    self.db
                        .put_description_artifact(&job.key, &encoded, &contents)?;
                    self.artifacts
                        .put_description(&self.db, &job.key, &encoded, &contents);
                    for job in jobs {
                        apply_description(prepared, &mut conversations, job, text.clone());
                    }
                    Ok(())
                },
            )?;
        }
    }

    fn ensure_embeddings(&self, inputs: &[String], query: bool) -> Result<()> {
        self.ensure_embedding_groups(vec![inputs.to_vec()], query)
    }

    fn ensure_embedding_groups(&self, groups: Vec<Vec<String>>, query: bool) -> Result<()> {
        self.ensure_embedding_groups_with(self.providers.vector(), groups, query)
    }

    fn ensure_embedding_groups_with(
        &self,
        vector: &dyn Vector,
        groups: Vec<Vec<String>>,
        query: bool,
    ) -> Result<()> {
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
        if self.readonly {
            for (key, _) in &keys {
                if self.db.embedding(key)?.is_none() {
                    return Err(NeedsWrite.into());
                }
            }
            return Ok(());
        }
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
        let global_path = self.db.global_path();
        run_jobs(
            "Generating embeddings",
            &batches,
            self.embedding_parallelism()?,
            |entries| entries.len(),
            |entries| {
                // Acquire in key order, then recheck: concurrent workspaces can
                // discover the same missing paid inputs before either publishes.
                let locks = registry::lock_paths(
                    global_path,
                    "embedding",
                    entries.iter().map(|(key, _)| key.as_str()),
                )?;
                let conn = crate::cache::open_store(global_path, true)?;
                let mut found = BTreeMap::new();
                let mut missing = Vec::new();
                for (key, text) in entries {
                    let bytes: Option<Vec<u8>> = conn
                        .query_row("SELECT vector FROM embeddings WHERE key=?", [key], |row| {
                            row.get(0)
                        })
                        .optional()?;
                    match bytes {
                        Some(bytes) => {
                            found.insert(key.clone(), crate::storage::decode(&bytes)?);
                        }
                        None => missing.push((key, text)),
                    }
                }
                if !missing.is_empty() {
                    let texts: Vec<_> = missing.iter().map(|(_, text)| (*text).clone()).collect();
                    let generated = vector.embed(&texts, query)?;
                    ensure!(
                        generated.len() == missing.len(),
                        "Provider returned the wrong number of embeddings"
                    );
                    for ((key, _), vector) in missing.into_iter().zip(generated) {
                        found.insert(key.clone(), vector);
                    }
                }
                let vectors: Vec<_> = entries
                    .iter()
                    .map(|(key, _)| found.remove(key).expect("resolved embedding"))
                    .collect();
                Ok((vectors, locks))
            },
            |entries, (vectors, _locks)| {
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

    fn embedding_parallelism(&self) -> Result<usize> {
        let default = if self.config["provider"] == "ollama"
            || self.config["embeddingProvider"] == "ollama"
        {
            1
        } else {
            10
        };
        let value = self
            .config
            .get("embeddingParallelism")
            .or_else(|| self.config.get("parallelism"))
            .and_then(Value::as_u64)
            .unwrap_or(default) as usize;
        ensure!(value > 0, "embeddingParallelism must be positive");
        Ok(value)
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
            self.db.file_records()?
        } else {
            let mut files = self.db.file_records()?;
            for file in &mut files {
                file.description_embedding = file
                    .description
                    .as_deref()
                    .map(|input| {
                        let key = Database::embedding_key(&profile, false, input);
                        self.db
                            .embedding_exists(&key)
                            .map(|exists| exists.then_some(key))
                    })
                    .transpose()?
                    .flatten();
            }
            files
        };
        self.files = files.into_iter().map(|f| (f.path.clone(), f)).collect();
        self.sources = self
            .files
            .keys()
            .map(|path| (path.clone(), OnceCell::new()))
            .collect();
        self.vectors.get_mut().clear();
        self.indexes.get_mut().clear();
        self.structures.get_mut().clear();
        *self.symbol_index.get_mut() = None;
        for item in &mut self.items {
            item.data["id"] = json!(item.id);
        }
        Ok(())
    }

    fn vector(&self, key: &str) -> Result<Vec<f32>> {
        if let Some(vector) = self.vectors.borrow().get(key) {
            return Ok(vector.clone());
        }
        let vector = self
            .db
            .embedding(key)?
            .context("Missing indexed embedding")?;
        self.vectors.borrow_mut().insert(key.into(), vector.clone());
        Ok(vector)
    }

    fn vectors(&self, matches: impl Fn(&str) -> bool) -> Result<HashMap<String, Vec<f32>>> {
        let keys: HashSet<_> = self
            .items
            .iter()
            .filter(|item| matches(&item.path))
            .flat_map(|i| {
                std::iter::once(i.embedding.clone()).chain(i.description_embedding.clone())
            })
            .chain(
                self.files
                    .values()
                    .filter(|file| matches(&file.path))
                    .filter_map(|f| f.description_embedding.clone()),
            )
            .filter(|key| !key.is_empty())
            .collect();
        let loading = ui::counted("Loading embeddings", keys.len());
        let mut vectors = HashMap::new();
        for key in keys {
            vectors.insert(key.clone(), self.vector(&key)?);
            loading.inc(1);
        }
        loading.finish();
        Ok(vectors)
    }

    fn ensure_index(&self, kind: &str) -> Result<()> {
        if self.indexes.borrow().contains_key(kind) {
            return Ok(());
        }
        let items: Vec<_> = self
            .items
            .iter()
            .filter(|i| {
                if kind == "descriptions" {
                    i.data["description"].is_string() && i.description_embedding.is_some()
                } else {
                    content_group(i).is_some_and(|group| group.as_str() == kind)
                        && !i.embedding.is_empty()
                }
            })
            .collect();
        let preparing = ui::counted(format_args!("Preparing {kind} vectors"), items.len());
        let mut vectors = Vec::with_capacity(items.len());
        for item in items {
            let key = if kind == "descriptions" {
                item.description_embedding
                    .as_ref()
                    .context("Missing description")?
            } else {
                &item.embedding
            };
            vectors.push((item.id, key.clone(), self.item_vector(item, kind)?));
            preparing.inc(1);
        }
        preparing.finish();
        let mut path = self.db.path.as_os_str().to_os_string();
        path.push(format!(".{kind}.shared.json"));
        ui::progress(format_args!("Loading {kind} vector index"));
        let mut profile = self.providers.vector().profile();
        profile["dimensions"] = json!(self.providers.vector().dimensions());
        profile["indexChannel"] = json!(if kind == "descriptions" {
            "descriptions"
        } else {
            "content"
        });
        if let Ok(group) = FormatGroup::from_name(kind) {
            profile["formatGroup"] = json!(group);
        }
        let index = SharedIndex::open(
            &self.global_indexes(),
            &PathBuf::from(path),
            &profile,
            &vectors,
            self.readonly,
        )?;
        if !self.readonly {
            self.db
                .set_meta(&format!("index_state_{kind}"), index.fingerprint())?;
        }
        self.indexes.borrow_mut().insert(kind.into(), index);
        Ok(())
    }

    fn global_indexes(&self) -> PathBuf {
        let mut path = self.db.global_path().as_os_str().to_os_string();
        path.push(".indexes");
        PathBuf::from(path)
    }

    fn item_vector(&self, item: &Item, kind: &str) -> Result<Vec<f32>> {
        let key = if kind == "descriptions" {
            item.description_embedding
                .as_ref()
                .context("Missing description")?
        } else {
            &item.embedding
        };
        self.vector(key)
    }

    pub fn search(&self, query: &str, kind: &str, options: &Value) -> Result<Vec<Value>> {
        Ok(self.search_limited(query, kind, options)?.rows)
    }

    /// Search the union of queries, retaining each match's best-ranked result.
    pub fn search_queries(
        &self,
        queries: &[String],
        kind: &str,
        options: &Value,
    ) -> Result<Vec<Value>> {
        Ok(self.search_queries_limited(queries, kind, options)?.rows)
    }

    pub(crate) fn search_queries_limited(
        &self,
        queries: &[String],
        kind: &str,
        options: &Value,
    ) -> Result<LimitedResults<Value>> {
        let queries: BTreeSet<_> = queries.iter().collect();
        if queries.len() == 1 {
            return self.search_limited(queries.first().unwrap(), kind, options);
        }
        let limit = limits::resolve(&self.config, options)?;
        let ranking_score = if flag(&self.config, "rerankingEnabled") {
            "rerankScore"
        } else {
            "similarity"
        };
        let order = |a: &Value, b: &Value| {
            score(b, ranking_score)
                .total_cmp(&score(a, ranking_score))
                .then_with(|| a.to_string().cmp(&b.to_string()))
        };
        // Scores belong to the query, not the match. Include full item data so
        // names shared by locations or streams stay distinct.
        let identity = |row: &Value| {
            json!([
                row["type"],
                row["function"],
                row["symbol"],
                row["chunk"],
                row["file"]
            ])
            .to_string()
        };
        let mut matches: BTreeMap<String, Value> = BTreeMap::new();
        let mut capped = false;
        for query in queries {
            let mut query_options = options.clone();
            query_options["limit"] = limit.value();
            let rows = loop {
                // A query's best count + 1 distinct matches suffice for both the
                // global top count and omission detection, even if queries overlap.
                // Keep using search_rows so individual queries reuse their caches.
                let mut rows = self.search_rows(query, kind, &query_options)?;
                capped |= take_omission_marker(&mut rows);
                let Some(count) = limit.count() else {
                    break rows;
                };
                let probe = query_options["limit"].as_u64().unwrap() as usize;
                let identities: HashSet<_> = rows.iter().map(&identity).collect();
                if identities.len() > count || rows.len() <= probe {
                    break rows;
                }
                // Duplicate streams can consume the spare candidate. Widen only
                // in that case, rather than misreporting an exactly full union.
                let wider = probe.saturating_mul(2);
                if wider == probe {
                    break rows;
                }
                query_options["limit"] = json!(wider);
            };
            for row in rows {
                matches
                    .entry(identity(&row))
                    .and_modify(|existing| {
                        if order(&row, existing).is_lt() {
                            *existing = row.clone();
                        }
                    })
                    .or_insert(row);
            }
        }
        let mut rows: Vec<_> = matches.into_values().collect();
        rows.sort_by(order);
        let omitted = limit
            .count()
            .is_some_and(|count| capped || rows.len() > count);
        if let Some(count) = limit.count() {
            rows.truncate(count);
        }
        Ok(LimitedResults {
            count: rows.len(),
            rows,
            omitted,
            limit: limit.count(),
        })
    }

    pub(crate) fn search_limited(
        &self,
        query: &str,
        kind: &str,
        options: &Value,
    ) -> Result<LimitedResults<Value>> {
        let mut options = options.clone();
        let limit = limits::resolve(&self.config, &options)?;
        options["limit"] = limit.value();
        let mut rows = self.search_rows(query, kind, &options)?;
        let capped = take_omission_marker(&mut rows);
        let omitted = limit
            .count()
            .is_some_and(|count| capped || rows.len() > count);
        if let Some(count) = limit.count() {
            rows.truncate(count);
        }
        Ok(LimitedResults {
            count: rows.len(),
            rows,
            omitted,
            limit: limit.count(),
        })
    }

    /// Ranked rows plus at most one probe beyond the output limit. When a
    /// reranker's candidate cap hides further candidates that the row count
    /// cannot reveal, the last row is an omission marker instead; callers must
    /// remove it with `take_omission_marker`.
    fn search_rows(&self, query: &str, kind: &str, options: &Value) -> Result<Vec<Value>> {
        let formats = Selection::compile(options)?;
        let SearchPlan {
            groups,
            descriptions,
            symbols,
        } = SearchPlan::compile(kind, options, &formats);
        let content = !groups.is_empty() || descriptions;
        if !content && !symbols {
            return Ok(Vec::new());
        }
        ensure!(
            (!content || !self.structural_only)
                && self.items.iter().all(|item| {
                    content_group(item).is_none_or(|group| !groups.contains(&group))
                        || !item.embedding.is_empty()
                })
                && (!descriptions
                    || (self
                        .items
                        .iter()
                        .filter(|item| formats.format_matches(&item.path))
                        .all(|item| !item.data["description"].is_string()
                            || item.description_embedding.is_some())
                        && self
                            .files
                            .values()
                            .filter(|file| formats.format_matches(&file.path))
                            .all(|file| file.description.is_none()
                                || file.description_embedding.is_some()))),
            "Semantic index is incomplete for the configured profiles; run without --no-reindex to prepare it"
        );
        let key = self.query_key(query, kind, options)?;
        if let Some(results) = self.db.search_cache(&key)? {
            return Ok(results);
        }
        if self.readonly {
            return Err(NeedsWrite.into());
        }
        let selection = self.selection(options)?;
        let vector = if content {
            let embedding_key = self.embed_one(query, true)?;
            self.db
                .embedding(&embedding_key)?
                .context("Missing query vector")?
        } else {
            Vec::new()
        };
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
            // Retrieve one extra threshold-passing hit to distinguish actual
            // omission from exactly filling the requested output limit.
            limit.map(|count| count.saturating_add(1))
        };
        // A reranker only ever sees `candidate_limit` documents. Retrieve one
        // more, never sent to it, to learn whether that cap hid any candidates.
        let fetch_limit = if rerank {
            candidate_limit.map(|count| count.saturating_add(1))
        } else {
            candidate_limit
        };
        let mut results = Vec::new();
        let searching = ui::counted(
            "Searching vector indexes",
            groups.len() + usize::from(descriptions) + usize::from(symbols),
        );
        let mut item_results: HashMap<u64, Value> = HashMap::new();
        for index_kind in groups
            .iter()
            .map(|group| group.as_str())
            .chain(descriptions.then_some("descriptions"))
        {
            ui::progress(format_args!("Searching {index_kind} index"));
            let mut allowed = HashSet::new();
            for i in &self.items {
                let in_channel = if index_kind == "descriptions" {
                    i.data["description"].is_string() && i.description_embedding.is_some()
                } else {
                    content_group(i).is_some_and(|group| group.as_str() == index_kind)
                };
                // Structure is only consulted for units whose path is selected.
                if in_channel
                    && selection.path_matches(&i.path)
                    && selection.unit_matches(
                        &i.path,
                        &i.data,
                        self.cached_structure(&i.path)?.as_deref(),
                        (selection.has_regex()
                            && matches!(i.kind.as_str(), "function" | "symbol-description"))
                        .then(|| self.source(&i.path))
                        .transpose()?,
                    )
                {
                    allowed.insert(i.id);
                }
            }
            for (id, similarity) in
                self.neighbors(index_kind, &vector, &allowed, fetch_limit, options)?
            {
                let item = self.item(id)?;
                let mut result = if item.kind == "symbol-description" {
                    let mut symbol = item.data.clone();
                    symbol.as_object_mut().unwrap().remove("calls");
                    json!({"type":"symbol","symbol":symbol,"similarity":similarity})
                } else if item.kind == "function" {
                    json!({"type":"function","function":item.data,"similarity":similarity})
                } else {
                    json!({"type":item.kind,"chunk":item.data,"similarity":similarity})
                };
                if matches!(item.kind.as_str(), "function" | "symbol-description") {
                    self.add_scores(&mut result, item, &vector, None)?;
                }
                item_results
                    .entry(id)
                    .and_modify(|existing| {
                        if similarity > score(existing, "similarity") {
                            *existing = result.clone();
                        }
                    })
                    .or_insert(result);
            }
            searching.inc(1);
        }
        results.extend(item_results.into_values());
        if descriptions {
            // Files are independent description hits, including files with no
            // callable units. Direct comparison avoids synthetic item IDs.
            let min = options["minSimilarity"].as_f64().unwrap_or(0.3);
            let max = options["maxSimilarity"].as_f64().unwrap_or(f64::INFINITY);
            for file in self
                .files
                .values()
                .filter(|file| selection.file_matches(&file.path))
            {
                let Some(key) = file.description_embedding.as_ref() else {
                    continue;
                };
                let similarity = cosine(&vector, &self.vector(key)?);
                if similarity >= min && similarity < max {
                    results.push(json!({"type":"file", "file":{
                        "path":file.path, "description":file.description,
                        "sourceMode":file.source_mode, "language":file.language
                    }, "similarity":similarity, "fileDescriptionSimilarity":similarity}));
                }
            }
        }
        if symbols {
            ui::progress("Searching symbol index");
            results.extend(self.symbol_results(query, &selection, options)?);
            searching.inc(1);
        }
        searching.finish();
        results.sort_by(|a, b| {
            score(b, "similarity")
                .total_cmp(&score(a, "similarity"))
                .then_with(|| a.to_string().cmp(&b.to_string()))
        });
        let mut capped = false;
        if let Some(limit) = candidate_limit {
            capped = rerank && results.len() > limit;
            results.truncate(limit);
        }
        if rerank && !results.is_empty() {
            let documents: Vec<_> = results
                .iter()
                .map(|row| {
                    if row["type"] == "symbol" {
                        json!({"name": row["symbol"]["name"], "names": row["symbol"]["names"],
                            "description": row["symbol"]["description"]})
                        .to_string()
                    } else {
                        row.to_string()
                    }
                })
                .collect();
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
            let _lock = registry::artifact_lock(&self.db, "rerank", &ranking_key)?;
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
            results.truncate(limit.saturating_add(1));
            // Rows beyond the limit already prove omission. Otherwise record
            // that candidates beyond the reranker's cap were never ranked.
            if capped && results.len() <= limit {
                results.push(json!({"type": OMISSION_MARKER}));
            }
        }
        self.db
            .put_search_cache(&self.query_key(query, kind, options)?, &results)?;
        Ok(results)
    }

    fn query_key(&self, query: &str, kind: &str, options: &Value) -> Result<String> {
        let mut config = self.config.clone();
        if let Some(config) = config.as_object_mut() {
            config.remove("noReindex");
            config.remove("forceReindex");
        }
        let formats = Selection::compile(options)?;
        let plan = SearchPlan::compile(kind, options, &formats);
        let selector_symbols =
            !plan.symbols && !crate::filter::strings(options, "symbolQuery")?.is_empty();
        let roles: Vec<_> = plan
            .roles()
            .chain(selector_symbols.then_some("symbols"))
            .map(|role| (role, self.index_fingerprint(role)))
            .collect();
        Ok(hash(
            json!([
                "query-v10-callable-source-regex",
                crate::formats::VERSION,
                self.db.meta("snapshot")?,
                self.db.generation()?,
                config,
                self.providers.vector().profile(),
                kind,
                query,
                options,
                roles
            ])
            .to_string(),
        ))
    }

    fn index_fingerprint(&self, role: &str) -> Option<String> {
        // The authoritative snapshot pins its plan independently of disposable
        // pointer/base files. A cached answer needs neither loading nor repair.
        self.db.meta(&format!("index_state_{role}")).ok().flatten()
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
        self.ensure_index(kind)?;
        let indexes = self.indexes.borrow();
        let index = indexes.get(kind).context("Search index unavailable")?;
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
        other: Option<DescriptionQueries<'_>>,
    ) -> Result<()> {
        if !item.embedding.is_empty() {
            result["codeSimilarity"] = json!(cosine(code, &self.vector(&item.embedding)?));
        }
        let (description, file) = other.unwrap_or((Some(code), Some(code)));
        if let Some((query, key)) = description.zip(item.description_embedding.as_ref()) {
            let similarity = cosine(query, &self.vector(key)?);
            result["descriptionSimilarity"] = json!(similarity);
            if item.kind == "function" {
                result["functionDescriptionSimilarity"] = json!(similarity);
            }
        }
        if let Some((query, key)) = file.zip(
            self.files
                .get(&item.path)
                .and_then(|file| file.description_embedding.as_ref()),
        ) {
            result["fileDescriptionSimilarity"] = json!(cosine(query, &self.vector(key)?));
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
        if !Selection::compile(options)?.includes_group(FormatGroup::Code) {
            return Ok(Vec::new());
        }
        let mut exclusions = cross_search_exclusions(&self.config)?;
        exclusions.extend(cross_search_exclusions(&target.config)?);
        ensure!(
            !self.structural_only
                && !target.structural_only
                && [self, target]
                    .iter()
                    .all(|engine| engine.items.iter().all(|item| {
                        content_group(item) != Some(FormatGroup::Code) || !item.embedding.is_empty()
                    })),
            "Semantic index is incomplete for the configured profiles; run without --no-reindex to prepare it"
        );
        ensure!(
            self.providers.vector().profile() == target.providers.vector().profile(),
            "Cross-search requires identical embedding profiles"
        );
        let same = self.db.path.canonicalize()? == target.db.path.canonicalize()?;
        let kind = "code";
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
        let cache_key = || -> Result<String> {
            Ok(hash(
                json!([
                    "cross-v8-callable-source-regex",
                    crate::formats::VERSION,
                    self.db.meta("snapshot")?,
                    self.db.generation()?,
                    target.db.path.canonicalize()?,
                    target.db.meta("incarnation")?,
                    target.db.meta("snapshot")?,
                    target.db.generation()?,
                    target.index_fingerprint("code"),
                    self.providers.vector().profile(),
                    kind,
                    base,
                    self.db.meta("checkpoint")?,
                    exclusions,
                    options
                ])
                .to_string(),
            ))
        };
        let key = cache_key()?;
        if let Some(results) = self.db.search_cache(&key)? {
            return Ok(results);
        }
        let selection = self.selection(options)?;
        let stored_vectors =
            self.vectors(|path| FormatGroup::for_path(path) == Some(FormatGroup::Code))?;
        let min_lines = options["minLines"].as_u64().unwrap_or(2);
        let max_lines = options["maxLines"].as_u64();
        let lines_match = |item: &Item| {
            let lines = item.data["lineCount"].as_u64().unwrap_or(0);
            lines >= min_lines && max_lines.is_none_or(|max| lines < max)
        };
        let excluded = |item: &Item| {
            item.data["sourceHash"]
                .as_str()
                .is_some_and(|hash| exclusions.contains(hash))
        };
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
            .filter(|i| {
                content_group(i) == Some(FormatGroup::Code)
                    && lines_match(i)
                    && !excluded(i)
                    && selection.format_matches(&i.path)
            })
            .map(|i| (i.id, target.root.join(&i.path)))
            .collect();
        let mut sources = Vec::new();
        for source in &self.items {
            // Structure is only consulted for units whose path is selected.
            if content_group(source) == Some(FormatGroup::Code)
                && lines_match(source)
                && !excluded(source)
                && selection.path_matches(&source.path)
                && selection.unit_matches(
                    &source.path,
                    &source.data,
                    self.cached_structure(&source.path)?.as_deref(),
                    selection
                        .has_regex()
                        .then(|| self.source(&source.path))
                        .transpose()?,
                )
                && source_path.as_ref().is_none_or(|p| under(&source.path, p))
                && (!flag(options, "uncommitted") || source.data["sourceMode"] == "working-tree")
                && changed.as_ref().is_none_or(|ids| ids.contains(&source.id))
            {
                sources.push(source);
            }
        }
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
                let other = Some((
                    source
                        .description_embedding
                        .as_ref()
                        .and_then(|key| stored_vectors.get(key))
                        .map(Vec::as_slice),
                    self.files
                        .get(&source.path)
                        .and_then(|file| file.description_embedding.as_ref())
                        .and_then(|key| stored_vectors.get(key))
                        .map(Vec::as_slice),
                ));
                target.add_scores(&mut row, item, &stored_vectors[&source.embedding], other)?;
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
                results.push(json!({"source":source.data,"matches":matches,"scoring":{
                    "similarityMode":"code", "similarityWeights":{"code":1,"description":0,"fileDescription":0}
                }}));
            }
            searching.inc(1);
        }
        searching.finish();
        self.db.put_search_cache(&cache_key()?, &results)?;
        Ok(results)
    }

    pub fn describe(&self, query: &str, options: &Value) -> Result<Value> {
        let expand_callers = options["expandCallers"].as_u64().unwrap_or(2) as usize;
        let expand_callees = options["expandCallees"].as_u64().unwrap_or(2) as usize;
        let callers = (options["callers"].as_u64().unwrap_or(0) as usize).max(expand_callers);
        let callees = (options["callees"].as_u64().unwrap_or(0) as usize).max(expand_callees);
        let refs = crate::map::reference_mode(options, callers, callees)?;
        let matches = self.search(query, "search", options)?;
        let mut files = BTreeMap::<String, Value>::new();
        for item in &matches {
            let data = match item["type"].as_str() {
                Some("markdown" | "document") => &item["chunk"],
                Some("symbol") => &item["symbol"],
                Some("file") => &item["file"],
                _ => &item["function"],
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
            (callers, callees, expand_callers, expand_callees),
            refs,
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
        let _lock = registry::artifact_lock(&self.db, "explanation", &key)?;
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

    fn description_generation_profile(&self) -> Value {
        json!([
            self.providers.llm().profile(),
            description_settings(&self.config),
            DESCRIPTION_SYSTEM
        ])
    }

    pub fn generate_descriptions(&mut self) -> Result<Value> {
        ensure!(
            !self.readonly && !self.structural_only,
            "Description generation requires a writable search engine"
        );
        let profile = self.description_generation_profile().to_string();
        let settings_changed =
            self.db.meta("description_generation_profile")?.as_deref() != Some(&profile);
        let needs_callables: HashSet<&str> = self
            .items
            .iter()
            .filter(|item| {
                item.kind == "function"
                    && !flag(&item.data, "sourceDescription")
                    && (settings_changed || !item.data["description"].is_string())
            })
            .map(|item| item.path.as_str())
            .collect();
        let mut inputs = Vec::new();
        for file in self.files.values() {
            // A file that could not be read has no source to describe. Parse
            // diagnostics only mark damaged regions: its other callables are fine.
            if FormatGroup::for_path(&file.path) == Some(FormatGroup::Docs) || has_read_error(file)
            {
                continue;
            }
            let source_description = file
                .description_hash
                .as_deref()
                .is_some_and(|h| h.starts_with("source:"));
            let regenerate_file = !source_description
                && (settings_changed || file.description_hash.as_deref() != Some(&file.hash));
            let needs_callable = needs_callables.contains(file.path.as_str());
            if regenerate_file || needs_callable {
                inputs.push(PrepareInput {
                    path: file.path.clone(),
                    source: self.source(&file.path)?.to_owned(),
                    source_mode: file.source_mode.clone(),
                    regenerate_file,
                    regenerate_callables: settings_changed,
                    generate_descriptions: true,
                });
            }
        }
        inputs.sort_by(|a, b| a.path.cmp(&b.path));
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
        self.db
            .set_meta("description_generation_profile", &profile)?;
        self.db.set_meta(
            "description_profile",
            &self.providers.llm().profile().to_string(),
        )?;
        self.load()?;
        self.artifacts.backfill_remote(&self.db);
        Ok(
            json!({"filesPrepared":changed.len(),"descriptionCount":self.items.iter().filter(|i|i.data["description"].is_string()).count(),"fileDescriptionCount":self.files.values().filter(|f|f.description.is_some()).count()}),
        )
    }

    pub fn status(&self) -> Result<Value> {
        let errors = self.errors()?;
        let mut status = json!({"rootDir":self.root,"indexPath":self.db.path,"generation":self.db.generation()?,"gitCheckpoint":self.db.meta("checkpoint")?,"fileCount":self.files.len(),"functionCount":self.items.iter().filter(|i|i.kind=="function").count(),"markdownChunkCount":self.items.iter().filter(|i|i.kind=="markdown").count(),"embeddingProfile":self.providers.vector().profile(),"descriptionProfile":self.providers.llm().profile(),"descriptionCount":self.items.iter().filter(|i|i.description_embedding.is_some()).count(),"fileDescriptionCount":self.files.values().filter(|f|f.description.is_some()).count(),"staleFileDescriptionCount":self.files.values().filter(|f|f.description.is_some() && f.description_hash.as_deref()!=Some(&f.hash) && !f.description_hash.as_deref().is_some_and(|h|h.starts_with("source:"))).count(),"indexingErrorCount":errors.len(),"failedFileCount":self.files.values().filter(|f|!f.errors.is_empty()).count(),"vectorBackend":"usearch","storageBackend":"sqlite"});
        status["globalStorePath"] = json!(self.db.global_path());
        status["snapshot"] = json!(self.db.meta("snapshot")?);
        status["repositoryIdentity"] = self
            .db
            .meta("repository_identity")?
            .map(|identity| serde_json::from_str(&identity))
            .transpose()?
            .unwrap_or(Value::Null);
        Ok(status)
    }

    pub fn errors(&self) -> Result<Vec<Value>> {
        Ok(self.files.values().flat_map(|f| f.errors.clone()).collect())
    }

    fn normalize_path(&self, path: &str) -> Result<String> {
        normalize_path(&self.root, path)
    }
}

impl crate::map::StructureSource for Engine {
    fn selection(&self, options: &Value) -> Result<Selection> {
        self.selection(options)
    }

    fn paths(&self) -> Result<Vec<String>> {
        self.db.paths()
    }

    fn structure(&self, path: &str) -> Result<Option<parse::FileStructure>> {
        let structure = self.presentation_structure(path)?;
        ensure!(
            structure.is_some(),
            "Structure for {path} requires refresh; run map without --no-reindex"
        );
        Ok(structure)
    }

    fn source(&self, path: &str) -> Option<&str> {
        self.presentation_source(path)
    }

    fn selection_source(&self, path: &str) -> Result<Option<&str>> {
        self.source(path).map(Some)
    }

    fn line_totals(&self, path: &str) -> Result<Option<crate::lines::Totals>> {
        self.cached_line_totals(path).map(Some)
    }

    fn call_graph(&self) -> Result<crate::callgraph::CallGraph> {
        self.call_graph()
    }

    fn file_description(&self, path: &str) -> Option<&str> {
        self.presentation_file_description(path)
    }

    fn symbol_descriptions(&self, path: &str) -> Result<HashMap<usize, String>> {
        self.presentation_symbol_descriptions(path)
    }
}

pub(crate) fn source_paths(root: &Path, index: &Path, config: &Value) -> Result<Vec<String>> {
    source_paths_subset(root, index, config, None, None, true)
}

/// File-level selection shared by discovery and Git status filtering.
struct Eligibility {
    include: GlobSet,
    exclude: GlobSet,
    lock_files: GlobSet,
}

impl Eligibility {
    fn new(config: &Value) -> Result<Self> {
        Ok(Self {
            include: globs(&config["include"])?,
            exclude: globs(&config["exclude"])?,
            lock_files: globs(&json!(EXCLUDED_LOCK_FILES))?,
        })
    }

    fn selects_file(&self, path: &str) -> bool {
        parse::language_for_path(path).is_some()
            && !self.lock_files.is_match(path)
            && !self.exclude.is_match(path)
            && (self.include.is_empty() || self.include.is_match(path))
    }

    /// Whether a root-relative path reported by Git could be walked and
    /// selected. Ignore files are not consulted: this may keep an ignored path,
    /// never drop a selectable one.
    fn selects_git_path(&self, path: &str) -> bool {
        self.selects_file(path)
            && !path
                .rsplit('/')
                .skip(1)
                .any(|directory| EXCLUDED.contains(&directory))
    }
}

/// A vanished or unreadable entry below the root is skipped; the root itself
/// must be walkable.
fn skippable_walk_error(error: &ignore::Error) -> bool {
    error.depth().is_some_and(|depth| depth > 0)
        && error.io_error().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            )
        })
}

fn source_paths_subset(
    root: &Path,
    index: &Path,
    config: &Value,
    candidates: Option<&HashSet<String>>,
    mut directories: Option<&mut HashSet<String>>,
    warn: bool,
) -> Result<Vec<String>> {
    let eligibility = Eligibility::new(config)?;
    ensure!(
        config["maxFileSize"].as_u64().unwrap_or(1_048_576) > 0,
        "maxFileSize must be positive"
    );
    let mut walker = ignore::WalkBuilder::new(root);
    let filter_root = root.to_owned();
    let candidates = candidates.cloned();
    let global_path = crate::cache::store_path(config)?;
    walker
        .hidden(false)
        .require_git(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            if global_artifact(entry.path(), &global_path) {
                return false;
            }
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                if EXCLUDED.contains(&entry.file_name().to_string_lossy().as_ref())
                    || entry.path().join(".git").exists()
                {
                    return false;
                }
                return candidates.as_ref().is_none_or(|paths| {
                    paths
                        .iter()
                        .any(|path| filter_root.join(path).starts_with(entry.path()))
                });
            }
            candidates.as_ref().is_none_or(|paths| {
                relative(&filter_root, entry.path())
                    .ok()
                    .is_some_and(|path| paths.contains(&path))
            })
        });
    let mut paths = Vec::new();
    let mut unreadable = 0usize;
    let mut undecodable = 0usize;
    for entry in walker.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if skippable_walk_error(&error) => {
                unreadable += 1;
                continue;
            }
            Err(error) => return Err(error).context("Cannot walk repository"),
        };
        // Indexed paths are UTF-8 strings. Other names cannot be source paths.
        let path = relative_utf8(root, entry.path())?;
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            if let (Some(directories), Some(path)) = (&mut directories, path) {
                directories.insert(path);
            }
            continue;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) || index_artifact(entry.path(), index) {
            continue;
        }
        let Some(path) = path else {
            undecodable += usize::from(eligibility.selects_file(&entry.path().to_string_lossy()));
            continue;
        };
        if eligibility.selects_file(&path) {
            paths.push(path);
        }
    }
    if warn && unreadable > 0 {
        ui::warning(format_args!(
            "slopdex: skipped {unreadable} unreadable or vanished path(s) while scanning {}",
            root.display()
        ));
    }
    if warn && undecodable > 0 {
        ui::warning(format_args!(
            "slopdex: skipped {undecodable} source file(s) whose path is not valid UTF-8"
        ));
    }
    paths.sort();
    Ok(paths)
}

fn global_artifact(path: &Path, global: &Path) -> bool {
    if path == global {
        return true;
    }
    path.parent() == global.parent()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix(global.file_name()?.to_str()?))
            .is_some_and(|suffix| matches!(suffix, "-wal" | "-shm" | ".locks" | ".indexes"))
}

fn policy_path(path: &str) -> bool {
    matches!(
        Path::new(path).file_name().and_then(|name| name.to_str()),
        Some(".gitignore" | ".ignore" | ".rgignore")
    )
}

fn has_read_error(file: &File) -> bool {
    file.errors
        .iter()
        .any(|error| error["code"] == "read-error")
}

const OMISSION_MARKER: &str = "omitted-candidates";

/// Remove the marker `search_rows` appends when a reranker's candidate cap hid
/// candidates that the number of returned rows cannot reveal.
fn take_omission_marker(rows: &mut Vec<Value>) -> bool {
    let marked = rows
        .last()
        .is_some_and(|row| row["type"] == OMISSION_MARKER);
    if marked {
        rows.pop();
    }
    marked
}

/// Ignore-policy probe for a remembered directory. An unreadable directory
/// cannot contribute rules, but regaining access must be noticed as a change.
fn policy_fingerprint(path: &Path) -> Result<Value> {
    match fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            Ok(json!("unreadable"))
        }
        _ => file_fingerprint(path),
    }
}

fn file_fingerprint(path: &Path) -> Result<Value> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        // A parent replaced by a file is as absent as a deleted file.
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(Value::Null);
        }
        Err(error) => return Err(error.into()),
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|time| time.as_nanos().to_string());
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        json!([
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec(),
            metadata.mode()
        ])
    };
    #[cfg(not(unix))]
    let identity = json!(
        metadata
            .created()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|time| time.as_nanos().to_string())
    );
    Ok(json!([metadata.len(), modified, identity]))
}

fn selection_contract(config: &Value) -> String {
    json!([
        STRUCTURE_PARSER_VERSION,
        config["include"],
        config["exclude"],
        config["maxFileSize"].as_u64().unwrap_or(1_048_576),
        EXCLUDED,
        EXCLUDED_LOCK_FILES
    ])
    .to_string()
}

fn indexing_policy(
    root: &Path,
    config: &Value,
    identity: Option<&str>,
    directories: &HashSet<String>,
) -> Result<String> {
    // Validate even on the unchanged fast path.
    globs(&config["include"])?;
    globs(&config["exclude"])?;
    ensure!(
        config["maxFileSize"].as_u64().unwrap_or(1_048_576) > 0,
        "maxFileSize must be positive"
    );
    let mut external = BTreeMap::new();
    // Effective ignore inputs can themselves be Git-ignored. Remember walked
    // directories and stat only their policy files, not every source file.
    for directory in directories {
        for name in [".gitignore", ".ignore", ".rgignore"] {
            let path = root.join(directory).join(name);
            external.insert(path.clone(), policy_fingerprint(&path)?);
        }
    }
    for parent in root.ancestors() {
        for name in [".gitignore", ".ignore", ".rgignore"] {
            let path = parent.join(name);
            external.insert(path.clone(), file_fingerprint(&path)?);
        }
    }
    if let Some(identity) = identity {
        let identity: Value = serde_json::from_str(identity)?;
        for name in [
            "info/exclude",
            "info/sparse-checkout",
            "config",
            "config.worktree",
        ] {
            for directory in [
                identity["common_dir"].as_str(),
                identity["git_dir"].as_str(),
            ]
            .into_iter()
            .flatten()
            {
                let path = Path::new(directory).join(name);
                external.insert(path.clone(), file_fingerprint(&path)?);
            }
        }
    }
    let global_ignore = git::excludes_file(root).or_else(|| {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
            .map(|directory| directory.join("git/ignore"))
    });
    if let Some(path) = global_ignore {
        external.insert(path.clone(), file_fingerprint(&path)?);
    }
    external.retain(|_, value| !value.is_null());
    Ok(json!([selection_contract(config), external]).to_string())
}

#[cfg(test)]
thread_local! {
    /// Source reads attempted on this thread, to prove refreshes skip them.
    static SOURCE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn read_source(root: &Path, path: &str, max_size: u64) -> Result<String> {
    #[cfg(test)]
    SOURCE_READS.with(|reads| reads.set(reads.get() + 1));
    let path = root.join(path);
    ensure!(
        fs::metadata(&path)?.len() <= max_size,
        "File exceeds maxFileSize ({max_size} bytes)"
    );
    fs::read_to_string(&path).context("Cannot read UTF-8 source")
}

pub(crate) fn normalize_path(root: &Path, path: &str) -> Result<String> {
    let path = Path::new(path);
    if path.is_absolute() {
        // Both indexed and direct maps use a canonical root; resolve aliases
        // before checking that an absolute source path is inside it.
        let path = path.canonicalize().context("Cannot resolve source path")?;
        return relative(root, &path);
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
    matches!(suffix, ".lock" | "-wal" | "-shm")
        || FormatGroup::ALL
            .into_iter()
            .map(FormatGroup::as_str)
            .chain(["markdown", "descriptions", "symbols"])
            .any(|role| suffix == format!(".{role}.shared.json"))
}

fn content_group(item: &Item) -> Option<FormatGroup> {
    matches!(item.kind.as_str(), "function" | "markdown" | "document")
        .then(|| FormatGroup::for_path(&item.path))
        .flatten()
}

pub(crate) fn cross_search_exclusions(config: &Value) -> Result<BTreeSet<&str>> {
    let Some(value) = config.get("crossSearchExclusions") else {
        return Ok(BTreeSet::new());
    };
    let values = value
        .as_array()
        .context("crossSearchExclusions must be an array of SHA-256 hashes")?;
    values
        .iter()
        .map(|value| {
            let hash = value
                .as_str()
                .context("crossSearchExclusions must contain SHA-256 hash strings")?;
            ensure!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "crossSearchExclusions must contain lowercase SHA-256 hashes"
            );
            Ok(hash)
        })
        .collect()
}

fn flag(value: &Value, key: &str) -> bool {
    value[key].as_bool().unwrap_or(false)
}

fn apply_description(
    prepared: &mut [PreparedFile],
    conversations: &mut [FileConversation],
    job: &DescriptionJob,
    text: String,
) {
    let prepared = &mut prepared[job.file_index];
    let conversation = &mut conversations[job.file_index];
    if let Some(index) = job.callable_index {
        prepared.callables[index].data["description"] = json!(text);
        conversation.next_callable = index + 1;
    } else {
        prepared.file.description = Some(text.clone());
        prepared.file.description_hash = Some(prepared.file.hash.clone());
        conversation.file_ready = true;
    }
    conversation.messages = job.generation.messages.clone();
    conversation.messages.push(Message::assistant(text));
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
fn score(value: &Value, key: &str) -> f64 {
    value[key].as_f64().unwrap_or(0.0)
}
pub(crate) fn under(path: &str, parent: &str) -> bool {
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
/// None for a path that cannot be represented as an indexed (UTF-8) path.
fn relative_utf8(root: &Path, path: &Path) -> Result<Option<String>> {
    Ok(path
        .strip_prefix(root)
        .context("Path is outside repository")?
        .to_str()
        .map(|path| path.replace('\\', "/")))
}
/// Config `include`/`exclude` patterns. These are plain globset globs matched
/// against the whole root-relative path: `*` and `?` also match `/`, a pattern
/// without a slash is not a basename pattern, and there is no negation. `-g`
/// selectors use gitignore semantics instead (see `filter::Selection`).
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

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    fn git_command(root: &Path, args: &[&str]) -> Result<()> {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    fn git_fixture() -> Result<(tempfile::TempDir, Engine)> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        fs::create_dir(&root)?;
        git_command(&root, &["init", "-q"])?;
        fs::write(root.join("source.rs"), "pub fn first() {}\n")?;
        git_command(&root, &["add", "."])?;
        git_command(&root, &["commit", "-qm", "initial"])?;
        let engine = Engine::open_map(
            &root,
            &temp.path().join("index.sqlite"),
            json!({"artifactCachePath":temp.path().join("global.sqlite")}),
        )?;
        Ok((temp, engine))
    }

    #[test]
    fn lock_file_patterns_cover_dependency_ecosystems_without_hiding_manifests() -> Result<()> {
        let locks = globs(&json!(EXCLUDED_LOCK_FILES))?;
        for name in [
            "package-lock.json",
            "npm-shrinkwrap.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "pnpm-lock.yml",
            "shrinkwrap.yaml",
            "bun.lock",
            "bun.lockb",
            "deno.lock",
            "lazy-lock.json",
            "esy.lock/index.json",
            "Cargo.lock",
            "Pipfile.lock",
            "poetry.lock",
            "pdm.lock",
            "uv.lock",
            "pylock.toml",
            "pylock.dev.toml",
            "requirements.lock",
            "Gemfile.lock",
            "gems.locked",
            "composer.lock",
            "go.sum",
            "go.work.sum",
            "Gopkg.lock",
            "glide.lock",
            "mix.lock",
            "pubspec.lock",
            "packages.lock.json",
            "project.lock.json",
            "paket.lock",
            "packages-lock.json",
            "Package.resolved",
            "Podfile.lock",
            "Cartfile.resolved",
            "gradle.lockfile",
            "compileClasspath.lockfile",
            ".terraform.lock.hcl",
            "flake.lock",
            "conan.lock",
            "vcpkg-lock.json",
            "manifest.toml",
            "Manifest.toml",
            "Manifest-v1.11.toml",
            "JuliaManifest.toml",
            "JuliaManifest-v1.11.toml",
            "renv.lock",
            "packrat.lock",
            "cpanfile.snapshot",
            "cabal.project.freeze",
            "cabal.project.local.freeze",
            "stack.yaml.lock",
            "example.opam.locked",
            "dub.selections.json",
            "Berksfile.lock",
            "Puppetfile.lock",
            "custom.lock.yaml",
            "custom.lock.yml",
            ".meteor/versions",
        ] {
            for prefix in ["", "nested/workspace/"] {
                let path = format!("{prefix}{name}");
                assert!(locks.is_match(&path), "lock file was not excluded: {path}");
            }
        }
        for path in [
            "package.json",
            "nested/Cargo.toml",
            "pyproject.toml",
            "go.mod",
            "Project.toml",
            "build.zig.zon",
            "src/lock.rs",
            "src/lock.json",
            "package-lock.json.md",
            "lockfile.py",
            "Manifest.toml.md",
        ] {
            assert!(!locks.is_match(path), "non-lock file was excluded: {path}");
        }
        Ok(())
    }

    #[test]
    fn discovery_excludes_lock_files_even_with_explicit_includes_and_candidates() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        let mut candidates = HashSet::new();
        for prefix in ["", "nested/workspace/"] {
            for name in [
                "package-lock.json",
                "npm-shrinkwrap.json",
                "lazy-lock.json",
                "pnpm-lock.yaml",
                "packages.lock.json",
                "pylock.toml",
                "pylock.dev.toml",
                ".terraform.lock.hcl",
                "Manifest.toml",
                "manifest.toml",
                "Manifest-v1.11.toml",
                "dub.selections.json",
                "package.json",
                "Cargo.toml",
                "pyproject.toml",
                "lock.rs",
            ] {
                let relative = format!("{prefix}{name}");
                let path = root.join(&relative);
                fs::create_dir_all(path.parent().unwrap())?;
                fs::write(path, "")?;
                candidates.insert(relative);
            }
        }
        // Only files are excluded; a directory with a lock-file name is still walked.
        fs::create_dir_all(root.join("workspace/package-lock.json"))?;
        fs::write(root.join("workspace/package-lock.json/source.rs"), "")?;
        candidates.insert("workspace/package-lock.json/source.rs".to_owned());
        let expected = [
            "Cargo.toml",
            "lock.rs",
            "nested/workspace/Cargo.toml",
            "nested/workspace/lock.rs",
            "nested/workspace/package.json",
            "nested/workspace/pyproject.toml",
            "package.json",
            "pyproject.toml",
            "workspace/package-lock.json/source.rs",
        ];
        for include in [json!([]), json!(["**"])] {
            let config = json!({
                "include":include,
                "artifactCachePath":temp.path().join("global.sqlite"),
            });
            for subset in [None, Some(&candidates)] {
                assert_eq!(
                    source_paths_subset(
                        &root,
                        &temp.path().join("index.sqlite"),
                        &config,
                        subset,
                        None,
                        false,
                    )?,
                    expected
                );
            }
        }
        Ok(())
    }

    #[test]
    fn lock_file_policy_change_removes_legacy_entries_from_clean_git_index() -> Result<()> {
        let (_temp, mut engine) = git_fixture()?;
        let path = "package-lock.json";
        let source = "{\"lockfileVersion\":3}";
        fs::write(engine.root.join(path), source)?;
        git_command(&engine.root, &["add", "."])?;
        git_command(&engine.root, &["commit", "-qm", "lock file"])?;
        engine.refresh_structure()?;
        assert_eq!(engine.files.len(), 1);

        // Seed the entry and policies an older version would have published.
        let mut file = engine.files["source.rs"].clone();
        file.path = path.to_owned();
        file.source = source.to_owned();
        file.hash = hash(source);
        file.language = "json".to_owned();
        engine.db.apply_structure(
            &[(file, parse::parse(path, source)?)],
            &[],
            git::head(&engine.root).as_deref(),
        )?;
        let mut legacy: Value = serde_json::from_str(&selection_contract(&engine.config))?;
        assert_eq!(
            legacy.as_array_mut().unwrap().pop(),
            Some(json!(EXCLUDED_LOCK_FILES))
        );
        let legacy = legacy.to_string();
        engine.db.set_meta("selection_policy", &legacy)?;
        let mut policy: Value =
            serde_json::from_str(&engine.db.meta("discovery_policy")?.unwrap())?;
        policy[0] = json!(legacy);
        engine
            .db
            .set_meta("discovery_policy", &policy.to_string())?;
        engine.load()?;
        assert!(engine.files.contains_key(path));
        assert!(git::dirty_paths(&engine.root)?.is_empty());

        assert_eq!(engine.refresh_structure()?["filesDeleted"], 1);
        assert_eq!(engine.db.paths()?, ["source.rs"]);
        assert!(!engine.files.contains_key(path));
        assert_eq!(engine.refresh_structure()?["filesDeleted"], 0);
        Ok(())
    }

    #[test]
    fn source_paths_subset_normalizes_nested_git_candidates() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        for path in [
            "nested/new.rs",
            "nested/deeper/source.rs",
            "nested/unselected.rs",
            "other/unselected.rs",
        ] {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, "fn example() {}")?;
        }
        let candidates = HashSet::from([
            "nested/new.rs".to_owned(),
            "nested/deeper/source.rs".to_owned(),
        ]);
        let paths = source_paths_subset(
            &root,
            &temp.path().join("index.sqlite"),
            &json!({"artifactCachePath":temp.path().join("global.sqlite")}),
            Some(&candidates),
            None,
            false,
        )?;
        assert_eq!(paths, ["nested/deeper/source.rs", "nested/new.rs"]);
        Ok(())
    }

    #[test]
    fn refresh_rechecks_dirty_provenance_before_publication_and_restore() -> Result<()> {
        let (_temp, mut engine) = git_fixture()?;
        engine.refresh_structure()?;
        let old_checkpoint = engine.db.meta("checkpoint")?;
        let path = engine.root.join("source.rs");
        let committed = "pub fn second() {}\n";
        fs::write(&path, committed)?;
        git_command(&engine.root, &["commit", "-qam", "second"])?;
        let error = engine
            .refresh_structure_with(|| {
                fs::write(&path, "pub fn concurrent_edit() {}\n")?;
                Ok(())
            })
            .unwrap_err();
        assert!(
            error.to_string().contains("working tree changed"),
            "{error}"
        );
        assert_eq!(engine.db.meta("checkpoint")?, old_checkpoint);
        assert_eq!(engine.source("source.rs")?, "pub fn first() {}\n");
        fs::write(&path, committed)?;
        engine.refresh_structure()?;
        assert_eq!(engine.source("source.rs")?, committed);
        assert!(git::dirty_paths(&engine.root)?.is_empty());
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 0);
        Ok(())
    }

    #[test]
    fn ignored_nested_policy_files_invalidate_clean_git_snapshots() -> Result<()> {
        let (_temp, mut engine) = git_fixture()?;
        fs::create_dir(engine.root.join("nested"))?;
        fs::write(engine.root.join(".gitignore"), "*.ignore\n")?;
        fs::write(engine.root.join("nested/.ignore"), "")?;
        fs::write(engine.root.join("nested/kept.rs"), "pub fn kept() {}\n")?;
        git_command(&engine.root, &["add", "."])?;
        git_command(&engine.root, &["commit", "-qm", "nested"])?;
        engine.refresh_structure()?;
        assert!(engine.files.contains_key("nested/kept.rs"));
        for policy in ["*.rs\n", ""] {
            fs::write(engine.root.join("nested/.ignore"), policy)?;
            assert!(git::dirty_paths(&engine.root)?.is_empty());
            engine.refresh_structure()?;
            assert_eq!(
                engine.files.contains_key("nested/kept.rs"),
                policy.is_empty()
            );
            assert_eq!(engine.refresh_structure()?["filesUpdated"], 0);
        }
        Ok(())
    }

    #[test]
    fn in_tree_artifact_writes_do_not_invalidate_git_discovery() -> Result<()> {
        let (_temp, engine) = git_fixture()?;
        let root = engine.root.clone();
        drop(engine);
        let mut engine = Engine::open_map(
            &root,
            &root.join("index.sqlite"),
            json!({"artifactCachePath":root.join("global.sqlite")}),
        )?;
        engine.refresh_structure()?;
        assert_eq!(engine.files.len(), 1);
        assert!(
            !git::dirty_paths(&root)?.is_empty(),
            "positive control: generated files are untracked"
        );
        let before = engine.db.meta("snapshot")?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 0);
        assert_eq!(engine.db.meta("snapshot")?, before);
        let dirty: HashSet<String> =
            serde_json::from_str(&engine.db.meta("dirty_paths")?.unwrap())?;
        assert!(dirty.is_empty());
        Ok(())
    }

    #[test]
    fn contended_writer_times_out_but_readers_can_share() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("index.lock");
        let first = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)?;
        let second = OpenOptions::new().read(true).write(true).open(&path)?;
        let writer = OpenOptions::new().read(true).write(true).open(&path)?;
        lock_index(&first, true)?;
        lock_index(&second, true)?;
        let error = lock_index_with_timeout(&writer, false, Duration::from_millis(60))
            .expect_err("writer must wait for other readers");
        assert!(error.to_string().contains("Index is in use"));
        drop(first);
        drop(second);
        lock_index(&writer, false)?;
        let reader = OpenOptions::new().read(true).write(true).open(&path)?;
        let error = lock_index_with_timeout(&reader, true, Duration::from_millis(60))
            .expect_err("reader must wait for a writer");
        assert!(error.to_string().contains("Index is in use"));
        drop(writer);
        lock_index(&reader, true)?;
        Ok(())
    }

    #[test]
    #[cfg(unix)]
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
        let engine = Engine::open(
            &alias,
            &temp.path().join("index.sqlite"),
            json!({"artifactCachePath":temp.path().join("global.sqlite")}),
        )?;

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
    #[test]
    fn all_grouped_index_pointers_are_excluded_from_discovery() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let index = temp.path().join("index.sqlite");
        fs::write(temp.path().join("visible.json"), "{}")?;
        for role in FormatGroup::ALL
            .into_iter()
            .map(FormatGroup::as_str)
            .chain(["markdown", "descriptions", "symbols"])
        {
            let pointer = temp.path().join(format!("index.sqlite.{role}.shared.json"));
            fs::write(&pointer, "{}")?;
            assert!(index_artifact(&pointer, &index));
        }
        assert_eq!(
            source_paths(temp.path(), &index, &json!({"include":["**"]}))?,
            ["visible.json"]
        );
        Ok(())
    }

    fn source_reads() -> usize {
        SOURCE_READS.with(|reads| reads.replace(0))
    }

    fn assert_send<T: Send>() {}

    #[test]
    fn engine_stays_send_with_derived_caches() {
        assert_send::<Engine>();
    }

    #[test]
    fn non_git_refresh_reuses_fingerprints_and_respects_size_policy() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("plain");
        fs::create_dir(&root)?;
        fs::write(root.join("first.rs"), "pub fn first() {}\n")?;
        fs::write(root.join("second.rs"), "pub fn second() {}\n")?;
        let index = temp.path().join("index.sqlite");
        let config = json!({"artifactCachePath":temp.path().join("global.sqlite")});
        let mut engine = Engine::open_map(&root, &index, config.clone())?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 2);
        assert!(engine.db.meta("checkpoint")?.is_none());
        let generation = engine.db.generation()?;
        source_reads();
        for _ in 0..2 {
            let refreshed = engine.refresh_structure()?;
            assert_eq!(refreshed["filesUpdated"], 0);
            assert_eq!(engine.db.generation()?, generation);
        }
        assert_eq!(source_reads(), 0, "unchanged non-Git files were re-read");

        // A different length always changes the fingerprint, whatever the clock.
        fs::write(root.join("second.rs"), "pub fn second_edited() {}\n")?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
        assert_eq!(source_reads(), 1, "only the edited file is read");
        drop(engine);

        // Discovery policy changes rescan without re-reading unchanged files.
        let mut excluded = config.clone();
        excluded["exclude"] = json!(["first.rs"]);
        let mut engine = Engine::open_map(&root, &index, excluded)?;
        let refreshed = engine.refresh_structure()?;
        assert_eq!(
            (&refreshed["filesUpdated"], &refreshed["filesDeleted"]),
            (&json!(0), &json!(1))
        );
        assert_eq!(source_reads(), 0);
        drop(engine);

        // The size limit is policy that changes the result for unchanged metadata.
        let mut small = config.clone();
        small["maxFileSize"] = json!(20);
        let mut engine = Engine::open_map(&root, &index, small)?;
        engine.refresh_structure()?;
        let errors = engine.errors()?;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0]["path"], "second.rs");
        assert_eq!(errors[0]["code"], "read-error");
        drop(engine);
        let mut engine = Engine::open_map(&root, &index, config)?;
        engine.refresh_structure()?;
        assert!(engine.errors()?.is_empty());
        assert_eq!(engine.db.paths()?, ["first.rs", "second.rs"]);
        Ok(())
    }

    #[test]
    fn ineligible_dirty_paths_do_not_defeat_the_unchanged_fast_path() -> Result<()> {
        let (_temp, mut engine) = git_fixture()?;
        engine.refresh_structure()?;
        let root = engine.root.clone();
        // Untracked files discovery never selects: an unsupported type, and a
        // supported type below an always-excluded directory.
        fs::write(root.join("scratch.bin"), "binary")?;
        fs::create_dir(root.join("target"))?;
        fs::write(root.join("target/built.rs"), "pub fn built() {}\n")?;
        let generation = engine.db.generation()?;
        engine.refresh_structure()?;
        assert_eq!(engine.db.meta("dirty_paths")?.as_deref(), Some("[]"));
        assert_eq!(engine.db.generation()?, generation);
        assert_eq!(engine.db.paths()?, ["source.rs"]);

        // Selectable changes and ignore-policy files remain triggers.
        fs::write(root.join("added.rs"), "pub fn added() {}\n")?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
        assert_eq!(
            engine.db.meta("dirty_paths")?.as_deref(),
            Some(r#"["added.rs"]"#)
        );
        fs::write(root.join(".gitignore"), "added.rs\n")?;
        assert_eq!(engine.refresh_structure()?["filesDeleted"], 1);
        assert_eq!(
            engine.db.meta("dirty_paths")?.as_deref(),
            Some(r#"[".gitignore"]"#)
        );
        assert_eq!(engine.db.paths()?, ["source.rs"]);
        // Indexed paths stay dirty even when configuration would not select them.
        fs::write(root.join("source.rs"), "pub fn edited() {}\n")?;
        let eligibility = Eligibility::new(&json!({"exclude":["source.rs"]}))?;
        let mut dirty = HashSet::from(["source.rs".to_owned(), "scratch.bin".to_owned()]);
        engine.retain_relevant_dirty(&mut dirty, &eligibility);
        assert_eq!(dirty, HashSet::from(["source.rs".to_owned()]));
        Ok(())
    }

    #[test]
    fn deleted_directories_are_forgotten_by_candidate_refreshes() -> Result<()> {
        let (_temp, mut engine) = git_fixture()?;
        let root = engine.root.clone();
        fs::create_dir_all(root.join("gone/deeper"))?;
        fs::write(root.join("gone/deeper/old.rs"), "pub fn old() {}\n")?;
        git_command(&root, &["add", "."])?;
        git_command(&root, &["commit", "-qm", "nested"])?;
        engine.refresh_structure()?;
        let directories = |engine: &Engine| -> Result<BTreeSet<String>> {
            Ok(serde_json::from_str(
                &engine.db.meta("source_directories")?.unwrap(),
            )?)
        };
        assert!(directories(&engine)?.contains("gone/deeper"));
        // Establish a checkpoint so the next refresh is a candidate walk.
        fs::remove_dir_all(root.join("gone"))?;
        git_command(&root, &["add", "--all"])?;
        git_command(&root, &["commit", "-qm", "removed"])?;
        assert_eq!(engine.refresh_structure()?["filesDeleted"], 1);
        assert_eq!(directories(&engine)?, BTreeSet::from([String::new()]));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn relative_utf8_skips_undecodable_paths_without_filesystem_access() -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let root = Path::new("repo");
        assert_eq!(
            relative_utf8(root, &root.join("kept.rs"))?,
            Some("kept.rs".into())
        );
        for name in [&b"na\xefve.rs"[..], b"caf\xe9/inner.rs", b"data\xff.bin"] {
            let path = root.join(std::ffi::OsStr::from_bytes(name));
            assert_eq!(relative_utf8(root, &path)?, None);
        }
        assert!(relative_utf8(root, Path::new("outside.rs")).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn discovery_skips_undecodable_names() -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("plain");
        fs::create_dir(&root)?;
        if !crate::test_support::supports_non_utf8_names(&root)? {
            return Ok(());
        }
        fs::create_dir_all(root.join(std::ffi::OsStr::from_bytes(b"caf\xe9")))?;
        fs::write(root.join("kept.rs"), "pub fn kept() {}\n")?;
        // Latin-1 names: a supported source file, a nested one, and other data.
        for name in [&b"na\xefve.rs"[..], b"caf\xe9/inner.rs", b"data\xff.bin"] {
            fs::write(
                root.join(std::ffi::OsStr::from_bytes(name)),
                "pub fn x() {}\n",
            )?;
        }
        let index = temp.path().join("index.sqlite");
        let config = json!({"artifactCachePath":temp.path().join("global.sqlite")});
        // This is exactly what an unindexed map walks.
        assert_eq!(source_paths(&root, &index, &config)?, ["kept.rs"]);
        let mut engine = Engine::open_map(&root, &index, config)?;
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 1);
        assert_eq!(engine.refresh_structure()?["filesUpdated"], 0);
        assert_eq!(engine.db.paths()?, ["kept.rs"]);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn discovery_skips_unreadable_directories() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("plain");
        fs::create_dir_all(root.join("locked"))?;
        fs::write(root.join("kept.rs"), "pub fn kept() {}\n")?;
        fs::write(root.join("locked/hidden.rs"), "pub fn hidden() {}\n")?;
        let index = temp.path().join("index.sqlite");
        let config = json!({"artifactCachePath":temp.path().join("global.sqlite")});
        assert_eq!(
            source_paths(&root, &index, &config)?,
            ["kept.rs", "locked/hidden.rs"]
        );

        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000))?;
        let restore =
            || fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755));
        if fs::read_dir(root.join("locked")).is_ok() {
            // Privileged users bypass directory permissions.
            return Ok(restore()?);
        }
        let walked = source_paths(&root, &index, &config);
        let indexed = Engine::open_map(&root, &index, config).and_then(|mut engine| {
            let first = engine.refresh_structure()?;
            // The remembered directory's policy probes are unreadable as well.
            let second = engine.refresh_structure()?;
            Ok((first, second, engine.db.paths()?))
        });
        restore()?;
        assert_eq!(walked?, ["kept.rs"]);
        let (first, second, paths) = indexed?;
        assert_eq!(first["filesUpdated"], 1);
        assert_eq!(second["filesUpdated"], 0);
        assert_eq!(paths, ["kept.rs"]);

        // The selected root itself must still be walkable.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000))?;
        let walked = source_paths(
            &root,
            &index,
            &json!({"artifactCachePath":temp.path().join("global.sqlite")}),
        );
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
        assert!(walked.is_err());
        Ok(())
    }

    /// Offline OpenAI-compatible provider: unit vectors, fixed prose, and
    /// identity rankings. Records how many documents each rerank request held.
    fn provider() -> Result<(String, std::sync::Arc<std::sync::Mutex<Vec<usize>>>)> {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let base = format!("http://{}/v1", listener.local_addr()?);
        let reranked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = reranked.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let respond = |stream: &mut std::net::TcpStream| -> Result<()> {
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    let mut reader = BufReader::new(&mut *stream);
                    let mut line = String::new();
                    reader.read_line(&mut line)?;
                    let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                    let mut length = 0;
                    loop {
                        line.clear();
                        ensure!(reader.read_line(&mut line)? > 0, "truncated headers");
                        if line == "\r\n" {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':')
                            && name.eq_ignore_ascii_case("content-length")
                        {
                            length = value.trim().parse()?;
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body)?;
                    let body: Value = serde_json::from_slice(&body)?;
                    let response = if path.ends_with("/embeddings") {
                        let dimensions = body["dimensions"].as_u64().unwrap_or(2) as usize;
                        let mut vector = vec![0.0; dimensions];
                        vector[0] = 1.0;
                        let data: Vec<_> = (0..body["input"].as_array().map_or(0, Vec::len))
                            .map(|index| json!({"index":index,"embedding":vector}))
                            .collect();
                        json!({"data":data})
                    } else if body["text"]["format"]["name"] == "function_ranking" {
                        let count =
                            body["text"]["format"]["schema"]["properties"]["ranking"]["minItems"]
                                .as_u64()
                                .unwrap_or(0) as usize;
                        seen.lock().unwrap().push(count);
                        let ranking: Vec<_> = (0..count)
                            .map(|index| json!({"index":index,"score":1.0 - index as f64 / 1000.0}))
                            .collect();
                        json!({"output_text":json!({"ranking":ranking}).to_string()})
                    } else {
                        json!({"output_text":"Generated summary."})
                    };
                    let bytes = serde_json::to_vec(&response)?;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    )?;
                    stream.write_all(&bytes)?;
                    Ok(())
                };
                let _ = respond(&mut stream);
            }
        });
        Ok((base, reranked))
    }

    fn provider_config(temp: &tempfile::TempDir, base: &str) -> Value {
        json!({
            "embeddingProvider":"openai", "embeddingModel":"unit-embedding",
            "embeddingDimensions":2, "symbolDimensions":2,
            "descriptionProvider":"openai", "descriptionModel":"unit-description",
            "rerankerProvider":"openai", "rerankerModel":"unit-reranker",
            "embeddingBaseUrl":base, "descriptionBaseUrl":base, "rerankerBaseUrl":base,
            "embeddingApiKey":"unit", "descriptionApiKey":"unit", "rerankerApiKey":"unit",
            "providerMaxRetries":0, "retryDelayMs":0, "providerTimeoutMs":5000,
            "artifactCachePath":temp.path().join("global.sqlite")
        })
    }

    #[test]
    fn reranker_candidate_cap_reports_omitted_results() -> Result<()> {
        let (base, reranked) = provider()?;
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("plain");
        fs::create_dir(&root)?;
        let source: String = (0..12)
            .map(|index| format!("pub fn candidate_{index}() -> usize {{\n    {index}\n}}\n\n"))
            .collect();
        fs::write(root.join("many.rs"), source)?;
        let mut config = provider_config(&temp, &base);
        config["rerankingEnabled"] = json!(true);
        let mut engine = Engine::open(&root, &temp.path().join("index.sqlite"), config)?;
        engine.refresh()?;
        assert_eq!(
            engine
                .items
                .iter()
                .filter(|item| item.kind == "function")
                .count(),
            12
        );
        // The default candidate count equals this limit, so the row count alone
        // cannot show that two more functions matched.
        for _ in 0..2 {
            let limited =
                engine.search_limited("candidate", "search-code", &json!({"limit":10}))?;
            assert_eq!(limited.rows.len(), 10);
            assert!(
                limited.omitted,
                "candidates beyond the reranker cap were not reported"
            );
            assert!(limited.rows.iter().all(|row| row["type"] == "function"));
            assert!(
                limited
                    .rows
                    .iter()
                    .all(|row| row["rerankScore"].is_number())
            );
        }
        // The probe is never sent to the reranker, and the answer was cached.
        assert_eq!(*reranked.lock().unwrap(), [10]);
        assert_eq!(
            engine
                .search("candidate", "search-code", &json!({"limit":10}))?
                .len(),
            10
        );
        let exact = engine.search_limited("candidate", "search-code", &json!({"limit":12}))?;
        assert_eq!(exact.rows.len(), 12);
        assert!(!exact.omitted);
        let merged = engine.search_queries_limited(
            &["candidate".to_owned(), "other".to_owned()],
            "search-code",
            &json!({"limit":10}),
        )?;
        assert_eq!(merged.rows.len(), 10);
        assert!(merged.omitted);
        assert!(merged.rows.iter().all(|row| row["type"] == "function"));
        Ok(())
    }

    #[test]
    fn descriptions_cover_healthy_callables_beside_parse_errors() -> Result<()> {
        let (base, _) = provider()?;
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("plain");
        fs::create_dir(&root)?;
        fs::write(
            root.join("damaged.rs"),
            "pub fn healthy() -> usize {\n    1\n}\n\npub fn broken( {\n",
        )?;
        fs::write(root.join("binary.rs"), [0xff, 0xfe, 0xfd])?;
        let config = provider_config(&temp, &base);
        let mut engine = Engine::open(&root, &temp.path().join("index.sqlite"), config)?;
        engine.refresh()?;
        let codes: BTreeSet<_> = engine
            .errors()?
            .iter()
            .map(|error| (error["path"].to_string(), error["code"].to_string()))
            .collect();
        assert!(codes.contains(&(r#""damaged.rs""#.into(), r#""parse-error""#.into())));
        assert!(codes.contains(&(r#""binary.rs""#.into(), r#""read-error""#.into())));
        let diagnostics = engine.errors()?.len();
        engine.generate_descriptions()?;
        let healthy = engine
            .items
            .iter()
            .find(|item| item.kind == "function" && item.data["name"] == "healthy")
            .context("healthy callable is indexed")?;
        assert_eq!(healthy.data["description"], "Generated summary.");
        assert_eq!(
            engine.files["damaged.rs"].description.as_deref(),
            Some("Generated summary.")
        );
        assert!(engine.files["binary.rs"].description.is_none());
        // The diagnostics survive description publication.
        assert_eq!(engine.errors()?.len(), diagnostics);
        Ok(())
    }

    /// Pins the dialect differences documented in docs/reference.md.
    #[test]
    fn config_globs_and_selector_globs_keep_their_documented_dialects() -> Result<()> {
        let config = |pattern: &str, path: &str| -> Result<bool> {
            Ok(globs(&json!([pattern]))?.is_match(path))
        };
        let selector = |pattern: &str, path: &str| -> Result<bool> {
            Ok(Selection::compile(&json!({"glob":[pattern]}))?.path_matches(path))
        };
        for (pattern, path, in_config, in_selector) in [
            // `*` crosses directories only in config globs.
            ("src/*.rs", "src/a.rs", true, true),
            ("src/*.rs", "src/a/b.rs", true, false),
            ("*.rs", "a/b.rs", true, true),
            ("src/?.rs", "src//.rs", true, false),
            // A slash-free pattern is a basename pattern only for selectors.
            ("test.rs", "test.rs", true, true),
            ("test.rs", "a/test.rs", false, true),
            // Anchors.
            ("/a.rs", "a.rs", false, true),
            ("/a.rs", "b/a.rs", false, false),
            // Directory patterns.
            ("src/", "src/a.rs", false, false),
            ("src", "src/a.rs", false, false),
            ("src/**", "src/a/b.rs", true, true),
            ("**/fixtures/**", "a/fixtures/b/c.rs", true, true),
            ("**/fixtures/**", "fixtures/c.rs", true, true),
            ("a/**/b.rs", "a/b.rs", true, true),
            // Alternation and classes exist in both.
            ("*.{rs,go}", "a.go", true, true),
            ("[ab].rs", "b.rs", true, true),
            // Matching is case-sensitive in both.
            ("*.RS", "a.rs", false, false),
        ] {
            assert_eq!(
                config(pattern, path)?,
                in_config,
                "config {pattern} on {path}"
            );
            assert_eq!(
                selector(pattern, path)?,
                in_selector,
                "selector {pattern} on {path}"
            );
        }
        // Negation exists only for selectors; config treats `!` literally.
        assert!(!config("!a.rs", "b.rs")?);
        assert!(config("!a.rs", "!a.rs")?);
        assert!(selector("!a.rs", "b.rs")?);
        assert!(!selector("!a.rs", "a.rs")?);
        Ok(())
    }
}
