//! Parse, description, and embedding preparation for changed files.

use super::discovery::*;
use super::*;

const DESCRIPTION_SYSTEM: &str = "Describe existing source code accurately. When asked about a file, return exactly one concise paragraph about its purpose, responsibilities, and important relationships. When asked about a callable, return exactly one sentence describing what it does, including relevant inputs, outputs, or side effects. Use plain text without a heading, bullets, or a preamble. Do not propose changes.";
// A byte cap is deliberately conservative even for providers with different tokenizers.
// Leave room for the task, expanded search context, and the model's response.

pub(super) struct PrepareInput {
    pub(super) path: String,
    pub(super) source: String,
    pub(super) source_mode: String,
    pub(super) regenerate_file: bool,
    pub(super) regenerate_callables: bool,
    pub(super) generate_descriptions: bool,
}

pub(super) struct PreparedCallable {
    pub(super) identity: String,
    pub(super) data: Value,
    pub(super) generate_description: bool,
}

pub(super) struct FileConversation {
    pub(super) messages: Vec<Message>,
    pub(super) session: String,
    pub(super) file_ready: bool,
    pub(super) next_callable: usize,
}

pub(super) struct DescriptionJob {
    pub(super) file_index: usize,
    pub(super) callable_index: Option<usize>,
    pub(super) key: String,
    pub(super) generation: DescriptionGeneration,
    pub(super) session: String,
}

pub(super) struct PreparedFile {
    pub(super) file: File,
    pub(super) parsed: parse::ParsedFile,
    pub(super) regenerate_file: bool,
    pub(super) regenerate_callables: bool,
    pub(super) callables: Vec<PreparedCallable>,
}

impl Engine {
    pub(super) fn prepare_all(&self, inputs: Vec<PrepareInput>) -> Result<Vec<(File, Vec<Item>)>> {
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

    pub(super) fn ensure_embedding_groups_with(
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

    pub(super) fn embed_one(&self, input: &str, query: bool) -> Result<String> {
        self.ensure_embeddings(&[input.into()], query)?;
        Ok(Database::embedding_key(
            &self.providers.vector().profile(),
            query,
            input,
        ))
    }

    pub(super) fn description_generation_profile(&self) -> Value {
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
            self.verify_source_unchanged(file)?;
        }
        self.write_index(|engine| {
            engine.db.apply(&changed, &[], checkpoint.as_deref())?;
            engine
                .db
                .set_meta("description_generation_profile", &profile)?;
            engine.db.set_meta(
                "description_profile",
                &engine.providers.llm().profile().to_string(),
            )?;
            Ok(())
        })?;
        self.load()?;
        self.artifacts.backfill_remote(&self.db);
        Ok(
            json!({"filesPrepared":changed.len(),"descriptionCount":self.items.iter().filter(|i|i.data["description"].is_string()).count(),"fileDescriptionCount":self.files.values().filter(|f|f.description.is_some()).count()}),
        )
    }
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
