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

mod cross;
mod discovery;
mod prepare;
mod presentation;
mod refresh;
mod search;

pub(crate) mod faq;
mod inspect;
mod locking;
use locking::IndexLock;

pub(crate) use cross::cross_search_exclusions;
pub(crate) use discovery::{normalize_path, read_source, source_paths, under};
use presentation::SymbolIndex;

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
    // Shared while reading/preparing, exclusive only during workspace writes.
    lock: IndexLock,
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
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("Lock index"),
        }
    }
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

    /// Fail if a prepared file's bytes changed since they were hashed.
    fn verify_source_unchanged(&self, file: &File) -> Result<()> {
        ensure!(
            hash(fs::read(self.root.join(&file.path))?) == file.hash,
            "Source changed during indexing: {}; rerun",
            file.path
        );
        Ok(())
    }

    fn open_internal(
        root: &Path,
        index: &Path,
        mut config: Value,
        structural_only: bool,
        readonly: bool,
    ) -> Result<Self> {
        limits::validate_config(&config)?;
        faq::threshold(&config, &json!({}))?;
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
        let lock = IndexLock::open(&PathBuf::from(lock_path))?;
        let db = if readonly {
            Database::open_readonly_with_config(&index, &root, &config)?
        } else if flag(&config, "forceReindex") {
            let _write = lock.write()?;
            Database::open_with_config(&index, &root, &config, true)?
        } else {
            let started = Instant::now();
            loop {
                match Database::open_existing_with_config(&index, &root, &config) {
                    Err(error) if error.is::<NeedsWrite>() => {
                        ensure!(
                            started.elapsed() < Duration::from_secs(10),
                            "Index is in use by another slopdex command (waited 10.00 seconds)"
                        );
                        // A peer may initialize and then keep reading. Recheck
                        // after short waits rather than waiting exclusively for
                        // an initialization that is no longer necessary.
                        if let Some(_write) = lock.write_for(Duration::from_millis(50))? {
                            break Database::open_with_config(&index, &root, &config, false)?;
                        }
                    }
                    result => break result?,
                }
            }
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
        // Never restore executable commands from an index or shared artifact.
        // Generation requires an explicitly configured, trusted command.
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
            lock,
            readonly,
        };
        if !structural_only && !readonly {
            let profile = engine.providers.vector().profile();
            if engine.db.meta("active_embedding_profile")?.as_deref() != Some(&profile.to_string())
            {
                engine.write_index(|engine| engine.db.set_projection_profile(&profile))?;
            }
        }
        engine.load()?;
        Ok(engine)
    }

    fn workspace_stamp(&self) -> Result<Vec<(String, String)>> {
        // Derived vector plans may be published by another query for the same
        // snapshot. Everything else must still match before prepared work commits.
        Ok(self
            .db
            .conn
            .prepare(
                "SELECT key,value FROM metadata WHERE key NOT GLOB 'index_state_*' ORDER BY key",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn write_index<T>(&mut self, write: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        ensure!(!self.readonly, "Cannot write a read-only index");
        let before = self.workspace_stamp()?;
        let guard = self.lock.write()?;
        ensure!(
            self.workspace_stamp()? == before,
            "Index changed while waiting to write; rerun the command"
        );
        let result = write(self);
        self.restore_index_read(guard, result)
    }

    /// Query caches and derived pointers are optional. A read must not wait for
    /// other readers merely to persist a reconstructible optimization.
    fn try_write_index(&self, write: impl FnOnce() -> Result<()>) -> Result<()> {
        ensure!(!self.readonly, "Cannot write a read-only index");
        let before = self.workspace_stamp()?;
        match self.lock.try_write()? {
            Some(guard) => self.finish_index_write(before, guard, write),
            None => {
                ensure!(
                    self.workspace_stamp()? == before,
                    "Index changed while restoring its read lock; rerun the command"
                );
                Ok(())
            }
        }
    }

    fn finish_index_write<T>(
        &self,
        before: Vec<(String, String)>,
        guard: locking::WriteLock,
        write: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        ensure!(
            self.workspace_stamp()? == before,
            "Index changed while waiting to write; rerun the command"
        );
        let result = write();
        self.restore_index_read(guard, result)
    }

    fn restore_index_read<T>(&self, guard: locking::WriteLock, result: Result<T>) -> Result<T> {
        let after = self.workspace_stamp()?;
        drop(guard);
        ensure!(
            self.workspace_stamp()? == after,
            "Index changed while restoring its read lock; rerun the command"
        );
        result
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

    fn structure(&self, path: &str) -> Result<Option<Arc<parse::FileStructure>>> {
        let structure = self.cached_structure(path)?;
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

    fn release_structures(&self) {
        self.structures.borrow_mut().clear();
    }

    fn file_description(&self, path: &str) -> Option<&str> {
        self.presentation_file_description(path)
    }

    fn symbol_descriptions(&self, path: &str) -> Result<HashMap<usize, String>> {
        self.presentation_symbol_descriptions(path)
    }
}

fn content_group(item: &Item) -> Option<FormatGroup> {
    matches!(item.kind.as_str(), "function" | "markdown" | "document")
        .then(|| FormatGroup::for_path(&item.path))
        .flatten()
}

fn flag(value: &Value, key: &str) -> bool {
    value[key].as_bool().unwrap_or(false)
}

fn score(value: &Value, key: &str) -> f64 {
    value[key].as_f64().unwrap_or(0.0)
}
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(a, b)| *a as f64 * *b as f64).sum();
    let norm = |v: &[f32]| v.iter().map(|v| (*v as f64).powi(2)).sum::<f64>().sqrt();
    (dot / (norm(a) * norm(b))).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests;
