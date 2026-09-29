use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    hash, map,
    parse::{FileStructure, ParsedFile, StructureNode},
};

/// Bump when the structure or search-unit extraction contract changes.
pub const STRUCTURE_PARSER_VERSION: &str = "structure-v3";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS embeddings(key TEXT PRIMARY KEY,vector BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS cache(kind TEXT NOT NULL,key TEXT NOT NULL,value TEXT NOT NULL,PRIMARY KEY(kind,key));
CREATE TABLE IF NOT EXISTS description_content(hash TEXT PRIMARY KEY,content TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files(
 path TEXT PRIMARY KEY,source TEXT NOT NULL,hash TEXT NOT NULL,language TEXT NOT NULL,
 source_mode TEXT NOT NULL,parser_version TEXT,structure_hash TEXT,data TEXT NOT NULL);
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
 embedding_input_hash TEXT NOT NULL,
 FOREIGN KEY(path,symbol_id) REFERENCES symbols(path,id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX IF NOT EXISTS search_units_path ON search_units(path);
CREATE TABLE IF NOT EXISTS unit_embeddings(
 unit_id INTEGER NOT NULL REFERENCES search_units(id) ON DELETE CASCADE,
 role TEXT NOT NULL CHECK(role IN ('code','description')),profile_key TEXT NOT NULL,input_hash TEXT NOT NULL,
 embedding_key TEXT NOT NULL REFERENCES embeddings(key),
 PRIMARY KEY(unit_id,role,profile_key,input_hash));
CREATE TABLE IF NOT EXISTS descriptions(
 scope TEXT NOT NULL CHECK(scope IN ('file','callable')),path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
 identity TEXT NOT NULL,source_hash TEXT,text TEXT NOT NULL,embedding_key TEXT REFERENCES embeddings(key),
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
        Self::open_internal(path, root, force, false)
    }

    pub fn open_readonly(path: &Path, root: &Path) -> Result<Self> {
        Self::open_internal(path, root, false, true)
    }

    fn open_internal(path: &Path, root: &Path, force: bool, readonly: bool) -> Result<Self> {
        let conn = if readonly {
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?
        } else {
            Connection::open(path)?
        };
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        // All compatibility checks precede even journal-mode changes. Force is
        // only a root reset, never permission to migrate an older schema.
        let old_schema: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND (name GLOB 'rust_*' OR name IN ('items','functions','markdown_chunks','description_cache')))",
            [],
            |r| r.get(0),
        )?;
        let has_metadata: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='metadata')",
            [],
            |r| r.get(0),
        )?;
        let legacy_metadata = has_metadata
            && conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='schema_version')",
                [],
                |r| r.get::<_, bool>(0),
            )?;
        let identity: Option<String> = if has_metadata {
            conn.query_row("SELECT value FROM metadata WHERE key='identity'", [], |r| {
                r.get(0)
            })
            .optional()?
        } else {
            None
        };
        let identity: Option<Value> = identity
            .map(|s| serde_json::from_str(&s))
            .transpose()
            .context(
                "Unsupported index table layout: invalid identity; rebuild using a new index path",
            )?;
        let has_tables: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%')", [], |r| r.get(0))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            !old_schema
                && !legacy_metadata
                && identity
                    .as_ref()
                    .is_none_or(|v| v["schema"] == 3 && v["root"].is_string())
                && (!has_tables || identity.is_some())
                && (version == 0 || version == 3),
            "Unsupported index table layout. Remove the existing SQLite index at {} and rebuild, or use --index with a new path.",
            path.display()
        );
        if has_tables {
            for sql in [
                "SELECT key,value FROM metadata LIMIT 0",
                "SELECT path,source,hash,language,source_mode,parser_version,structure_hash,data FROM files LIMIT 0",
                "SELECT path,id,parent_id,ordinal,language,kind,name,qualified_name,signature,start_byte,end_byte,start_line,start_column,end_line,end_column,metadata FROM symbols LIMIT 0",
                "SELECT path,symbol_id,ordinal,name FROM symbol_names LIMIT 0",
                "SELECT id,path,identity,kind,symbol_id,data,embedding_input_hash FROM search_units LIMIT 0",
                "SELECT unit_id,role,profile_key,input_hash,embedding_key FROM unit_embeddings LIMIT 0",
                "SELECT scope,path,identity,source_hash,text,embedding_key FROM descriptions LIMIT 0",
                "SELECT path,ordinal,code,message,start_line,end_line,data FROM diagnostics LIMIT 0",
                "SELECT key,vector FROM embeddings LIMIT 0",
                "SELECT kind,key,value FROM cache LIMIT 0",
                "SELECT key,value FROM search_cache LIMIT 0",
            ] {
                conn.prepare(sql)
                    .context("Unsupported index table layout. Rebuild using a new index path.")?;
            }
        }
        let expected = json!({"schema":3,"root":root});
        let incompatible_root = identity
            .as_ref()
            .is_some_and(|v| v["root"] != expected["root"]);
        if incompatible_root && !force {
            bail!(
                "Incompatible index root. Use --force-reindex to rebuild live state (artifact caches are retained)."
            );
        }
        if readonly {
            ensure!(
                has_tables && identity.is_some() && version == 3,
                "Index is not initialized; reopen for writing"
            );
            return Ok(Self {
                conn,
                path: path.to_owned(),
            });
        }
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(SCHEMA)?;
        tx.execute_batch("PRAGMA user_version=3;")?;
        if incompatible_root {
            reset_live(&tx)?;
        }
        tx.execute("INSERT INTO metadata VALUES('identity',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [expected.to_string()])?;
        tx.commit()?;
        let db = Self {
            conn,
            path: path.to_owned(),
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
                "SELECT value FROM cache WHERE kind=? AND key=?",
                params![kind, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn cache_put(&self, kind: &str, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO cache VALUES(?,?,?) ON CONFLICT(kind,key) DO UPDATE SET value=excluded.value", params![kind,key,value])?;
        Ok(())
    }

    pub(crate) fn description_content(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT content FROM description_content WHERE hash=?",
                [key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub(crate) fn put_description_content(&self, key: &str, value: &str) -> Result<()> {
        ensure!(hash(value) == key, "Description content hash mismatch");
        self.conn.execute("INSERT INTO description_content VALUES(?,?) ON CONFLICT(hash) DO UPDATE SET content=excluded.content WHERE content<>excluded.content", params![key,value])?;
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
            tx.execute("INSERT INTO description_content VALUES(?,?) ON CONFLICT(hash) DO UPDATE SET content=excluded.content WHERE content<>excluded.content", params![content_hash,content])?;
        }
        tx.execute("INSERT INTO cache VALUES('description',?,?) ON CONFLICT(kind,key) DO UPDATE SET value=excluded.value", params![key,value])?;
        tx.commit()?;
        Ok(())
    }

    pub fn embedding_key(profile: &Value, query: bool, input: &str) -> String {
        hash(json!([profile, if query { "query" } else { "document" }, input]).to_string())
    }

    pub fn embedding(&self, key: &str) -> Result<Option<Vec<f32>>> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT vector FROM embeddings WHERE key=?", [key], |r| {
                r.get(0)
            })
            .optional()?;
        bytes.map(|b| decode(&b)).transpose()
    }

    pub fn put_embedding(&self, key: &str, vector: &[f32]) -> Result<()> {
        ensure!(
            !vector.is_empty() && vector.iter().all(|v| v.is_finite()),
            "Invalid embedding"
        );
        let bytes: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.conn.execute(
            "INSERT OR IGNORE INTO embeddings VALUES(?,?)",
            params![key, bytes],
        )?;
        Ok(())
    }

    pub fn files(&self) -> Result<Vec<File>> {
        // files.data is a write-only compatibility snapshot. Live reads depend
        // exclusively on normalized source/provenance and artifact tables.
        let mut stmt = self
            .conn
            .prepare("SELECT path,source,hash,language,source_mode FROM files ORDER BY path")?;
        let rows = stmt.query_map([], |r| {
            Ok(File {
                path: r.get(0)?,
                source: r.get(1)?,
                hash: r.get(2)?,
                language: r.get(3)?,
                source_mode: r.get(4)?,
                description: None,
                description_hash: None,
                description_embedding: None,
                errors: Vec::new(),
            })
        })?;
        rows.map(|r| {
            let mut file = r?;
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

    /// Compatibility view: the latest published association for the current
    /// input. Engine search should use items_for_profile instead.
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
        // Validate cached vectors, rather than silently hiding corrupt artifacts.
        Ok(self.embedding(&key)?.map(|_| key))
    }

    fn read_items(&self, profile: Option<&Value>) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,path,identity,kind,data,embedding_input_hash FROM search_units ORDER BY id",
        )?;
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?
        .map(|r| {
            let (id, path, identity, kind, data, input_hash) = r?;
            let mut data: Value = serde_json::from_str(&data)?;
            ensure!(data.is_object(), "Invalid stored search unit");
            let stored_description = description(&self.conn, "callable", &path, &identity)?
                .filter(|d| d.0.as_deref() == data["sourceHash"].as_str());
            if let Some(d) = &stored_description {
                data["description"] = json!(d.1);
            } else if data.get("description").is_some() {
                data["description"] = Value::Null;
            }
            let embedding = if let Some(profile) = profile {
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

    /// Saved callable descriptions keyed by their structural symbol, without
    /// loading vectors or requiring a description provider for `map`.
    pub fn symbol_descriptions(&self, path: &str) -> Result<HashMap<usize, String>> {
        let mut stmt = self.conn.prepare("SELECT s.symbol_id,s.data,d.source_hash,d.text
            FROM search_units s JOIN descriptions d ON d.scope='callable' AND d.path=s.path AND d.identity=s.identity
            WHERE s.path=? AND s.kind='function' AND s.symbol_id IS NOT NULL ORDER BY s.id")?;
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
            if source_hash
                .as_deref()
                .is_some_and(|hash| Some(hash) == data["sourceHash"].as_str())
            {
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
        if !dirty && old_checkpoint.as_deref() == checkpoint {
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
        let tx = self.conn.transaction()?;
        for path in removed {
            tx.execute("DELETE FROM files WHERE path=?", [path])?;
        }
        for (file, items, structure) in changed {
            write_file(&tx, file)?;
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
    // Provider artifacts are durable even across a forced root reset. Retaining
    // identity keeps a reset v3 database identifiable when it is reopened.
    conn.execute_batch("DELETE FROM search_units; DELETE FROM files; DELETE FROM search_cache; DELETE FROM metadata WHERE key<>'identity';")?;
    Ok(())
}

type Description = (Option<String>, String, Option<String>);

fn description(
    conn: &Connection,
    scope: &str,
    path: &str,
    identity: &str,
) -> Result<Option<Description>> {
    Ok(conn.query_row("SELECT source_hash,text,embedding_key FROM descriptions WHERE scope=? AND path=? AND identity=?",
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

fn write_file(conn: &Connection, file: &File) -> Result<()> {
    conn.execute("INSERT INTO files(path,source,hash,language,source_mode,data) VALUES(?,?,?,?,?,?) ON CONFLICT(path) DO UPDATE SET
        source=excluded.source,hash=excluded.hash,language=excluded.language,source_mode=excluded.source_mode,data=excluded.data,
        parser_version=CASE WHEN files.hash=excluded.hash THEN files.parser_version END,
        structure_hash=CASE WHEN files.hash=excluded.hash THEN files.structure_hash END",
        params![file.path, file.source, file.hash, file.language, file.source_mode, serde_json::to_string(file)?])?;
    conn.execute(
        "DELETE FROM descriptions WHERE scope='file' AND path=?",
        [&file.path],
    )?;
    if let Some(text) = &file.description {
        conn.execute("INSERT INTO descriptions(scope,path,identity,source_hash,text,embedding_key) VALUES('file',?,'',?,?,?)",
            params![file.path, file.description_hash, text, file.description_embedding])?;
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
        let metadata = json!({"attributes":node.attributes,"imports":node.imports,"headingLevel":node.heading_level});
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
            node.names = names.remove(&node.id).unwrap_or_default();
            Ok(node)
        })
        .collect::<Result<_>>()?;
    Ok(FileStructure { nodes })
}

fn parsed_items(file: &File, parsed: &ParsedFile) -> Result<Vec<Item>> {
    let mut occurrences = HashMap::<&str, usize>::new();
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
        data["description"] = Value::Null;
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
    for (ordinal, chunk) in parsed.chunks.iter().enumerate() {
        let kind = if file.language == "markdown" {
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
    if let Some(profile) = profile {
        ensure!(
            key == Database::embedding_key(profile, false, input),
            "Embedding reference does not match projection profile/input"
        );
    }
    let profile_key = profile.map(|p| hash(p.to_string())).unwrap_or_default();
    let input_hash = hash(input);
    // Reinsert the same association to make rowid a deterministic latest-write
    // order for the compatibility items() view; other profiles/inputs survive.
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
        let previous_path: Option<String> = conn
            .query_row(
                "SELECT path FROM search_units WHERE identity=?",
                [&item.identity],
                |r| r.get(0),
            )
            .optional()?;
        ensure!(
            previous_path.as_ref().is_none_or(|p| p == &file.path),
            "Search unit identity belongs to another file"
        );
        let mut data = item.data.clone();
        let old_description = description(conn, "callable", &file.path, &item.identity)?;
        if structural
            && let Some(d) = &old_description
            && d.0.as_deref() == data["sourceHash"].as_str()
        {
            data["description"] = json!(d.1);
        }
        let input = data["embeddingInput"].as_str().unwrap_or("");
        let input_hash = hash(input);
        conn.execute("INSERT INTO search_units(path,identity,kind,symbol_id,data,embedding_input_hash) VALUES(?,?,?,?,?,?) ON CONFLICT(identity) DO UPDATE SET
            kind=excluded.kind,symbol_id=excluded.symbol_id,data=excluded.data,embedding_input_hash=excluded.embedding_input_hash",
            params![item.path, item.identity, item.kind, symbol_for(item, structure).map(i64::try_from).transpose()?, data.to_string(), input_hash])?;
        let id: i64 = conn.query_row(
            "SELECT id FROM search_units WHERE identity=?",
            [&item.identity],
            |r| r.get(0),
        )?;
        associate(conn, id, "code", profile, input, &item.embedding)?;
        conn.execute(
            "DELETE FROM descriptions WHERE scope='callable' AND path=? AND identity=?",
            params![item.path, item.identity],
        )?;
        if let Some(text) = data["description"].as_str() {
            let embedding = if structural {
                old_description.as_ref().and_then(|d| d.2.as_deref())
            } else {
                item.description_embedding.as_deref()
            };
            conn.execute("INSERT INTO descriptions(scope,path,identity,source_hash,text,embedding_key) VALUES('callable',?,?,?,?,?)",
                params![item.path, item.identity, data["sourceHash"].as_str(), text, embedding])?;
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
        let db = Database::open(
            &dir.path().join("index.sqlite"),
            dir.path(),
            &json!({}),
            false,
        )?;
        db.put_embedding("code", &[0.6, 0.8])?;
        db.put_embedding("description", &[1.0, 0.0])?;
        Ok((dir, db))
    }

    fn record(path: &str) -> (File, Vec<Item>) {
        let file = File {
            path: path.into(),
            hash: "source-hash".into(),
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
            "search": db.search_cache("query")?,
        }))
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
                "INSERT OR REPLACE INTO embeddings VALUES('corrupt', ?)",
                [blob],
            )?;
            assert_eq!(db.embedding("corrupt").unwrap_err().to_string(), message);
            assert_eq!(db.embedding("code")?, Some(vec![0.6, 0.8]));
        }
        Ok(())
    }

    #[test]
    fn corrupt_json_records_fail_reads_and_can_be_repaired() -> Result<()> {
        let (_dir, mut db) = fixture()?;
        db.apply(&[record("code.rs")], &[], None)?;
        let before = snapshot(&db)?;
        for value in ["{", "null", "{}"] {
            db.conn.execute("UPDATE files SET data=?", [value])?;
            assert_eq!(
                snapshot(&db)?,
                before,
                "compatibility JSON is not authoritative"
            );
        }
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
    fn missing_embedding_foreign_keys_abort_publication_and_retry_cleanly() -> Result<()> {
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
        let db = Database::open(&path, dir.path(), &profile, false)?;
        let tables: Vec<String> = db.conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        assert_eq!(
            tables,
            [
                "cache",
                "description_content",
                "descriptions",
                "diagnostics",
                "embeddings",
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
        let item = db.items_for_profile(&a)?.remove(0);
        assert_eq!(item.id, id);
        assert_eq!(item.data["description"], text);
        assert_eq!(item.embedding, code_a);
        assert!(item.description_embedding.is_some());
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
}
