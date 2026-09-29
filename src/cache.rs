//! Per-user paths and reusable provider artifacts. Workspace snapshots stay in their own index.

use std::{
    collections::{BTreeMap, HashSet},
    io::Read,
    path::PathBuf,
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

const S3_SEED_KEY: &str = "artifact_s3_seed_v3";
const MAX_S3_OBJECT_BYTES: usize = 16 * 1024 * 1024;

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
    pub regenerate: bool,
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

/// The shared cache is independent of any workspace's root, generation, or item IDs.
pub struct Artifacts {
    local: Option<Connection>,
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
    pub fn open(config: &Value) -> Self {
        let path = config["artifactCachePath"]
            .as_str()
            .map(PathBuf::from)
            .or_else(|| directory().ok().map(|dir| dir.join("artifacts-v2.sqlite")));
        let local = path.and_then(|path| (|| -> Result<Connection> {
            std::fs::create_dir_all(path.parent().context("Artifact cache has no parent")?)?;
            let conn = Connection::open(path)?;
            conn.busy_timeout(Duration::from_secs(30))?;
            conn.execute_batch("PRAGMA journal_mode=WAL;
                CREATE TABLE IF NOT EXISTS artifacts(kind TEXT NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL,
                  PRIMARY KEY(kind,key));
                CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
                INSERT OR IGNORE INTO metadata VALUES('identity',lower(hex(randomblob(16))));")?;
            Ok(conn)
        })().map_err(|_| ui::warning("Shared local cache unavailable; using workspace index")).ok());
        let remote = config.get("artifactS3").and_then(|settings| {
            Remote::open(settings)
                .map_err(|_| ui::warning("S3 artifact cache unavailable; continuing locally"))
                .ok()
        });
        Self { local, remote }
    }

    /// Populate the per-user cache from an existing workspace index once. This
    /// also covers the first open after copying an old default index to XDG.
    /// Legacy description keys have changed; retaining those entries is harmless
    /// because only matching current keys can be reused by a new workspace.
    pub fn import_workspace(&self, db: &Database) -> Result<()> {
        let Some(local) = &self.local else {
            return Ok(());
        };
        let identity: String = local.query_row(
            "SELECT value FROM metadata WHERE key='identity'",
            [],
            |row| row.get(0),
        )?;
        if db.meta("artifact_cache_seed_v2")?.as_deref() == Some(identity.as_str()) {
            return Ok(());
        }
        let mut embeddings = db.conn.prepare("SELECT key,vector FROM embeddings")?;
        let rows = embeddings.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (key, bytes) = row?;
            if key.len() == 64
                && key.bytes().all(|b| b.is_ascii_hexdigit())
                && crate::storage::decode(&bytes).is_ok()
            {
                local.execute(
                    "INSERT OR IGNORE INTO artifacts VALUES('embedding',?,?)",
                    params![key, bytes],
                )?;
            }
        }
        let mut texts = db.conn.prepare(
            "SELECT kind,key,value FROM cache WHERE kind IN ('description','rerank','explanation')",
        )?;
        let rows = texts.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (kind, key, text) = row?;
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
                    save_description(local, &key, &text, &contents)?;
                }
            } else if valid_hash(&key) && valid_text(&kind, &text) {
                local.execute(
                    "INSERT OR IGNORE INTO artifacts VALUES(?,?,?)",
                    params![kind, key, text],
                )?;
            }
        }
        db.set_meta("artifact_cache_seed_v2", &identity)?;
        Ok(())
    }

    /// Publish paid artifacts from a pre-existing workspace when remote sharing
    /// is enabled later. An interrupted export remains retryable next refresh.
    /// Never make the refresh depend on S3 availability.
    pub fn backfill_remote(&self, db: &Database) {
        let Some(remote) = &self.remote else { return };
        if remote.unavailable.load(Ordering::Relaxed)
            || db.meta(S3_SEED_KEY).ok().flatten().as_deref() == Some(&remote.identity)
        {
            return;
        }
        let export = (|| -> Result<()> {
            let mut embeddings = db.conn.prepare("SELECT key,vector FROM embeddings")?;
            let rows = embeddings.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            for row in rows {
                let (key, bytes) = row?;
                if key.len() == 64
                    && key.bytes().all(|b| b.is_ascii_hexdigit())
                    && crate::storage::decode(&bytes).is_ok()
                {
                    remote.put("embedding", &key, &bytes)?;
                }
            }
            let mut texts = db.conn.prepare("SELECT kind,key,value FROM cache WHERE kind IN ('description','rerank','explanation')")?;
            let rows = texts.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (kind, key, text) = row?;
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
                            remote.put_content(&content_hash, &content)?;
                        }
                        remote.put("description", &key, text.as_bytes())?;
                    }
                } else if valid_hash(&key) && valid_text(&kind, &text) {
                    remote.put(&kind, &key, text.as_bytes())?;
                }
            }
            db.set_meta(S3_SEED_KEY, &remote.identity)?;
            Ok(())
        })();
        if export.is_err() {
            remote.disable();
        }
    }

    fn local(&self, kind: &str, key: &str) -> Result<Option<Vec<u8>>> {
        let Some(conn) = &self.local else {
            return Ok(None);
        };
        Ok(conn
            .query_row(
                "SELECT value FROM artifacts WHERE kind=? AND key=?",
                params![kind, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn save_local(&self, kind: &str, key: &str, bytes: &[u8]) {
        if let Some(conn) = &self.local
            && conn
                .execute(
                    "INSERT OR IGNORE INTO artifacts VALUES(?,?,?)",
                    params![kind, key, bytes],
                )
                .is_err()
        {
            ui::warning("Could not write shared local cache");
        }
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
            && (remote.unavailable.load(Ordering::Relaxed) || remote.put(kind, key, bytes).is_err())
        {
            let _ = db.set_meta(S3_SEED_KEY, "");
            remote.disable();
        }
    }

    /// Fetch all missing embeddings before issuing the provider batches. Local
    /// validation happens before the artifact becomes authoritative in SQLite.
    pub fn hydrate_embeddings(&self, db: &Database, keys: &[(String, usize)]) -> Result<()> {
        let mut missing = Vec::new();
        let mut seen = HashSet::new();
        for (key, dimensions) in keys {
            if !seen.insert(key.clone()) || db.embedding(key)?.is_some() {
                continue;
            }
            if let Some(bytes) = self.local("embedding", key)?
                && let Ok(vector) = crate::storage::decode(&bytes)
                && valid_vector(&vector, *dimensions)
            {
                db.put_embedding(key, &vector)?;
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
                self.save_local("embedding", &key, &bytes);
            }
        }
        Ok(())
    }

    pub fn put_embedding(&self, db: &Database, key: &str, vector: &[f32]) {
        let bytes: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.save_local("embedding", key, &bytes);
        self.upload(db, "embedding", key, &bytes);
    }

    pub(crate) fn put_description(
        &self,
        db: &Database,
        key: &str,
        record: &str,
        contents: &[(String, String)],
    ) {
        if let Some(local) = &self.local
            && save_description(local, key, record, contents).is_err()
        {
            ui::warning("Could not write shared local description cache");
        }
        let Some(remote) = &self.remote else { return };
        if remote.unavailable.load(Ordering::Relaxed) {
            let _ = db.set_meta(S3_SEED_KEY, "");
            return;
        }
        let result = (|| -> Result<()> {
            for (content_hash, content) in contents {
                remote.put_content(content_hash, content)?;
            }
            remote.put("description", key, record.as_bytes())
        })();
        if result.is_err() || remote.unavailable.load(Ordering::Relaxed) {
            let _ = db.set_meta(S3_SEED_KEY, "");
            remote.disable();
        }
    }

    /// Resolve references for a batch of descriptions. A missing shared or S3
    /// fragment makes that answer a cache miss rather than an incomplete hit.
    pub(crate) fn hydrate_descriptions(&self, db: &Database, keys: &[String]) -> Result<()> {
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
                }
                db.conn.execute(
                    "DELETE FROM cache WHERE kind='description' AND key=?",
                    [key],
                )?;
            }
            if let Some(bytes) = self.local("description", key)?
                && let Ok(record) = String::from_utf8(bytes)
                && DescriptionArtifact::decode(&record).is_ok()
            {
                candidates.push((key.clone(), record));
            } else {
                remote_keys.push(key.clone());
            }
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
                if let Some(bytes) = self.local("content", content_hash)?
                    && let Ok(content) = String::from_utf8(bytes)
                    && hash(&content) == content_hash
                {
                    db.put_description_content(content_hash, &content)?;
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
                self.save_local_content(&content_hash, &content);
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
                for content_hash in artifact.generation.hashes() {
                    if let Some(content) = db.description_content(content_hash)? {
                        self.save_local_content(content_hash, &content);
                    }
                }
                self.save_local("description", &key, record.as_bytes());
            }
        }
        Ok(())
    }

    fn save_local_content(&self, key: &str, value: &str) {
        if let Some(local) = &self.local
            && local.execute("INSERT INTO artifacts VALUES('content',?,?) ON CONFLICT(kind,key) DO UPDATE SET value=excluded.value WHERE value<>excluded.value", params![key,value.as_bytes()]).is_err() {
                ui::warning("Could not write shared local description content");
            }
    }

    pub fn hydrate_text(&self, db: &Database, kind: &str, keys: &[String]) -> Result<()> {
        let mut missing = Vec::new();
        let mut seen = HashSet::new();
        for key in keys {
            if !seen.insert(key) || db.cache(kind, key)?.is_some() {
                continue;
            }
            if let Some(bytes) = self.local(kind, key)?
                && let Ok(text) = String::from_utf8(bytes)
                && valid_text(kind, &text)
            {
                db.cache_put(kind, key, &text)?;
                continue;
            }
            missing.push(key.clone());
        }
        for (key, bytes) in self.fetch(kind, &missing) {
            if let Ok(text) = String::from_utf8(bytes)
                && valid_text(kind, &text)
            {
                db.cache_put(kind, &key, &text)?;
                self.save_local(kind, &key, text.as_bytes());
            }
        }
        Ok(())
    }

    pub fn put_text(&self, db: &Database, kind: &str, key: &str, text: &str) {
        self.save_local(kind, key, text.as_bytes());
        self.upload(db, kind, key, text.as_bytes());
    }
}

fn valid_vector(vector: &[f32], dimensions: usize) -> bool {
    vector.len() == dimensions && vector.iter().any(|value| *value != 0.0)
}

fn save_description(
    conn: &Connection,
    key: &str,
    record: &str,
    contents: &[(String, String)],
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for (content_hash, content) in contents {
        anyhow::ensure!(
            hash(content) == *content_hash,
            "Description content hash mismatch"
        );
        tx.execute("INSERT INTO artifacts VALUES('content',?,?) ON CONFLICT(kind,key) DO UPDATE SET value=excluded.value", params![content_hash,content.as_bytes()])?;
    }
    tx.execute("INSERT INTO artifacts VALUES('description',?,?) ON CONFLICT(kind,key) DO UPDATE SET value=excluded.value", params![key,record.as_bytes()])?;
    tx.commit()?;
    Ok(())
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
