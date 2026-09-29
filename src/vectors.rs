//! Persistent cosine HNSW cache of caller-owned embeddings.
//!
//! `path` names the USearch binary; its manifest is `<path>.manifest.json`.
//! The caller must hold an exclusive interprocess index lock for `open`, or a
//! shared lock for `open_readonly` (which never publishes sidecars).
//! The supplied vectors are authoritative: sidecars can always be regenerated.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

const VERSION: u32 = 1;
const CONNECTIVITY: usize = 16;
const EXPANSION_ADD: usize = 128;
const EXPANSION_SEARCH: usize = 64;

/// An owned, mutable-on-open F32 HNSW index. Candidates come from graph traversal.
pub struct VectorIndex {
    index: Index,
    dimensions: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    usearch_version: String,
    generation: u64,
    dimensions: usize,
    /// Hashes of original F32 bits, ordered by caller key (input order is irrelevant).
    vectors: BTreeMap<u64, String>,
    fingerprint: String,
    binary_hash: String,
}

impl Manifest {
    fn new(dimensions: usize, generation: u64, vectors: &[(u64, Vec<f32>)]) -> Result<Self> {
        ensure!(dimensions > 0, "vector dimensions must be positive");
        let mut hashes = BTreeMap::new();
        let progress = crate::ui::counted("Validating and hashing snapshot vectors", vectors.len());
        for (key, vector) in vectors {
            // USearch's native tombstone cannot be a searchable/persistable key.
            ensure!(
                *key != u64::MAX,
                "vector key u64::MAX is reserved by USearch"
            );
            validate(vector, dimensions)
                .with_context(|| format!("invalid vector for key {key}"))?;
            ensure!(
                hashes.insert(*key, vector_hash(vector)).is_none(),
                "duplicate vector key {key}"
            );
            progress.inc(1);
        }
        let mut manifest = Self {
            version: VERSION,
            usearch_version: usearch::version().to_owned(),
            generation,
            dimensions,
            vectors: hashes,
            fingerprint: String::new(),
            binary_hash: String::new(),
        };
        manifest.fingerprint = manifest.compute_fingerprint()?;
        progress.finish();
        Ok(manifest)
    }

    fn compute_fingerprint(&self) -> Result<String> {
        Ok(crate::hash(serde_json::to_vec(&(
            self.version,
            &self.usearch_version,
            self.generation,
            self.dimensions,
            &self.vectors,
        ))?))
    }
}

impl VectorIndex {
    /// Open or reconcile a persistent index with a complete authoritative snapshot.
    ///
    /// Missing, corrupt or incompatible caches are rebuilt without model calls.
    /// Valid caches are updated by removing deleted/changed keys and adding only
    /// new/replacement vectors. An unchanged snapshot performs no writes.
    /// Keys must be unique and cannot equal USearch's reserved `u64::MAX`.
    pub fn open(
        path: &Path,
        dimensions: usize,
        generation: u64,
        vectors: &[(u64, Vec<f32>)],
    ) -> Result<Self> {
        // Validate the entire snapshot before touching either cache file.
        let mut manifest = Manifest::new(dimensions, generation, vectors)?;
        let index = match load_cached(path, dimensions) {
            Ok((index, old)) => {
                if old.fingerprint == manifest.fingerprint {
                    return Ok(Self { index, dimensions });
                }
                if old.vectors == manifest.vectors {
                    // A new generation with identical data only needs a new manifest.
                    manifest.binary_hash = old.binary_hash;
                    atomic_write(&manifest_path(path), &serde_json::to_vec(&manifest)?)?;
                    return Ok(Self { index, dimensions });
                }
                if reconcile(&index, &old, &manifest, vectors).is_ok() {
                    index
                } else {
                    // Never publish a partially applied native-index mutation.
                    build(dimensions, vectors)?
                }
            }
            Err(_) => build(dimensions, vectors)?,
        };
        persist(path, &index, &mut manifest)
            .with_context(|| format!("persist vector index {}", path.display()))?;
        Ok(Self { index, dimensions })
    }

    /// Use the persisted index if it matches, otherwise rebuild in memory.
    /// Multiple readers can do this without publishing sidecar files.
    pub fn open_readonly(
        path: &Path,
        dimensions: usize,
        generation: u64,
        vectors: &[(u64, Vec<f32>)],
    ) -> Result<Self> {
        let manifest = Manifest::new(dimensions, generation, vectors)?;
        let index = match load_cached(path, dimensions) {
            Ok((index, old)) if old.fingerprint == manifest.fingerprint => index,
            _ => build(dimensions, vectors)?,
        };
        Ok(Self { index, dimensions })
    }

    /// Return up to `limit` allowed keys with cosine similarity.
    ///
    /// Filtering is applied inside USearch's graph traversal, not to an unfiltered
    /// top-k list. Candidate retrieval is approximate; returned candidates are
    /// scored in F64 and ordered by decreasing similarity.
    /// Queries must have the configured dimensions, finite values and nonzero norm,
    /// including when the index, limit, or allowed set is empty.
    pub fn search(
        &self,
        query: &[f32],
        limit: usize,
        allowed: &HashSet<u64>,
    ) -> Result<Vec<(u64, f64)>> {
        self.search_filtered(query, limit.min(allowed.len()), |key| {
            allowed.contains(&key)
        })
    }

    /// Predicate form avoids materializing a new eligible-key set per source
    /// during cross-search. The predicate is evaluated during graph traversal.
    pub fn search_filtered(
        &self,
        query: &[f32],
        limit: usize,
        allowed: impl Fn(u64) -> bool,
    ) -> Result<Vec<(u64, f64)>> {
        let query = normalized(query, self.dimensions).context("invalid vector query")?;
        let limit = limit.min(self.len());
        if limit == 0 {
            return Ok(Vec::new());
        }
        let matches = self
            .index
            .filtered_search(&query, limit, allowed)
            .context("search vector index")?;
        // Native SIMD cosine kernels use architecture-specific approximations,
        // which can give identical vectors a similarity just below 1. Recompute
        // only the returned candidates so strict threshold bounds are portable.
        let query_norm: f64 = query.iter().map(|&v| f64::from(v).powi(2)).sum();
        let mut stored = vec![0.0_f32; self.dimensions];
        let mut results = Vec::with_capacity(matches.keys.len());
        let progress = crate::ui::counted("Rescoring search candidates", matches.keys.len());
        for key in matches.keys {
            ensure!(
                self.index
                    .get(key, &mut stored)
                    .context("read candidate vector")?
                    == 1,
                "missing candidate vector for key {key}"
            );
            let mut dot = 0.0;
            let mut stored_norm = 0.0;
            for (&a, &b) in query.iter().zip(&stored) {
                dot += f64::from(a) * f64::from(b);
                stored_norm += f64::from(b).powi(2);
            }
            let similarity = (dot / (query_norm * stored_norm).sqrt()).clamp(-1.0, 1.0);
            results.push((key, similarity));
            progress.inc(1);
        }
        results.sort_by(|a, b| b.1.total_cmp(&a.1));
        progress.finish();
        Ok(results)
    }

    pub fn len(&self) -> usize {
        self.index.size()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn validate(vector: &[f32], dimensions: usize) -> Result<f64> {
    ensure!(
        vector.len() == dimensions,
        "expected {dimensions} dimensions, got {}",
        vector.len()
    );
    ensure!(
        vector.iter().all(|v| v.is_finite()),
        "non-finite vector component"
    );
    let squared_norm: f64 = vector.iter().map(|&v| f64::from(v).powi(2)).sum();
    ensure!(squared_norm > 0.0, "vector must have nonzero norm");
    Ok(squared_norm.sqrt())
}

fn normalized(vector: &[f32], dimensions: usize) -> Result<Vec<f32>> {
    // F64 accumulation also handles finite F32 inputs whose squared norm would
    // overflow/underflow F32. Unit vectors keep native cosine arithmetic stable.
    let norm = validate(vector, dimensions)?;
    Ok(vector
        .iter()
        .map(|&v| (f64::from(v) / norm) as f32)
        .collect())
}

fn vector_hash(vector: &[f32]) -> String {
    let bytes: Vec<u8> = vector
        .iter()
        .flat_map(|v| v.to_bits().to_le_bytes())
        .collect();
    crate::hash(bytes)
}

fn new_index(dimensions: usize) -> Result<Index> {
    Index::new(&IndexOptions {
        dimensions,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: CONNECTIVITY,
        expansion_add: EXPANSION_ADD,
        expansion_search: EXPANSION_SEARCH,
        multi: false,
    })
    .context("create vector index")
}

fn build(dimensions: usize, vectors: &[(u64, Vec<f32>)]) -> Result<Index> {
    let index = new_index(dimensions)?;
    index
        .reserve(vectors.len().max(1))
        .context("reserve vector index")?;
    let progress = crate::ui::counted("Building native vector index", vectors.len());
    for (key, vector) in vectors {
        index
            .add(*key, &normalized(vector, dimensions)?)
            .with_context(|| format!("add vector key {key}"))?;
        progress.inc(1);
    }
    progress.finish();
    Ok(index)
}

fn load_cached(path: &Path, dimensions: usize) -> Result<(Index, Manifest)> {
    let manifest: Manifest = serde_json::from_slice(&fs::read(manifest_path(path))?)?;
    ensure!(
        manifest.version == VERSION
            && manifest.usearch_version == usearch::version()
            && manifest.dimensions == dimensions,
        "incompatible vector manifest"
    );
    ensure!(
        manifest.fingerprint == manifest.compute_fingerprint()?,
        "invalid snapshot fingerprint"
    );
    let binary = fs::read(path)?;
    ensure!(
        crate::hash(&binary) == manifest.binary_hash,
        "vector binary hash mismatch"
    );

    let index = new_index(dimensions)?;
    // USearch's load_from_buffer copies through its bounds-checked input stream.
    // Hash and load exactly the same bytes; never use view/view_from_buffer (mmap).
    index
        .load_from_buffer(&binary)
        .context("load vector binary")?;
    ensure!(
        index.dimensions() == dimensions
            && index.metric_kind() == MetricKind::Cos
            && index.scalar_kind() == ScalarKind::F32
            && index.connectivity() == CONNECTIVITY
            && !index.multi()
            && index.size() == manifest.vectors.len(),
        "vector binary configuration/count mismatch"
    );
    ensure!(
        manifest
            .vectors
            .keys()
            .all(|&key| key != u64::MAX && index.contains(key)),
        "vector binary key mismatch"
    );
    // Expansion settings are runtime configuration, not a cache-format contract.
    index.change_expansion_add(EXPANSION_ADD);
    index.change_expansion_search(EXPANSION_SEARCH);
    Ok((index, manifest))
}

fn reconcile(
    index: &Index,
    old: &Manifest,
    new: &Manifest,
    vectors: &[(u64, Vec<f32>)],
) -> Result<()> {
    // Count both scans, including unchanged entries; replacements are visited
    // once for removal in the old snapshot and once for addition in the new one.
    let progress = crate::ui::counted(
        "Reconciling vector snapshot (entries scanned)",
        old.vectors.len() + vectors.len(),
    );
    for (key, hash) in &old.vectors {
        if new.vectors.get(key) != Some(hash) {
            ensure!(
                index.remove(*key)? == 1,
                "failed to remove vector key {key}"
            );
        }
        progress.inc(1);
    }
    // Deleted slots are reused by USearch. Keep existing capacity (including
    // tombstones), and grow before additions if the new live set exceeds it.
    index.reserve(index.capacity().max(vectors.len()).max(1))?;
    for (key, vector) in vectors {
        if old.vectors.get(key) != new.vectors.get(key) {
            index.add(*key, &normalized(vector, new.dimensions)?)?;
        }
        progress.inc(1);
    }
    ensure!(
        index.size() == vectors.len(),
        "vector update count mismatch"
    );
    progress.finish();
    Ok(())
}

fn persist(path: &Path, index: &Index, manifest: &mut Manifest) -> Result<()> {
    let mut binary = vec![0; index.serialized_length()];
    index
        .save_to_buffer(&mut binary)
        .context("serialize vector index")?;
    manifest.binary_hash = crate::hash(&binary);
    let json = serde_json::to_vec(manifest)?;
    // Commit binary first, manifest last. A crash between these independent
    // atomic renames leaves a hash mismatch and forces a rebuild on next open.
    atomic_write(path, &binary)?;
    atomic_write(&manifest_path(path), &json)?;
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn manifest_path(path: &Path) -> PathBuf {
    with_suffix(path, ".manifest.json")
}

struct TemporaryFile(PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let (temp, mut file) = loop {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let name = with_suffix(path, &format!(".tmp-{}-{id}", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&name) {
            Ok(file) => break (TemporaryFile(name), file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("create vector cache temporary file"),
        }
    };
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp.0, path).with_context(|| format!("replace {}", path.display()))?;
    // Persist the directory entry as well as file contents on Unix.
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::tempdir;

    fn sample() -> Vec<(u64, Vec<f32>)> {
        vec![
            (0, vec![1.0, 0.0]),
            (42, vec![0.0, 1.0]),
            (u64::MAX - 1, vec![-1.0, 0.0]),
        ]
    }

    fn keys(vectors: &[(u64, Vec<f32>)]) -> HashSet<u64> {
        vectors.iter().map(|(key, _)| *key).collect()
    }

    fn read_manifest(path: &Path) -> Manifest {
        serde_json::from_slice(&fs::read(manifest_path(path)).unwrap()).unwrap()
    }

    fn assert_snapshot(path: &Path, generation: u64, vectors: &[(u64, Vec<f32>)]) {
        let manifest = read_manifest(path);
        assert_eq!(manifest.generation, generation);
        assert_eq!(
            manifest.vectors,
            Manifest::new(2, generation, vectors).unwrap().vectors
        );
        assert_eq!(
            manifest.fingerprint,
            manifest.compute_fingerprint().unwrap()
        );
        assert_eq!(manifest.binary_hash, crate::hash(fs::read(path).unwrap()));
        let (index, _) = load_cached(path, 2).unwrap();
        assert_eq!(index.size(), vectors.len());
        for (key, vector) in vectors {
            let mut stored = vec![0.0_f32; 2];
            assert_eq!(index.get(*key, &mut stored).unwrap(), 1);
            assert_eq!(stored, normalized(vector, 2).unwrap());
        }
    }

    fn mark_old(path: &Path) -> std::time::SystemTime {
        let time = UNIX_EPOCH + Duration::from_secs(1_000_000);
        // Windows requires write access to update file timestamps.
        OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(time))
            .unwrap();
        fs::metadata(path).unwrap().modified().unwrap()
    }

    #[test]
    fn save_reload_unchanged_and_generation_only() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/vectors.usearch");
        let vectors = sample();
        let index = VectorIndex::open(&path, 2, 7, &vectors).unwrap();
        assert_eq!(index.len(), 3);
        assert!(!index.is_empty());
        assert_eq!(index.index.scalar_kind(), ScalarKind::F32);
        assert_eq!(index.index.connectivity(), 16);
        assert_eq!(index.index.expansion_add(), 128);
        assert_eq!(index.index.expansion_search(), 64);
        let expected = index.search(&[3.0, 0.0], 10, &keys(&vectors)).unwrap();
        assert_eq!(
            expected.iter().map(|r| r.0).collect::<Vec<_>>(),
            vec![0, 42, u64::MAX - 1]
        );
        for (row, score) in expected.iter().zip([1.0, 0.0, -1.0]) {
            assert!((row.1 - score).abs() < 1e-5);
        }
        drop(index);
        let binary_time = mark_old(&path);
        let manifest_time = mark_old(&manifest_path(&path));
        let mut reordered = vectors.clone();
        reordered.reverse();
        let reloaded = VectorIndex::open(&path, 2, 7, &reordered).unwrap();
        assert_eq!(
            reloaded.search(&[1.0, 0.0], 10, &keys(&vectors)).unwrap(),
            expected
        );
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            binary_time
        );
        assert_eq!(
            fs::metadata(manifest_path(&path))
                .unwrap()
                .modified()
                .unwrap(),
            manifest_time
        );
        drop(reloaded);
        VectorIndex::open(&path, 2, 8, &vectors).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            binary_time
        );
        assert_snapshot(&path, 8, &vectors);
    }

    #[test]
    fn incremental_replacement_growth_and_deletion_preserve_graph() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        let mut vectors = sample();
        VectorIndex::open(&path, 2, 1, &vectors).unwrap();
        vectors[0].1 = vec![0.5, 1.0];
        vectors.extend((100..200).map(|key| (key, vec![1.0, key as f32 / 100.0])));
        // Data fingerprint must detect changes even if the caller reused generation.
        let grown = VectorIndex::open(&path, 2, 1, &vectors).unwrap();
        assert_eq!(grown.len(), 103);
        assert!(grown.index.capacity() >= 103);
        assert_snapshot(&path, 1, &vectors);
        let nodes_before_delete = grown.index.stats().nodes;
        drop(grown);
        vectors.retain(|(key, _)| *key == 0 || *key == 42);
        let shrunk = VectorIndex::open(&path, 2, 2, &vectors).unwrap();
        assert_eq!(shrunk.len(), 2);
        // A full rebuild would have two nodes. Native removal keeps tombstones.
        assert_eq!(shrunk.index.stats().nodes, nodes_before_delete);
        let allowed: HashSet<_> = (0..200).chain([u64::MAX - 1]).collect();
        let results = shrunk.search(&[1.0, 0.0], 200, &allowed).unwrap();
        assert_eq!(
            results.iter().map(|r| r.0).collect::<HashSet<_>>(),
            keys(&vectors)
        );
        assert_snapshot(&path, 2, &vectors);
        drop(shrunk);
        vectors[0].1 = vec![-1.0, 0.0];
        let replaced = VectorIndex::open(&path, 2, 3, &vectors).unwrap();
        assert_eq!(replaced.index.stats().nodes, nodes_before_delete);
        assert_eq!(
            replaced
                .search(&[-1.0, 0.0], 1, &HashSet::from([0]))
                .unwrap()[0]
                .0,
            0
        );
        assert_snapshot(&path, 3, &vectors);
    }

    #[test]
    fn empty_index_delete_all_and_repopulate() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        for generation in [1, 1] {
            let empty = VectorIndex::open(&path, 2, generation, &[]).unwrap();
            assert!(empty.is_empty());
            assert!(
                empty
                    .search(&[1.0, 0.0], 10, &HashSet::from([0]))
                    .unwrap()
                    .is_empty()
            );
            assert_snapshot(&path, generation, &[]);
        }
        VectorIndex::open(&path, 2, 2, &sample()).unwrap();
        let empty = VectorIndex::open(&path, 2, 3, &[]).unwrap();
        assert_eq!(empty.len(), 0);
        assert_snapshot(&path, 3, &[]);
        drop(empty);
        assert!(VectorIndex::open(&path, 2, 3, &[]).unwrap().is_empty());
        let populated = VectorIndex::open(&path, 2, 4, &sample()).unwrap();
        assert_eq!(
            populated
                .search(&[1.0, 0.0], 1, &HashSet::from([0]))
                .unwrap()[0]
                .0,
            0
        );
        assert_snapshot(&path, 4, &sample());
    }

    #[test]
    fn missing_corrupt_and_incompatible_sidecars_rebuild() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        let vectors = sample();
        VectorIndex::open(&path, 2, 1, &vectors).unwrap();
        for damage in 0..8 {
            match damage {
                0 => fs::remove_file(&path).unwrap(),
                1 => fs::remove_file(manifest_path(&path)).unwrap(),
                2 => fs::write(&path, b"truncated binary").unwrap(),
                3 => fs::write(manifest_path(&path), b"{broken json").unwrap(),
                4 => {
                    let mut bytes = fs::read(&path).unwrap();
                    let last = bytes.len() - 1;
                    bytes[last] ^= 0xff;
                    fs::write(&path, bytes).unwrap();
                }
                5..=7 => {
                    let mut manifest = read_manifest(&path);
                    match damage {
                        5 => manifest.generation += 1, // Valid JSON, invalid fingerprint.
                        6 => manifest.version += 1,
                        _ => manifest.usearch_version = "incompatible".into(),
                    }
                    fs::write(manifest_path(&path), serde_json::to_vec(&manifest).unwrap())
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let index = VectorIndex::open(&path, 2, 1, &vectors).unwrap();
            assert_eq!(
                index.search(&[1.0, 0.0], 1, &keys(&vectors)).unwrap()[0].0,
                0
            );
            assert_snapshot(&path, 1, &vectors);
        }
        let changed_dimensions = vec![(42, vec![1.0, 0.0, 0.0])];
        let index = VectorIndex::open(&path, 3, 2, &changed_dimensions).unwrap();
        assert_eq!(
            index
                .search(&[1.0, 0.0, 0.0], 1, &HashSet::from([42]))
                .unwrap()[0]
                .0,
            42
        );
        assert_eq!(read_manifest(&path).dimensions, 3);
    }

    #[test]
    fn crash_between_binary_and_manifest_renames_rebuilds() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        let old_vectors = sample();
        VectorIndex::open(&path, 2, 1, &old_vectors).unwrap();
        let old_manifest = fs::read(manifest_path(&path)).unwrap();
        let old_binary = fs::read(&path).unwrap();
        let new_vectors = vec![(42, vec![1.0, 0.0])];
        VectorIndex::open(&path, 2, 2, &new_vectors).unwrap();
        let new_manifest = fs::read(manifest_path(&path)).unwrap();
        // New binary + old manifest: the crash window of our commit protocol.
        fs::write(manifest_path(&path), old_manifest).unwrap();
        fs::write(with_suffix(&path, ".tmp-abandoned"), b"unfinished").unwrap();
        assert!(load_cached(&path, 2).is_err());
        let recovered = VectorIndex::open(&path, 2, 2, &new_vectors).unwrap();
        assert_eq!(recovered.index.stats().nodes, 1); // Rebuilt, no old tombstones.
        assert_snapshot(&path, 2, &new_vectors);
        drop(recovered);
        // Also reject the inverse pairing, e.g. externally restored sidecars.
        fs::write(&path, old_binary).unwrap();
        fs::write(manifest_path(&path), new_manifest).unwrap();
        assert!(load_cached(&path, 2).is_err());
        VectorIndex::open(&path, 2, 2, &new_vectors).unwrap();
        assert_snapshot(&path, 2, &new_vectors);
    }

    #[test]
    fn restrictive_filter_finds_distant_allowed_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        let vectors: Vec<_> = (0..1000)
            .map(|key| {
                let angle = key as f32 * std::f32::consts::PI / 1000.0;
                (key, vec![angle.cos(), angle.sin()])
            })
            .collect();
        let allowed = HashSet::from([997, 998, 999, 9999]);
        for _ in 0..2 {
            let index = VectorIndex::open(&path, 2, 1, &vectors).unwrap();
            let unfiltered = index.index.search(&[1.0_f32, 0.0], 64).unwrap();
            assert!(unfiltered.keys.iter().all(|key| !allowed.contains(key)));
            let results = index.search(&[1.0, 0.0], 3, &allowed).unwrap();
            assert_eq!(
                results.iter().map(|r| r.0).collect::<HashSet<_>>(),
                HashSet::from([997, 998, 999])
            );
            assert!(results.windows(2).all(|pair| pair[0].1 >= pair[1].1));
            assert!(
                index
                    .search(&[1.0, 0.0], 3, &HashSet::from([9999]))
                    .unwrap()
                    .is_empty()
            );
            assert!(index.search(&[1.0, 0.0], 0, &allowed).unwrap().is_empty());
            assert!(
                index
                    .search(&[1.0, 0.0], 3, &HashSet::new())
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn invalid_inputs_do_not_modify_snapshot() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        let index = VectorIndex::open(&path, 2, 1, &sample()).unwrap();
        let binary = fs::read(&path).unwrap();
        let manifest = fs::read(manifest_path(&path)).unwrap();
        let invalid = [
            vec![],
            vec![1.0],
            vec![0.0, -0.0],
            vec![f32::NAN, 1.0],
            vec![1.0, f32::INFINITY],
        ];
        for vector in invalid {
            assert!(VectorIndex::open(&path, 2, 2, &[(5, vector.clone())]).is_err());
            assert!(index.search(&vector, 1, &HashSet::from([0])).is_err());
            assert!(index.search(&vector, 0, &HashSet::new()).is_err());
        }
        assert!(VectorIndex::open(&path, 0, 2, &[]).is_err());
        assert!(
            VectorIndex::open(&path, 2, 2, &[(1, vec![1.0, 0.0]), (1, vec![0.0, 1.0])]).is_err()
        );
        assert!(VectorIndex::open(&path, 2, 2, &[(u64::MAX, vec![1.0, 0.0])]).is_err());
        assert_eq!(fs::read(&path).unwrap(), binary);
        assert_eq!(fs::read(manifest_path(&path)).unwrap(), manifest);
    }

    #[test]
    fn cosine_boundaries_preserve_identical_and_nearby_vectors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        let vectors = vec![
            (1, vec![1.0, 0.0, 0.0, 0.0]),
            (2, vec![1.0, 0.0001, 0.0, 0.0]),
            (3, vec![0.0, 1.0, 0.0, 0.0]),
            (4, vec![-1.0, 0.0, 0.0, 0.0]),
        ];
        for _ in 0..2 {
            let index = VectorIndex::open(&path, 4, 1, &vectors).unwrap();
            let results = index.search(&vectors[0].1, 4, &keys(&vectors)).unwrap();
            assert_eq!(
                results.iter().map(|r| r.0).collect::<Vec<_>>(),
                [1, 2, 3, 4]
            );
            assert_eq!(results[0].1, 1.0);
            assert!(results[1].1 > 0.99999999 && results[1].1 < 1.0);
            assert_eq!(results[2].1, 0.0);
            assert_eq!(results[3].1, -1.0);
        }
    }

    #[test]
    fn finite_extreme_magnitudes_have_valid_cosine_scores() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        let vectors = vec![
            (1, vec![f32::MAX, f32::MAX]),
            (2, vec![f32::from_bits(1), 0.0]),
        ];
        let index = VectorIndex::open(&path, 2, 1, &vectors).unwrap();
        for (key, vector) in &vectors {
            let result = index.search(vector, 1, &HashSet::from([*key])).unwrap();
            assert_eq!(result.len(), 1);
            assert!((result[0].1 - 1.0).abs() < 1e-5);
        }
        assert_snapshot(&path, 1, &vectors);
    }

    #[test]
    fn non_axis_cosines_match_analytic_f64_scores_after_reload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        // Every vector has norm 3. Normalization preserves their component
        // ratios exactly in F32, so these rational cosines need no F32 tolerance.
        let vectors = vec![
            (1, vec![1.0, 2.0, 2.0]),
            (2, vec![-2.0, 1.0, 2.0]),
            (3, vec![2.0, -2.0, 1.0]),
            (4, vec![2.0, -1.0, -2.0]),
            (5, vec![-1.0, -2.0, -2.0]),
        ];
        for _ in 0..2 {
            let index = VectorIndex::open(&path, 3, 1, &vectors).unwrap();
            let results = index.search(&vectors[0].1, 5, &keys(&vectors)).unwrap();
            assert_eq!(
                results.iter().map(|r| r.0).collect::<Vec<_>>(),
                [1, 2, 3, 4, 5]
            );
            for ((_, actual), expected) in
                results.iter().zip([1.0, 4.0 / 9.0, 0.0, -4.0 / 9.0, -1.0])
            {
                assert!((actual - expected).abs() < 1e-14, "{actual} != {expected}");
            }
        }
    }

    #[test]
    fn fused_vectors_preserve_weighted_cosines_in_f64() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        // Concatenate two norm-3 embeddings, scaling the second by 2.
        // Their cosine contributions consequently have weights 1/5 and 4/5.
        let query = [1.0, 2.0, 2.0, 4.0, -2.0, 4.0];
        let vectors = vec![
            (1, query.to_vec()),
            (2, vec![2.0, -1.0, 2.0, 2.0, 4.0, 4.0]),
            (3, vec![1.0, 2.0, 2.0, -4.0, 2.0, -4.0]),
            (4, query.iter().map(|v| -v).collect()),
        ];
        for _ in 0..2 {
            let index = VectorIndex::open(&path, 6, 1, &vectors).unwrap();
            let results = index.search(&query, 4, &keys(&vectors)).unwrap();
            assert_eq!(
                results.iter().map(|r| r.0).collect::<Vec<_>>(),
                [1, 2, 3, 4]
            );
            for ((_, actual), expected) in results.iter().zip([1.0, 4.0 / 9.0, -0.6, -1.0]) {
                assert!((actual - expected).abs() < 1e-14, "{actual} != {expected}");
            }
        }
    }

    #[test]
    fn positive_scaling_preserves_scores_and_caller_query_bits() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        let vectors = vec![
            (1, vec![1.0, 2.0, -2.0, 0.0]),
            (2, vec![2.0, 1.0, 2.0, 0.0]),
            (3, vec![-1.0, -2.0, 2.0, 0.0]),
        ];
        let query = [1.0, 2.0, -2.0, -0.0];
        let expected = VectorIndex::open(&path, 4, 1, &vectors)
            .unwrap()
            .search(&query, 3, &keys(&vectors))
            .unwrap();
        // Powers of two change magnitude without perturbing component ratios;
        // the extremes would underflow/overflow an F32 squared-norm calculation.
        for exponent in [-80, 0, 80] {
            let scale = 2.0_f32.powi(exponent);
            let scaled: Vec<_> = vectors
                .iter()
                .map(|(key, vector)| (*key, vector.iter().map(|v| v * scale).collect()))
                .collect();
            let original = scaled.clone();
            let index = VectorIndex::open(&path, 4, 1, &scaled).unwrap();
            assert_eq!(scaled, original);
            for query_exponent in [-80, 0, 80] {
                let scaled_query = query.map(|v| v * 2.0_f32.powi(query_exponent));
                let bits = scaled_query.map(f32::to_bits);
                assert_eq!(
                    index.search(&scaled_query, 3, &keys(&vectors)).unwrap(),
                    expected
                );
                assert_eq!(scaled_query.map(f32::to_bits), bits);
            }
        }
    }

    #[test]
    fn predicate_filter_fills_limit_with_eligible_lower_scoring_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        let vectors = vec![
            (10, vec![3.0, 4.0]),
            (20, vec![4.0, 3.0]),
            (30, vec![-4.0, 3.0]),
            (40, vec![-3.0, -4.0]),
        ];
        for _ in 0..2 {
            let index = VectorIndex::open(&path, 2, 1, &vectors).unwrap();
            let cutoff = 30;
            let results = index
                .search_filtered(&[3.0, 4.0], 2, |key| key >= cutoff)
                .unwrap();
            assert_eq!(results.iter().map(|r| r.0).collect::<Vec<_>>(), [30, 40]);
            assert!(results[0].1.abs() < 1e-14);
            assert!((results[1].1 + 1.0).abs() < 1e-14);
            assert_eq!(
                results,
                index
                    .search(&[3.0, 4.0], 2, &HashSet::from([30, 40, 999]))
                    .unwrap()
            );
            assert_eq!(
                index
                    .search_filtered(&[3.0, 4.0], 1, |key| key >= cutoff)
                    .unwrap(),
                results[..1]
            );
            assert!(
                index
                    .search_filtered(&[3.0, 4.0], usize::MAX, |_| false)
                    .unwrap()
                    .is_empty()
            );
            assert!(
                index
                    .search_filtered(&[3.0, 4.0], 0, |_| panic!("zero limit invoked predicate"))
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn repeated_key_reuse_and_replacement_survive_reopening() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        let snapshots = [
            sample(),
            vec![(42, vec![3.0, 4.0]), (7, vec![-4.0, 3.0])],
            vec![
                (0, vec![-3.0, 4.0]),
                (42, vec![-4.0, -3.0]),
                (u64::MAX - 1, vec![4.0, -3.0]),
            ],
            vec![(7, vec![4.0, 3.0])],
        ];
        let all_keys = HashSet::from([0, 7, 42, u64::MAX - 1]);
        for generation in 0..12 {
            let vectors = &snapshots[generation as usize % snapshots.len()];
            for _ in 0..2 {
                let index = VectorIndex::open(&path, 2, generation, vectors).unwrap();
                assert_eq!(index.len(), vectors.len());
                let results = index
                    .search(&[3.0, 4.0], all_keys.len(), &all_keys)
                    .unwrap();
                assert_eq!(
                    results.iter().map(|r| r.0).collect::<HashSet<_>>(),
                    keys(vectors)
                );
                assert_eq!(results.len(), vectors.len());
                assert!(results.windows(2).all(|pair| pair[0].1 >= pair[1].1));
                for (key, actual) in results {
                    let vector = &vectors.iter().find(|(k, _)| *k == key).unwrap().1;
                    let x = f64::from(vector[0]);
                    let y = f64::from(vector[1]);
                    let expected = (3.0 * x + 4.0 * y) / (5.0 * x.hypot(y));
                    assert!(
                        (actual - expected).abs() < 1e-7,
                        "generation {generation}, key {key}: {actual} != {expected}"
                    );
                }
                assert_snapshot(&path, generation, vectors);
            }
        }
    }

    fn replace_native_sidecar(path: &Path, options: IndexOptions, vectors: &[(u64, Vec<f32>)]) {
        let dimensions = options.dimensions;
        let native = Index::new(&options).unwrap();
        native.reserve(vectors.len()).unwrap();
        for (key, vector) in vectors {
            native
                .add(*key, &normalized(vector, dimensions).unwrap())
                .unwrap();
        }
        // Publish a valid native binary with a matching hash: rejection must
        // come from semantic validation rather than corruption detection.
        let mut manifest = read_manifest(path);
        persist(path, &native, &mut manifest).unwrap();
        assert_eq!(manifest.binary_hash, crate::hash(fs::read(path).unwrap()));
        assert_eq!(
            manifest.fingerprint,
            manifest.compute_fingerprint().unwrap()
        );
    }

    #[test]
    fn valid_native_sidecars_with_wrong_metric_or_scalar_are_rebuilt() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        let vectors = sample();
        VectorIndex::open(&path, 2, 1, &vectors).unwrap();
        for (metric, quantization) in [
            (MetricKind::L2sq, ScalarKind::F32),
            (MetricKind::Cos, ScalarKind::F16),
        ] {
            replace_native_sidecar(
                &path,
                IndexOptions {
                    dimensions: 2,
                    metric,
                    quantization,
                    connectivity: CONNECTIVITY,
                    expansion_add: EXPANSION_ADD,
                    expansion_search: EXPANSION_SEARCH,
                    multi: false,
                },
                &vectors,
            );
            let error = load_cached(&path, 2).err().unwrap();
            assert!(
                error.to_string().contains("configuration/count mismatch"),
                "{error:#}"
            );
            let recovered = VectorIndex::open(&path, 2, 1, &vectors).unwrap();
            assert_eq!(recovered.index.metric_kind(), MetricKind::Cos);
            assert_eq!(recovered.index.scalar_kind(), ScalarKind::F32);
            assert_snapshot(&path, 1, &vectors);
        }
    }

    #[test]
    fn valid_native_sidecar_with_same_count_but_wrong_keys_is_rebuilt() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        let vectors = sample();
        VectorIndex::open(&path, 2, 1, &vectors).unwrap();
        let mut wrong_keys = vectors.clone();
        wrong_keys[0].0 = 7;
        replace_native_sidecar(
            &path,
            IndexOptions {
                dimensions: 2,
                metric: MetricKind::Cos,
                quantization: ScalarKind::F32,
                connectivity: CONNECTIVITY,
                expansion_add: EXPANSION_ADD,
                expansion_search: EXPANSION_SEARCH,
                multi: false,
            },
            &wrong_keys,
        );
        let error = load_cached(&path, 2).err().unwrap();
        assert!(
            error.to_string().contains("vector binary key mismatch"),
            "{error:#}"
        );
        let recovered = VectorIndex::open(&path, 2, 1, &vectors).unwrap();
        assert!(!recovered.index.contains(7));
        assert_eq!(
            recovered.search(&[1.0, 0.0], 1, &keys(&vectors)).unwrap(),
            [(0, 1.0)]
        );
        assert_snapshot(&path, 1, &vectors);
    }

    #[test]
    fn failed_manifest_publication_cleans_temporary_files_and_recovers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vectors");
        VectorIndex::open(&path, 2, 1, &sample()).unwrap();
        let old_binary = fs::read(&path).unwrap();
        let old_manifest = fs::read(manifest_path(&path)).unwrap();
        // A directory at the manifest destination reliably makes its rename
        // fail, including when the test runs with elevated permissions.
        fs::remove_file(manifest_path(&path)).unwrap();
        fs::create_dir(manifest_path(&path)).unwrap();
        let vectors = vec![(7, vec![3.0, 4.0])];
        let error = VectorIndex::open(&path, 2, 2, &vectors).err().unwrap();
        assert!(format!("{error:#}").contains("replace"), "{error:#}");
        assert_ne!(fs::read(&path).unwrap(), old_binary);
        let entries: HashSet<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(entries, HashSet::from([path.clone(), manifest_path(&path)]));
        fs::remove_dir(manifest_path(&path)).unwrap();
        fs::write(manifest_path(&path), old_manifest).unwrap();
        let error = load_cached(&path, 2).err().unwrap();
        assert!(
            error.to_string().contains("vector binary hash mismatch"),
            "{error:#}"
        );
        for _ in 0..2 {
            let recovered = VectorIndex::open(&path, 2, 2, &vectors).unwrap();
            assert_snapshot(&path, 2, &vectors);
            assert_eq!(
                recovered.search(&[3.0, 4.0], 1, &keys(&vectors)).unwrap(),
                [(7, 1.0)]
            );
        }
    }
}
