//! Cached artifacts, embedding records, and committed record reads.

use super::units::*;
use super::*;

impl Database {
    pub fn cache(&self, kind: &str, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM global.cache WHERE kind=? AND key=?",
                params![kind, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn cache_put(&self, kind: &str, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO global.cache VALUES(?,?,?) ON CONFLICT(kind,key) DO UPDATE SET value=excluded.value", params![kind,key,value])?;
        Ok(())
    }

    pub(crate) fn description_content(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT content FROM global.description_content WHERE hash=?",
                [key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub(crate) fn put_description_content(&self, key: &str, value: &str) -> Result<()> {
        ensure!(hash(value) == key, "Description content hash mismatch");
        self.conn.execute("INSERT INTO global.description_content VALUES(?,?) ON CONFLICT(hash) DO UPDATE SET content=excluded.content WHERE content<>excluded.content", params![key,value])?;
        Ok(())
    }

    pub(crate) fn put_description_artifact(
        &self,
        key: &str,
        value: &str,
        contents: &[(String, String)],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (content_hash, content) in contents {
            ensure!(
                hash(content) == *content_hash,
                "Description content hash mismatch"
            );
            tx.execute("INSERT INTO global.description_content VALUES(?,?) ON CONFLICT(hash) DO UPDATE SET content=excluded.content WHERE content<>excluded.content", params![content_hash,content])?;
        }
        tx.execute("INSERT INTO global.cache VALUES('description',?,?) ON CONFLICT(kind,key) DO UPDATE SET value=excluded.value", params![key,value])?;
        tx.commit()?;
        Ok(())
    }

    pub fn embedding_key(profile: &Value, query: bool, input: &str) -> String {
        hash(json!([profile, if query { "query" } else { "document" }, input]).to_string())
    }

    pub fn embedding(&self, key: &str) -> Result<Option<Vec<f32>>> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT vector FROM global.embeddings WHERE key=?",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        bytes.map(|b| decode(&b)).transpose()
    }

    pub fn embedding_exists(&self, key: &str) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM global.embeddings WHERE key=?)")?
            .query_row([key], |row| row.get(0))?)
    }

    pub fn put_embedding(&self, key: &str, vector: &[f32]) -> Result<()> {
        ensure!(
            !vector.is_empty() && vector.iter().all(|v| v.is_finite()),
            "Invalid embedding"
        );
        let bytes: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.conn.execute(
            "INSERT OR IGNORE INTO global.embeddings VALUES(?,?)",
            params![key, bytes],
        )?;
        Ok(())
    }

    /// Eagerly loads every source. Not used by the engine, which reads
    /// `file_records` and sources on demand; retained for the integration suites.
    pub fn files(&self) -> Result<Vec<File>> {
        let mut files = self.file_records()?;
        for file in &mut files {
            file.source = self.source(&file.hash)?;
        }
        Ok(files)
    }

    /// Metadata-only records. Source bytes are loaded and validated on demand.
    pub fn file_records(&self) -> Result<Vec<File>> {
        // Two scans for the whole snapshot rather than several statements per file.
        let mut diagnostics = HashMap::<String, Vec<String>>::new();
        let mut stmt = self
            .conn
            .prepare_cached("SELECT path,data FROM diagnostics ORDER BY path,ordinal")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (path, data) = row?;
            diagnostics.entry(path).or_default().push(data);
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT f.path,f.hash,f.language,f.source_mode,s.hash IS NOT NULL,
                    d.path IS NOT NULL,d.source_hash,d.content_hash,c.content,d.embedding_key
             FROM files f LEFT JOIN global.sources s ON s.hash=f.hash
             LEFT JOIN descriptions d ON d.scope='file' AND d.path=f.path AND d.identity=''
             LEFT JOIN global.description_content c ON c.hash=d.content_hash
             ORDER BY f.path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                File {
                    path: r.get(0)?,
                    source: String::new(),
                    hash: r.get(1)?,
                    language: r.get(2)?,
                    source_mode: r.get(3)?,
                    description: None,
                    description_hash: r.get(6)?,
                    description_embedding: r.get(9)?,
                    errors: Vec::new(),
                },
                r.get::<_, bool>(4)?,
                r.get::<_, bool>(5)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        })?;
        rows.map(|r| {
            let (mut file, source_exists, described, content_hash, content) = r?;
            ensure!(
                source_exists,
                "Missing global source artifact for {} ({}); rebuild the index",
                file.path,
                file.hash
            );
            if described {
                file.description = Some(description_text(
                    content_hash.as_deref().unwrap_or_default(),
                    content,
                )?);
            }
            file.errors = diagnostics
                .remove(&file.path)
                .unwrap_or_default()
                .iter()
                .map(|data| Ok(serde_json::from_str(data)?))
                .collect::<Result<_>>()?;
            Ok(file)
        })
        .collect()
    }

    pub fn source(&self, hash: &str) -> Result<String> {
        let source: Option<String> = self
            .conn
            .query_row(
                "SELECT source FROM global.sources WHERE hash=?",
                [hash],
                |row| row.get(0),
            )
            .optional()?;
        let source = source
            .with_context(|| format!("Missing global source artifact {hash}; rebuild the index"))?;
        ensure!(
            crate::hash(&source) == hash,
            "Corrupt global source bytes for {hash}; rebuild the index"
        );
        Ok(source)
    }

    /// File description vectors, like unit vectors, must be projected for the
    /// requested profile. Stale file descriptions remain available by policy.
    /// Not used by the engine; retained for the integration suites.
    pub fn files_for_profile(&self, profile: &Value) -> Result<Vec<File>> {
        let mut files = self.files()?;
        for file in &mut files {
            file.description_embedding = file
                .description
                .as_deref()
                .map(|input| self.cached_embedding_key(profile, input))
                .transpose()?
                .flatten();
        }
        Ok(files)
    }

    /// The latest published association for the current input. Profile-specific
    /// searches should use items_for_profile instead.
    /// Not used by the engine; retained for the integration suites.
    pub fn items(&self) -> Result<Vec<Item>> {
        self.read_items(None)
    }

    /// A missing vector is represented by an empty embedding string. This also
    /// discovers durable vectors completed before a failed publication, without
    /// requiring an association row or another provider call.
    pub fn items_for_profile(&self, profile: &Value) -> Result<Vec<Item>> {
        self.read_items(Some(profile))
    }

    pub fn set_projection_profile(&self, profile: &Value) -> Result<()> {
        let value = profile.to_string();
        if self.meta("active_embedding_profile")?.as_deref() != Some(&value) {
            let tx = self.conn.unchecked_transaction()?;
            tx.execute("INSERT INTO metadata VALUES('active_embedding_profile',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [value])?;
            tx.execute("DELETE FROM search_cache", [])?;
            tx.commit()?;
        }
        Ok(())
    }

    fn cached_embedding_key(&self, profile: &Value, input: &str) -> Result<Option<String>> {
        let key = Self::embedding_key(profile, false, input);
        Ok(self.embedding_exists(&key)?.then_some(key))
    }

    fn read_items(&self, profile: Option<&Value>) -> Result<Vec<Item>> {
        // One scan resolves each unit's shared payload and callable description;
        // the remaining per-unit vector lookups reuse cached statements.
        let mut stmt = self.conn.prepare_cached(
            "SELECT u.id,u.path,u.identity,u.kind,u.data,u.embedding_input_hash,u.content_hash,p.value,
                    d.path IS NOT NULL,d.source_hash,d.content_hash,c.content
             FROM search_units u
             LEFT JOIN global.cache p ON p.kind='unit-content' AND p.key=u.content_hash
             LEFT JOIN descriptions d ON d.scope='callable' AND d.path=u.path AND d.identity=u.identity
             LEFT JOIN global.description_content c ON c.hash=d.content_hash
             ORDER BY u.id",
        )?;
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, bool>(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, Option<String>>(11)?,
            ))
        })?
        .map(|r| {
            let (
                id,
                path,
                identity,
                kind,
                data,
                input_hash,
                content_hash,
                payload,
                described,
                description_source,
                description_hash,
                description,
            ) = r?;
            let mut data: Value = serde_json::from_str(&data)?;
            ensure!(data.is_object(), "Invalid stored search unit");
            data.as_object_mut()
                .unwrap()
                .extend(decode_unit_content(&content_hash, payload)?);
            let stored_description = if described {
                let text =
                    description_text(description_hash.as_deref().unwrap_or_default(), description)?;
                (description_source.as_deref() == data["sourceHash"].as_str()).then_some(text)
            } else {
                None
            };
            if let Some(text) = &stored_description {
                data["description"] = json!(text);
            } else if data.get("description").is_some() {
                data["description"] = Value::Null;
            }
            let embedding = if kind == "symbol-description" {
                None
            } else if let Some(profile) = profile {
                match data["embeddingInput"].as_str() {
                    Some(input) if hash(input) == input_hash => {
                        self.cached_embedding_key(profile, input)?
                    }
                    _ => None,
                }
            } else {
                latest_embedding(&self.conn, id, "code", &input_hash)?
            }
            .unwrap_or_default();
            let description_embedding = if let Some(text) = stored_description {
                if let Some(profile) = profile {
                    self.cached_embedding_key(profile, &text)?
                } else {
                    latest_embedding(&self.conn, id, "description", &hash(&text))?
                }
            } else {
                None
            };
            Ok(Item {
                id,
                path,
                identity,
                kind,
                data,
                embedding,
                description_embedding,
            })
        })
        .collect()
    }

    pub fn paths(&self) -> Result<Vec<String>> {
        Ok(self
            .conn
            .prepare_cached("SELECT path FROM files ORDER BY path")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn structure_current(&self, path: &str, source_hash: &str, version: &str) -> Result<bool> {
        Ok(self.conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM files WHERE path=? AND hash=? AND structure_hash=? AND parser_version=?)")?
            .query_row(params![path, source_hash, source_hash, version], |r| r.get(0))?)
    }

    /// Structures for every file current for `version`, loaded with bulk
    /// scans instead of per-file statements. Non-current files are omitted.
    pub fn all_structures(
        &self,
        version: &str,
    ) -> Result<std::collections::BTreeMap<String, FileStructure>> {
        read_all_structures(&self.conn, version)
    }

    /// Read one file's stored structure without a currency check. Callers
    /// that already verified currency avoid a second existence query.
    pub(crate) fn read_structure(&self, path: &str) -> Result<FileStructure> {
        read_structure(&self.conn, path)
    }

    pub fn missing_structure_paths(&self, version: &str) -> Result<Vec<String>> {
        Ok(self.conn.prepare_cached("SELECT path FROM files WHERE parser_version IS NULL OR parser_version<>? OR structure_hash IS NULL OR structure_hash<>hash ORDER BY path")?
            .query_map([version], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Missing/unpublished structure is an error, distinct from a parsed file
    /// with zero declarations. No parsing or providers occur in this accessor.
    pub fn structure(&self, path: &str) -> Result<FileStructure> {
        let current: bool = self.conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM files WHERE path=? AND parser_version IS NOT NULL AND structure_hash=hash)")?
            .query_row([path], |r| r.get(0))?;
        ensure!(current, "No current structure for {path}");
        read_structure(&self.conn, path)
    }

    /// Saved declaration descriptions keyed by their structural symbol, without
    /// loading vectors or requiring a description provider for `map`.
    pub fn symbol_descriptions(&self, path: &str) -> Result<HashMap<usize, String>> {
        let mut stmt = self.conn.prepare_cached("SELECT s.symbol_id,s.data,d.source_hash,d.content_hash,c.content
            FROM search_units s JOIN descriptions d ON d.scope='callable' AND d.path=s.path AND d.identity=s.identity
            LEFT JOIN global.description_content c ON c.hash=d.content_hash
            WHERE s.path=? AND s.kind IN ('function','symbol-description') AND s.symbol_id IS NOT NULL ORDER BY s.id")?;
        let rows = stmt.query_map([path], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut descriptions = HashMap::new();
        for row in rows {
            let (id, data, source_hash, content_hash, description) = row?;
            let description = description_text(&content_hash, description)?;
            let data: Value = serde_json::from_str(&data)?;
            if source_hash.as_deref() == data["sourceHash"].as_str() {
                descriptions.insert(usize::try_from(id)?, description);
            }
        }
        Ok(descriptions)
    }
}
