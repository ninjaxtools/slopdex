use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    cache, hash, map,
    parse::{FileStructure, ParsedFile, StructureNode},
};

/// Bump when the structure or search-unit extraction contract changes.
pub const STRUCTURE_PARSER_VERSION: &str = "structure-v9-description-identities";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files(
 path TEXT PRIMARY KEY,hash TEXT NOT NULL,language TEXT NOT NULL,
 source_mode TEXT NOT NULL,parser_version TEXT,structure_hash TEXT);
CREATE TABLE IF NOT EXISTS symbols(
 path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,id INTEGER NOT NULL,parent_id INTEGER,
 ordinal INTEGER NOT NULL,language TEXT NOT NULL,kind TEXT NOT NULL,name TEXT NOT NULL,
 qualified_name TEXT NOT NULL,signature TEXT NOT NULL,start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,
 start_line INTEGER NOT NULL,start_column INTEGER NOT NULL,end_line INTEGER NOT NULL,end_column INTEGER NOT NULL,
 metadata TEXT NOT NULL,PRIMARY KEY(path,id),
 FOREIGN KEY(path,parent_id) REFERENCES symbols(path,id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX IF NOT EXISTS symbols_qualified_name ON symbols(qualified_name);
CREATE INDEX IF NOT EXISTS symbols_parent ON symbols(path,parent_id,ordinal);
CREATE INDEX IF NOT EXISTS symbols_kind ON symbols(kind,path);
CREATE INDEX IF NOT EXISTS symbols_path_order ON symbols(path,ordinal);
CREATE TABLE IF NOT EXISTS symbol_names(
 path TEXT NOT NULL,symbol_id INTEGER NOT NULL,ordinal INTEGER NOT NULL,name TEXT NOT NULL,
 PRIMARY KEY(path,symbol_id,ordinal),FOREIGN KEY(path,symbol_id) REFERENCES symbols(path,id) ON DELETE CASCADE);
CREATE INDEX IF NOT EXISTS symbol_names_name ON symbol_names(name);
CREATE TABLE IF NOT EXISTS search_units(
 id INTEGER PRIMARY KEY AUTOINCREMENT,path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
 identity TEXT NOT NULL UNIQUE,kind TEXT NOT NULL,symbol_id INTEGER,data TEXT NOT NULL,
  embedding_input_hash TEXT NOT NULL,content_hash TEXT NOT NULL,
 FOREIGN KEY(path,symbol_id) REFERENCES symbols(path,id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX IF NOT EXISTS search_units_path ON search_units(path);
CREATE TABLE IF NOT EXISTS unit_embeddings(
 unit_id INTEGER NOT NULL REFERENCES search_units(id) ON DELETE CASCADE,
 role TEXT NOT NULL CHECK(role IN ('code','description')),profile_key TEXT NOT NULL,input_hash TEXT NOT NULL,
  embedding_key TEXT NOT NULL,
 PRIMARY KEY(unit_id,role,profile_key,input_hash));
CREATE TABLE IF NOT EXISTS descriptions(
 scope TEXT NOT NULL CHECK(scope IN ('file','callable')),path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
  identity TEXT NOT NULL,source_hash TEXT,content_hash TEXT NOT NULL,embedding_key TEXT,
 PRIMARY KEY(scope,path,identity));
CREATE TABLE IF NOT EXISTS diagnostics(
 path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,ordinal INTEGER NOT NULL,
 code TEXT,message TEXT,start_line INTEGER,end_line INTEGER,data TEXT NOT NULL,PRIMARY KEY(path,ordinal));
CREATE TABLE IF NOT EXISTS search_cache(key TEXT PRIMARY KEY,value TEXT NOT NULL);
";

/// SQLite is authoritative. USearch files are disposable materialized indexes.
pub struct Database {
    pub conn: Connection,
    pub path: PathBuf,
    global_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct File {
    pub path: String,
    pub hash: String,
    pub source: String,
    pub language: String,
    pub source_mode: String,
    pub description: Option<String>,
    pub description_hash: Option<String>,
    pub description_embedding: Option<String>,
    pub errors: Vec<Value>,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub id: u64,
    pub path: String,
    pub identity: String,
    pub kind: String,
    pub data: Value,
    pub embedding: String,
    pub description_embedding: Option<String>,
}

impl Database {
    pub fn open(path: &Path, root: &Path, _profile: &Value, force: bool) -> Result<Self> {
        Self::open_with_config(path, root, &json!({}), force)
    }

    pub fn open_with_config(path: &Path, root: &Path, config: &Value, force: bool) -> Result<Self> {
        Self::open_internal(path, root, config, force, false)
    }

    pub fn open_readonly(path: &Path, root: &Path) -> Result<Self> {
        Self::open_readonly_with_config(path, root, &json!({}))
    }

    pub fn open_readonly_with_config(path: &Path, root: &Path, config: &Value) -> Result<Self> {
        Self::open_internal(path, root, config, false, true)
    }

    pub fn global_path(&self) -> &Path {
        &self.global_path
    }

    fn open_internal(
        path: &Path,
        root: &Path,
        config: &Value,
        force: bool,
        readonly: bool,
    ) -> Result<Self> {
        let conn = if readonly {
            Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )?
        } else {
            Connection::open(path)?
        };
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        let has_tables: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%')", [], |r| r.get(0))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            (!has_tables && version == 0) || (has_tables && version == 4),
            "Unsupported index table layout. Remove the existing SQLite index at {} and rebuild, or use --index with a new path.",
            path.display()
        );
        let identity: Option<Value> = if has_tables {
            let value: String = conn.query_row("SELECT value FROM metadata WHERE key='identity'", [], |r| r.get(0))
                .context("Unsupported index table layout: missing identity; rebuild using a new index path")?;
            let value: Value = serde_json::from_str(&value).context(
                "Unsupported index table layout: invalid identity; rebuild using a new index path",
            )?;
            ensure!(
                value["schema"] == 4 && value["root"].is_string(),
                "Unsupported index table layout: rebuild using a new index path"
            );
            Some(value)
        } else {
            None
        };
        if has_tables {
            for sql in [
                "SELECT key,value FROM metadata LIMIT 0",
                "SELECT path,hash,language,source_mode,parser_version,structure_hash FROM files LIMIT 0",
                "SELECT path,id,parent_id,ordinal,language,kind,name,qualified_name,signature,start_byte,end_byte,start_line,start_column,end_line,end_column,metadata FROM symbols LIMIT 0",
                "SELECT path,symbol_id,ordinal,name FROM symbol_names LIMIT 0",
                "SELECT id,path,identity,kind,symbol_id,data,embedding_input_hash,content_hash FROM search_units LIMIT 0",
                "SELECT unit_id,role,profile_key,input_hash,embedding_key FROM unit_embeddings LIMIT 0",
                "SELECT scope,path,identity,source_hash,content_hash,embedding_key FROM descriptions LIMIT 0",
                "SELECT path,ordinal,code,message,start_line,end_line,data FROM diagnostics LIMIT 0",
                "SELECT key,value FROM search_cache LIMIT 0",
            ] {
                conn.prepare(sql)
                    .context("Unsupported index table layout. Rebuild using a new index path.")?;
            }
        }
        let expected = json!({"schema":4,"root":root});
        let incompatible_root = identity
            .as_ref()
            .is_some_and(|v| v["root"] != expected["root"]);
        if incompatible_root && !force {
            bail!(
                "Incompatible index root. Use --force-reindex to rebuild live state (artifact caches are retained)."
            );
        }
        ensure!(
            !readonly || has_tables,
            "Index is not initialized; reopen for writing"
        );
        let binding: Option<String> = if has_tables {
            conn.query_row(
                "SELECT value FROM metadata WHERE key='global_path'",
                [],
                |r| r.get(0),
            )
            .optional()?
        } else {
            None
        };
        ensure!(
            !has_tables || binding.is_some(),
            "Missing global artifact store binding; rebuild using a new index path"
        );
        let explicit = config
            .get("artifactCachePath")
            .is_some_and(|v| !v.is_null());
        let global_path = match binding.as_ref().filter(|_| !explicit) {
            Some(path) => PathBuf::from(path),
            None => cache::store_path(config)?,
        };
        ensure!(
            binding.as_ref().is_none_or(|p| Path::new(p) == global_path),
            "Global artifact store conflict: index {} is bound to {}. Use a new index path to select artifactCachePath {}; --force-reindex does not change the store binding.",
            path.display(),
            binding.as_deref().unwrap_or_default(),
            global_path.display()
        );
        ensure!(
            std::path::absolute(path)? != global_path,
            "Workspace index and global artifact store must use different SQLite paths"
        );
        let global = cache::open_store(&global_path, readonly).with_context(|| {
            format!(
                "Cannot open global artifact store {}",
                global_path.display()
            )
        })?;
        let global_path = std::fs::canonicalize(&global_path)?;
        drop(global);
        if readonly {
            // URI mode makes the attached store genuinely read-only too.
            let uri = sqlite_readonly_uri(&global_path)?;
            conn.execute("ATTACH DATABASE ? AS global", [uri])?;
            conn.execute_batch("PRAGMA query_only=ON;")?;
            return Ok(Self {
                conn,
                path: path.to_owned(),
                global_path,
            });
        }
        conn.execute(
            "ATTACH DATABASE ? AS global",
            [global_path
                .to_str()
                .context("Global artifact store path is not UTF-8")?],
        )?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(SCHEMA)?;
        tx.execute_batch("PRAGMA user_version=4;")?;
        if incompatible_root {
            reset_live(&tx)?;
        }
        tx.execute("INSERT INTO metadata VALUES('identity',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [expected.to_string()])?;
        tx.execute("INSERT INTO metadata VALUES('global_path',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [global_path.to_str().context("Global artifact store path is not UTF-8")?])?;
        tx.execute(
            "INSERT OR IGNORE INTO metadata VALUES('incarnation',lower(hex(randomblob(16))))",
            [],
        )?;
        tx.commit()?;
        let db = Self {
            conn,
            path: path.to_owned(),
            global_path,
        };
        Ok(db)
    }

    pub fn reset(&self) -> Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        reset_live(&tx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM metadata WHERE key=?", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO metadata VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn generation(&self) -> Result<u64> {
        Ok(self
            .meta("generation")?
            .map(|v| v.parse())
            .transpose()?
            .unwrap_or(0))
    }

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
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM global.embeddings WHERE key=?)",
            [key],
            |row| row.get(0),
        )?)
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

    pub fn files(&self) -> Result<Vec<File>> {
        let mut files = self.file_records()?;
        for file in &mut files {
            file.source = self.source(&file.hash)?;
        }
        Ok(files)
    }

    /// Metadata-only records. Source bytes are loaded and validated on demand.
    pub fn file_records(&self) -> Result<Vec<File>> {
        let mut stmt = self
            .conn
            .prepare("SELECT f.path,f.hash,f.language,f.source_mode,s.hash IS NOT NULL FROM files f LEFT JOIN global.sources s ON s.hash=f.hash ORDER BY f.path")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                File {
                    path: r.get(0)?,
                    source: String::new(),
                    hash: r.get(1)?,
                    language: r.get(2)?,
                    source_mode: r.get(3)?,
                    description: None,
                    description_hash: None,
                    description_embedding: None,
                    errors: Vec::new(),
                },
                r.get::<_, bool>(4)?,
            ))
        })?;
        rows.map(|r| {
            let (mut file, source_exists) = r?;
            ensure!(
                source_exists,
                "Missing global source artifact for {} ({}); rebuild the index",
                file.path,
                file.hash
            );
            let description = description(&self.conn, "file", &file.path, "")?;
            file.description = description.as_ref().map(|d| d.1.clone());
            file.description_hash = description.as_ref().and_then(|d| d.0.clone());
            file.description_embedding = description.and_then(|d| d.2);
            file.errors = self
                .conn
                .prepare("SELECT data FROM diagnostics WHERE path=? ORDER BY ordinal")?
                .query_map([&file.path], |r| r.get::<_, String>(0))?
                .map(|r| Ok(serde_json::from_str(&r?)?))
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
        let mut stmt = self.conn.prepare(
            "SELECT id,path,identity,kind,data,embedding_input_hash,content_hash FROM search_units ORDER BY id",
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
            ))
        })?
        .map(|r| {
            let (id, path, identity, kind, data, input_hash, content_hash) = r?;
            let mut data: Value = serde_json::from_str(&data)?;
            ensure!(data.is_object(), "Invalid stored search unit");
            data.as_object_mut()
                .unwrap()
                .extend(unit_content(&self.conn, &content_hash)?);
            let stored_description = description(&self.conn, "callable", &path, &identity)?
                .filter(|d| d.0.as_deref() == data["sourceHash"].as_str());
            if let Some(d) = &stored_description {
                data["description"] = json!(d.1);
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
            let description_embedding = if let Some(d) = stored_description {
                if let Some(profile) = profile {
                    self.cached_embedding_key(profile, &d.1)?
                } else {
                    latest_embedding(&self.conn, id, "description", &hash(&d.1))?
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
            .prepare("SELECT path FROM files ORDER BY path")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn structure_current(&self, path: &str, source_hash: &str, version: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM files WHERE path=? AND hash=? AND structure_hash=? AND parser_version=?)",
            params![path, source_hash, source_hash, version], |r| r.get(0))?)
    }

    pub fn missing_structure_paths(&self, version: &str) -> Result<Vec<String>> {
        Ok(self.conn.prepare("SELECT path FROM files WHERE parser_version IS NULL OR parser_version<>? OR structure_hash IS NULL OR structure_hash<>hash ORDER BY path")?
            .query_map([version], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Missing/unpublished structure is an error, distinct from a parsed file
    /// with zero declarations. No parsing or providers occur in this accessor.
    pub fn structure(&self, path: &str) -> Result<FileStructure> {
        let current: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM files WHERE path=? AND parser_version IS NOT NULL AND structure_hash=hash)", [path], |r| r.get(0))?;
        ensure!(current, "No current structure for {path}");
        read_structure(&self.conn, path)
    }

    /// Saved declaration descriptions keyed by their structural symbol, without
    /// loading vectors or requiring a description provider for `map`.
    pub fn symbol_descriptions(&self, path: &str) -> Result<HashMap<usize, String>> {
        let mut stmt = self.conn.prepare("SELECT s.symbol_id,s.data,d.source_hash,c.content
            FROM search_units s JOIN descriptions d ON d.scope='callable' AND d.path=s.path AND d.identity=s.identity
            LEFT JOIN global.description_content c ON c.hash=d.content_hash
            WHERE s.path=? AND s.kind IN ('function','symbol-description') AND s.symbol_id IS NOT NULL ORDER BY s.id")?;
        let rows = stmt.query_map([path], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut descriptions = HashMap::new();
        for row in rows {
            let (id, data, source_hash, description) = row?;
            let data: Value = serde_json::from_str(&data)?;
            if source_hash.as_deref() == data["sourceHash"].as_str() {
                descriptions.insert(usize::try_from(id)?, description);
            }
        }
        Ok(descriptions)
    }

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
        let mut global = cache::open_store(&self.global_path, false)?;
        global.execute_batch("PRAGMA synchronous=FULL;")?;
        persist_publication_artifacts(&mut global, changed)?;
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
        let manifest = snapshot_manifest(&tx)?;
        let digest = hash(&manifest);
        // This independent WAL commit must precede the local snapshot pointer.
        // The attached store is only read by `tx`, so it holds no writer lock.
        global.execute(
            "INSERT OR IGNORE INTO snapshots VALUES(?,?)",
            params![digest, manifest],
        )?;
        ensure!(
            global.query_row(
                "SELECT EXISTS(SELECT 1 FROM snapshots WHERE digest=? AND manifest=?)",
                params![digest, manifest],
                |row| row.get::<_, bool>(0)
            )?,
            "Corrupt global snapshot artifact {digest}"
        );
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

fn reset_live(conn: &Connection) -> Result<()> {
    conn.execute_batch("DELETE FROM search_units; DELETE FROM files; DELETE FROM search_cache; DELETE FROM metadata WHERE key NOT IN ('identity','global_path');")?;
    conn.execute(
        "INSERT INTO metadata VALUES('incarnation',lower(hex(randomblob(16))))",
        [],
    )?;
    Ok(())
}

fn sqlite_readonly_uri(path: &Path) -> Result<String> {
    let mut uri = String::from("file:");
    for byte in path
        .to_str()
        .context("Global artifact store path is not UTF-8")?
        .bytes()
    {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(uri, "%{byte:02X}")?;
        }
    }
    uri.push_str("?mode=ro");
    Ok(uri)
}

fn persist_publication_artifacts(
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

fn description_reference(conn: &Connection, text: &str) -> Result<String> {
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

fn validate_embedding_reference(conn: &Connection, key: Option<&str>) -> Result<()> {
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

/// The digest excludes workspace root, row IDs, generation and git checkpoint.
/// It includes every normalized binding, parser contract and selection/provenance
/// policy, so identical published content has the same immutable manifest.
fn snapshot_manifest(conn: &Connection) -> Result<String> {
    let mut manifest = serde_json::Map::new();
    manifest.insert("schema".into(), json!(1));
    for (name, sql) in [
        (
            "files",
            "SELECT path,hash,language,source_mode,parser_version,structure_hash FROM files ORDER BY path",
        ),
        (
            "symbols",
            "SELECT path,id,parent_id,ordinal,language,kind,name,qualified_name,signature,start_byte,end_byte,start_line,start_column,end_line,end_column,metadata FROM symbols ORDER BY path,ordinal",
        ),
        (
            "names",
            "SELECT path,symbol_id,ordinal,name FROM symbol_names ORDER BY path,symbol_id,ordinal",
        ),
        (
            "units",
            "SELECT path,identity,kind,symbol_id,data,embedding_input_hash,content_hash FROM search_units ORDER BY path,identity",
        ),
        (
            "embeddings",
            "SELECT s.path,s.identity,e.role,e.profile_key,e.input_hash,e.embedding_key FROM unit_embeddings e JOIN search_units s ON s.id=e.unit_id ORDER BY s.path,s.identity,e.role,e.profile_key,e.input_hash",
        ),
        (
            "descriptions",
            "SELECT scope,path,identity,source_hash,content_hash,embedding_key FROM descriptions ORDER BY path,scope,identity",
        ),
        (
            "diagnostics",
            "SELECT path,ordinal,data FROM diagnostics ORDER BY path,ordinal",
        ),
        (
            "policy",
            "SELECT key,value FROM metadata WHERE key='selection_policy' ORDER BY key",
        ),
    ] {
        let mut stmt = conn.prepare(sql)?;
        let columns = stmt.column_count();
        let rows = stmt
            .query_map([], |r| {
                (0..columns)
                    .map(|i| {
                        Ok(match r.get_ref(i)? {
                            rusqlite::types::ValueRef::Null => Value::Null,
                            rusqlite::types::ValueRef::Integer(v) => json!(v),
                            rusqlite::types::ValueRef::Text(v) => json!(String::from_utf8_lossy(v)),
                            _ => {
                                return Err(rusqlite::Error::InvalidColumnType(
                                    i,
                                    name.into(),
                                    r.get_ref(i)?.data_type(),
                                ));
                            }
                        })
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        manifest.insert(name.into(), json!(rows));
    }
    // These parser bindings identify reusable extraction independent of paths.
    let bindings = conn.prepare("SELECT DISTINCT hash,language,parser_version FROM files WHERE parser_version IS NOT NULL AND structure_hash=hash ORDER BY hash,language,parser_version")?
        .query_map([], |r| Ok(json!({"sourceHash": r.get::<_, String>(0)?, "language": r.get::<_, String>(1)?, "parserVersion": r.get::<_, String>(2)?})))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    manifest.insert("parseBindings".into(), json!(bindings));
    Ok(serde_json::to_string(&manifest)?)
}

type Description = (Option<String>, String, Option<String>);

fn unit_payload(data: &Value) -> Result<String> {
    ensure!(data.is_object(), "Invalid stored search unit");
    let mut content = serde_json::Map::new();
    for field in ["source", "embeddingInput"] {
        if let Some(value) = data.get(field) {
            content.insert(field.into(), value.clone());
        }
    }
    Ok(serde_json::to_string(&content)?)
}

fn unit_content(conn: &Connection, key: &str) -> Result<serde_json::Map<String, Value>> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT value FROM global.cache WHERE kind='unit-content' AND key=?",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    let payload = payload.with_context(|| {
        format!("Missing global unit content artifact {key}; rebuild the index")
    })?;
    ensure!(
        hash(&payload) == key,
        "Corrupt global unit content artifact {key}: content hash mismatch"
    );
    let content: Value = serde_json::from_str(&payload)
        .with_context(|| format!("Invalid global unit content artifact {key}"))?;
    let Value::Object(content) = content else {
        bail!("Invalid global unit content artifact {key}: expected an object");
    };
    ensure!(
        content
            .keys()
            .all(|field| matches!(field.as_str(), "source" | "embeddingInput")),
        "Invalid global unit content artifact {key}: unexpected fields"
    );
    Ok(content)
}

fn description(
    conn: &Connection,
    scope: &str,
    path: &str,
    identity: &str,
) -> Result<Option<Description>> {
    Ok(conn.query_row("SELECT d.source_hash,c.content,d.embedding_key FROM descriptions d LEFT JOIN global.description_content c ON c.hash=d.content_hash WHERE d.scope=? AND d.path=? AND d.identity=?",
        params![scope, path, identity], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?)
}

fn latest_embedding(
    conn: &Connection,
    id: u64,
    role: &str,
    input_hash: &str,
) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT embedding_key FROM unit_embeddings WHERE unit_id=? AND role=? AND input_hash=? ORDER BY rowid DESC LIMIT 1",
        params![i64::try_from(id)?, role, input_hash], |r| r.get(0)).optional()?)
}

fn write_file(conn: &Connection, file: &File, structural: bool) -> Result<()> {
    let mut file = file.clone();
    if !structural
        && !file
            .description_hash
            .as_deref()
            .is_some_and(|hash| hash.starts_with("source:"))
        && conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM files WHERE path=? AND hash=?)",
            params![file.path, file.hash],
            |row| row.get::<_, bool>(0),
        )?
        && let Some((source_hash, text, embedding)) = description(conn, "file", &file.path, "")?
        && source_hash
            .as_deref()
            .is_some_and(|hash| hash.starts_with("source:"))
    {
        if file.description.as_ref() != Some(&text) {
            file.description_embedding = embedding;
        }
        file.description = Some(text);
        file.description_hash = source_hash;
    }
    ensure!(
        hash(&file.source) == file.hash,
        "Source content hash mismatch for {}",
        file.path
    );
    ensure!(
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM global.sources WHERE hash=? AND source=?)",
            params![file.hash, file.source],
            |row| row.get::<_, bool>(0)
        )?,
        "Missing or corrupt global source artifact for {} ({})",
        file.path,
        file.hash
    );
    conn.execute("INSERT INTO files(path,hash,language,source_mode) VALUES(?,?,?,?) ON CONFLICT(path) DO UPDATE SET
        hash=excluded.hash,language=excluded.language,source_mode=excluded.source_mode,
        parser_version=CASE WHEN files.hash=excluded.hash THEN files.parser_version END,
        structure_hash=CASE WHEN files.hash=excluded.hash THEN files.structure_hash END",
            params![file.path, file.hash, file.language, file.source_mode])?;
    conn.execute(
        "DELETE FROM descriptions WHERE scope='file' AND path=?",
        [&file.path],
    )?;
    if let Some(text) = &file.description {
        let content_hash = description_reference(conn, text)?;
        validate_embedding_reference(conn, file.description_embedding.as_deref())?;
        conn.execute("INSERT INTO descriptions(scope,path,identity,source_hash,content_hash,embedding_key) VALUES('file',?,'',?,?,?)",
            params![file.path, file.description_hash, content_hash, file.description_embedding])?;
    }
    conn.execute("DELETE FROM diagnostics WHERE path=?", [&file.path])?;
    for (ordinal, error) in file.errors.iter().enumerate() {
        conn.execute("INSERT INTO diagnostics(path,ordinal,code,message,start_line,end_line,data) VALUES(?,?,?,?,?,?,?)",
            params![file.path, i64::try_from(ordinal)?, error["code"].as_str(), error["message"].as_str(),
                error["startLine"].as_i64(), error["endLine"].as_i64(), error.to_string()])?;
    }
    Ok(())
}

fn write_structure(conn: &Connection, path: &str, structure: &FileStructure) -> Result<()> {
    for (ordinal, node) in structure.nodes.iter().enumerate() {
        let description_hash = node
            .description
            .as_deref()
            .map(|text| description_reference(conn, text))
            .transpose()?;
        let metadata = json!({"attributes":node.attributes,"imports":node.imports,"headingLevel":node.heading_level,"calls":node.calls,"descriptionHash":description_hash});
        conn.execute("INSERT INTO symbols(path,id,parent_id,ordinal,language,kind,name,qualified_name,signature,start_byte,end_byte,start_line,start_column,end_line,end_column,metadata) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![path, i64::try_from(node.id)?, node.parent_id.map(i64::try_from).transpose()?, i64::try_from(ordinal)?, node.language, node.kind, node.name, node.qualified_name,
                node.signature, i64::try_from(node.start_byte)?, i64::try_from(node.end_byte)?, i64::try_from(node.start_line)?, i64::try_from(node.start_column)?, i64::try_from(node.end_line)?, i64::try_from(node.end_column)?, metadata.to_string()])?;
        for (ordinal, name) in node.names.iter().enumerate() {
            conn.execute(
                "INSERT INTO symbol_names(path,symbol_id,ordinal,name) VALUES(?,?,?,?)",
                params![path, i64::try_from(node.id)?, i64::try_from(ordinal)?, name],
            )?;
        }
    }
    Ok(())
}

fn row_usize(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<usize> {
    let value: i64 = row.get(column)?;
    usize::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(column, value))
}

fn read_structure(conn: &Connection, path: &str) -> Result<FileStructure> {
    let mut names = HashMap::<usize, Vec<String>>::new();
    let mut names_stmt = conn.prepare(
        "SELECT symbol_id,name FROM symbol_names WHERE path=? ORDER BY symbol_id,ordinal",
    )?;
    for row in names_stmt.query_map([path], |r| Ok((row_usize(r, 0)?, r.get::<_, String>(1)?)))? {
        let (id, name) = row?;
        names.entry(id).or_default().push(name);
    }
    let mut stmt = conn.prepare("SELECT id,parent_id,language,kind,name,qualified_name,signature,start_byte,end_byte,start_line,start_column,end_line,end_column,metadata FROM symbols WHERE path=? ORDER BY ordinal")?;
    let rows = stmt.query_map([path], |r| {
        Ok((
            StructureNode {
                id: row_usize(r, 0)?,
                parent_id: r
                    .get::<_, Option<i64>>(1)?
                    .map(|_| row_usize(r, 1))
                    .transpose()?,
                language: r.get(2)?,
                kind: r.get(3)?,
                name: r.get(4)?,
                qualified_name: r.get(5)?,
                signature: r.get(6)?,
                start_byte: row_usize(r, 7)?,
                end_byte: row_usize(r, 8)?,
                start_line: row_usize(r, 9)?,
                start_column: row_usize(r, 10)?,
                end_line: row_usize(r, 11)?,
                end_column: row_usize(r, 12)?,
                ..Default::default()
            },
            r.get::<_, String>(13)?,
        ))
    })?;
    let nodes = rows
        .map(|r| {
            let (mut node, metadata) = r?;
            let metadata: Value = serde_json::from_str(&metadata)?;
            node.attributes = serde_json::from_value(metadata["attributes"].clone())?;
            node.imports = serde_json::from_value(metadata["imports"].clone())?;
            node.heading_level = serde_json::from_value(metadata["headingLevel"].clone())?;
            node.calls =
                serde_json::from_value(metadata.get("calls").cloned().unwrap_or(json!([])))?;
            node.description = metadata["descriptionHash"]
                .as_str()
                .map(|key| {
                    conn.query_row(
                        "SELECT content FROM global.description_content WHERE hash=?",
                        [key],
                        |r| r.get::<_, String>(0),
                    )
                    .with_context(|| format!("Missing global description content {key}"))
                })
                .transpose()?;
            node.names = names.remove(&node.id).unwrap_or_default();
            Ok(node)
        })
        .collect::<Result<_>>()?;
    Ok(FileStructure { nodes })
}

pub(crate) fn parsed_items(file: &File, parsed: &ParsedFile) -> Result<Vec<Item>> {
    let mut occurrences = HashMap::<&str, usize>::new();
    let mut callable_symbols = HashSet::new();
    let mut items = Vec::with_capacity(parsed.callables.len() + parsed.chunks.len());
    for callable in &parsed.callables {
        let occurrence = occurrences.entry(&callable.qualified_name).or_default();
        let identity = hash(
            json!([
                file.path,
                callable.qualified_name,
                callable.kind,
                *occurrence
            ])
            .to_string(),
        );
        *occurrence += 1;
        let mut data = serde_json::to_value(callable)?;
        data["path"] = json!(file.path);
        data["sourceMode"] = json!(file.source_mode);
        data["description"] = json!(callable.description);
        data["sourceDescription"] = json!(callable.description.is_some());
        if let Some(node) = map::matching_node(&parsed.structure, &data) {
            callable_symbols.insert(node.id);
        }
        items.push(Item {
            id: 0,
            path: file.path.clone(),
            identity,
            kind: "function".into(),
            data,
            embedding: String::new(),
            description_embedding: None,
        });
    }
    // Count every noncallable declaration, including those without prose, so
    // removing a preceding comment cannot renumber a later declaration.
    occurrences.clear();
    for node in &parsed.structure.nodes {
        if callable_symbols.contains(&node.id) {
            continue;
        }
        let occurrence = occurrences.entry(&node.qualified_name).or_default();
        // Description-only declarations (including overload signatures) have
        // their own occurrence sequence, separate from callable implementations.
        let identity = hash(
            json!([
                file.path,
                "symbol-description",
                node.qualified_name,
                node.kind,
                *occurrence
            ])
            .to_string(),
        );
        *occurrence += 1;
        if node.description.is_none() {
            continue;
        }
        let mut data = serde_json::to_value(node)?;
        data["path"] = json!(file.path);
        data["sourceMode"] = json!(file.source_mode);
        data["sourceDescription"] = json!(true);
        data["sourceHash"] = json!(hash(
            file.source
                .get(node.start_byte..node.end_byte)
                .context("Invalid described symbol source range")?
        ));
        items.push(Item {
            id: 0,
            path: file.path.clone(),
            identity,
            kind: "symbol-description".into(),
            data,
            embedding: String::new(),
            description_embedding: None,
        });
    }
    for (ordinal, chunk) in parsed.chunks.iter().enumerate() {
        let kind = if crate::formats::FormatGroup::for_language(&file.language)
            == Some(crate::formats::FormatGroup::Docs)
        {
            "markdown"
        } else {
            "document"
        };
        let mut data = serde_json::to_value(chunk)?;
        data["path"] = json!(file.path);
        data["sourceMode"] = json!(file.source_mode);
        items.push(Item {
            id: 0,
            path: file.path.clone(),
            identity: hash(json!([file.path, kind, ordinal]).to_string()),
            kind: kind.into(),
            data,
            embedding: String::new(),
            description_embedding: None,
        });
    }
    Ok(items)
}

fn symbol_for(item: &Item, structure: &FileStructure) -> Option<usize> {
    if item.kind == "document" {
        return None;
    }
    if item.kind == "symbol-description" {
        let start = item.data["startLine"].as_u64()? as usize;
        let end = item.data["endLine"].as_u64().unwrap_or(start as u64) as usize;
        return structure
            .nodes
            .iter()
            .filter(|node| {
                item.data["qualifiedName"].as_str() == Some(node.qualified_name.as_str())
                    && item.data["kind"]
                        .as_str()
                        .is_none_or(|kind| kind == node.kind)
            })
            .min_by_key(|node| {
                (
                    node.start_line.abs_diff(start) + node.end_line.abs_diff(end),
                    node.start_column
                        .abs_diff(item.data["startColumn"].as_u64().unwrap_or(1) as usize),
                    node.id,
                )
            })
            .map(|node| node.id);
    }
    if item.kind != "markdown" {
        return map::matching_node(structure, &item.data).map(|node| node.id);
    }
    let start = item.data["startLine"].as_u64()? as usize;
    let end = item.data["endLine"].as_u64().unwrap_or(start as u64) as usize;
    structure
        .nodes
        .iter()
        .filter(|node| node.kind == "heading" && node.start_line <= start && node.end_line >= start)
        .min_by_key(|node| {
            (
                node.start_line.abs_diff(start) + node.end_line.abs_diff(end),
                node.start_column
                    .abs_diff(item.data["startColumn"].as_u64().unwrap_or(1) as usize),
                node.id,
            )
        })
        .map(|node| node.id)
}

fn associate(
    conn: &Connection,
    id: i64,
    role: &str,
    profile: Option<&Value>,
    input: &str,
    key: &str,
) -> Result<()> {
    if key.is_empty() {
        return Ok(());
    }
    validate_embedding_reference(conn, Some(key))?;
    if let Some(profile) = profile {
        ensure!(
            key == Database::embedding_key(profile, false, input),
            "Embedding reference does not match projection profile/input"
        );
    }
    let profile_key = profile.map(|p| hash(p.to_string())).unwrap_or_default();
    let input_hash = hash(input);
    // Reinsert the same association to make rowid a deterministic latest-write
    // order for items(); other profiles/inputs survive.
    conn.execute(
        "DELETE FROM unit_embeddings WHERE unit_id=? AND role=? AND profile_key=? AND input_hash=?",
        params![id, role, profile_key, input_hash],
    )?;
    conn.execute("INSERT INTO unit_embeddings(unit_id,role,profile_key,input_hash,embedding_key) VALUES(?,?,?,?,?)", params![id, role, profile_key, input_hash, key])?;
    Ok(())
}

fn write_units(
    conn: &Connection,
    file: &File,
    items: &[Item],
    structure: &FileStructure,
    structural: bool,
    profile: Option<&Value>,
) -> Result<()> {
    let identities: HashSet<_> = items.iter().map(|i| i.identity.as_str()).collect();
    ensure!(
        identities.len() == items.len(),
        "Duplicate search unit identity for {}",
        file.path
    );
    let existing: Vec<(i64, String)> = conn
        .prepare("SELECT id,identity FROM search_units WHERE path=?")?
        .query_map([&file.path], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, identity) in existing {
        if !identities.contains(identity.as_str()) {
            conn.execute("DELETE FROM search_units WHERE id=?", [id])?;
            conn.execute(
                "DELETE FROM descriptions WHERE scope='callable' AND path=? AND identity=?",
                params![file.path, identity],
            )?;
        }
    }
    for item in items {
        ensure!(
            item.path == file.path && item.data.is_object(),
            "Invalid search unit for {}",
            file.path
        );
        let previous: Option<(String, String)> = conn
            .query_row(
                "SELECT path,data FROM search_units WHERE identity=?",
                [&item.identity],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        ensure!(
            previous.as_ref().is_none_or(|(p, _)| p == &file.path),
            "Search unit identity belongs to another file"
        );
        let old_source_description = previous
            .map(|(_, data)| serde_json::from_str::<Value>(&data))
            .transpose()?
            .is_some_and(|data| data["sourceDescription"] == true);
        let mut data = item.data.clone();
        let old_description = description(conn, "callable", &file.path, &item.identity)?;
        let symbol = symbol_for(item, structure);
        // Structural publication is authoritative for comment removal. During
        // semantic preparation, canonical source prose outranks generated prose.
        if !structural
            && matches!(item.kind.as_str(), "function" | "symbol-description")
            && let Some(text) = symbol
                .and_then(|id| structure.nodes.iter().find(|node| node.id == id))
                .and_then(|node| node.description.as_ref())
        {
            data["description"] = json!(text);
            data["sourceDescription"] = json!(true);
        }
        if structural
            && data["sourceDescription"] != true
            && !old_source_description
            && let Some(d) = &old_description
            && d.0.as_deref() == data["sourceHash"].as_str()
        {
            data["description"] = json!(d.1);
        }
        ensure!(
            data["sourceDescription"] != true || data["description"].is_string(),
            "Source description has no description text"
        );
        let input = data["embeddingInput"].as_str().unwrap_or("");
        let input_hash = hash(input);
        let mut stored_data = data.clone();
        let payload = unit_payload(&data)?;
        for field in ["source", "embeddingInput"] {
            stored_data.as_object_mut().unwrap().remove(field);
        }
        let content_hash = hash(&payload);
        ensure!(conn.query_row("SELECT EXISTS(SELECT 1 FROM global.cache WHERE kind='unit-content' AND key=? AND value=?)",
            params![content_hash, payload], |row| row.get::<_, bool>(0))?, "Missing or corrupt global unit content artifact {content_hash}");
        if stored_data.get("description").is_some() {
            stored_data["description"] = Value::Null;
        }
        conn.execute("INSERT INTO search_units(path,identity,kind,symbol_id,data,embedding_input_hash,content_hash) VALUES(?,?,?,?,?,?,?) ON CONFLICT(identity) DO UPDATE SET
            kind=excluded.kind,symbol_id=excluded.symbol_id,data=excluded.data,embedding_input_hash=excluded.embedding_input_hash,content_hash=excluded.content_hash",
            params![item.path, item.identity, item.kind, symbol.map(i64::try_from).transpose()?, stored_data.to_string(), input_hash, content_hash])?;
        let id: i64 = conn.query_row(
            "SELECT id FROM search_units WHERE identity=?",
            [&item.identity],
            |r| r.get(0),
        )?;
        if item.kind != "symbol-description" {
            associate(conn, id, "code", profile, input, &item.embedding)?;
        }
        conn.execute(
            "DELETE FROM descriptions WHERE scope='callable' AND path=? AND identity=?",
            params![item.path, item.identity],
        )?;
        if let Some(text) = data["description"].as_str() {
            let embedding = if !structural && data["description"] == item.data["description"] {
                item.description_embedding.as_deref()
            } else if structural {
                old_description
                    .as_ref()
                    .filter(|d| d.1 == text)
                    .and_then(|d| d.2.as_deref())
            } else {
                None
            };
            let content_hash = description_reference(conn, text)?;
            validate_embedding_reference(conn, embedding)?;
            conn.execute("INSERT INTO descriptions(scope,path,identity,source_hash,content_hash,embedding_key) VALUES('callable',?,?,?,?,?)",
                params![item.path, item.identity, data["sourceHash"].as_str(), content_hash, embedding])?;
            if !structural && let Some(key) = embedding {
                associate(conn, id, "description", profile, text, key)?;
            }
        } else {
            ensure!(
                item.description_embedding
                    .as_deref()
                    .is_none_or(str::is_empty),
                "Description vector has no description text"
            );
        }
    }
    Ok(())
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<f32>> {
    ensure!(
        !bytes.is_empty() && bytes.len().is_multiple_of(4),
        "Invalid stored vector length"
    );
    let values: Vec<_> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    ensure!(
        values.iter().all(|v| v.is_finite()),
        "Non-finite stored vector"
    );
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Result<(tempfile::TempDir, Database)> {
        let dir = tempfile::tempdir()?;
        let db = Database::open_with_config(
            &dir.path().join("index.sqlite"),
            dir.path(),
            &json!({"artifactCachePath": dir.path().join("global # é.sqlite")}),
            false,
        )?;
        db.put_embedding("code", &[0.6, 0.8])?;
        db.put_embedding("description", &[1.0, 0.0])?;
        Ok((dir, db))
    }

    fn record(path: &str) -> (File, Vec<Item>) {
        let file = File {
            path: path.into(),
            hash: hash("fn example() {}"),
            source: "fn example() {}".into(),
            language: "rust".into(),
            source_mode: "working-tree".into(),
            description: Some("A description".into()),
            description_hash: Some("older-hash".into()),
            description_embedding: Some("description".into()),
            errors: vec![json!({"message": "recoverable", "startLine": 2})],
        };
        let item = Item {
            id: 0,
            path: path.into(),
            identity: format!("{path}:example"),
            kind: "function".into(),
            data: json!({"qualifiedName": "example", "startLine": 1,"sourceHash":"callable-hash",
                "embeddingInput":"fn example() {}","description":"Callable description"}),
            embedding: "code".into(),
            description_embedding: Some("description".into()),
        };
        (file, vec![item])
    }

    fn snapshot(db: &Database) -> Result<Value> {
        Ok(json!({
            "files": db.files()?,
            "items": db.items()?.iter().map(|i| json!([
                i.id, i.path, i.identity, i.kind, i.data, i.embedding, i.description_embedding
            ])).collect::<Vec<_>>(),
            "generation": db.generation()?,
            "checkpoint": db.meta("checkpoint")?,
            "identity": db.meta("identity")?,
            "snapshot": db.meta("snapshot")?,
            "search": db.search_cache("query")?,
        }))
    }

    #[test]
    fn workspaces_share_artifacts_but_keep_bindings_and_query_cache_local() -> Result<()> {
        let (dir, mut first) = fixture()?;
        first.cache_put("parse", "shared-parse", "extraction")?;
        first.cache_put("explanation", "paid", "answer")?;
        first.apply(&[record("first.rs")], &[], None)?;
        first.put_search_cache("query", &[json!("first")])?;
        let mut second = Database::open_with_config(
            &dir.path().join("second.sqlite"),
            &dir.path().join("other-root"),
            &json!({"artifactCachePath": first.global_path()}),
            false,
        )?;
        assert_eq!(
            second.cache("parse", "shared-parse")?.as_deref(),
            Some("extraction")
        );
        assert_eq!(
            second.cache("explanation", "paid")?.as_deref(),
            Some("answer")
        );
        assert_eq!(second.embedding("code")?, Some(vec![0.6, 0.8]));
        assert!(second.files()?.is_empty());
        assert!(second.search_cache("query")?.is_none());
        second.apply(&[record("second.rs")], &[], None)?;
        assert_eq!(second.files()?[0].source, "fn example() {}");
        assert_eq!(first.paths()?, ["first.rs"]);
        assert_eq!(
            first
                .conn
                .query_row("SELECT count(*) FROM global.sources", [], |r| r
                    .get::<_, i64>(0))?,
            1
        );
        assert!(
            first
                .conn
                .prepare("SELECT vector FROM main.embeddings")
                .is_err()
        );
        assert!(first.conn.prepare("SELECT value FROM main.cache").is_err());
        assert!(
            first
                .conn
                .prepare("SELECT content FROM main.description_content")
                .is_err()
        );
        assert!(
            first
                .conn
                .prepare("SELECT source,data FROM main.files")
                .is_err()
        );
        first.reset()?;
        assert_eq!(second.files()?.len(), 1);
        assert!(first.embedding("code")?.is_some());
        Ok(())
    }

    #[test]
    fn overlapping_workspaces_share_unit_content_without_local_payload_copies() -> Result<()> {
        let (dir, mut first) = fixture()?;
        let original = parsed_record("code.rs", "fn example() -> i32 { 1 }")?;
        first.apply_structure(std::slice::from_ref(&original), &[], None)?;
        let mut second = Database::open_with_config(
            &dir.path().join("second.sqlite"),
            &dir.path().join("other-root"),
            &json!({"artifactCachePath": first.global_path()}),
            false,
        )?;
        second.apply_structure(std::slice::from_ref(&original), &[], None)?;
        let expected = parsed_items(&original.0, &original.1)?.remove(0).data;
        let mut keys = Vec::new();
        for db in [&first, &second] {
            let (data, key): (String, String) = db.conn.query_row(
                "SELECT data,content_hash FROM search_units WHERE kind='function'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let data: Value = serde_json::from_str(&data)?;
            assert!(data.get("source").is_none());
            assert!(data.get("embeddingInput").is_none());
            assert_eq!(db.items()?[0].data, expected);
            assert_eq!(
                db.items_for_profile(&json!({"model":"a"}))?[0].data,
                expected
            );
            let manifest: String = db.conn.query_row(
                "SELECT manifest FROM global.snapshots WHERE digest=?",
                [db.meta("snapshot")?.unwrap()],
                |row| row.get(0),
            )?;
            let manifest: Value = serde_json::from_str(&manifest)?;
            assert_eq!(manifest["units"][0][6], key);
            keys.push(key);
        }
        assert_eq!(keys[0], keys[1]);
        assert_eq!(
            first.conn.query_row(
                "SELECT count(*) FROM global.cache WHERE kind='unit-content'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            1
        );
        let content = unit_content(&first.conn, &keys[0])?;
        assert_eq!(content["source"], expected["source"]);
        assert_eq!(content["embeddingInput"], expected["embeddingInput"]);
        first.reset()?;
        assert_eq!(second.items()?[0].data, expected);
        let path = second.path.clone();
        drop(second);
        let readonly = Database::open_readonly(&path, &dir.path().join("other-root"))?;
        assert_eq!(readonly.items()?[0].data, expected);
        Ok(())
    }

    #[test]
    fn missing_corrupt_or_invalid_unit_content_fails_item_reads() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        let original = parsed_record("code.rs", "fn example() -> i32 { 1 }")?;
        db.apply_structure(std::slice::from_ref(&original), &[], None)?;
        let key: String =
            db.conn
                .query_row("SELECT content_hash FROM search_units", [], |row| {
                    row.get(0)
                })?;
        db.conn.execute(
            "UPDATE global.cache SET value='corrupt' WHERE kind='unit-content' AND key=?",
            [&key],
        )?;
        for error in [
            db.items().unwrap_err(),
            db.items_for_profile(&json!({})).unwrap_err(),
        ] {
            assert!(
                error
                    .to_string()
                    .contains("Corrupt global unit content artifact")
            );
        }
        db.conn.execute(
            "DELETE FROM global.cache WHERE kind='unit-content' AND key=?",
            [&key],
        )?;
        db.cache_put("parse", &key, "wrong namespace")?;
        for error in [
            db.items().unwrap_err(),
            db.items_for_profile(&json!({})).unwrap_err(),
        ] {
            assert!(
                error
                    .to_string()
                    .contains("Missing global unit content artifact")
            );
        }
        for payload in ["{", "null", "[]", "{\"path\":\"other.rs\"}"] {
            let key = hash(payload);
            db.cache_put("unit-content", &key, payload)?;
            db.conn
                .execute("UPDATE search_units SET content_hash=?", [&key])?;
            assert!(
                db.items()
                    .unwrap_err()
                    .to_string()
                    .contains("Invalid global unit content artifact"),
                "{payload}"
            );
        }
        db.apply_structure(std::slice::from_ref(&original), &[], None)?;
        assert_eq!(
            db.items()?[0].data,
            parsed_items(&original.0, &original.1)?[0].data
        );
        Ok(())
    }

    #[test]
    fn store_binding_reopens_readonly_and_conflicts_require_a_new_index() -> Result<()> {
        let (dir, mut db) = fixture()?;
        db.apply(&[record("code.rs")], &[], None)?;
        let path = db.path.clone();
        let global = db.global_path().to_owned();
        drop(db);
        let db = Database::open(
            &path,
            dir.path(),
            &json!({"artifactCachePath":"ignored-profile"}),
            false,
        )?;
        assert_eq!(db.global_path(), global);
        drop(db);
        let readonly = Database::open_readonly(&path, dir.path())?;
        assert_eq!(readonly.files()?[0].source, "fn example() {}");
        assert!(readonly.put_embedding("readonly", &[1.0]).is_err());
        assert!(readonly.cache_put("parse", "readonly", "value").is_err());
        assert!(readonly.set_meta("readonly", "value").is_err());
        let config = json!({"artifactCachePath":dir.path().join("new-global.sqlite")});
        for force in [false, true] {
            let error = Database::open_with_config(&path, dir.path(), &config, force)
                .err()
                .unwrap();
            assert!(error.to_string().contains("Global artifact store conflict"));
            assert!(error.to_string().contains("new index path"));
        }
        assert!(!dir.path().join("new-global.sqlite").exists());
        let fresh =
            Database::open_with_config(&dir.path().join("new.sqlite"), dir.path(), &config, false)?;
        assert_ne!(fresh.global_path(), global);
        assert!(fresh.embedding("code")?.is_none());
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn configured_store_alias_reopens_the_same_canonical_binding() -> Result<()> {
        let (dir, db) = fixture()?;
        let path = db.path.clone();
        let global = db.global_path().to_owned();
        drop(db);
        let alias = dir.path().join("store-alias.sqlite");
        std::os::unix::fs::symlink(&global, &alias)?;
        let config = json!({"artifactCachePath":alias});
        let db = Database::open_with_config(&path, dir.path(), &config, false)?;
        assert_eq!(db.global_path(), global);
        drop(db);
        let db = Database::open_readonly_with_config(&path, dir.path(), &config)?;
        assert_eq!(db.global_path(), global);
        Ok(())
    }

    #[test]
    fn workspace_incarnation_survives_reopen_but_changes_on_each_reset() -> Result<()> {
        let (dir, db) = fixture()?;
        let path = db.path.clone();
        let first = db.meta("incarnation")?.unwrap();
        assert_eq!(first.len(), 32);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        drop(db);
        let db = Database::open(&path, dir.path(), &json!({}), false)?;
        assert_eq!(db.meta("incarnation")?.as_deref(), Some(first.as_str()));
        db.reset()?;
        let second = db.meta("incarnation")?.unwrap();
        assert_ne!(second, first);
        db.reset()?;
        let third = db.meta("incarnation")?.unwrap();
        assert_ne!(third, second);
        drop(db);
        let db = Database::open(&path, &dir.path().join("other-root"), &json!({}), true)?;
        assert_ne!(db.meta("incarnation")?.as_deref(), Some(third.as_str()));
        Ok(())
    }

    #[test]
    fn snapshot_registry_is_stable_immutable_and_survives_root_reset() -> Result<()> {
        let (dir, mut first) = fixture()?;
        first.set_meta("selection_policy", "all-rust")?;
        first.set_meta("discovery_policy", "first-root-fingerprints")?;
        first.set_meta("dirty_paths", "[\"a.rs\"]")?;
        first.apply(&[record("a.rs"), record("z.rs")], &[], Some("commit-a"))?;
        let digest = first.meta("snapshot")?.unwrap();
        let manifest: String = first.conn.query_row(
            "SELECT manifest FROM global.snapshots WHERE digest=?",
            [&digest],
            |r| r.get(0),
        )?;
        assert_eq!(hash(&manifest), digest);
        let mut second = Database::open_with_config(
            &dir.path().join("second.sqlite"),
            &dir.path().join("second-root"),
            &json!({"artifactCachePath": first.global_path()}),
            false,
        )?;
        second.set_meta("selection_policy", "all-rust")?;
        second.set_meta("discovery_policy", "second-root-fingerprints")?;
        second.set_meta("dirty_paths", "[]")?;
        second.apply(&[record("z.rs")], &[], None)?;
        second.apply(&[record("a.rs")], &[], Some("commit-b"))?;
        assert_eq!(second.meta("snapshot")?.as_deref(), Some(digest.as_str()));
        second.apply(&[], &[], Some("another-checkpoint"))?;
        assert_eq!(second.meta("snapshot")?.as_deref(), Some(digest.as_str()));
        second.set_meta("selection_policy", "selected-rust")?;
        second.apply(&[], &[], Some("another-checkpoint"))?;
        assert_ne!(second.meta("snapshot")?.as_deref(), Some(digest.as_str()));
        assert_eq!(
            second.meta("snapshot_policy")?.as_deref(),
            Some("selected-rust")
        );
        second
            .conn
            .execute("DELETE FROM metadata WHERE key='selection_policy'", [])?;
        second.apply(&[], &[], Some("another-checkpoint"))?;
        assert!(second.meta("snapshot_policy")?.is_none());
        let path = first.path.clone();
        drop(first);
        let first = Database::open(&path, &dir.path().join("new-root"), &json!({}), true)?;
        assert!(first.paths()?.is_empty());
        assert!(first.meta("snapshot")?.is_none());
        assert_eq!(
            first.conn.query_row(
                "SELECT manifest FROM global.snapshots WHERE digest=?",
                [&digest],
                |r| r.get::<_, String>(0)
            )?,
            manifest
        );
        assert_eq!(
            first.conn.query_row(
                "SELECT source FROM global.sources WHERE hash=?",
                [hash("fn example() {}")],
                |r| r.get::<_, String>(0)
            )?,
            "fn example() {}"
        );
        Ok(())
    }

    #[test]
    fn source_hash_mismatch_rolls_back_live_publication() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        db.apply(&[record("old.rs")], &[], None)?;
        let before = snapshot(&db)?;
        let mut invalid = record("new.rs");
        invalid.0.hash = "incorrect".into();
        assert!(
            db.apply(&[invalid], &["old.rs".into()], None)
                .unwrap_err()
                .to_string()
                .contains("Source content hash mismatch")
        );
        assert_eq!(snapshot(&db)?, before);
        Ok(())
    }

    #[test]
    fn unchanged_publication_does_not_read_the_complete_manifest() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        db.set_meta("selection_policy", "all-rust")?;
        db.apply(&[record("code.rs")], &[], Some("commit"))?;
        let digest = db.meta("snapshot")?;
        // Make a manifest-only table unavailable: unchanged publication must
        // consult only its checkpoint and policy, not traverse published rows.
        db.conn
            .execute("ALTER TABLE symbol_names RENAME TO unread_names", [])?;
        let writes = db.conn.total_changes();
        assert!(!db.apply(&[], &[], Some("commit"))?);
        assert_eq!(db.conn.total_changes(), writes);
        assert_eq!(db.meta("snapshot")?, digest);
        db.conn
            .execute("ALTER TABLE unread_names RENAME TO symbol_names", [])?;
        Ok(())
    }

    #[test]
    fn file_records_load_metadata_without_source_bytes() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        let record = record("code.rs");
        db.apply(std::slice::from_ref(&record), &[], None)?;
        let file = db.file_records()?.remove(0);
        let mut expected = record.0.clone();
        expected.source.clear();
        assert_eq!(serde_json::to_value(file)?, serde_json::to_value(expected)?);
        assert_eq!(db.source(&record.0.hash)?, record.0.source);
        db.conn
            .execute("UPDATE global.sources SET source='corrupt'", [])?;
        assert!(db.file_records()?[0].source.is_empty());
        assert!(
            db.source(&record.0.hash)
                .unwrap_err()
                .to_string()
                .contains("Corrupt global source bytes")
        );
        Ok(())
    }

    #[test]
    fn missing_or_corrupt_global_source_is_an_error() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        db.apply(&[record("code.rs")], &[], None)?;
        db.conn
            .execute("UPDATE global.sources SET source='corrupt'", [])?;
        assert!(
            db.files()
                .unwrap_err()
                .to_string()
                .contains("Corrupt global source bytes")
        );
        db.conn.execute("DELETE FROM global.sources", [])?;
        for error in [db.files().unwrap_err(), db.file_records().unwrap_err()] {
            let message = error.to_string();
            assert!(
                message.contains("Missing global source artifact"),
                "{message}"
            );
            assert!(message.contains("code.rs"), "{message}");
        }
        assert!(
            db.source(&hash("fn example() {}"))
                .unwrap_err()
                .to_string()
                .contains("Missing global source artifact")
        );
        assert_eq!(db.paths()?, ["code.rs"]);
        assert!(db.embedding("code")?.is_some());
        Ok(())
    }

    #[test]
    fn rejected_local_bindings_leave_precommitted_reusable_artifacts() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        let mut changed = parsed_record("new.rs", "fn example() -> i32 { 7 }")?;
        changed.0.description = Some("New file prose".into());
        changed.0.description_hash = Some(changed.0.hash.clone());
        changed.1.callables[0].description = Some("New callable prose".into());
        changed.1.structure.nodes[0].description = Some("New symbol prose".into());
        let payload = unit_payload(&parsed_items(&changed.0, &changed.1)?[0].data)?;
        db.conn.execute_batch("CREATE TRIGGER reject_binding BEFORE INSERT ON files BEGIN SELECT RAISE(ABORT, 'binding rejected'); END;")?;
        let before = snapshot(&db)?;
        assert!(
            db.apply_structure(std::slice::from_ref(&changed), &[], Some("new"))
                .unwrap_err()
                .to_string()
                .contains("binding rejected")
        );
        assert!(db.conn.is_autocommit());
        assert_eq!(snapshot(&db)?, before);
        // Observe through an independent connection: these were committed
        // before the rejected workspace transaction, not rolled back with it.
        let global = cache::open_store(db.global_path(), true)?;
        assert_eq!(
            global.query_row(
                "SELECT source FROM sources WHERE hash=?",
                [&changed.0.hash],
                |row| row.get::<_, String>(0)
            )?,
            changed.0.source
        );
        assert_eq!(
            global.query_row(
                "SELECT value FROM cache WHERE kind='unit-content' AND key=?",
                [hash(&payload)],
                |row| row.get::<_, String>(0)
            )?,
            payload
        );
        for text in ["New file prose", "New callable prose", "New symbol prose"] {
            assert_eq!(
                global.query_row(
                    "SELECT content FROM description_content WHERE hash=?",
                    [hash(text)],
                    |row| row.get::<_, String>(0)
                )?,
                text
            );
        }
        let counts = global.query_row("SELECT (SELECT count(*) FROM sources),(SELECT count(*) FROM cache WHERE kind='unit-content'),(SELECT count(*) FROM description_content)", [], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)))?;
        db.conn.execute_batch("DROP TRIGGER reject_binding")?;
        assert!(db.apply_structure(&[changed], &[], Some("new"))?);
        assert_eq!(global.query_row("SELECT (SELECT count(*) FROM sources),(SELECT count(*) FROM cache WHERE kind='unit-content'),(SELECT count(*) FROM description_content)", [], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)))?, counts);
        Ok(())
    }

    #[test]
    fn snapshot_manifest_commits_before_a_rejected_local_pointer_and_retry_reuses_it() -> Result<()>
    {
        let (_dir, mut db) = fixture()?;
        let original = parsed_record("code.rs", "fn example() -> i32 { 1 }")?;
        db.apply_structure(&[original], &[], Some("old"))?;
        db.put_search_cache("query", &[json!("old result")])?;
        let before = snapshot(&db)?;
        let old_digest = db.meta("snapshot")?.unwrap();
        let changed = parsed_record("code.rs", "fn example() -> i32 { 2 }")?;
        db.conn.execute_batch("CREATE TRIGGER reject_snapshot_pointer BEFORE UPDATE ON metadata WHEN OLD.key='snapshot' BEGIN SELECT RAISE(ABORT, 'snapshot pointer rejected'); END;")?;
        assert!(
            db.apply_structure(std::slice::from_ref(&changed), &[], Some("new"))
                .unwrap_err()
                .to_string()
                .contains("snapshot pointer rejected")
        );
        assert!(db.conn.is_autocommit());
        assert_eq!(snapshot(&db)?, before);
        let global = cache::open_store(db.global_path(), true)?;
        let (digest, manifest): (String, String) = global.query_row(
            "SELECT digest,manifest FROM snapshots WHERE digest<>?",
            [&old_digest],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(hash(&manifest), digest);
        let manifest: Value = serde_json::from_str(&manifest)?;
        assert_eq!(manifest["files"][0][1], changed.0.hash);
        let content_hash = manifest["units"][0][6].as_str().unwrap();
        assert!(global.query_row(
            "SELECT EXISTS(SELECT 1 FROM cache WHERE kind='unit-content' AND key=?)",
            [content_hash],
            |row| row.get::<_, bool>(0)
        )?);
        db.conn
            .execute_batch("DROP TRIGGER reject_snapshot_pointer")?;
        assert!(db.apply_structure(&[changed], &[], Some("new"))?);
        assert_eq!(db.meta("snapshot")?.as_deref(), Some(digest.as_str()));
        assert_eq!(
            global.query_row("SELECT count(*) FROM snapshots", [], |row| row
                .get::<_, i64>(0))?,
            2
        );
        assert!(db.search_cache("query")?.is_none());
        Ok(())
    }

    #[test]
    fn late_publication_failure_rolls_back_metadata_and_cache_deletion() -> Result<()> {
        let (dir, mut db) = fixture()?;
        db.apply(&[record("old.rs")], &[], Some("old-commit"))?;
        db.put_search_cache("query", &[json!({"id": 1})])?;
        let before = snapshot(&db)?;
        // Fail after all live rows, generation and checkpoint have been written.
        db.conn.execute_batch("CREATE TRIGGER abort_cache BEFORE DELETE ON search_cache BEGIN SELECT RAISE(ABORT, 'late failure'); END;")?;
        db.cache_put("parse", "completed", "paid artifact")?;
        db.put_embedding("completed", &[0.0, 1.0])?;
        let error = db
            .apply(&[record("new.rs")], &["old.rs".into()], Some("new-commit"))
            .unwrap_err();
        assert!(error.to_string().contains("late failure"));
        assert!(db.conn.is_autocommit());
        assert_eq!(snapshot(&db)?, before);
        drop(db);
        let mut db = Database::open(
            &dir.path().join("index.sqlite"),
            dir.path(),
            &json!({}),
            false,
        )?;
        assert_eq!(snapshot(&db)?, before);
        assert_eq!(
            db.cache("parse", "completed")?.as_deref(),
            Some("paid artifact")
        );
        assert_eq!(db.embedding("completed")?, Some(vec![0.0, 1.0]));
        db.conn.execute_batch("DROP TRIGGER abort_cache")?;
        assert!(db.apply(&[record("new.rs")], &["old.rs".into()], Some("new-commit"))?);
        assert_eq!(db.generation()?, 2);
        assert_eq!(db.meta("checkpoint")?.as_deref(), Some("new-commit"));
        assert!(db.search_cache("query")?.is_none());
        Ok(())
    }

    #[test]
    fn failed_reset_rolls_back_and_leaves_connection_reusable() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        db.apply(&[record("old.rs")], &[], Some("commit"))?;
        db.put_search_cache("query", &[json!("cached")])?;
        let before = snapshot(&db)?;
        db.conn.execute_batch("CREATE TRIGGER abort_reset BEFORE DELETE ON metadata BEGIN SELECT RAISE(ABORT, 'reset failure'); END;")?;
        assert!(
            db.reset()
                .unwrap_err()
                .to_string()
                .contains("reset failure")
        );
        assert!(
            db.conn.is_autocommit(),
            "failed reset must release its transaction"
        );
        assert_eq!(snapshot(&db)?, before);
        db.conn.execute_batch("DROP TRIGGER abort_reset")?;
        db.reset()?;
        assert!(db.files()?.is_empty());
        assert!(db.items()?.is_empty());
        assert_eq!(
            db.meta("identity")?,
            before["identity"].as_str().map(str::to_owned)
        );
        assert!(db.meta("checkpoint")?.is_none());
        assert_eq!(db.generation()?, 0);
        assert!(db.search_cache("query")?.is_none());
        assert!(db.embedding("code")?.is_some());
        Ok(())
    }

    #[test]
    fn incompatible_identity_rejection_preserves_snapshot_and_force_resets_live_state() -> Result<()>
    {
        for change_root in [false, true] {
            let (dir, mut db) = fixture()?;
            db.apply(&[record("old.rs")], &[], Some("commit"))?;
            db.cache_put("description", "paid", "retained")?;
            db.put_search_cache("query", &[json!(1)])?;
            let before = snapshot(&db)?;
            let path = db.path.clone();
            drop(db);
            let root = if change_root {
                dir.path().join("other")
            } else {
                dir.path().to_owned()
            };
            let profile = if change_root {
                json!({})
            } else {
                json!({"model": "other"})
            };
            if !change_root {
                for force in [false, true] {
                    let db = Database::open(&path, &root, &profile, force)?;
                    assert_eq!(snapshot(&db)?, before);
                    assert_eq!(
                        db.cache("description", "paid")?.as_deref(),
                        Some("retained")
                    );
                }
                continue;
            }
            let error = Database::open(&path, &root, &profile, false)
                .err()
                .expect("identity mismatch");
            assert!(error.to_string().contains("Incompatible index"));
            let db = Database::open(&path, dir.path(), &json!({}), false)?;
            assert_eq!(snapshot(&db)?, before);
            drop(db);
            let db = Database::open(&path, &root, &profile, true)?;
            assert!(db.files()?.is_empty());
            assert!(db.items()?.is_empty());
            assert_eq!(db.generation()?, 0);
            assert!(db.meta("checkpoint")?.is_none());
            assert!(db.search_cache("query")?.is_none());
            assert_eq!(
                db.cache("description", "paid")?.as_deref(),
                Some("retained")
            );
            assert_eq!(db.embedding("code")?, Some(vec![0.6, 0.8]));
            let identity = db.meta("identity")?;
            drop(db);
            assert_eq!(
                Database::open(&path, &root, &profile, false)?.meta("identity")?,
                identity
            );
        }
        Ok(())
    }

    #[test]
    fn checkpoint_only_updates_preserve_generation_and_search_cache() -> Result<()> {
        let (dir, mut db) = fixture()?;
        assert!(!db.apply(&[], &[], None)?);
        assert!(db.meta("generation")?.is_none());
        db.apply(&[record("code.rs")], &[], Some("first"))?;
        db.put_search_cache("query", &[json!({"score": 0.75})])?;
        let before = snapshot(&db)?;
        for checkpoint in [Some("first"), Some("second"), None, None] {
            assert!(!db.apply(&[], &[], checkpoint)?);
            assert_eq!(db.generation()?, 1);
            assert_eq!(db.meta("checkpoint")?.as_deref(), checkpoint);
            let mut expected = before.clone();
            expected["checkpoint"] = json!(checkpoint);
            assert_eq!(snapshot(&db)?, expected);
        }
        drop(db);
        let db = Database::open(
            &dir.path().join("index.sqlite"),
            dir.path(),
            &json!({}),
            false,
        )?;
        assert_eq!(db.generation()?, 1);
        assert!(db.meta("checkpoint")?.is_none());
        assert_eq!(
            db.search_cache("query")?,
            Some(vec![json!({"score": 0.75})])
        );
        Ok(())
    }

    #[test]
    fn generation_corruption_and_exhaustion_fail_without_publication() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        db.apply(&[record("old.rs")], &[], Some("old"))?;
        db.put_search_cache("query", &[json!(1)])?;
        for invalid in [
            "not-a-number",
            "-1",
            "18446744073709551616",
            "18446744073709551615",
        ] {
            db.set_meta("generation", invalid)?;
            assert!(
                db.apply(&[record("new.rs")], &["old.rs".into()], Some("new"))
                    .is_err(),
                "{invalid}"
            );
            assert!(db.conn.is_autocommit());
            assert_eq!(db.files()?[0].path, "old.rs");
            assert_eq!(db.items()?.len(), 1);
            assert_eq!(db.meta("generation")?.as_deref(), Some(invalid));
            assert_eq!(db.meta("checkpoint")?.as_deref(), Some("old"));
            assert_eq!(db.search_cache("query")?, Some(vec![json!(1)]));
        }
        // Exhaustion does not prevent checkpoint-only updates.
        assert!(!db.apply(&[], &[], Some("new"))?);
        assert_eq!(db.generation()?, u64::MAX);
        Ok(())
    }

    #[test]
    fn embeddings_validate_before_writing_and_preserve_exact_first_value() -> Result<()> {
        let (dir, db) = fixture()?;
        let vector = [0.0, -0.0, f32::MIN_POSITIVE, f32::MAX, -3.25];
        db.put_embedding("exact", &vector)?;
        db.put_embedding("exact", &[9.0])?;
        for invalid in [
            vec![],
            vec![f32::NAN],
            vec![f32::INFINITY],
            vec![f32::NEG_INFINITY],
        ] {
            assert!(db.put_embedding("invalid", &invalid).is_err());
            assert!(db.put_embedding("exact", &invalid).is_err());
            assert!(db.embedding("invalid")?.is_none());
        }
        drop(db);
        let db = Database::open(
            &dir.path().join("index.sqlite"),
            dir.path(),
            &json!({}),
            false,
        )?;
        let stored = db.embedding("exact")?.unwrap();
        assert_eq!(
            stored.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            vector.map(f32::to_bits)
        );
        assert!(db.embedding("missing")?.is_none());
        Ok(())
    }

    #[test]
    fn corrupt_embedding_blobs_are_errors_not_cache_misses() -> Result<()> {
        let (_dir, db) = fixture()?;
        assert!(!db.embedding_exists("missing")?);
        let cases = [
            (vec![], "Invalid stored vector length"),
            (vec![0, 0, 0], "Invalid stored vector length"),
            (vec![0; 5], "Invalid stored vector length"),
            (f32::NAN.to_le_bytes().to_vec(), "Non-finite stored vector"),
            (
                f32::INFINITY.to_le_bytes().to_vec(),
                "Non-finite stored vector",
            ),
            (
                f32::NEG_INFINITY.to_le_bytes().to_vec(),
                "Non-finite stored vector",
            ),
        ];
        for (blob, message) in cases {
            db.conn.execute(
                "INSERT OR REPLACE INTO global.embeddings VALUES('corrupt', ?)",
                [blob],
            )?;
            assert!(db.embedding_exists("corrupt")?);
            assert_eq!(db.embedding("corrupt").unwrap_err().to_string(), message);
            assert_eq!(db.embedding("code")?, Some(vec![0.6, 0.8]));
        }
        Ok(())
    }

    #[test]
    fn projection_presence_defers_vector_validation_until_embedding_read() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        let record = record("code.rs");
        db.apply(std::slice::from_ref(&record), &[], None)?;
        let profile = json!({"model":"a"});
        let code_key = Database::embedding_key(&profile, false, "fn example() {}");
        let file_key = Database::embedding_key(&profile, false, "A description");
        let callable_key = Database::embedding_key(&profile, false, "Callable description");
        for key in [&code_key, &file_key, &callable_key] {
            db.conn.execute(
                "INSERT INTO global.embeddings VALUES(?,?)",
                params![key, vec![0_u8; 3]],
            )?;
            assert!(db.embedding_exists(key)?);
            assert!(db.embedding(key).is_err());
        }
        let item = db.items_for_profile(&profile)?.remove(0);
        assert_eq!(item.embedding, code_key);
        assert_eq!(item.description_embedding, Some(callable_key));
        assert_eq!(
            db.files_for_profile(&profile)?[0].description_embedding,
            Some(file_key)
        );
        Ok(())
    }

    #[test]
    fn corrupt_json_records_fail_reads_and_can_be_repaired() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        db.apply(&[record("code.rs")], &[], None)?;
        assert!(
            db.conn
                .prepare("SELECT data,source FROM main.files")
                .is_err()
        );
        for value in ["{", "[", "not-json"] {
            db.conn.execute("UPDATE diagnostics SET data=?", [value])?;
            assert!(db.files().is_err(), "{value}");
        }
        db.apply(&[record("code.rs")], &[], None)?;
        for value in ["{", "null", "[]"] {
            db.conn.execute("UPDATE search_units SET data=?", [value])?;
            assert!(db.items().is_err(), "{value}");
        }
        for value in ["{", "null", "{}"] {
            db.conn.execute(
                "INSERT OR REPLACE INTO search_cache VALUES('query', ?)",
                [value],
            )?;
            assert!(
                db.search_cache("query")
                    .unwrap_err()
                    .to_string()
                    .contains("Corrupt search cache")
            );
        }
        db.apply(&[record("code.rs")], &[], None)?;
        assert_eq!(db.files()?.len(), 1);
        assert_eq!(db.items()?.len(), 1);
        assert!(db.search_cache("query")?.is_none());
        Ok(())
    }

    #[test]
    fn artifacts_are_namespaced_and_reopen_with_complete_live_records() -> Result<()> {
        let (dir, mut db) = fixture()?;
        let record = record("space/é.rs");
        db.apply(std::slice::from_ref(&record), &[], Some("commit"))?;
        db.cache_put("parse", "same-key", "old")?;
        db.cache_put("description", "same-key", "description")?;
        db.cache_put("parse", "same-key", "new")?;
        let before = snapshot(&db)?;
        drop(db);
        let db = Database::open(
            &dir.path().join("index.sqlite"),
            dir.path(),
            &json!({}),
            false,
        )?;
        assert_eq!(snapshot(&db)?, before);
        assert_eq!(
            serde_json::to_value(&db.files()?[0])?,
            serde_json::to_value(record.0)?
        );
        assert_eq!(
            db.items()?[0].description_embedding.as_deref(),
            Some("description")
        );
        assert_eq!(db.cache("parse", "same-key")?.as_deref(), Some("new"));
        assert_eq!(
            db.cache("description", "same-key")?.as_deref(),
            Some("description")
        );
        assert!(db.cache("other", "same-key")?.is_none());
        let profile = json!({"model": "a", "dimensions": 2});
        let document = Database::embedding_key(&profile, false, "input");
        let keys = [
            document.clone(),
            Database::embedding_key(&profile, true, "input"),
            Database::embedding_key(&json!({"model": "b", "dimensions": 2}), false, "input"),
            Database::embedding_key(&profile, false, "input "),
        ];
        assert_eq!(
            keys.iter().collect::<std::collections::HashSet<_>>().len(),
            keys.len()
        );
        assert_eq!(document, Database::embedding_key(&profile, false, "input"));
        Ok(())
    }

    #[test]
    fn missing_global_embedding_references_abort_publication_and_retry_cleanly() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        for description in [false, true] {
            let mut record = record("code.rs");
            if description {
                record.1[0].description_embedding = Some("missing".into());
            } else {
                record.1[0].embedding = "missing".into();
            }
            assert!(db.apply(&[record], &[], Some("commit")).is_err());
            assert!(db.files()?.is_empty());
            assert!(db.items()?.is_empty());
            assert_eq!(db.generation()?, 0);
            assert!(db.meta("checkpoint")?.is_none());
            assert!(db.conn.is_autocommit());
        }
        assert!(db.apply(&[record("code.rs")], &[], Some("commit"))?);
        assert_eq!(db.generation()?, 1);
        assert_eq!(db.items()?.len(), 1);
        Ok(())
    }

    #[test]
    fn unprefixed_schema_reopens_and_retains_artifacts_on_reset() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("index.sqlite");
        let profile = json!({"model": "test", "dimensions": 2});
        let db = Database::open_with_config(
            &path,
            dir.path(),
            &json!({"artifactCachePath": dir.path().join("global.sqlite")}),
            false,
        )?;
        let tables: Vec<String> = db.conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        assert_eq!(
            tables,
            [
                "descriptions",
                "diagnostics",
                "files",
                "metadata",
                "search_cache",
                "search_units",
                "symbol_names",
                "symbols",
                "unit_embeddings"
            ]
        );
        db.put_embedding("vector", &[0.6, 0.8])?;
        db.cache_put("parse", "source", "parsed")?;
        db.put_search_cache("query", &[json!({"id": 1})])?;
        db.set_meta("generation", "3")?;
        drop(db);

        let db = Database::open(&path, dir.path(), &profile, false)?;
        assert_eq!(db.generation()?, 3);
        assert!(db.search_cache("query")?.is_some());
        db.reset()?;
        assert_eq!(db.embedding("vector")?, Some(vec![0.6, 0.8]));
        assert_eq!(db.cache("parse", "source")?.as_deref(), Some("parsed"));
        assert_eq!(db.generation()?, 0);
        assert!(db.search_cache("query")?.is_none());
        Ok(())
    }

    #[test]
    fn old_layouts_require_rebuilding_even_with_force() -> Result<()> {
        for schema in [
            "CREATE TABLE rust_metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO rust_metadata VALUES('identity','old');",
            "CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO metadata VALUES('schema_version','11');",
            "CREATE TABLE functions(id INTEGER PRIMARY KEY);",
            "CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO metadata VALUES('identity','{\"schema\":2,\"root\":\"old\",\"embedding\":{}}');",
            "CREATE TABLE files(path TEXT PRIMARY KEY,data TEXT NOT NULL); CREATE TABLE items(id INTEGER PRIMARY KEY);",
        ] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("index.sqlite");
            let conn = Connection::open(&path)?;
            conn.execute_batch(schema)?;
            let before: Vec<Option<String>> = conn
                .prepare("SELECT sql FROM sqlite_master ORDER BY name")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let before_bytes = std::fs::read(&path)?;
            for force in [false, true] {
                let error = Database::open(&path, dir.path(), &json!({}), force)
                    .err()
                    .expect("old layout must be rejected");
                assert!(error.to_string().contains("Unsupported index table layout"));
                let after: Vec<Option<String>> = conn
                    .prepare("SELECT sql FROM sqlite_master ORDER BY name")?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                assert_eq!(before, after);
                assert_eq!(std::fs::read(&path)?, before_bytes);
            }
        }
        Ok(())
    }

    fn parsed_record(path: &str, source: &str) -> Result<(File, ParsedFile)> {
        let mut file = record(path).0;
        file.source = source.into();
        file.hash = hash(source);
        file.language = crate::parse::language_for_path(path).unwrap().into();
        file.description = None;
        file.description_hash = None;
        file.description_embedding = None;
        file.errors.clear();
        Ok((file, crate::parse::parse(path, source)?))
    }

    #[test]
    fn structure_roundtrips_full_metadata_and_links_pending_units() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        let mut record = parsed_record(
            "code.rs",
            "use std::collections::HashMap as Map;\nstruct Café { value: i32 }\nimpl Café { fn run(&self) -> i32 { self.value } }\n",
        )?;
        // Storage preserves the extraction in full; presentation limits belong
        // to the renderer, never to the normalized source of truth.
        record.1.structure.nodes[0].signature = "é".repeat(20_000);
        record.1.structure.nodes[0].names = vec!["Map".into(), "HashMap".into(), "Map".into()];
        record.1.structure.nodes[0]
            .attributes
            .push("#[cfg(feature = \"test\")]".into());
        db.apply_structure(std::slice::from_ref(&record), &[], Some("commit"))?;
        assert_eq!(db.structure("code.rs")?, record.1.structure);
        assert!(db.structure_current("code.rs", &record.0.hash, STRUCTURE_PARSER_VERSION)?);
        assert!(!db.structure_current("code.rs", "different", STRUCTURE_PARSER_VERSION)?);
        assert_eq!(db.missing_structure_paths("next-version")?, ["code.rs"]);
        assert!(
            db.missing_structure_paths(STRUCTURE_PARSER_VERSION)?
                .is_empty()
        );
        assert_eq!(db.paths()?, ["code.rs"]);
        let items = db.items_for_profile(&json!({"model":"a"}))?;
        assert_eq!(items.len(), record.1.callables.len());
        assert!(items.iter().all(|i| i.embedding.is_empty()));
        let linked: i64 = db.conn.query_row(
            "SELECT count(*) FROM search_units WHERE symbol_id IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(linked as usize, items.len());
        assert_eq!(
            items[0].identity,
            hash(
                json!([
                    "code.rs",
                    record.1.callables[0].qualified_name,
                    record.1.callables[0].kind,
                    0
                ])
                .to_string()
            )
        );
        let mut file = db.files()?.remove(0);
        file.source_mode = "git-head".into();
        db.apply(&[(file, items)], &[], Some("commit"))?;
        assert_eq!(
            db.structure("code.rs")?,
            record.1.structure,
            "semantic apply must reuse stored structure"
        );
        assert_eq!(db.files()?[0].source_mode, "git-head");
        // File-local symbol IDs overlap; the bulk name load must stay scoped
        // to its file and preserve name order, duplicates and empty lists.
        let mut other = parsed_record("other.rs", "struct Other; impl Other { fn run() {} }")?;
        other.1.structure.nodes[0].names.clear();
        db.apply_structure(std::slice::from_ref(&other), &[], Some("commit"))?;
        assert_eq!(db.structure("code.rs")?, record.1.structure);
        assert_eq!(db.structure("other.rs")?, other.1.structure);
        Ok(())
    }

    #[test]
    fn projections_reuse_durable_artifacts_and_never_return_stale_or_other_profile_vectors()
    -> Result<()> {
        let (dir, mut db) = fixture()?;
        let a = json!({"model":"a","dimensions":2});
        let b = json!({"model":"b","dimensions":2});
        let original = parsed_record("code.rs", "fn example() -> i32 { 1 }")?;
        db.apply_structure(std::slice::from_ref(&original), &[], None)?;
        let id = db.items()?[0].id;
        let input = original.1.callables[0].embedding_input.clone();
        let code_a = Database::embedding_key(&a, false, &input);
        db.put_embedding(&code_a, &[1.0, 0.0])?;
        assert_eq!(
            db.items_for_profile(&a)?[0].embedding,
            code_a,
            "cache lookup requires no published association"
        );
        assert!(db.items_for_profile(&b)?[0].embedding.is_empty());
        let text = "Returns one";
        let mut file = original.0.clone();
        file.description = Some("Example file".into());
        file.description_hash = Some(file.hash.clone());
        for profile in [&a, &b] {
            db.set_projection_profile(profile)?;
            let key = Database::embedding_key(profile, false, &input);
            db.put_embedding(&key, &[1.0, 0.0])?;
            let description_key = Database::embedding_key(profile, false, text);
            db.put_embedding(&description_key, &[0.0, 1.0])?;
            let file_key =
                Database::embedding_key(profile, false, file.description.as_deref().unwrap());
            db.put_embedding(&file_key, &[0.6, 0.8])?;
            file.description_embedding = Some(file_key);
            let mut items = db.items_for_profile(profile)?;
            items[0].data["description"] = json!(text);
            items[0].description_embedding = Some(description_key.clone());
            db.apply(&[(file.clone(), items)], &[], None)?;
            assert_eq!(db.items_for_profile(profile)?[0].embedding, key);
            assert_eq!(
                db.items_for_profile(profile)?[0]
                    .description_embedding
                    .as_deref(),
                Some(description_key.as_str())
            );
        }
        let associations: i64 =
            db.conn
                .query_row("SELECT count(*) FROM unit_embeddings", [], |r| r.get(0))?;
        assert_eq!(associations, 4);
        assert_eq!(db.items_for_profile(&a)?[0].embedding, code_a);
        assert_eq!(
            db.files_for_profile(&a)?[0].description_embedding,
            Some(Database::embedding_key(&a, false, "Example file"))
        );
        let shifted = parsed_record("code.rs", "\n\nfn example() -> i32 { 1 }")?;
        db.apply_structure(&[shifted], &[], None)?;
        // Even an unchanged callable has new full-file generation context.
        // Code vectors survive, but generated description bindings do not.
        for profile in [&a, &b] {
            let item = db.items_for_profile(profile)?.remove(0);
            assert_eq!(item.id, id);
            assert!(item.data["description"].is_null());
            assert_eq!(
                item.embedding,
                Database::embedding_key(profile, false, &input)
            );
            assert!(item.description_embedding.is_none());
            assert!(
                db.embedding(&Database::embedding_key(profile, false, text))?
                    .is_some()
            );
        }
        assert!(db.items()?[0].description_embedding.is_none());
        assert!(db.symbol_descriptions("code.rs")?.is_empty());
        assert_eq!(
            db.conn.query_row(
                "SELECT count(*) FROM descriptions WHERE scope='callable'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            0
        );
        let changed = parsed_record("code.rs", "fn example() -> i32 { 2 }")?;
        db.apply_structure(&[changed], &[], Some("edited"))?;
        for profile in [&a, &b] {
            let item = db.items_for_profile(profile)?.remove(0);
            assert_eq!(item.id, id);
            assert!(item.embedding.is_empty());
            assert!(item.description_embedding.is_none());
            assert!(item.data["description"].is_null());
        }
        assert!(db.items()?[0].embedding.is_empty());
        assert_eq!(db.files()?[0].description.as_deref(), Some("Example file"));
        assert_eq!(
            db.files()?[0].description_hash,
            Some(original.0.hash.clone())
        );
        assert_eq!(
            db.conn
                .query_row("SELECT count(*) FROM unit_embeddings", [], |r| r
                    .get::<_, i64>(0))?,
            associations
        );
        db.cache_put("rerank", "paid", "cached response")?;
        drop(db);
        let mut db = Database::open(&dir.path().join("index.sqlite"), dir.path(), &a, true)?;
        assert_eq!(db.items()?[0].id, id);
        db.apply_structure(&[original], &[], None)?;
        assert_eq!(db.items_for_profile(&a)?[0].embedding, code_a);
        db.reset()?;
        assert_eq!(
            db.cache("rerank", "paid")?.as_deref(),
            Some("cached response")
        );
        assert!(db.embedding(&code_a)?.is_some());
        drop(db);
        let db = Database::open(&dir.path().join("index.sqlite"), dir.path(), &b, false)?;
        assert!(db.paths()?.is_empty());
        assert!(db.embedding(&code_a)?.is_some());
        Ok(())
    }

    #[test]
    fn structural_publication_rolls_back_symbols_units_diagnostics_and_checkpoint() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        let original = parsed_record("code.rs", "struct A; impl A { fn run() {} }")?;
        db.apply_structure(std::slice::from_ref(&original), &[], Some("old"))?;
        db.put_search_cache("query", &[json!("old result")])?;
        let before = snapshot(&db)?;
        db.conn.execute_batch("CREATE TRIGGER abort_structure BEFORE DELETE ON search_cache BEGIN SELECT RAISE(ABORT, 'late structure failure'); END;")?;
        db.cache_put("description", "paid", "survives")?;
        let mut changed = parsed_record("code.rs", "struct B; impl B { fn run() { let x = 2; } }")?;
        changed.1.errors.push(crate::parse::Diagnostic {
            message: "new diagnostic".into(),
            start_line: 1,
            end_line: 1,
        });
        let read_error =
            json!({"code":"read-error","message":"using saved source","path":"code.rs"});
        changed.0.errors.push(read_error.clone());
        assert!(
            db.apply_structure(std::slice::from_ref(&changed), &[], Some("new"))
                .is_err()
        );
        assert!(db.conn.is_autocommit());
        assert_eq!(snapshot(&db)?, before);
        assert_eq!(db.structure("code.rs")?, original.1.structure);
        assert_eq!(
            db.cache("description", "paid")?.as_deref(),
            Some("survives")
        );
        db.conn.execute_batch("DROP TRIGGER abort_structure")?;
        // A deferred FK failure at commit must roll back the entire publication.
        let mut invalid = changed.clone();
        invalid.1.structure.nodes[0].parent_id = Some(999_999);
        assert!(
            db.apply_structure(&[invalid], &[], Some("invalid"))
                .is_err()
        );
        assert!(db.conn.is_autocommit());
        assert_eq!(snapshot(&db)?, before);
        assert_eq!(db.structure("code.rs")?, original.1.structure);
        db.apply_structure(&[changed], &[], Some("new"))?;
        assert_eq!(db.files()?[0].errors[0], read_error);
        assert_eq!(db.files()?[0].errors[1]["message"], "new diagnostic");
        assert_eq!(db.generation()?, 2);
        assert_eq!(db.meta("checkpoint")?.as_deref(), Some("new"));
        db.apply_structure(&[], &["code.rs".into()], None)?;
        assert!(db.paths()?.is_empty());
        for table in [
            "symbols",
            "symbol_names",
            "search_units",
            "unit_embeddings",
            "descriptions",
            "diagnostics",
        ] {
            assert_eq!(
                db.conn
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))?,
                0
            );
        }
        Ok(())
    }

    #[test]
    fn markdown_and_empty_structures_are_versioned_without_embeddings() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        let markdown = parsed_record("readme.md", "# Overview\n\nHello.\n\n## Detail\n\nWorld.\n")?;
        let empty = parsed_record("empty.rs", "// nothing to declare\n")?;
        db.apply_structure_with_version(&[markdown.clone(), empty.clone()], &[], None, "parser-2")?;
        assert!(db.structure("empty.rs")?.nodes.is_empty());
        assert!(db.structure_current("empty.rs", &empty.0.hash, "parser-2")?);
        assert!(db.structure("missing.rs").is_err());
        assert_eq!(db.structure("readme.md")?, markdown.1.structure);
        for (ordinal, item) in db.items()?.iter().enumerate() {
            assert_eq!(
                item.identity,
                hash(json!(["readme.md", "markdown", ordinal]).to_string())
            );
            assert_eq!(item.kind, "markdown");
            assert!(item.embedding.is_empty());
        }
        let mut semantic = empty.0.clone();
        semantic.source = "fn new() {}".into();
        semantic.hash = hash(&semantic.source);
        db.apply(&[(semantic, vec![])], &[], None)?;
        assert!(
            db.structure("empty.rs").is_err(),
            "old structures must not be presented for new source"
        );
        assert_eq!(db.missing_structure_paths("parser-2")?, ["empty.rs"]);
        Ok(())
    }

    #[test]
    fn projection_selection_and_mismatched_associations_fail_atomically() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        let record = parsed_record("code.rs", "fn example() {}")?;
        db.apply_structure(std::slice::from_ref(&record), &[], None)?;
        let a = json!({"model":"a"});
        let b = json!({"model":"b"});
        db.set_projection_profile(&a)?;
        db.put_search_cache("query", &[json!("a result")])?;
        db.conn.execute_batch("CREATE TRIGGER abort_profile BEFORE DELETE ON search_cache BEGIN SELECT RAISE(ABORT, 'profile failure'); END;")?;
        assert!(db.set_projection_profile(&b).is_err());
        assert_eq!(db.meta("active_embedding_profile")?, Some(a.to_string()));
        assert!(db.search_cache("query")?.is_some());
        db.conn.execute_batch("DROP TRIGGER abort_profile")?;
        let before = snapshot(&db)?;
        let input = &record.1.callables[0].embedding_input;
        let wrong_key = Database::embedding_key(&b, false, input);
        db.put_embedding(&wrong_key, &[0.6, 0.8])?;
        let mut items = db.items_for_profile(&b)?;
        assert_eq!(items[0].embedding, wrong_key);
        assert!(
            db.apply(&[(record.0.clone(), items.clone())], &[], Some("wrong"))
                .is_err()
        );
        assert_eq!(snapshot(&db)?, before);
        let right_key = Database::embedding_key(&a, false, input);
        db.put_embedding(&right_key, &[0.6, 0.8])?;
        items[0].embedding = right_key;
        db.apply(&[(record.0, items)], &[], Some("right"))?;
        db.set_projection_profile(&b)?;
        assert_eq!(db.meta("active_embedding_profile")?, Some(b.to_string()));
        assert_eq!(db.structure("code.rs")?, record.1.structure);
        assert_eq!(db.items_for_profile(&b)?[0].embedding, wrong_key);
        Ok(())
    }

    #[test]
    fn source_descriptions_publish_without_vectors_and_hydrate_for_each_profile() -> Result<()> {
        let (dir, mut db) = fixture()?;
        let mut record = parsed_record(
            "code.rs",
            "const LIMIT: usize = 10;\nstruct Settings;\nfn example() {}\n",
        )?;
        record.1.description = Some("Configuration utilities".into());
        record.1.callables[0].description = Some("Runs the example".into());
        for node in &mut record.1.structure.nodes {
            node.description = Some(
                match node.kind.as_str() {
                    "constant" => "Maximum number of requests",
                    "struct" => "Request configuration",
                    "function" => "Runs the example",
                    kind => panic!("unexpected declaration: {kind}"),
                }
                .into(),
            );
        }
        db.apply_structure(std::slice::from_ref(&record), &[], None)?;
        assert_eq!(db.structure("code.rs")?, record.1.structure);
        let file = db.files()?.remove(0);
        assert_eq!(file.hash, hash(&record.0.source));
        assert_eq!(file.description.as_deref(), Some("Configuration utilities"));
        assert_eq!(
            file.description_hash,
            Some(format!("source:{}", hash("Configuration utilities")))
        );
        assert!(file.description_embedding.is_none());
        let items = db.items()?;
        assert_eq!(items.len(), 3);
        assert_eq!(
            items
                .iter()
                .filter(|item| item.kind == "symbol-description")
                .count(),
            2
        );
        assert!(items.iter().all(|item| item.embedding.is_empty()
            && item.description_embedding.is_none()
            && item.data["sourceDescription"] == true));
        let descriptions = db.symbol_descriptions("code.rs")?;
        for node in &record.1.structure.nodes {
            assert_eq!(descriptions.get(&node.id), node.description.as_ref());
        }
        let a = json!({"model":"a"});
        let b = json!({"model":"b"});
        for text in std::iter::once("Configuration utilities").chain(
            items
                .iter()
                .map(|item| item.data["description"].as_str().unwrap()),
        ) {
            db.put_embedding(&Database::embedding_key(&a, false, text), &[1.0, 0.0])?;
        }
        assert!(db.files_for_profile(&a)?[0].description_embedding.is_some());
        assert!(db.files_for_profile(&b)?[0].description_embedding.is_none());
        assert!(
            db.items_for_profile(&a)?
                .iter()
                .all(|item| item.description_embedding.is_some())
        );
        assert!(
            db.items_for_profile(&b)?
                .iter()
                .all(|item| item.description_embedding.is_none())
        );
        // Publish the same description-only units through the engine contract,
        // omitting byte offsets and code input; declaration links must survive.
        db.set_projection_profile(&a)?;
        let mut items = db.items_for_profile(&a)?;
        for item in &mut items {
            if item.kind == "symbol-description" {
                item.data.as_object_mut().unwrap().remove("startByte");
                item.data.as_object_mut().unwrap().remove("endByte");
            }
        }
        db.apply(&[(file, items)], &[], None)?;
        assert_eq!(db.symbol_descriptions("code.rs")?, descriptions);
        assert_eq!(db.conn.query_row(
            "SELECT count(*) FROM unit_embeddings u JOIN search_units s ON s.id=u.unit_id WHERE s.kind='symbol-description' AND u.role='code'",
            [], |r| r.get::<_, i64>(0)
        )?, 0);
        drop(db);
        let db = Database::open_readonly(&dir.path().join("index.sqlite"), dir.path())?;
        assert_eq!(db.symbol_descriptions("code.rs")?, descriptions);
        assert_eq!(
            db.conn
                .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
            4
        );
        assert!(
            db.items()?
                .iter()
                .all(|item| item.description_embedding.is_some())
        );
        Ok(())
    }

    #[test]
    fn source_prose_supersedes_generated_prose_and_comment_removal_invalidates_it() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        let original = parsed_record("code.rs", "fn example() -> i32 { 1 }")?;
        db.apply_structure(std::slice::from_ref(&original), &[], None)?;
        let mut file = db.files()?.remove(0);
        file.description = Some("Generated file prose".into());
        file.description_hash = Some(file.hash.clone());
        file.description_embedding = Some("description".into());
        let mut items = db.items()?;
        items[0].data["description"] = json!("Generated callable prose");
        items[0].description_embedding = Some("description".into());
        db.apply(&[(file, items)], &[], None)?;

        let mut commented = original.clone();
        commented.1.description = Some("Source file prose".into());
        commented.1.callables[0].description = Some("Source callable prose".into());
        commented.1.structure.nodes[0].description = Some("Source callable prose".into());
        db.apply_structure(std::slice::from_ref(&commented), &[], None)?;
        let item = db.items()?.remove(0);
        let id = item.id;
        assert_eq!(item.data["description"], "Source callable prose");
        assert_eq!(item.data["sourceDescription"], true);
        assert!(item.description_embedding.is_none());
        assert_eq!(
            db.files()?[0].description.as_deref(),
            Some("Source file prose")
        );
        assert!(db.files()?[0].description_embedding.is_none());

        // Semantic generation cannot overwrite canonical declaration prose.
        let mut generated = item;
        generated.data["description"] = json!("Replacement generated prose");
        generated.data["sourceDescription"] = json!(false);
        generated.description_embedding = Some("description".into());
        let mut generated_file = db.files()?.remove(0);
        generated_file.description = Some("Replacement generated file prose".into());
        generated_file.description_hash = Some(generated_file.hash.clone());
        generated_file.description_embedding = Some("description".into());
        db.apply(&[(generated_file, vec![generated])], &[], None)?;
        assert_eq!(db.items()?[0].data["description"], "Source callable prose");
        assert!(db.items()?[0].description_embedding.is_none());
        assert_eq!(
            db.files()?[0].description.as_deref(),
            Some("Source file prose")
        );
        assert!(db.files()?[0].description_embedding.is_none());

        // Source prose remains authoritative even when executable code changes.
        let mut edited = parsed_record("code.rs", "\nfn example() -> i32 { 2 }")?;
        edited.1.description = commented.1.description.clone();
        edited.1.callables[0].description = commented.1.callables[0].description.clone();
        edited.1.structure.nodes[0].description =
            commented.1.structure.nodes[0].description.clone();
        db.apply_structure(std::slice::from_ref(&edited), &[], None)?;
        assert_eq!(db.items()?[0].id, id);
        assert_eq!(db.items()?[0].data["description"], "Source callable prose");

        // A removed comment must not be retained merely because callable source
        // hashes match. The engine may pass the previously hydrated file slot.
        edited.0 = db.files()?.remove(0);
        edited.1.description = None;
        edited.1.callables[0].description = None;
        edited.1.structure.nodes[0].description = None;
        db.apply_structure(&[edited], &[], None)?;
        let item = db.items()?.remove(0);
        assert_eq!(item.id, id);
        assert!(item.data["description"].is_null());
        assert_eq!(item.data["sourceDescription"], false);
        assert!(item.description_embedding.is_none());
        let file = db.files()?.remove(0);
        assert!(file.description.is_none());
        assert!(file.description_hash.is_none());
        assert!(file.description_embedding.is_none());
        assert!(db.symbol_descriptions("code.rs")?.is_empty());
        Ok(())
    }

    #[test]
    fn symbol_description_occurrences_and_vectors_survive_structural_edits() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        let mut record = parsed_record("code.ts", "type A = string;\ntype A = number;\n")?;
        assert_eq!(record.1.structure.nodes.len(), 2);
        record.1.structure.nodes[0].description = Some("First overload".into());
        record.1.structure.nodes[1].description = Some("Second overload".into());
        db.apply_structure(std::slice::from_ref(&record), &[], None)?;
        let profile = json!({"model":"a"});
        db.set_projection_profile(&profile)?;
        let mut items = db.items_for_profile(&profile)?;
        for item in &mut items {
            let text = item.data["description"].as_str().unwrap();
            let key = Database::embedding_key(&profile, false, text);
            db.put_embedding(&key, &[1.0, 0.0])?;
            item.description_embedding = Some(key);
        }
        let second_id = items[1].id;
        let second_identity = items[1].identity.clone();
        assert_eq!(
            second_identity,
            hash(
                json!([
                    "code.ts",
                    "symbol-description",
                    "A",
                    record.1.structure.nodes[1].kind,
                    1
                ])
                .to_string()
            )
        );
        db.apply(&[(db.files()?.remove(0), items)], &[], None)?;
        record.1.structure.nodes[0].description = None;
        db.apply_structure(std::slice::from_ref(&record), &[], None)?;
        let second = db.items()?.remove(0);
        assert_eq!(second.id, second_id);
        assert_eq!(second.identity, second_identity);
        assert!(second.embedding.is_empty());
        assert!(second.description_embedding.is_some());
        assert_eq!(db.symbol_descriptions("code.ts")?.len(), 1);
        record.1.structure.nodes[1].description = Some("Updated second overload".into());
        db.apply_structure(std::slice::from_ref(&record), &[], None)?;
        let second = db.items_for_profile(&profile)?.remove(0);
        assert_eq!(second.id, second_id);
        assert_eq!(second.data["description"], "Updated second overload");
        assert!(second.description_embedding.is_none());
        record.1.structure.nodes[1].description = None;
        db.apply_structure(&[record], &[], None)?;
        assert!(db.items()?.is_empty());
        assert!(db.symbol_descriptions("code.ts")?.is_empty());
        assert_eq!(
            db.conn.query_row(
                "SELECT count(*) FROM descriptions WHERE scope='callable'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        Ok(())
    }
}
