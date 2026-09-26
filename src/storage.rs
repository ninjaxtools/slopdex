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
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        tx.execute_batch(
            "DELETE FROM items; DELETE FROM files; DELETE FROM search_cache; DELETE FROM metadata;",
        )?;
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
        let generation = self
            .generation()?
            .checked_add(u64::from(dirty))
            .context("Index generation exhausted")?;
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
            data: json!({"qualifiedName": "example", "startLine": 1}),
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
        assert!(db.meta("identity")?.is_none());
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
        for value in ["{", "null", "{}"] {
            db.conn.execute("UPDATE files SET data=?", [value])?;
            assert!(db.files().is_err(), "{value}");
        }
        db.apply(&[record("code.rs")], &[], None)?;
        db.conn.execute("UPDATE items SET data='{'", [])?;
        assert!(db.items().is_err());
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
