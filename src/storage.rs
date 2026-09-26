use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hash;

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
    pub fn open(path: &Path, root: &Path, profile: &Value, force: bool) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS rust_metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS rust_embeddings(key TEXT PRIMARY KEY,vector BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS rust_cache(kind TEXT NOT NULL,key TEXT NOT NULL,value TEXT NOT NULL,PRIMARY KEY(kind,key));
             CREATE TABLE IF NOT EXISTS rust_files(path TEXT PRIMARY KEY,data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS rust_items(
               id INTEGER PRIMARY KEY AUTOINCREMENT,path TEXT NOT NULL REFERENCES rust_files(path) ON DELETE CASCADE,
               identity TEXT NOT NULL UNIQUE,kind TEXT NOT NULL,data TEXT NOT NULL,
               embedding TEXT NOT NULL REFERENCES rust_embeddings(key),
               description_embedding TEXT REFERENCES rust_embeddings(key));
             CREATE INDEX IF NOT EXISTS rust_items_path ON rust_items(path);
             CREATE TABLE IF NOT EXISTS rust_search_cache(key TEXT PRIMARY KEY,value TEXT NOT NULL);")?;
        let db = Self {
            conn,
            path: path.to_owned(),
        };
        let expected = json!({"schema":1,"root":root,"embedding":profile});
        let expected = expected.to_string();
        if let Some(stored) = db.meta("identity")?
            && stored != expected
        {
            if !force {
                bail!(
                    "Incompatible index root or embedding profile. Use --force-reindex to rebuild live state (artifact caches are retained)."
                );
            }
            db.reset()?;
        }
        db.set_meta("identity", &expected)?;
        Ok(db)
    }

    pub fn reset(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE; DELETE FROM rust_items; DELETE FROM rust_files; DELETE FROM rust_search_cache; DELETE FROM rust_metadata; DELETE FROM rust_cache WHERE kind IN ('legacy-file','legacy-function'); COMMIT;")?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM rust_metadata WHERE key=?", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO rust_metadata VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key,value])?;
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
                "SELECT value FROM rust_cache WHERE kind=? AND key=?",
                params![kind, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn cache_put(&self, kind: &str, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO rust_cache VALUES(?,?,?) ON CONFLICT(kind,key) DO UPDATE SET value=excluded.value", params![kind,key,value])?;
        Ok(())
    }

    pub fn embedding_key(profile: &Value, query: bool, input: &str) -> String {
        hash(json!([profile, if query { "query" } else { "document" }, input]).to_string())
    }

    pub fn embedding(&self, key: &str) -> Result<Option<Vec<f32>>> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT vector FROM rust_embeddings WHERE key=?",
                [key],
                |r| r.get(0),
            )
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
            "INSERT OR IGNORE INTO rust_embeddings VALUES(?,?)",
            params![key, bytes],
        )?;
        Ok(())
    }

    pub fn files(&self) -> Result<Vec<File>> {
        let mut stmt = self
            .conn
            .prepare("SELECT data FROM rust_files ORDER BY path")?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .map(|r| Ok(serde_json::from_str(&r?)?))
            .collect()
    }

    pub fn items(&self) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare("SELECT id,path,identity,kind,data,embedding,description_embedding FROM rust_items ORDER BY id")?;
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?
        .map(|r| {
            let (id, path, identity, kind, data, embedding, description_embedding) = r?;
            Ok(Item {
                id,
                path,
                identity,
                kind,
                data: serde_json::from_str(&data)?,
                embedding,
                description_embedding,
            })
        })
        .collect()
    }

    /// A refresh commits live records and its generation together. Completed model
    /// and parse artifacts were already committed independently for retry reuse.
    pub fn apply(
        &mut self,
        changed: &[(File, Vec<Item>)],
        removed: &[String],
        checkpoint: Option<&str>,
    ) -> Result<bool> {
        let old_checkpoint = self.meta("checkpoint")?;
        let dirty = !changed.is_empty() || !removed.is_empty();
        if !dirty && old_checkpoint.as_deref() == checkpoint {
            return Ok(false);
        }
        let generation = self.generation()? + u64::from(dirty);
        let tx = self.conn.transaction()?;
        for path in removed {
            tx.execute("DELETE FROM rust_files WHERE path=?", [path])?;
        }
        for (file, items) in changed {
            tx.execute("INSERT INTO rust_files VALUES(?,?) ON CONFLICT(path) DO UPDATE SET data=excluded.data",params![file.path,serde_json::to_string(file)?])?;
            // Reconcile by identity to preserve IDs across edits and line shifts.
            let identities: std::collections::HashSet<_> =
                items.iter().map(|i| i.identity.as_str()).collect();
            let existing: Vec<(i64, String)> = tx
                .prepare("SELECT id,identity FROM rust_items WHERE path=?")?
                .query_map([&file.path], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, identity) in existing {
                if !identities.contains(identity.as_str()) {
                    tx.execute("DELETE FROM rust_items WHERE id=?", [id])?;
                }
            }
            for item in items {
                tx.execute("INSERT INTO rust_items(path,identity,kind,data,embedding,description_embedding) VALUES(?,?,?,?,?,?) ON CONFLICT(identity) DO UPDATE SET path=excluded.path,kind=excluded.kind,data=excluded.data,embedding=excluded.embedding,description_embedding=excluded.description_embedding",
                    params![item.path,item.identity,item.kind,item.data.to_string(),item.embedding,item.description_embedding])?;
            }
        }
        tx.execute("INSERT INTO rust_metadata VALUES('generation',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[generation.to_string()])?;
        tx.execute("DELETE FROM rust_metadata WHERE key='checkpoint'", [])?;
        if let Some(commit) = checkpoint {
            tx.execute("INSERT INTO rust_metadata VALUES('checkpoint',?)", [commit])?;
        }
        if dirty {
            tx.execute("DELETE FROM rust_search_cache", [])?;
        }
        tx.commit()?;
        Ok(dirty)
    }

    pub fn search_cache(&self, key: &str) -> Result<Option<Vec<Value>>> {
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM rust_search_cache WHERE key=?",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        value
            .map(|v| serde_json::from_str(&v).context("Corrupt search cache"))
            .transpose()
    }

    pub fn put_search_cache(&self, key: &str, value: &[Value]) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO rust_search_cache VALUES(?,?)",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }

    /// Import description state and reusable document artifacts from TS schema 11.
    /// Original tables remain intact; vec0 is never loaded or mutated.
    pub fn import_legacy(&self, profile: &Value) -> Result<()> {
        if self.meta("legacy_imported")?.is_some() {
            return Ok(());
        }
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='metadata')",
            [],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(());
        }
        let legacy_meta = |key: &str| -> Result<Option<String>> {
            Ok(self
                .conn
                .query_row("SELECT value FROM metadata WHERE key=?", [key], |r| {
                    r.get(0)
                })
                .optional()?)
        };
        let schema = legacy_meta("schema_version")?;
        let empty: bool =
            self.conn
                .query_row("SELECT NOT EXISTS(SELECT 1 FROM metadata)", [], |r| {
                    r.get(0)
                })?;
        if empty {
            return Ok(());
        }
        ensure!(
            schema.as_deref() == Some("11"),
            "Unsupported legacy index schema version {}",
            schema.as_deref().unwrap_or("unknown")
        );

        // Commit the marker with the artifacts, so a failed import is retryable.
        let tx = self.conn.unchecked_transaction()?;
        for key in ["descriptions_enabled", "description_profile"] {
            if let Some(value) = legacy_meta(key)? {
                // Description settings are independent of the embedding model.
                // Keep the raw profile: the engine uses its provider/model defaults.
                if self.meta(key)?.is_none() {
                    self.set_meta(key, &value)?;
                }
            }
        }
        let previous: Option<Value> = legacy_meta("embedding_profile")?
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .context("Invalid legacy embedding profile")?;
        let compatible = previous.as_ref().is_some_and(|previous| {
            ["provider", "model", "dimensions"]
                .iter()
                .all(|key| !previous[key].is_null() && previous[key] == profile[key])
        });
        if compatible {
            // chunkMarkdown's embeddingInput was exactly the stored content,
            // including its ancestor headings (no path or heading-path prefix).
            let mut stmt = self.conn.prepare("SELECT f.embedding_input,e.vector FROM functions f JOIN embeddings e ON e.id=f.embedding_id UNION ALL SELECT m.content,e.vector FROM markdown_chunks m JOIN embeddings e ON e.id=m.embedding_id")?;
            for row in stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })? {
                let (input, bytes) = row?;
                self.import_legacy_embedding(profile, &input, Some(&bytes))?;
            }

            let mut stmt = self.conn.prepare("SELECT f.path,f.file_description,f.file_description_content_hash,e.vector FROM files f LEFT JOIN embeddings e ON e.id=f.file_description_embedding_id WHERE f.file_description IS NOT NULL")?;
            for row in stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<Vec<u8>>>(3)?,
                ))
            })? {
                let (path, description, description_hash, bytes) = row?;
                let embedding =
                    self.import_legacy_embedding(profile, &description, bytes.as_deref())?;
                self.cache_put("legacy-file", &path, &json!({
                    "description": description, "descriptionHash": description_hash, "embeddingKey": embedding
                }).to_string())?;
            }

            let mut stmt = self.conn.prepare("SELECT f.path,f.qualified_name,f.source_hash,f.description,e.vector FROM functions f LEFT JOIN embeddings e ON e.id=f.description_embedding_id WHERE f.description IS NOT NULL")?;
            for row in stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<Vec<u8>>>(4)?,
                ))
            })? {
                let (path, qualified_name, source_hash, description, bytes) = row?;
                let embedding =
                    self.import_legacy_embedding(profile, &description, bytes.as_deref())?;
                let key = hash(json!([path, qualified_name, source_hash]).to_string());
                self.cache_put(
                    "legacy-function",
                    &key,
                    &json!({
                        "description": description, "embeddingKey": embedding
                    })
                    .to_string(),
                )?;
            }
        }
        self.set_meta("legacy_imported", "true")?;
        tx.commit()?;
        Ok(())
    }

    fn import_legacy_embedding(
        &self,
        profile: &Value,
        input: &str,
        bytes: Option<&[u8]>,
    ) -> Result<Option<String>> {
        // TS OpenAI truncated at 8192 tokens; Rust truncates at 8191 bytes.
        // Jina uses the same code.passage task and server-side truncate=true.
        let equivalent = match profile["provider"].as_str() {
            Some("openai") => input.len() <= 8191,
            Some("jina") => true,
            _ => false,
        };
        let Some(bytes) = bytes.filter(|_| equivalent) else {
            return Ok(None);
        };
        let vector = decode(bytes)?;
        if Some(vector.len() as u64) != profile["dimensions"].as_u64() {
            return Ok(None);
        }
        let key = Self::embedding_key(profile, false, input);
        self.put_embedding(&key, &vector)?;
        Ok(Some(key))
    }
}

fn decode(bytes: &[u8]) -> Result<Vec<f32>> {
    ensure!(
        !bytes.is_empty() && bytes.len().is_multiple_of(4),
        "Invalid stored vector length"
    );
    let values: Vec<_> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
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

    fn profile(provider: &str) -> Value {
        json!({"provider": provider, "model": "test-embedding", "dimensions": 2, "strategyVersion": "rust-v1"})
    }

    fn database(profile: &Value) -> Database {
        Database::open(Path::new(":memory:"), Path::new("/repo"), profile, false).unwrap()
    }

    fn legacy_meta(db: &Database, key: &str, value: &str) {
        db.conn
            .execute(
                "INSERT OR REPLACE INTO metadata VALUES(?,?)",
                params![key, value],
            )
            .unwrap();
    }

    fn legacy(db: &Database, profile: &Value) {
        // Only ordinary schema-11 tables are needed: no vec0 extension or index.
        db.conn.execute_batch("
            CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE embeddings(id INTEGER PRIMARY KEY,embedding_key TEXT UNIQUE,vector BLOB NOT NULL);
            CREATE TABLE files(path TEXT PRIMARY KEY,file_description TEXT,file_description_content_hash TEXT,file_description_embedding_id INTEGER);
            CREATE TABLE functions(path TEXT,qualified_name TEXT,source_hash TEXT,embedding_input TEXT,embedding_id INTEGER,description TEXT,description_embedding_id INTEGER);
            CREATE TABLE markdown_chunks(path TEXT,heading_path TEXT,content TEXT,embedding_id INTEGER);
            CREATE TABLE description_cache(description_key TEXT PRIMARY KEY,description TEXT NOT NULL);
            INSERT INTO description_cache VALUES('old-prompt-key','Cached legacy response');
        ").unwrap();
        legacy_meta(db, "schema_version", "11");
        let mut old_profile = profile.clone();
        old_profile["strategyVersion"] = json!(if profile["provider"] == "jina" {
            "callable-v2:code-query-passage"
        } else {
            "callable-v2"
        });
        legacy_meta(db, "embedding_profile", &old_profile.to_string());
        legacy_meta(db, "descriptions_enabled", "true");
        legacy_meta(
            db,
            "description_profile",
            r#"{ "provider": "opencode", "model": "old-model", "strategyVersion": "older-purpose" }"#,
        );
        let bytes: Vec<u8> = [0.6_f32, 0.8]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        db.conn
            .execute(
                "INSERT INTO embeddings VALUES(1,'old-vector-key',?)",
                [bytes],
            )
            .unwrap();
    }

    fn file(db: &Database, path: &str, description: &str, embedding: Option<i64>) {
        db.conn
            .execute(
                "INSERT INTO files VALUES(?,?,?,?)",
                params![path, description, hash(format!("source:{path}")), embedding],
            )
            .unwrap();
    }

    fn function(db: &Database, path: &str, input: &str, description: &str, embedding: Option<i64>) {
        db.conn
            .execute(
                "INSERT INTO functions VALUES(?,'Widget.run','source-hash',?,1,?,?)",
                params![path, input, description, embedding],
            )
            .unwrap();
    }

    fn markdown(db: &Database, path: &str, content: &str) {
        db.conn
            .execute(
                "INSERT INTO markdown_chunks VALUES(?,'[\"Guide\",\"Usage\"]',?,1)",
                params![path, content],
            )
            .unwrap();
    }

    fn cache(db: &Database, kind: &str, key: &str) -> Value {
        serde_json::from_str(&db.cache(kind, key).unwrap().unwrap()).unwrap()
    }

    fn function_key(path: &str) -> String {
        hash(json!([path, "Widget.run", "source-hash"]).to_string())
    }

    fn snapshot(db: &Database, tables: &[&str]) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
        tables
            .iter()
            .map(|table| {
                let mut stmt = db
                    .conn
                    .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                    .unwrap();
                let columns = stmt.column_count();
                stmt.query_map([], |r| {
                    (0..columns)
                        .map(|i| r.get(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
            })
            .collect()
    }

    const LEGACY_TABLES: &[&str] = &[
        "metadata",
        "files",
        "functions",
        "markdown_chunks",
        "embeddings",
        "description_cache",
    ];
    const RUST_TABLES: &[&str] = &[
        "rust_metadata",
        "rust_cache",
        "rust_embeddings",
        "rust_files",
        "rust_items",
    ];

    #[test]
    fn legacy_descriptions_and_markdown_survive_reopen_without_vec0() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".slopdex");
        let profile = profile("openai");
        let db = Database::open(&path, dir.path(), &profile, false).unwrap();
        legacy(&db, &profile);
        file(&db, "src/é.rs", "File purpose", Some(1));
        file(&db, "README.md", "Documentation purpose", Some(1));
        file(&db, "empty.txt", "", None);
        function(&db, "src/é.rs", "fn run() {}", "Callable purpose", Some(1));
        let content = "# Guide\n## Usage\n\nRun `slopdex`.";
        markdown(&db, "README.md", content);
        let original = snapshot(&db, LEGACY_TABLES);

        db.import_legacy(&profile).unwrap();
        assert_eq!(
            db.meta("descriptions_enabled").unwrap().as_deref(),
            Some("true")
        );
        let old_profile: String = db
            .conn
            .query_row(
                "SELECT value FROM metadata WHERE key='description_profile'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(db.meta("description_profile").unwrap(), Some(old_profile));
        for (path, description) in [
            ("src/é.rs", "File purpose"),
            ("README.md", "Documentation purpose"),
            ("empty.txt", ""),
        ] {
            assert_eq!(
                cache(&db, "legacy-file", path),
                json!({
                    "description": description,
                    "descriptionHash": hash(format!("source:{path}")),
                    "embeddingKey": if description.is_empty() { None } else { Some(Database::embedding_key(&profile, false, description)) }
                })
            );
        }
        assert_eq!(
            cache(&db, "legacy-function", &function_key("src/é.rs")),
            json!({
                "description": "Callable purpose",
                "embeddingKey": Database::embedding_key(&profile, false, "Callable purpose")
            })
        );
        for input in [
            "fn run() {}",
            content,
            "File purpose",
            "Documentation purpose",
            "Callable purpose",
        ] {
            assert_eq!(
                db.embedding(&Database::embedding_key(&profile, false, input))
                    .unwrap(),
                Some(vec![0.6, 0.8])
            );
            assert!(
                db.embedding(&Database::embedding_key(&profile, true, input))
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(snapshot(&db, LEGACY_TABLES), original);
        assert!(db.files().unwrap().is_empty());
        assert!(db.items().unwrap().is_empty());
        assert_eq!(db.meta("legacy_imported").unwrap().as_deref(), Some("true"));

        // A subsequent open must not undo settings/artifacts updated by the engine.
        db.set_meta("descriptions_enabled", "false").unwrap();
        db.cache_put("legacy-file", "README.md", "updated by engine")
            .unwrap();
        let imported = snapshot(&db, RUST_TABLES);
        drop(db);
        let db = Database::open(&path, dir.path(), &profile, false).unwrap();
        db.import_legacy(&profile).unwrap();
        assert_eq!(snapshot(&db, RUST_TABLES), imported);
        assert_eq!(snapshot(&db, LEGACY_TABLES), original);
    }

    #[test]
    fn legacy_vectors_require_equivalent_preprocessing_for_every_artifact() {
        for provider in ["openai", "jina"] {
            for input in [
                "x".repeat(8190),
                "x".repeat(8191),
                "x".repeat(8192),
                "é".repeat(4096),
            ] {
                let profile = profile(provider);
                let db = database(&profile);
                legacy(&db, &profile);
                file(&db, "described", &input, Some(1));
                function(
                    &db,
                    "callable",
                    &format!("{input}c"),
                    &format!("{input}d"),
                    Some(1),
                );
                markdown(&db, "README.md", &format!("{input}m"));
                db.import_legacy(&profile).unwrap();
                for text in [
                    &input,
                    &format!("{input}c"),
                    &format!("{input}d"),
                    &format!("{input}m"),
                ] {
                    let reusable = provider == "jina" || text.len() <= 8191;
                    assert_eq!(
                        db.embedding(&Database::embedding_key(&profile, false, text))
                            .unwrap()
                            .is_some(),
                        reusable
                    );
                }
                for (kind, key, text) in [
                    ("legacy-file", "described".to_owned(), input.clone()),
                    (
                        "legacy-function",
                        function_key("callable"),
                        format!("{input}d"),
                    ),
                ] {
                    let cached = cache(&db, kind, &key);
                    assert_eq!(cached["description"], text);
                    if provider == "jina" || text.len() <= 8191 {
                        let key = cached["embeddingKey"].as_str().unwrap();
                        assert!(db.embedding(key).unwrap().is_some());
                    } else {
                        assert!(cached["embeddingKey"].is_null());
                    }
                }
            }
        }
    }

    #[test]
    fn legacy_description_text_survives_missing_or_wrong_dimension_vectors() {
        let profile = profile("openai");
        let db = database(&profile);
        legacy(&db, &profile);
        db.conn
            .execute(
                "INSERT INTO embeddings VALUES(2,'wrong-dimensions',?)",
                [1_f32.to_le_bytes().to_vec()],
            )
            .unwrap();
        for (path, vector) in [
            ("missing", None),
            ("dangling", Some(99)),
            ("wrong", Some(2)),
        ] {
            file(&db, path, "File text", vector);
            function(&db, path, "code", "Function text", vector);
        }
        db.conn
            .execute(
                "UPDATE files SET file_description_content_hash=NULL WHERE path='missing'",
                [],
            )
            .unwrap();
        db.import_legacy(&profile).unwrap();
        for path in ["missing", "dangling", "wrong"] {
            let file = cache(&db, "legacy-file", path);
            assert_eq!(file["description"], "File text");
            assert!(file["embeddingKey"].is_null());
            assert_eq!(
                cache(&db, "legacy-function", &function_key(path)),
                json!({"description": "Function text", "embeddingKey": null})
            );
        }
        assert!(cache(&db, "legacy-file", "missing")["descriptionHash"].is_null());
        assert!(
            db.embedding(&Database::embedding_key(&profile, false, "File text"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn legacy_embedding_mismatch_keeps_settings_but_not_artifacts() {
        for (key, value) in [
            ("provider", json!("jina")),
            ("model", json!("other")),
            ("dimensions", json!(3)),
            ("dimensions", Value::Null),
        ] {
            for enabled in ["true", "false"] {
                let profile = profile("openai");
                let db = database(&profile);
                legacy(&db, &profile);
                legacy_meta(&db, "descriptions_enabled", enabled);
                let mut previous = profile.clone();
                previous[key] = value.clone();
                legacy_meta(&db, "embedding_profile", &previous.to_string());
                file(&db, "file", "description", Some(1));
                function(&db, "file", "code", "description", Some(1));
                markdown(&db, "README.md", "markdown");
                let original = snapshot(&db, LEGACY_TABLES);
                db.import_legacy(&profile).unwrap();
                assert_eq!(
                    db.meta("descriptions_enabled").unwrap().as_deref(),
                    Some(enabled)
                );
                assert!(
                    db.meta("description_profile")
                        .unwrap()
                        .unwrap()
                        .contains("old-model")
                );
                assert!(
                    snapshot(&db, &["rust_cache", "rust_embeddings"])
                        .iter()
                        .all(Vec::is_empty)
                );
                let imported = snapshot(&db, RUST_TABLES);
                db.import_legacy(&profile).unwrap();
                assert_eq!(snapshot(&db, RUST_TABLES), imported);
                assert_eq!(snapshot(&db, LEGACY_TABLES), original);
            }
        }
    }

    #[test]
    fn legacy_missing_metadata_is_harmless_and_unknown_schema_is_explicit() {
        let profile = profile("openai");
        let db = database(&profile);
        let original = snapshot(&db, RUST_TABLES);
        db.import_legacy(&profile).unwrap();
        assert_eq!(snapshot(&db, RUST_TABLES), original);
        db.conn
            .execute_batch("CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL)")
            .unwrap();
        db.import_legacy(&profile).unwrap();
        assert_eq!(snapshot(&db, RUST_TABLES), original);
        legacy_meta(&db, "descriptions_enabled", "true");
        for schema in [None, Some("10"), Some("12")] {
            if let Some(schema) = schema {
                legacy_meta(&db, "schema_version", schema);
            }
            let old = snapshot(&db, &["metadata"]);
            let error = db.import_legacy(&profile).unwrap_err().to_string();
            assert!(error.contains(&format!(
                "Unsupported legacy index schema version {}",
                schema.unwrap_or("unknown")
            )));
            assert_eq!(snapshot(&db, RUST_TABLES), original);
            assert_eq!(snapshot(&db, &["metadata"]), old);
        }
        // Schema 11 without an embedding profile can still preserve settings.
        legacy_meta(&db, "schema_version", "11");
        db.import_legacy(&profile).unwrap();
        assert_eq!(
            db.meta("descriptions_enabled").unwrap().as_deref(),
            Some("true")
        );
        assert!(db.meta("description_profile").unwrap().is_none());
        assert_eq!(db.meta("legacy_imported").unwrap().as_deref(), Some("true"));
    }

    #[test]
    fn failed_legacy_import_rolls_back_and_can_retry() {
        let profile = profile("openai");
        let db = database(&profile);
        legacy(&db, &profile);
        file(&db, "file", "valid file description", Some(1));
        db.conn
            .execute("INSERT INTO embeddings VALUES(2,'corrupt',x'00')", [])
            .unwrap();
        function(&db, "file", "valid code", "corrupt description", Some(2));
        let original = snapshot(&db, LEGACY_TABLES);
        let before = snapshot(&db, RUST_TABLES);
        assert!(
            db.import_legacy(&profile)
                .unwrap_err()
                .to_string()
                .contains("Invalid stored vector length")
        );
        assert_eq!(snapshot(&db, RUST_TABLES), before);
        assert_eq!(snapshot(&db, LEGACY_TABLES), original);
        db.conn
            .execute("UPDATE functions SET description_embedding_id=1", [])
            .unwrap();
        db.set_meta("descriptions_enabled", "false").unwrap();
        db.set_meta("description_profile", "explicit rust profile")
            .unwrap();
        db.import_legacy(&profile).unwrap();
        assert_eq!(
            db.meta("descriptions_enabled").unwrap().as_deref(),
            Some("false")
        );
        assert_eq!(
            db.meta("description_profile").unwrap().as_deref(),
            Some("explicit rust profile")
        );
        assert!(cache(&db, "legacy-function", &function_key("file"))["embeddingKey"].is_string());
    }
}
