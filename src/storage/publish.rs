//! Two-phase publication: reusable artifacts commit before workspace bindings.

use super::*;
use super::{snapshot::*, units::*};

impl Database {
    /// A refresh commits live records and its generation together. Completed model
    /// and parse artifacts were already committed independently for retry reuse.
    pub fn apply(
        &mut self,
        changed: &[(File, Vec<Item>)],
        removed: &[String],
        checkpoint: Option<&str>,
    ) -> Result<bool> {
        let changed: Vec<_> = changed
            .iter()
            .map(|(f, items)| (f.clone(), items.clone(), None))
            .collect();
        self.publish(&changed, removed, checkpoint, STRUCTURE_PARSER_VERSION)
    }

    /// Publish parse results and pending search units atomically, without any
    /// model calls. Unit identities use the same occurrence counting as engine.
    pub fn apply_structure(
        &mut self,
        changed: &[(File, ParsedFile)],
        removed: &[String],
        checkpoint: Option<&str>,
    ) -> Result<bool> {
        self.apply_structure_with_version(changed, removed, checkpoint, STRUCTURE_PARSER_VERSION)
    }

    pub fn apply_structure_with_version(
        &mut self,
        changed: &[(File, ParsedFile)],
        removed: &[String],
        checkpoint: Option<&str>,
        version: &str,
    ) -> Result<bool> {
        ensure!(!version.is_empty(), "Empty structure parser version");
        let mut records = Vec::with_capacity(changed.len());
        for (file, parsed) in changed {
            let mut file = file.clone();
            if file.description.is_none()
                && let Some((source_hash, text, embedding)) =
                    description(&self.conn, "file", &file.path, "")?
            {
                file.description = Some(text);
                file.description_hash = source_hash;
                file.description_embedding = embedding;
            }
            if let Some(text) = &parsed.description {
                if file.description.as_ref() != Some(text) {
                    file.description_embedding = None;
                }
                file.description = Some(text.clone());
                file.description_hash = Some(format!("source:{}", hash(text)));
            } else if file
                .description_hash
                .as_deref()
                .is_some_and(|hash| hash.starts_with("source:"))
            {
                file.description = None;
                file.description_hash = None;
                file.description_embedding = None;
            }
            file.errors.extend(parsed.errors.iter().map(|e| json!({"path":file.path,"code":"parse-error",
                "message":e.message,"startLine":e.start_line,"endLine":e.end_line,"sourceMode":file.source_mode})));
            let items = parsed_items(&file, parsed)?;
            records.push((file, items, Some(parsed.structure.clone())));
        }
        self.publish(&records, removed, checkpoint, version)
    }

    fn publish(
        &mut self,
        changed: &[(File, Vec<Item>, Option<FileStructure>)],
        removed: &[String],
        checkpoint: Option<&str>,
        version: &str,
    ) -> Result<bool> {
        let old_checkpoint = self.meta("checkpoint")?;
        let dirty = !changed.is_empty() || !removed.is_empty();
        let policy = self.meta("selection_policy")?;
        if !dirty
            && old_checkpoint.as_deref() == checkpoint
            && self.meta("snapshot_policy")? == policy
        {
            return Ok(false);
        }
        let generation = self
            .generation()?
            .checked_add(u64::from(dirty))
            .context("Index generation exhausted")?;
        let profile: Option<Value> = self
            .meta("active_embedding_profile")?
            .map(|v| serde_json::from_str(&v))
            .transpose()?;
        // WAL cannot atomically commit writes across attached databases. Make
        // reusable content durable first; the workspace transaction only binds
        // already committed artifacts and may safely leave orphans on failure.
        if !changed.is_empty() {
            let mut global = cache::open_store(&self.global_path, false)?;
            global.execute_batch("PRAGMA synchronous=FULL;")?;
            persist_publication_artifacts(&mut global, changed)?;
        }
        let tx = self.conn.transaction()?;
        for path in removed {
            tx.execute("DELETE FROM files WHERE path=?", [path])?;
        }
        for (file, items, structure) in changed {
            if structure.is_some()
                && tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM files WHERE path=? AND hash<>?)",
                    params![file.path, file.hash],
                    |row| row.get::<_, bool>(0),
                )?
            {
                // Callable generation uses the full file/transcript, not just
                // the callable body. A changed context invalidates that binding.
                tx.execute(
                    "DELETE FROM descriptions WHERE scope='callable' AND path=?",
                    [&file.path],
                )?;
            }
            write_file(&tx, file, structure.is_some())?;
            if let Some(structure) = structure {
                // IDs are file-local parser IDs. Relink all units after replacing
                // symbols; deferred foreign keys keep the transaction atomic.
                tx.execute(
                    "UPDATE search_units SET symbol_id=NULL WHERE path=?",
                    [&file.path],
                )?;
                tx.execute("DELETE FROM symbols WHERE path=?", [&file.path])?;
                write_structure(&tx, &file.path, structure)?;
                tx.execute(
                    "UPDATE files SET parser_version=?,structure_hash=? WHERE path=?",
                    params![version, file.hash, file.path],
                )?;
            } else {
                let current: bool = tx.query_row("SELECT parser_version IS NOT NULL AND structure_hash=hash FROM files WHERE path=?", [&file.path], |r| r.get(0))?;
                if !current {
                    tx.execute(
                        "UPDATE search_units SET symbol_id=NULL WHERE path=?",
                        [&file.path],
                    )?;
                    tx.execute("DELETE FROM symbols WHERE path=?", [&file.path])?;
                }
            }
            let nodes = read_structure(&tx, &file.path)?;
            write_units(
                &tx,
                file,
                items,
                &nodes,
                structure.is_some(),
                profile.as_ref(),
            )?;
        }
        tx.execute("INSERT INTO metadata VALUES('generation',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[generation.to_string()])?;
        tx.execute("DELETE FROM metadata WHERE key='checkpoint'", [])?;
        if let Some(commit) = checkpoint {
            tx.execute("INSERT INTO metadata VALUES('checkpoint',?)", [commit])?;
        }
        if dirty {
            tx.execute("DELETE FROM search_cache", [])?;
        }
        // Only the digest is published. The manifest body is hashed as a stream
        // and never stored: nothing reads it back, and persisting an O(repository)
        // document per publication dominated the global store's size.
        let digest = snapshot_digest(&tx)?;
        tx.execute("INSERT INTO metadata VALUES('snapshot',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [digest])?;
        tx.execute("DELETE FROM metadata WHERE key='snapshot_policy'", [])?;
        if let Some(policy) = policy {
            tx.execute("INSERT INTO metadata VALUES('snapshot_policy',?)", [policy])?;
        }
        tx.commit()?;
        Ok(dirty)
    }

    pub fn search_cache(&self, key: &str) -> Result<Option<Vec<Value>>> {
        let value: Option<String> = self
            .conn
            .query_row("SELECT value FROM search_cache WHERE key=?", [key], |r| {
                r.get(0)
            })
            .optional()?;
        value
            .map(|v| serde_json::from_str(&v).context("Corrupt search cache"))
            .transpose()
    }

    pub fn put_search_cache(&self, key: &str, value: &[Value]) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO search_cache VALUES(?,?)",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }
}

pub(super) fn persist_publication_artifacts(
    global: &mut Connection,
    changed: &[(File, Vec<Item>, Option<FileStructure>)],
) -> Result<()> {
    let tx = global.transaction()?;
    for (file, items, structure) in changed {
        ensure!(
            hash(&file.source) == file.hash,
            "Source content hash mismatch for {}",
            file.path
        );
        tx.execute(
            "INSERT OR IGNORE INTO sources VALUES(?,?)",
            params![file.hash, file.source],
        )?;
        let mut descriptions = Vec::new();
        descriptions.extend(file.description.as_deref());
        for item in items {
            let payload = unit_payload(&item.data)?;
            tx.execute(
                "INSERT OR IGNORE INTO cache(kind,key,value) VALUES('unit-content',?,?)",
                params![hash(&payload), payload],
            )?;
            descriptions.extend(item.data["description"].as_str());
        }
        if let Some(structure) = structure {
            descriptions.extend(
                structure
                    .nodes
                    .iter()
                    .filter_map(|node| node.description.as_deref()),
            );
        }
        for text in descriptions {
            tx.execute(
                "INSERT OR IGNORE INTO description_content VALUES(?,?)",
                params![hash(text), text],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn description_reference(conn: &Connection, text: &str) -> Result<String> {
    let key = hash(text);
    ensure!(
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM global.description_content WHERE hash=? AND content=?)",
            params![key, text],
            |row| row.get::<_, bool>(0)
        )?,
        "Missing or corrupt global description content artifact {key}"
    );
    Ok(key)
}

pub(super) fn validate_embedding_reference(conn: &Connection, key: Option<&str>) -> Result<()> {
    if let Some(key) = key.filter(|k| !k.is_empty()) {
        let bytes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT vector FROM global.embeddings WHERE key=?",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        let bytes = bytes.with_context(|| format!("Missing global embedding artifact {key}"))?;
        decode(&bytes)?;
    }
    Ok(())
}
