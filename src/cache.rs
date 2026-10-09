//! Authoritative per-user artifact storage and optional S3 sharing.

use std::{
    collections::{BTreeMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, IF_NONE_MATCH};
use rusqlite::{Connection, OptionalExtension, params};
use s3::{Bucket, creds::Credentials, region::Region};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{hash, storage::Database, ui};

const MAX_S3_OBJECT_BYTES: usize = 16 * 1024 * 1024;

const WORKSPACE_EMBEDDINGS: &str = "SELECT key,vector FROM global.embeddings WHERE key IN (
    SELECT u.embedding_key FROM unit_embeddings u JOIN search_units s ON s.id=u.unit_id WHERE u.role='code' AND u.input_hash=s.embedding_input_hash
    UNION SELECT embedding_key FROM descriptions WHERE embedding_key IS NOT NULL)";
const WORKSPACE_DESCRIPTIONS: &str = "SELECT kind,key,value FROM global.cache c WHERE kind='description' AND json_valid(value) AND EXISTS(
    SELECT 1 FROM files f WHERE f.path=json_extract(c.value,'$.generation.path') AND f.hash=json_extract(c.value,'$.generation.file_hash'))";

const GLOBAL_SCHEMA: &str = "
CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE sources(hash TEXT PRIMARY KEY,source TEXT NOT NULL);
CREATE TABLE embeddings(key TEXT PRIMARY KEY,vector BLOB NOT NULL);
CREATE TABLE cache(kind TEXT NOT NULL,key TEXT NOT NULL,value TEXT NOT NULL,PRIMARY KEY(kind,key));
CREATE TABLE description_content(hash TEXT PRIMARY KEY,content TEXT NOT NULL);
CREATE TABLE snapshots(digest TEXT PRIMARY KEY,manifest TEXT NOT NULL);
CREATE TABLE remote_uploads(remote TEXT NOT NULL,kind TEXT NOT NULL,key TEXT NOT NULL,digest TEXT NOT NULL,
 PRIMARY KEY(remote,kind,key));
PRAGMA user_version=1;
";

/// The answer is keyed by the reusable source/context, while the request that
/// produced it remains available for inspection and future invalidation policy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DescriptionArtifact {
    pub text: String,
    pub generation: DescriptionReferences,
}

#[derive(Clone, Debug)]
pub(crate) struct DescriptionGeneration {
    pub scope: String,
    pub path: String,
    pub symbol: Option<String>,
    pub source_hash: String,
    pub file_hash: String,
    pub file_description: Option<String>,
    pub profile: Value,
    pub settings: Value,
    pub system: String,
    pub prompt: String,
    pub messages: Vec<crate::models::Message>,
    pub regenerate: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DescriptionReferences {
    pub scope: String,
    pub path: String,
    pub symbol: Option<String>,
    pub source_hash: String,
    pub file_hash: String,
    pub file_description_hash: Option<String>,
    pub profile_hash: String,
    pub settings_hash: String,
    pub system_hash: String,
    pub prompt_hash: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<DescriptionMessageReference>,
    pub regenerate: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DescriptionMessageReference {
    pub role: String,
    pub content_hash: String,
}

impl DescriptionArtifact {
    pub(crate) fn new(
        text: String,
        generation: &DescriptionGeneration,
    ) -> Result<(Self, Vec<(String, String)>)> {
        let mut contents = BTreeMap::new();
        let mut intern = |value: &str| {
            let key = hash(value);
            contents
                .entry(key.clone())
                .or_insert_with(|| value.to_owned());
            key
        };
        let references = DescriptionReferences {
            scope: generation.scope.clone(),
            path: generation.path.clone(),
            symbol: generation.symbol.clone(),
            source_hash: generation.source_hash.clone(),
            file_hash: generation.file_hash.clone(),
            file_description_hash: generation.file_description.as_deref().map(&mut intern),
            profile_hash: intern(&serde_json::to_string(&generation.profile)?),
            settings_hash: intern(&serde_json::to_string(&generation.settings)?),
            system_hash: intern(&generation.system),
            prompt_hash: intern(&generation.prompt),
            messages: generation
                .messages
                .iter()
                .map(|message| DescriptionMessageReference {
                    role: message.role.clone(),
                    content_hash: intern(&message.content),
                })
                .collect(),
            regenerate: generation.regenerate,
        };
        Ok((
            Self {
                text,
                generation: references,
            },
            contents.into_iter().collect(),
        ))
    }

    pub(crate) fn decode(text: &str) -> Result<Self> {
        let artifact: Self = serde_json::from_str(text)?;
        anyhow::ensure!(
            !artifact.text.trim().is_empty()
                && matches!(artifact.generation.scope.as_str(), "file" | "callable")
                && artifact
                    .generation
                    .hashes()
                    .iter()
                    .all(|key| valid_hash(key)),
            "Invalid description artifact"
        );
        Ok(artifact)
    }
}

impl DescriptionReferences {
    pub(crate) fn hashes(&self) -> Vec<&str> {
        [
            self.file_description_hash.as_deref(),
            Some(&self.profile_hash),
            Some(&self.settings_hash),
            Some(&self.system_hash),
            Some(&self.prompt_hash),
        ]
        .into_iter()
        .flatten()
        .chain(
            self.messages
                .iter()
                .map(|message| message.content_hash.as_str()),
        )
        .collect()
    }
}

fn valid_hash(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn directory() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(dirs::cache_dir)
        .context("Cannot locate the user's cache directory")?;
    Ok(base.join("slopdex"))
}

/// Resolve the one authoritative artifact store. Relative configured paths are
/// made absolute before being persisted in a workspace binding.
pub fn store_path(config: &Value) -> Result<PathBuf> {
    let path = match config.get("artifactCachePath").filter(|v| !v.is_null()) {
        Some(value) => {
            let path = value
                .as_str()
                .filter(|s| !s.is_empty())
                .context("artifactCachePath must be a nonempty SQLite file path")?;
            PathBuf::from(path)
        }
        None => directory()?.join("global-v1.sqlite"),
    };
    let path = std::path::absolute(path)?;
    Ok(if path.exists() {
        std::fs::canonicalize(path)?
    } else {
        path
    })
}

pub(crate) fn open_store(path: &Path, readonly: bool) -> Result<Connection> {
    if !readonly {
        std::fs::create_dir_all(
            path.parent()
                .context("Global artifact store has no parent")?,
        )?;
    }
    let conn = if readonly {
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?
    } else {
        Connection::open(path)?
    };
    conn.busy_timeout(Duration::from_secs(30))?;
    let (version, populated): (i64, bool) = conn.query_row(
        "SELECT user_version,EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%') FROM pragma_user_version",
        [], |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    anyhow::ensure!(
        (!populated && version == 0 && !readonly) || (populated && version == 1),
        "Unsupported global artifact store at {}. Configure artifactCachePath with a new SQLite path; no migration or import is supported.",
        path.display()
    );
    if !readonly {
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        // Concurrent initializers recheck after acquiring the short schema lock.
        let tx =
            rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
        let current: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current == 0 {
            tx.execute_batch(GLOBAL_SCHEMA)?;
        }
        tx.commit()?;
    }
    conn.prepare("SELECT hash,source FROM sources LIMIT 0")?;
    conn.prepare("SELECT key,vector FROM embeddings LIMIT 0")?;
    conn.prepare("SELECT kind,key,value FROM cache LIMIT 0")?;
    conn.prepare("SELECT hash,content FROM description_content LIMIT 0")?;
    conn.prepare("SELECT digest,manifest FROM snapshots LIMIT 0")?;
    conn.prepare("SELECT remote,kind,key,digest FROM remote_uploads LIMIT 0")?;
    Ok(conn)
}

/// The shared cache is independent of any workspace's root, generation, or item IDs.
pub struct Artifacts {
    local: Connection,
    path: PathBuf,
    remote: Option<Remote>,
}

struct Remote {
    bucket: Box<Bucket>,
    prefix: String,
    identity: String,
    unavailable: AtomicBool,
    uploaded_content: Mutex<HashSet<String>>,
}

impl Artifacts {
    pub fn open(config: &Value) -> Result<Self> {
        Self::open_internal(config, false)
    }

    pub(crate) fn open_readonly(config: &Value) -> Result<Self> {
        Self::open_internal(config, true)
    }

    fn open_internal(config: &Value, readonly: bool) -> Result<Self> {
        let path = store_path(config)?;
        let local = open_store(&path, readonly)
            .with_context(|| format!("Cannot open global artifact store {}", path.display()))?;
        let path = std::fs::canonicalize(path)?;
        let remote = config.get("artifactS3").and_then(|settings| {
            Remote::open(settings)
                .map_err(|_| ui::warning("S3 artifact cache unavailable; continuing locally"))
                .ok()
        });
        Ok(Self {
            local,
            path,
            remote,
        })
    }

    fn check_store(&self, db: &Database) -> Result<()> {
        anyhow::ensure!(
            self.path == db.global_path(),
            "Artifact store mismatch: workspace uses {}, Artifacts uses {}. Open Artifacts with artifactCachePath set to Database::global_path().",
            db.global_path().display(),
            self.path.display()
        );
        Ok(())
    }

    /// Export only artifacts bound to this workspace when remote sharing is
    /// enabled. A repository's S3 settings never authorize exporting unrelated
    /// repositories' artifacts from the global store. No transaction spans HTTP.
    pub fn backfill_remote(&self, db: &Database) {
        let Some(remote) = &self.remote else { return };
        if remote.unavailable.load(Ordering::Relaxed) {
            return;
        }
        let export = (|| -> Result<()> {
            self.check_store(db)?;
            let mut embeddings = db.conn.prepare(WORKSPACE_EMBEDDINGS)?;
            let rows = embeddings
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(embeddings);
            for row in rows {
                let (key, bytes) = row;
                if valid_hash(&key) && crate::storage::decode(&bytes).is_ok() {
                    self.upload_once(remote, "embedding", &key, &bytes)?;
                }
            }
            let mut texts = db.conn.prepare(WORKSPACE_DESCRIPTIONS)?;
            let rows = texts
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(texts);
            for row in rows {
                let (kind, key, text) = row;
                if !valid_hash(&key) {
                    continue;
                }
                if kind == "description" {
                    let Ok(artifact) = DescriptionArtifact::decode(&text) else {
                        continue;
                    };
                    let mut contents = Vec::new();
                    for content_hash in artifact.generation.hashes() {
                        match db.description_content(content_hash)? {
                            Some(content) if hash(&content) == content_hash => {
                                contents.push((content_hash.to_owned(), content))
                            }
                            _ => {
                                contents.clear();
                                break;
                            }
                        }
                    }
                    if contents.len() == artifact.generation.hashes().len() {
                        for (content_hash, content) in contents {
                            self.upload_once(remote, "content", &content_hash, content.as_bytes())?;
                        }
                        self.upload_once(remote, "description", &key, text.as_bytes())?;
                    }
                } else if valid_hash(&key) && valid_text(&kind, &text) {
                    self.upload_once(remote, &kind, &key, text.as_bytes())?;
                }
            }
            Ok(())
        })();
        if export.is_err() {
            remote.disable();
        }
    }

    fn upload_once(&self, remote: &Remote, kind: &str, key: &str, bytes: &[u8]) -> Result<()> {
        anyhow::ensure!(valid_hash(key), "Invalid S3 artifact key");
        let digest = hash(bytes);
        let uploaded: Option<String> = self
            .local
            .query_row(
                "SELECT digest FROM remote_uploads WHERE remote=? AND kind=? AND key=?",
                params![remote.identity, kind, key],
                |r| r.get(0),
            )
            .optional()?;
        if uploaded.as_deref() == Some(&digest) {
            return Ok(());
        }
        if kind == "content" {
            remote.put_content(key, std::str::from_utf8(bytes)?)?;
        } else {
            remote.put(kind, key, bytes)?;
        }
        self.remember_remote(remote, kind, key, bytes)
    }

    fn remember_remote(&self, remote: &Remote, kind: &str, key: &str, bytes: &[u8]) -> Result<()> {
        self.local.execute("INSERT INTO remote_uploads VALUES(?,?,?,?) ON CONFLICT(remote,kind,key) DO UPDATE SET digest=excluded.digest",
            params![remote.identity, kind, key, hash(bytes)])?;
        Ok(())
    }

    fn fetch(&self, kind: &str, keys: &[String]) -> Vec<(String, Vec<u8>)> {
        let Some(remote) = &self.remote else {
            return Vec::new();
        };
        if remote.unavailable.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let mut found = Vec::new();
        for window in keys.chunks(10) {
            std::thread::scope(|scope| {
                let handles: Vec<_> = window
                    .iter()
                    .map(|key| scope.spawn(move || (key.clone(), remote.get(kind, key))))
                    .collect();
                for handle in handles {
                    let (key, result) = handle.join().expect("S3 lookup thread panicked");
                    match result {
                        Ok(Some(bytes)) => found.push((key, bytes)),
                        Ok(None) => {}
                        Err(_) => remote.disable(),
                    }
                }
            });
            if remote.unavailable.load(Ordering::Relaxed) {
                break;
            }
        }
        found
    }

    fn upload(&self, db: &Database, kind: &str, key: &str, bytes: &[u8]) {
        if let Some(remote) = &self.remote
            && (self.check_store(db).is_err()
                || remote.unavailable.load(Ordering::Relaxed)
                || self.upload_once(remote, kind, key, bytes).is_err())
        {
            remote.disable();
        }
    }

    /// Fetch all missing embeddings before issuing the provider batches. Local
    /// validation happens before the artifact becomes authoritative in SQLite.
    pub fn hydrate_embeddings(&self, db: &Database, keys: &[(String, usize)]) -> Result<()> {
        self.check_store(db)?;
        let mut missing = Vec::new();
        let mut seen = HashSet::new();
        for (key, dimensions) in keys {
            if !seen.insert(key.clone()) {
                continue;
            }
            if let Some(vector) = db.embedding(key)? {
                self.put_embedding(db, key, &vector);
                continue;
            }
            missing.push((key.clone(), *dimensions));
        }
        let wanted: Vec<_> = missing.iter().map(|(key, _)| key.clone()).collect();
        for (key, bytes) in self.fetch("embedding", &wanted) {
            if let Some((_, dimensions)) = missing.iter().find(|(candidate, _)| *candidate == key)
                && let Ok(vector) = crate::storage::decode(&bytes)
                && valid_vector(&vector, *dimensions)
            {
                db.put_embedding(&key, &vector)?;
                if let Some(remote) = &self.remote {
                    self.remember_remote(remote, "embedding", &key, &bytes)?;
                }
            }
        }
        Ok(())
    }

    pub fn put_embedding(&self, db: &Database, key: &str, vector: &[f32]) {
        let bytes: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.upload(db, "embedding", key, &bytes);
    }

    pub(crate) fn put_description(
        &self,
        db: &Database,
        key: &str,
        record: &str,
        contents: &[(String, String)],
    ) {
        if self.check_store(db).is_err() {
            return;
        }
        let Some(remote) = &self.remote else { return };
        if remote.unavailable.load(Ordering::Relaxed) {
            return;
        }
        let result = (|| -> Result<()> {
            for (content_hash, content) in contents {
                self.upload_once(remote, "content", content_hash, content.as_bytes())?;
            }
            self.upload_once(remote, "description", key, record.as_bytes())
        })();
        if result.is_err() || remote.unavailable.load(Ordering::Relaxed) {
            remote.disable();
        }
    }

    /// Resolve references for a batch of descriptions. A missing shared or S3
    /// fragment makes that answer a cache miss rather than an incomplete hit.
    pub(crate) fn hydrate_descriptions(&self, db: &Database, keys: &[String]) -> Result<()> {
        self.check_store(db)?;
        let mut candidates = Vec::new();
        let mut remote_keys = Vec::new();
        let mut seen = HashSet::new();
        for key in keys {
            if !seen.insert(key) {
                continue;
            }
            if let Some(record) = db.cache("description", key)? {
                if let Ok(artifact) = DescriptionArtifact::decode(&record) {
                    let mut complete = true;
                    for content_hash in artifact.generation.hashes() {
                        if db
                            .description_content(content_hash)?
                            .is_none_or(|text| hash(text) != content_hash)
                        {
                            complete = false;
                            break;
                        }
                    }
                    if complete {
                        continue;
                    }
                    candidates.push((key.clone(), record));
                    continue;
                }
                db.conn.execute(
                    "DELETE FROM global.cache WHERE kind='description' AND key=?",
                    [key],
                )?;
            }
            remote_keys.push(key.clone());
        }
        for (key, bytes) in self.fetch("description", &remote_keys) {
            if let Ok(record) = String::from_utf8(bytes)
                && DescriptionArtifact::decode(&record).is_ok()
            {
                candidates.push((key, record));
            }
        }
        let mut needed = HashSet::new();
        for (_, record) in &candidates {
            let artifact = DescriptionArtifact::decode(record)?;
            for content_hash in artifact.generation.hashes() {
                if db
                    .description_content(content_hash)?
                    .is_some_and(|text| hash(text) == content_hash)
                {
                    continue;
                }
                needed.insert(content_hash.to_owned());
            }
        }
        for (content_hash, bytes) in self.fetch("content", &needed.into_iter().collect::<Vec<_>>())
        {
            if let Ok(content) = String::from_utf8(bytes)
                && hash(&content) == content_hash
            {
                db.put_description_content(&content_hash, &content)?;
                if let Some(remote) = &self.remote {
                    self.remember_remote(remote, "content", &content_hash, content.as_bytes())?;
                }
            }
        }
        for (key, record) in candidates {
            let artifact = DescriptionArtifact::decode(&record)?;
            let mut complete = true;
            for content_hash in artifact.generation.hashes() {
                if db
                    .description_content(content_hash)?
                    .is_none_or(|text| hash(text) != content_hash)
                {
                    complete = false;
                    break;
                }
            }
            if complete {
                db.cache_put("description", &key, &record)?;
                if let Some(remote) = &self.remote {
                    self.remember_remote(remote, "description", &key, record.as_bytes())?;
                }
            } else {
                // An incomplete answer must never become an authoritative hit.
                db.conn.execute(
                    "DELETE FROM global.cache WHERE kind='description' AND key=?",
                    [&key],
                )?;
            }
        }
        Ok(())
    }

    pub fn hydrate_text(&self, db: &Database, kind: &str, keys: &[String]) -> Result<()> {
        self.check_store(db)?;
        let mut missing = Vec::new();
        let mut seen = HashSet::new();
        for key in keys {
            if !seen.insert(key) || db.cache(kind, key)?.is_some() {
                continue;
            }
            missing.push(key.clone());
        }
        for (key, bytes) in self.fetch(kind, &missing) {
            if let Ok(text) = String::from_utf8(bytes)
                && valid_text(kind, &text)
            {
                db.cache_put(kind, &key, &text)?;
                if let Some(remote) = &self.remote {
                    self.remember_remote(remote, kind, &key, text.as_bytes())?;
                }
            }
        }
        Ok(())
    }

    pub fn put_text(&self, db: &Database, kind: &str, key: &str, text: &str) {
        self.upload(db, kind, key, text.as_bytes());
    }
}

fn valid_vector(vector: &[f32], dimensions: usize) -> bool {
    vector.len() == dimensions && vector.iter().any(|value| *value != 0.0)
}

fn valid_text(kind: &str, text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    if kind == "description" {
        return DescriptionArtifact::decode(text).is_ok();
    }
    if kind != "rerank" {
        return true;
    }
    serde_json::from_str::<Vec<(usize, f64)>>(text)
        .is_ok_and(|rank| rank.iter().all(|(_, score)| score.is_finite()))
}

impl Remote {
    fn open(config: &Value) -> Result<Self> {
        let bucket_name = config["bucket"]
            .as_str()
            .context("artifactS3.bucket is required")?;
        let region_name = config["region"].as_str().unwrap_or("us-east-1");
        let region = if let Some(endpoint) = config["endpoint"].as_str() {
            Region::Custom {
                region: region_name.into(),
                endpoint: endpoint.into(),
            }
        } else {
            region_name.parse()?
        };
        let credentials = Credentials::default()?;
        let mut bucket = Bucket::new(bucket_name, region, credentials)?;
        if config["pathStyle"]
            .as_bool()
            .unwrap_or(config["endpoint"].is_string())
        {
            bucket = bucket.with_path_style();
        }
        bucket = bucket.with_request_timeout(Duration::from_secs(3))?;
        let prefix = config["prefix"]
            .as_str()
            .unwrap_or("slopdex")
            .trim_matches('/');
        anyhow::ensure!(
            !prefix.split('/').any(|part| part == ".." || part == "."),
            "Invalid artifactS3.prefix"
        );
        let identity = hash(
            serde_json::json!([bucket_name, region_name, config["endpoint"], prefix]).to_string(),
        );
        Ok(Self {
            bucket,
            prefix: prefix.into(),
            identity,
            unavailable: AtomicBool::new(false),
            uploaded_content: Mutex::new(HashSet::new()),
        })
    }

    fn path(&self, kind: &str, key: &str) -> String {
        format!("/{}/v3/{kind}/{}/{key}", self.prefix, &key[..2])
    }

    fn get(&self, kind: &str, key: &str) -> Result<Option<Vec<u8>>> {
        anyhow::ensure!(valid_hash(key), "Invalid S3 artifact key");
        if self.unavailable.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let data = self.bucket.get_object(self.path(kind, key))?;
        if data.status_code() == 404 {
            return Ok(None);
        }
        anyhow::ensure!(data.status_code() == 200, "S3 cache GET failed");
        anyhow::ensure!(
            data.as_slice().len() <= MAX_S3_OBJECT_BYTES,
            "S3 artifact too large"
        );
        Ok(decode_object(data.as_slice()))
    }

    fn put(&self, kind: &str, key: &str, bytes: &[u8]) -> Result<()> {
        let compressed = encode_object(bytes)?;
        let result = self.bucket.put_object(self.path(kind, key), &compressed)?;
        anyhow::ensure!(
            (200..300).contains(&result.status_code()),
            "S3 cache PUT failed"
        );
        Ok(())
    }

    fn put_content(&self, key: &str, content: &str) -> Result<()> {
        anyhow::ensure!(hash(content) == key, "Description content hash mismatch");
        let mut uploaded = self
            .uploaded_content
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if uploaded.contains(key) {
            return Ok(());
        }
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, HeaderValue::from_static("*"));
        let compressed = encode_object(content.as_bytes())?;
        let result = self.bucket.put_object_with_headers(
            self.path("content", key),
            &compressed,
            Some(headers),
        )?;
        anyhow::ensure!(
            (200..300).contains(&result.status_code()) || result.status_code() == 412,
            "S3 content PUT failed"
        );
        uploaded.insert(key.to_owned());
        Ok(())
    }

    fn disable(&self) {
        if !self.unavailable.swap(true, Ordering::Relaxed) {
            ui::warning("S3 artifact cache unavailable; continuing locally");
        }
    }
}

/// All S3 objects are independent zstd frames containing a checksum line and
/// payload. The checksum continues to identify the uncompressed value.
fn encode_object(payload: &[u8]) -> Result<Vec<u8>> {
    anyhow::ensure!(
        payload.len() <= MAX_S3_OBJECT_BYTES,
        "S3 artifact too large"
    );
    let mut framed = format!("{}\n", hash(payload)).into_bytes();
    framed.extend_from_slice(payload);
    Ok(zstd::stream::encode_all(framed.as_slice(), 3)?)
}

fn decode_object(compressed: &[u8]) -> Option<Vec<u8>> {
    let decoder = zstd::stream::read::Decoder::new(compressed).ok()?;
    let mut framed = Vec::new();
    decoder
        .take((MAX_S3_OBJECT_BYTES + 66) as u64)
        .read_to_end(&mut framed)
        .ok()?;
    if framed.len() > MAX_S3_OBJECT_BYTES + 65 {
        return None;
    }
    let (header, payload) = framed.split_first_chunk::<65>()?;
    (header[64] == b'\n' && header[..64] == hash(payload).as_bytes()[..]).then(|| payload.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Result<(tempfile::TempDir, Value, Database, Artifacts)> {
        let dir = tempfile::tempdir()?;
        let config = serde_json::json!({"artifactCachePath":dir.path().join("global.sqlite")});
        let db = Database::open_with_config(
            &dir.path().join("index.sqlite"),
            dir.path(),
            &config,
            false,
        )?;
        let artifacts = Artifacts::open(&config)?;
        Ok((dir, config, db, artifacts))
    }

    #[test]
    fn store_path_defaults_to_global_v1_and_resolves_configured_paths() -> Result<()> {
        assert_eq!(
            store_path(&serde_json::json!({}))?,
            std::path::absolute(directory()?.join("global-v1.sqlite"))?
        );
        assert_eq!(
            store_path(&serde_json::json!({"artifactCachePath":"custom-global.sqlite"}))?,
            std::path::absolute("custom-global.sqlite")?
        );
        Ok(())
    }

    fn description_record() -> Result<(DescriptionArtifact, Vec<(String, String)>)> {
        description_record_for(
            "code.rs",
            "fn run() {}",
            serde_json::json!({"model":"test"}),
        )
    }

    fn description_record_for(
        path: &str,
        source: &str,
        profile: Value,
    ) -> Result<(DescriptionArtifact, Vec<(String, String)>)> {
        DescriptionArtifact::new(
            "Summarizes the function".into(),
            &DescriptionGeneration {
                scope: "callable".into(),
                path: path.into(),
                symbol: Some("run".into()),
                source_hash: hash(source),
                file_hash: hash(source),
                file_description: Some("Example file".into()),
                profile,
                settings: serde_json::json!({}),
                system: "Summarize".into(),
                prompt: source.into(),
                messages: vec![crate::models::Message::user(source.into())],
                regenerate: false,
            },
        )
    }

    fn backfill_fixture() -> Result<(tempfile::TempDir, Database, Database, Artifacts)> {
        let (dir, config, mut db, artifacts) = fixture()?;
        let mut other = Database::open_with_config(
            &dir.path().join("other.sqlite"),
            &dir.path().join("other-root"),
            &config,
            false,
        )?;
        for (workspace, files) in [
            (&mut db, vec![("code.rs", "fn run() {}")]),
            (
                &mut other,
                vec![
                    ("code.rs", "// Private context\nfn run() {}"),
                    ("other.rs", "fn run() {}"),
                ],
            ),
        ] {
            let records = files
                .into_iter()
                .map(|(path, source)| {
                    Ok((
                        crate::storage::File {
                            path: path.into(),
                            hash: hash(source),
                            source: source.into(),
                            language: "rust".into(),
                            source_mode: "worktree".into(),
                            description: None,
                            description_hash: None,
                            description_embedding: None,
                            errors: Vec::new(),
                        },
                        crate::parse::parse(path, source)?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            workspace.apply_structure(&records, &[], None)?;
        }
        assert_eq!(db.global_path(), other.global_path());
        assert_eq!(
            db.conn
                .query_row("SELECT count(*) FROM global.sources", [], |r| r
                    .get::<_, i64>(0))?,
            2
        );
        Ok((dir, db, other, artifacts))
    }

    #[test]
    fn backfill_embeddings_selects_only_current_workspace_references() -> Result<()> {
        let (_dir, db, other, _artifacts) = backfill_fixture()?;
        db.set_projection_profile(&serde_json::json!({"model":"a"}))?;
        let keys: Vec<_> = [
            "code-a",
            "code-b",
            "file-description",
            "callable-description",
            "stale-code",
            "wrong-role",
            "other-code",
            "other-description",
            "unbound",
        ]
        .into_iter()
        .map(hash)
        .collect();
        for key in &keys {
            db.put_embedding(key, &[1.0, 0.0])?;
        }
        for (workspace, role, profile, input_hash, key) in [
            (&db, "code", "a", None, &keys[0]),
            (&db, "code", "b", None, &keys[1]),
            (&db, "code", "a", Some(hash("old input")), &keys[4]),
            (&db, "description", "a", None, &keys[5]),
            (&other, "code", "a", None, &keys[6]),
        ] {
            workspace.conn.execute(
                "INSERT INTO unit_embeddings(unit_id,role,profile_key,input_hash,embedding_key)
                 SELECT id,?,?,coalesce(?,embedding_input_hash),? FROM search_units WHERE path='code.rs'",
                params![role, profile, input_hash, key],
            )?;
        }
        for (workspace, scope, identity, key) in [
            (&db, "file", "", &keys[2]),
            (&db, "callable", "run", &keys[3]),
            (&other, "file", "", &keys[7]),
        ] {
            let text = format!("{scope} description");
            let content_hash = hash(&text);
            workspace.put_description_content(&content_hash, &text)?;
            workspace.conn.execute(
                "INSERT INTO descriptions(scope,path,identity,source_hash,content_hash,embedding_key)
                 VALUES(?,'code.rs',?,NULL,?,?)
                 ON CONFLICT(scope,path,identity) DO UPDATE SET content_hash=excluded.content_hash,embedding_key=excluded.embedding_key",
                params![scope, identity, content_hash, key],
            )?;
        }
        assert_eq!(
            db.conn
                .query_row("SELECT count(*) FROM global.embeddings", [], |r| r
                    .get::<_, i64>(0))?,
            9
        );
        for (workspace, expected) in [(&db, &keys[..4]), (&other, &keys[6..8])] {
            let selected = workspace
                .conn
                .prepare(WORKSPACE_EMBEDDINGS)?
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert_eq!(selected.len(), expected.len());
            assert_eq!(
                selected.iter().map(|(key, _)| key).collect::<HashSet<_>>(),
                expected.iter().collect::<HashSet<_>>()
            );
            for (_, bytes) in selected {
                assert_eq!(crate::storage::decode(&bytes)?, vec![1.0, 0.0]);
            }
        }
        Ok(())
    }

    #[test]
    fn backfill_descriptions_matches_generation_path_and_file_hash_across_profiles() -> Result<()> {
        let (_dir, db, other, _artifacts) = backfill_fixture()?;
        let mut keys = Vec::new();
        for (workspace, path, source, profile, scope) in [
            (&db, "code.rs", "fn run() {}", "a", "callable"),
            (&db, "code.rs", "fn run() {}", "b", "callable"),
            (&db, "code.rs", "fn run() {}", "b", "file"),
            (
                &other,
                "code.rs",
                "// Private context\nfn run() {}",
                "a",
                "callable",
            ),
            (&other, "other.rs", "fn run() {}", "a", "callable"),
        ] {
            let (mut record, contents) =
                description_record_for(path, source, serde_json::json!({"model":profile}))?;
            record.generation.scope = scope.into();
            record.generation.symbol = (scope == "callable").then(|| "run".into());
            // The callable body is shared; only its full generation context
            // authorizes export, not the source hash or the provider profile.
            record.generation.source_hash = hash("fn run() {}");
            let encoded = serde_json::to_string(&record)?;
            let key = hash(&encoded);
            workspace.put_description_artifact(&key, &encoded, &contents)?;
            keys.push(key);
        }
        let matching_record = db.cache("description", &keys[0])?.unwrap();
        db.cache_put("explanation", &hash("explanation"), &matching_record)?;
        db.cache_put("rerank", &hash("rerank"), "[[0,1.0]]")?;
        db.cache_put(
            "description",
            &hash("missing generation"),
            r#"{"text":"legacy"}"#,
        )?;
        db.cache_put("description", &hash("invalid JSON"), "invalid JSON")?;
        assert_eq!(
            db.conn.query_row(
                "SELECT count(*) FROM global.cache WHERE kind='description'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            7
        );
        assert_eq!(
            db.conn.query_row(
                "SELECT count(*) FROM global.cache WHERE kind IN ('explanation','rerank')",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            2
        );
        for (workspace, expected) in [(&db, &keys[..3]), (&other, &keys[3..])] {
            let selected = workspace
                .conn
                .prepare(WORKSPACE_DESCRIPTIONS)?
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert_eq!(selected.len(), expected.len());
            assert_eq!(
                selected
                    .iter()
                    .map(|(_, key, _)| key)
                    .collect::<HashSet<_>>(),
                expected.iter().collect::<HashSet<_>>()
            );
            for (kind, _, record) in selected {
                assert_eq!(kind, "description");
                let record = DescriptionArtifact::decode(&record)?;
                for content_hash in record.generation.hashes() {
                    let content = workspace.description_content(content_hash)?.unwrap();
                    assert_eq!(hash(content), content_hash);
                }
            }
        }
        Ok(())
    }

    #[test]
    fn artifacts_and_database_use_one_store_without_duplicate_paid_tables() -> Result<()> {
        let (dir, config, db, artifacts) = fixture()?;
        assert_eq!(artifacts.path, db.global_path());
        let key = hash("embedding");
        db.put_embedding(&key, &[1.0, 0.0])?;
        artifacts.put_embedding(&db, &key, &[1.0, 0.0]);
        artifacts.hydrate_embeddings(&db, &[(key.clone(), 2), (key.clone(), 2)])?;
        let text_key = hash("explanation");
        db.cache_put("explanation", &text_key, "answer")?;
        artifacts.put_text(&db, "explanation", &text_key, "answer");
        artifacts.hydrate_text(&db, "explanation", &[text_key.clone(), text_key.clone()])?;
        let (record, contents) = description_record()?;
        let description_key = hash("description");
        let encoded = serde_json::to_string(&record)?;
        db.put_description_artifact(&description_key, &encoded, &contents)?;
        artifacts.put_description(&db, &description_key, &encoded, &contents);
        artifacts.hydrate_descriptions(&db, std::slice::from_ref(&description_key))?;
        assert!(
            artifacts
                .local
                .prepare("SELECT value FROM artifacts")
                .is_err()
        );
        assert_eq!(
            artifacts
                .local
                .query_row("SELECT count(*) FROM embeddings", [], |r| r
                    .get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            artifacts
                .local
                .query_row("SELECT count(*) FROM cache", [], |r| r.get::<_, i64>(0))?,
            2
        );
        assert_eq!(
            artifacts
                .local
                .query_row("SELECT count(*) FROM description_content", [], |r| r
                    .get::<_, i64>(0))?,
            contents.len() as i64
        );
        drop(db);
        let db = Database::open_with_config(
            &dir.path().join("second.sqlite"),
            &dir.path().join("second-root"),
            &config,
            false,
        )?;
        assert_eq!(db.embedding(&key)?, Some(vec![1.0, 0.0]));
        assert_eq!(
            db.cache("explanation", &text_key)?.as_deref(),
            Some("answer")
        );
        assert_eq!(
            db.cache("description", &description_key)?.as_deref(),
            Some(encoded.as_str())
        );
        for (key, content) in contents {
            assert_eq!(
                db.description_content(&key)?.as_deref(),
                Some(content.as_str())
            );
        }
        Ok(())
    }

    #[test]
    fn incomplete_or_invalid_descriptions_are_global_misses() -> Result<()> {
        let (_dir, _config, db, artifacts) = fixture()?;
        let (record, contents) = description_record()?;
        let key = hash("description");
        db.put_description_artifact(&key, &serde_json::to_string(&record)?, &contents)?;
        db.conn.execute(
            "DELETE FROM global.description_content WHERE hash=?",
            [&contents[0].0],
        )?;
        artifacts.hydrate_descriptions(&db, std::slice::from_ref(&key))?;
        assert!(db.cache("description", &key)?.is_none());
        db.cache_put("description", &key, "invalid JSON")?;
        artifacts.hydrate_descriptions(&db, std::slice::from_ref(&key))?;
        assert!(db.cache("description", &key)?.is_none());
        assert!(
            db.put_description_content(&contents[0].0, "incorrect content")
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn failed_or_old_global_store_is_an_error_without_workspace_fallback() -> Result<()> {
        let dir = tempfile::tempdir()?;
        for config in [
            serde_json::json!({"artifactCachePath": ""}),
            serde_json::json!({"artifactCachePath":42}),
            serde_json::json!({"artifactCachePath":dir.path()}),
        ] {
            assert!(Artifacts::open(&config).is_err());
        }
        let path = dir.path().join("old.sqlite");
        let conn = Connection::open(&path)?;
        conn.execute_batch("CREATE TABLE artifacts(kind TEXT,key TEXT,value BLOB);")?;
        drop(conn);
        let before = std::fs::read(&path)?;
        let config = serde_json::json!({"artifactCachePath":path});
        assert!(
            Artifacts::open(&config)
                .err()
                .unwrap()
                .chain()
                .any(|e| e.to_string().contains("Unsupported global artifact store"))
        );
        assert_eq!(std::fs::read(path)?, before);
        Ok(())
    }

    #[test]
    fn hydrate_rejects_independently_configured_stores() -> Result<()> {
        let (dir, _config, db, _artifacts) = fixture()?;
        let other = Artifacts::open(
            &serde_json::json!({"artifactCachePath":dir.path().join("other.sqlite")}),
        )?;
        assert!(
            other
                .hydrate_embeddings(&db, &[])
                .unwrap_err()
                .to_string()
                .contains("Artifact store mismatch")
        );
        assert!(other.hydrate_text(&db, "explanation", &[]).is_err());
        assert!(other.hydrate_descriptions(&db, &[]).is_err());
        Ok(())
    }

    #[test]
    fn s3_values_are_zstd_frames_with_bounded_checked_payloads() -> Result<()> {
        let payload = "repeat-this-value".repeat(100);
        let compressed = encode_object(payload.as_bytes())?;
        assert_eq!(&compressed[..4], &[0x28, 0xb5, 0x2f, 0xfd]);
        assert!(compressed.len() < payload.len());
        assert_eq!(decode_object(&compressed), Some(payload.into_bytes()));
        assert_eq!(decode_object(b"uncompressed"), None);

        let invalid = zstd::stream::encode_all(b"not-a-valid-checksum\nvalue".as_slice(), 3)?;
        assert_eq!(decode_object(&invalid), None);
        let oversized =
            zstd::stream::encode_all(vec![b'x'; MAX_S3_OBJECT_BYTES + 66].as_slice(), 3)?;
        assert_eq!(decode_object(&oversized), None);
        Ok(())
    }
}
