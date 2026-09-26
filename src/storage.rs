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
        // Reject old layouts before creating tables with overlapping names.
        let old_schema: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND (name GLOB 'rust_*' OR name IN ('functions','markdown_chunks','description_cache')))",
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
        ensure!(
            !old_schema && !legacy_metadata,
            "Unsupported index table layout. Remove the existing SQLite index at {} and rebuild, or use --index with a new path.",
            path.display()
        );
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS embeddings(key TEXT PRIMARY KEY,vector BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS cache(kind TEXT NOT NULL,key TEXT NOT NULL,value TEXT NOT NULL,PRIMARY KEY(kind,key));
             CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY,data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS items(
               id INTEGER PRIMARY KEY AUTOINCREMENT,path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
               identity TEXT NOT NULL UNIQUE,kind TEXT NOT NULL,data TEXT NOT NULL,
               embedding TEXT NOT NULL REFERENCES embeddings(key),
               description_embedding TEXT REFERENCES embeddings(key));
             CREATE INDEX IF NOT EXISTS items_path ON items(path);
             CREATE TABLE IF NOT EXISTS search_cache(key TEXT PRIMARY KEY,value TEXT NOT NULL);")?;
        let db = Self {
            conn,
            path: path.to_owned(),
        };
        let expected = json!({"schema":2,"root":root,"embedding":profile});
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
        self.conn.execute_batch("BEGIN IMMEDIATE; DELETE FROM items; DELETE FROM files; DELETE FROM search_cache; DELETE FROM metadata; COMMIT;")?;
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
        let mut stmt = self.conn.prepare("SELECT data FROM files ORDER BY path")?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .map(|r| Ok(serde_json::from_str(&r?)?))
            .collect()
    }

    pub fn items(&self) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare("SELECT id,path,identity,kind,data,embedding,description_embedding FROM items ORDER BY id")?;
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
            tx.execute("DELETE FROM files WHERE path=?", [path])?;
        }
        for (file, items) in changed {
            tx.execute(
                "INSERT INTO files VALUES(?,?) ON CONFLICT(path) DO UPDATE SET data=excluded.data",
                params![file.path, serde_json::to_string(file)?],
            )?;
            // Reconcile by identity to preserve IDs across edits and line shifts.
            let identities: std::collections::HashSet<_> =
                items.iter().map(|i| i.identity.as_str()).collect();
            let existing: Vec<(i64, String)> = tx
                .prepare("SELECT id,identity FROM items WHERE path=?")?
                .query_map([&file.path], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, identity) in existing {
                if !identities.contains(identity.as_str()) {
                    tx.execute("DELETE FROM items WHERE id=?", [id])?;
                }
            }
            for item in items {
                tx.execute("INSERT INTO items(path,identity,kind,data,embedding,description_embedding) VALUES(?,?,?,?,?,?) ON CONFLICT(identity) DO UPDATE SET path=excluded.path,kind=excluded.kind,data=excluded.data,embedding=excluded.embedding,description_embedding=excluded.description_embedding",
                    params![item.path,item.identity,item.kind,item.data.to_string(),item.embedding,item.description_embedding])?;
            }
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
                "embeddings",
                "files",
                "items",
                "metadata",
                "search_cache"
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
        ] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("index.sqlite");
            let conn = Connection::open(&path)?;
            conn.execute_batch(schema)?;
            let before: Vec<Option<String>> = conn
                .prepare("SELECT sql FROM sqlite_master ORDER BY name")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
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
            }
        }
        Ok(())
    }
}
