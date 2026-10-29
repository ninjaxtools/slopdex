//! Repository change discovery and index refresh orchestration.

use super::*;
use super::{discovery::*, prepare::*};

impl Engine {
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
            self.verify_source_unchanged(file)?;
        }
        self.write_index(|engine| engine.db.apply(&changed, &[], checkpoint.as_deref()))?;
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

    pub(super) fn refresh_structure_with(
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
            if !has_read_error(file) {
                self.verify_source_unchanged(file)?;
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
                self.write_index(|engine| {
                    engine.save_refresh_state(&dirty, &fingerprints, &policy, &directories)?;
                    engine.db.apply_structure(&[], &[], checkpoint.as_deref())
                })?;
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
        let published = self.write_index(|engine| {
            engine
                .db
                .set_meta("dirty_paths", &serde_json::to_string(&pending_dirty)?)?;
            engine
                .db
                .set_meta("selection_policy", &selection_contract(&engine.config))?;
            engine
                .db
                .set_meta("source_directories", &serde_json::to_string(&directories)?)?;
            let published = engine
                .db
                .apply_structure(&changed, &removed, checkpoint.as_deref())?;
            engine.save_refresh_state(&dirty, &fingerprints, &policy, &directories)?;
            registry::observe_checkpoint(&engine.db, &engine.root, checkpoint.as_deref())?;
            Ok(published)
        })?;
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
    pub(super) fn retain_relevant_dirty(
        &self,
        dirty: &mut HashSet<String>,
        eligibility: &Eligibility,
    ) {
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
}
