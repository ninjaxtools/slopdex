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
    registry,
};

mod faq;
mod publish;
mod records;
mod snapshot;
mod units;

pub(crate) use faq::FaqEntry;
pub(crate) use units::{decode, parsed_items};

/// Bump when the structure or search-unit extraction contract changes.
pub const STRUCTURE_PARSER_VERSION: &str = "structure-v14-symbol-references";

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
    // Declared after the connection so it remains held until that connection closes.
    _store_usage: registry::ArtifactLock,
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
        Self::open_internal(path, root, config, force, false, true)
    }

    /// Open an initialized index without changing its schema or workspace binding.
    /// Engine callers hold a shared index lock and retry initialization exclusively.
    pub(crate) fn open_existing_with_config(
        path: &Path,
        root: &Path,
        config: &Value,
    ) -> Result<Self> {
        Self::open_internal(path, root, config, false, false, false)
    }

    pub fn open_readonly(path: &Path, root: &Path) -> Result<Self> {
        Self::open_readonly_with_config(path, root, &json!({}))
    }

    pub fn open_readonly_with_config(path: &Path, root: &Path, config: &Value) -> Result<Self> {
        Self::open_internal(path, root, config, false, true, false)
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
        initialize: bool,
    ) -> Result<Self> {
        let initial_file = match std::fs::metadata(path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let conn = if readonly {
            Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )?
        } else if initialize {
            Connection::open(path)?
        } else {
            if initial_file.is_none() {
                return Err(crate::engine::NeedsWrite.into());
            }
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?
        };
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        const HAS_TABLES: &str = "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%')";
        // WAL activation and first schema creation cannot rely on SQLite's busy
        // handler, so cold opens are serialized across processes. An empty file
        // is locked before its schema state is read; a nonempty file has at
        // least completed WAL activation and can be inspected without a lock.
        // Initialized indexes never take this lock.
        let mut initialization = None;
        if initialize && std::fs::metadata(path).map_or(true, |file| file.len() == 0) {
            initialization = Some(initialization_lock(path, config)?);
        }
        let mut has_tables: bool = conn.query_row(HAS_TABLES, [], |r| r.get(0))?;
        if !readonly && !has_tables && !initialize {
            return Err(crate::engine::NeedsWrite.into());
        }
        if initialize && !has_tables && initialization.is_none() {
            // A peer may be between WAL activation and its schema commit.
            initialization = Some(initialization_lock(path, config)?);
            has_tables = conn.query_row(HAS_TABLES, [], |r| r.get(0))?;
        }
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
        let store_usage = registry::store_usage(&global_path, false, registry::LOCK_TIMEOUT)?;
        if let Some(initial) = initial_file {
            // Direct Database users do not own an Engine index lock. Pruning
            // may have removed the file while this opener waited for the store
            // guard; never continue using SQLite's now-unlinked old connection.
            let current = std::fs::metadata(path)
                .context("Index changed while waiting for the artifact store; retry opening it")?;
            #[cfg(unix)]
            let same_file = {
                use std::os::unix::fs::MetadataExt;
                initial.dev() == current.dev() && initial.ino() == current.ino()
            };
            #[cfg(not(unix))]
            let same_file = initial.created().ok() == current.created().ok();
            ensure!(
                same_file && current.is_file(),
                "Index changed while waiting for the artifact store; retry opening it"
            );
        }
        if readonly {
            // URI mode makes the attached store genuinely read-only too.
            let uri = sqlite_readonly_uri(&global_path)?;
            conn.execute("ATTACH DATABASE ? AS global", [uri])?;
            conn.execute_batch("PRAGMA query_only=ON;")?;
            return Ok(Self {
                conn,
                path: path.to_owned(),
                global_path,
                _store_usage: store_usage,
            });
        }
        conn.execute(
            "ATTACH DATABASE ? AS global",
            [global_path
                .to_str()
                .context("Global artifact store path is not UTF-8")?],
        )?;
        let journal: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        if journal != "wal" {
            if !initialize {
                return Err(crate::engine::NeedsWrite.into());
            }
            conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        }
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        let global_text = global_path
            .to_str()
            .context("Global artifact store path is not UTF-8")?;
        // Reopening an initialized index is read-only setup: it must neither
        // wait for nor compete with a live writer of this worktree database.
        let current = has_tables
            && identity.as_ref() == Some(&expected)
            && binding.as_deref() == Some(global_text)
            && conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='incarnation')",
                [],
                |r| r.get::<_, bool>(0),
            )?;
        if !current {
            if !initialize {
                return Err(crate::engine::NeedsWrite.into());
            }
            let tx = rusqlite::Transaction::new_unchecked(
                &conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            tx.execute_batch(SCHEMA)?;
            tx.execute_batch("PRAGMA user_version=4;")?;
            if incompatible_root {
                reset_live(&tx)?;
            }
            tx.execute("INSERT INTO metadata VALUES('identity',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [expected.to_string()])?;
            tx.execute("INSERT INTO metadata VALUES('global_path',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [global_text])?;
            tx.execute(
                "INSERT OR IGNORE INTO metadata VALUES('incarnation',lower(hex(randomblob(16))))",
                [],
            )?;
            tx.commit()?;
        }
        drop(initialization);
        let db = Self {
            conn,
            path: path.to_owned(),
            global_path,
            _store_usage: store_usage,
        };
        registry::register_workspace(&db, root)?;
        Ok(db)
    }

    /// Not used by the engine; retained for the integration suites.
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
}

/// Serialize cold initialization of one worktree index across processes. The
/// lock lives with the store a new index would bind, keyed by the index file.
fn initialization_lock(path: &Path, config: &Value) -> Result<crate::registry::ArtifactLock> {
    let index = std::path::absolute(path)?;
    let global = cache::store_path(config)?;
    ensure!(
        index != global,
        "Workspace index and global artifact store must use different SQLite paths"
    );
    drop(
        cache::open_store(&global, false)
            .with_context(|| format!("Cannot open global artifact store {}", global.display()))?,
    );
    // Path aliases of one index must contend on the same lock.
    let index = match index.canonicalize() {
        Ok(index) => index,
        Err(_) => index,
    };
    crate::registry::lock_path(
        &std::fs::canonicalize(&global)?,
        "index-initialize",
        &hash(index.as_os_str().as_encoded_bytes()),
    )
}

fn reset_live(conn: &Connection) -> Result<()> {
    if faq::exists(conn)? {
        conn.execute("DELETE FROM faq_entries", [])?;
    }
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

#[cfg(test)]
mod tests;
