//! Immutable, content-addressed ANN bases and occurrence-local exact deltas.
//!
//! Only publication holds a per-base interprocess lock. Readers own copied native
//! indexes, and workspace pointers contain identifiers and hashes, never vectors.

use super::*;
use fs2::FileExt;
use serde_json::Value;

const SHARED_VERSION: u32 = 1;

/// An immutable shared ANN base with an exact cosine delta for one worktree.
///
/// Embedding keys are full lowercase SHA-256 hashes supplied by the caller (they
/// need not be hashes of the vector itself). Equal keys must have bit-identical
/// vectors. Native IDs are assigned in full-key order, never by truncating hashes.
/// Occurrence IDs are independent of native IDs, so even `u64::MAX` is supported.
pub struct SharedIndex {
    base: VectorIndex,
    base_occurrences: BTreeMap<u64, Vec<u64>>,
    delta: BTreeMap<String, Delta>,
    len: usize,
    fingerprint: String,
}

struct Delta {
    vector: Vec<f32>,
    occurrences: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Embedding {
    native_id: u64,
    vector_hash: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct BaseManifest {
    version: u32,
    contract: String,
    dimensions: usize,
    embeddings: BTreeMap<String, Embedding>,
    id: String,
}

impl BaseManifest {
    fn identity(&self) -> Result<String> {
        Ok(crate::hash(serde_json::to_vec(&(
            self.version,
            &self.contract,
            self.dimensions,
            &self.embeddings,
        ))?))
    }

    fn validate(&self, contract: &str, dimensions: usize) -> Result<()> {
        ensure!(
            self.version == SHARED_VERSION
                && self.contract == contract
                && self.dimensions == dimensions
                && self.id == self.identity()?,
            "incompatible shared base manifest"
        );
        for (id, (key, embedding)) in self.embeddings.iter().enumerate() {
            ensure!(
                full_hash(key)
                    && full_hash(&embedding.vector_hash)
                    && embedding.native_id == u64::try_from(id)?
                    && embedding.native_id != u64::MAX,
                "invalid shared embedding mapping"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Pointer {
    version: u32,
    contract: String,
    base_id: String,
    /// Exact current occurrence membership; removed/replaced occurrences vanish.
    membership: BTreeMap<u64, String>,
    /// Full embedding keys and content hashes for vectors absent from the base.
    delta: BTreeMap<String, String>,
    fingerprint: String,
}

impl Pointer {
    fn identity(&self) -> Result<String> {
        Ok(crate::hash(serde_json::to_vec(&(
            self.version,
            &self.contract,
            &self.base_id,
            &self.membership,
            &self.delta,
        ))?))
    }

    fn valid(&self, contract: &str) -> bool {
        self.version == SHARED_VERSION
            && self.contract == contract
            && full_hash(&self.base_id)
            && self.identity().is_ok_and(|id| id == self.fingerprint)
    }
}

struct Snapshot<'a> {
    membership: BTreeMap<u64, String>,
    vectors: BTreeMap<String, &'a [f32]>,
    hashes: BTreeMap<String, String>,
}

impl<'a> Snapshot<'a> {
    fn new(vectors: &'a [(u64, String, Vec<f32>)], dimensions: usize) -> Result<Self> {
        let mut snapshot = Self {
            membership: BTreeMap::new(),
            vectors: BTreeMap::new(),
            hashes: BTreeMap::new(),
        };
        for (occurrence, key, vector) in vectors {
            ensure!(
                full_hash(key),
                "embedding key must be a full lowercase SHA-256 hash"
            );
            ensure!(
                snapshot
                    .membership
                    .insert(*occurrence, key.clone())
                    .is_none(),
                "duplicate occurrence ID {occurrence}"
            );
            if let Some(old) = snapshot.vectors.get(key) {
                ensure!(
                    old.len() == vector.len()
                        && old
                            .iter()
                            .zip(vector)
                            .all(|(a, b)| a.to_bits() == b.to_bits()),
                    "embedding key collision for {key}"
                );
            } else {
                validate(vector, dimensions).with_context(|| format!("invalid embedding {key}"))?;
                snapshot.vectors.insert(key.clone(), vector);
                snapshot.hashes.insert(key.clone(), vector_hash(vector));
            }
        }
        Ok(snapshot)
    }

    fn check_content(&self, base: &BaseManifest) -> Result<()> {
        for (key, hash) in &self.hashes {
            if let Some(embedding) = base.embeddings.get(key) {
                ensure!(
                    embedding.vector_hash == *hash,
                    "embedding key collision for {key}"
                );
            }
        }
        Ok(())
    }

    fn overlap(&self, base: &BaseManifest) -> usize {
        self.vectors
            .keys()
            .filter(|key| base.embeddings.contains_key(*key))
            .count()
    }

    fn suitable(&self, base: &BaseManifest) -> bool {
        let overlap = self.overlap(base);
        (overlap > 0 || self.vectors.is_empty())
            && self.vectors.len() - overlap <= 64.max(base.embeddings.len() / 4)
            && overlap >= base.embeddings.len().div_ceil(2)
    }

    fn manifest(&self, contract: &str, dimensions: usize) -> Result<BaseManifest> {
        let mut base = BaseManifest {
            version: SHARED_VERSION,
            contract: contract.to_owned(),
            dimensions,
            embeddings: self
                .hashes
                .iter()
                .enumerate()
                .map(|(id, (key, hash))| {
                    Ok((
                        key.clone(),
                        Embedding {
                            native_id: u64::try_from(id)?,
                            vector_hash: hash.clone(),
                        },
                    ))
                })
                .collect::<Result<_>>()?,
            id: String::new(),
        };
        base.id = base.identity()?;
        base.validate(contract, dimensions)?;
        Ok(base)
    }
}

impl SharedIndex {
    /// Open a worktree snapshot, sharing a suitable immutable base across worktrees.
    ///
    /// The profile and native index contract isolate the global registry. Dimensions
    /// come from `profile["dimensions"]`, or the first vector if that field is absent;
    /// an empty snapshot therefore requires profile dimensions. All supplied inputs
    /// are checked before writes; repeated payloads are validated/hashed only once.
    /// A missing/stale pointer scans only this profile's registry metadata, choosing
    /// the greatest overlap (ties prefer smaller bases, then lexicographic base ID).
    /// Delta overflow (> max(64, base size / 4)) or <50% active base overlap compacts.
    /// Readonly opens neither create directories/locks nor publish any files.
    pub fn open(
        global_directory: &Path,
        workspace_pointer: &Path,
        profile: &Value,
        vectors: &[(u64, String, Vec<f32>)],
        readonly: bool,
    ) -> Result<Self> {
        let dimensions = match profile.get("dimensions") {
            Some(value) => usize::try_from(value.as_u64().context("invalid profile dimensions")?)?,
            None => vectors
                .first()
                .context("empty snapshot requires profile dimensions")?
                .2
                .len(),
        };
        ensure!(dimensions > 0, "vector dimensions must be positive");
        let snapshot = Snapshot::new(vectors, dimensions)?;
        // Include every setting affecting graph construction/traversal and scoring.
        let contract = crate::hash(serde_json::to_vec(&(
            SHARED_VERSION,
            VERSION,
            usearch::version(),
            profile,
            dimensions,
            "cos-f32-normalized-f64-rescore-v1",
            CONNECTIVITY,
            EXPANSION_ADD,
            EXPANSION_SEARCH,
        ))?);
        let directory = global_directory.join(&contract);
        let old_pointer = fs::read(workspace_pointer)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Pointer>(&bytes).ok())
            .filter(|pointer| pointer.valid(&contract));

        let mut selected = None;
        if let Some(pointer) = &old_pointer {
            // A previously exact-delta key must also retain its original content.
            for (key, hash) in &pointer.delta {
                if let Some(current) = snapshot.hashes.get(key) {
                    ensure!(current == hash, "embedding key collision for {key}");
                }
            }
            if let Ok(base) = read_base(&directory, &pointer.base_id, &contract, dimensions) {
                snapshot.check_content(&base)?;
                if snapshot.suitable(&base)
                    && let Ok(index) = load_base(&directory, &base)
                {
                    selected = Some((index, base));
                }
            }
        }

        if selected.is_none() {
            // Read metadata only; checksum/load binaries only for suitable candidates.
            let mut candidates = Vec::new();
            if let Ok(entries) = fs::read_dir(&directory) {
                for entry in entries {
                    let entry = entry?;
                    let name = entry.file_name();
                    let Some(id) = name
                        .to_str()
                        .and_then(|name| name.strip_suffix(".base.json"))
                    else {
                        continue;
                    };
                    if let Ok(base) = read_base(&directory, id, &contract, dimensions) {
                        snapshot.check_content(&base)?;
                        if snapshot.suitable(&base) {
                            candidates.push(base);
                        }
                    }
                }
            }
            candidates.sort_by(|a, b| {
                snapshot
                    .overlap(b)
                    .cmp(&snapshot.overlap(a))
                    .then_with(|| a.embeddings.len().cmp(&b.embeddings.len()))
                    .then_with(|| a.id.cmp(&b.id))
            });
            for base in candidates {
                if let Ok(index) = load_base(&directory, &base) {
                    selected = Some((index, base));
                    break;
                }
            }
        }

        let (base_index, base) = match selected {
            Some(selected) => selected,
            None => {
                let base = snapshot.manifest(&contract, dimensions)?;
                let index = if readonly {
                    build_base(&base, &snapshot)?
                } else {
                    publish_base(&directory, &base, &snapshot)?
                };
                (index, base)
            }
        };
        let mut pointer = Pointer {
            version: SHARED_VERSION,
            contract,
            base_id: base.id,
            membership: snapshot.membership,
            delta: snapshot
                .hashes
                .into_iter()
                .filter(|(key, _)| !base.embeddings.contains_key(key))
                .collect(),
            fingerprint: String::new(),
        };
        pointer.fingerprint = pointer.identity()?;
        if !readonly
            && old_pointer
                .as_ref()
                .is_none_or(|old| old.fingerprint != pointer.fingerprint)
        {
            atomic_write(workspace_pointer, &serde_json::to_vec(&pointer)?)?;
        }

        let mut base_occurrences: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        let mut delta = BTreeMap::new();
        for (occurrence, key) in &pointer.membership {
            if let Some(embedding) = base.embeddings.get(key) {
                base_occurrences
                    .entry(embedding.native_id)
                    .or_default()
                    .push(*occurrence);
            } else {
                if !delta.contains_key(key) {
                    delta.insert(
                        key.clone(),
                        Delta {
                            vector: normalized(snapshot.vectors[key], dimensions)?,
                            occurrences: Vec::new(),
                        },
                    );
                }
                delta
                    .get_mut(key)
                    .expect("delta inserted")
                    .occurrences
                    .push(*occurrence);
            }
        }
        Ok(Self {
            base: VectorIndex {
                index: base_index,
                dimensions,
            },
            base_occurrences,
            delta,
            len: pointer.membership.len(),
            fingerprint: pointer.fingerprint,
        })
    }

    /// Exact small eligible base sets (otherwise ANN candidates) plus exact delta
    /// cosine scores, fanned out only to allowed current occurrences. Similarity is
    /// accumulated in F64; ties are ordered by occurrence ID. The limit counts
    /// occurrences, not vectors.
    pub fn search_filtered(
        &self,
        query: &[f32],
        limit: usize,
        allowed: impl Fn(u64) -> bool,
    ) -> Result<Vec<(u64, f64)>> {
        // Validate even for an empty index/limit/filter, as VectorIndex does.
        let normalized_query =
            normalized(query, self.base.dimensions).context("invalid vector query")?;
        let limit = limit.min(self.len);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let eligible: BTreeMap<_, Vec<_>> = self
            .base_occurrences
            .iter()
            .filter_map(|(&id, occurrences)| {
                let occurrences: Vec<_> = occurrences
                    .iter()
                    .copied()
                    .filter(|&id| allowed(id))
                    .collect();
                (!occurrences.is_empty()).then_some((id, occurrences))
            })
            .collect();
        let query_norm: f64 = normalized_query.iter().map(|&v| f64::from(v).powi(2)).sum();
        let mut results = Vec::new();
        if eligible.len() <= EXPANSION_SEARCH {
            // Sparse native filters can miss distant eligible IDs entirely. Read
            // only the small eligible set, not every vector in the shared base.
            let mut stored = vec![0.0_f32; self.base.dimensions];
            for (&native_id, occurrences) in &eligible {
                ensure!(
                    self.base
                        .index
                        .get(native_id, &mut stored)
                        .context("read eligible base vector")?
                        == 1,
                    "missing eligible base vector for key {native_id}"
                );
                let score = cosine_score(&normalized_query, query_norm, &stored);
                results.extend(occurrences.iter().map(|&id| (id, score)));
            }
        } else {
            // Oversampling provides deterministic tie ordering across nearby ANN
            // candidates without turning base search into a full exact scan.
            let mut candidate_limit = limit.max(EXPANSION_SEARCH).min(eligible.len());
            loop {
                let candidates = self
                    .base
                    .search_filtered(query, candidate_limit, |id| eligible.contains_key(&id))?;
                results.clear();
                for &(native_id, score) in &candidates {
                    for &occurrence in &eligible[&native_id] {
                        results.push((occurrence, score));
                    }
                }
                sort_results(&mut results);
                // Expand boundary ties so native key ordering cannot choose an
                // arbitrary occurrence when many embeddings have the same score.
                let boundary_tie = results
                    .get(limit - 1)
                    .is_some_and(|cutoff| candidates.last().is_some_and(|last| last.1 >= cutoff.1));
                if candidate_limit == eligible.len()
                    || candidates.len() < candidate_limit
                    || !boundary_tie
                {
                    break;
                }
                candidate_limit = candidate_limit.saturating_mul(2).min(eligible.len());
            }
        }
        for delta in self.delta.values() {
            let occurrences: Vec<_> = delta
                .occurrences
                .iter()
                .copied()
                .filter(|&id| allowed(id))
                .collect();
            if occurrences.is_empty() {
                continue;
            }
            let score = cosine_score(&normalized_query, query_norm, &delta.vector);
            results.extend(occurrences.into_iter().map(|id| (id, score)));
        }
        sort_results(&mut results);
        results.truncate(limit);
        Ok(results)
    }

    /// Number of current occurrences, including repeated embeddings.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Deterministic snapshot key covering profile/settings, base topology,
    /// occurrence membership, and delta content. Available in pointer metadata too.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

fn cosine_score(query: &[f32], query_norm: f64, vector: &[f32]) -> f64 {
    let mut dot = 0.0;
    let mut norm = 0.0;
    for (&a, &b) in query.iter().zip(vector) {
        dot += f64::from(a) * f64::from(b);
        norm += f64::from(b).powi(2);
    }
    (dot / (query_norm * norm).sqrt()).clamp(-1.0, 1.0)
}

fn sort_results(results: &mut [(u64, f64)]) {
    results.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
}

fn full_hash(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn base_path(directory: &Path, id: &str) -> PathBuf {
    directory.join(format!("{id}.usearch"))
}

fn base_manifest_path(directory: &Path, id: &str) -> PathBuf {
    directory.join(format!("{id}.base.json"))
}

fn read_base(
    directory: &Path,
    id: &str,
    contract: &str,
    dimensions: usize,
) -> Result<BaseManifest> {
    ensure!(full_hash(id), "invalid shared base ID");
    let base: BaseManifest = serde_json::from_slice(&fs::read(base_manifest_path(directory, id))?)?;
    base.validate(contract, dimensions)?;
    ensure!(base.id == id, "shared base ID mismatch");
    Ok(base)
}

fn load_base(directory: &Path, base: &BaseManifest) -> Result<Index> {
    // Preserve the primitive's full checksum + bounds-checked copied load. Do not
    // call VectorIndex::open/Manifest::new or reconcile immutable base membership.
    let (index, manifest) = load_cached(&base_path(directory, &base.id), base.dimensions)?;
    ensure!(
        manifest.generation == 0
            && manifest.vectors.len() == base.embeddings.len()
            && base
                .embeddings
                .values()
                .all(|embedding| manifest.vectors.get(&embedding.native_id)
                    == Some(&embedding.vector_hash)),
        "shared base/native mapping mismatch"
    );
    Ok(index)
}

fn native_vectors(base: &BaseManifest, snapshot: &Snapshot<'_>) -> Vec<(u64, Vec<f32>)> {
    base.embeddings
        .iter()
        .map(|(key, embedding)| (embedding.native_id, snapshot.vectors[key].to_vec()))
        .collect()
}

fn build_base(base: &BaseManifest, snapshot: &Snapshot<'_>) -> Result<Index> {
    build(base.dimensions, &native_vectors(base, snapshot))
}

fn publish_base(directory: &Path, base: &BaseManifest, snapshot: &Snapshot<'_>) -> Result<Index> {
    fs::create_dir_all(directory)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(format!("{}.lock", base.id)))?;
    lock.lock_exclusive()
        .context("lock shared vector base publication")?;
    // A concurrent publisher may already have finished. Never mutate a valid base.
    if let Ok(existing) = read_base(directory, &base.id, &base.contract, base.dimensions)
        && let Ok(index) = load_base(directory, &existing)
    {
        return Ok(index);
    }
    let index = build_base(base, snapshot)?;
    let mut manifest = Manifest {
        version: VERSION,
        usearch_version: usearch::version().to_owned(),
        generation: 0,
        dimensions: base.dimensions,
        vectors: base
            .embeddings
            .values()
            .map(|embedding| (embedding.native_id, embedding.vector_hash.clone()))
            .collect(),
        fingerprint: String::new(),
        binary_hash: String::new(),
    };
    manifest.fingerprint = manifest.compute_fingerprint()?;
    persist(&base_path(directory, &base.id), &index, &mut manifest)?;
    // Publication marker last: registry readers ignore incomplete/crashed builds.
    atomic_write(
        &base_manifest_path(directory, &base.id),
        &serde_json::to_vec(base)?,
    )?;
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    type Row = (u64, String, Vec<f32>);

    fn row(id: u64, name: &str, vector: &[f32]) -> Row {
        (id, crate::hash(name), vector.to_vec())
    }

    fn profile() -> Value {
        json!({"provider": "test", "model": "one", "dimensions": 2})
    }

    fn pointer(path: &Path) -> Pointer {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn mark_old(path: &Path) -> std::time::SystemTime {
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(time))
            .unwrap();
        fs::metadata(path).unwrap().modified().unwrap()
    }

    fn ids(index: &SharedIndex, query: &[f32]) -> Vec<u64> {
        index
            .search_filtered(query, usize::MAX, |_| true)
            .unwrap()
            .into_iter()
            .map(|row| row.0)
            .collect()
    }

    fn base_files(global: &Path, pointer: &Pointer) -> (PathBuf, PathBuf, PathBuf) {
        let directory = global.join(&pointer.contract);
        let binary = base_path(&directory, &pointer.base_id);
        (
            binary.clone(),
            manifest_path(&binary),
            base_manifest_path(&directory, &pointer.base_id),
        )
    }

    #[test]
    fn overlapping_worktrees_edit_delete_restore_and_unchanged_open() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("global");
        let a = dir.path().join("a/pointer.json");
        let b = dir.path().join("b/pointer.json");
        let original = vec![
            row(10, "a", &[1.0, 0.0]),
            row(20, "b", &[0.0, 1.0]),
            row(30, "c", &[-1.0, 0.0]),
        ];
        let first = SharedIndex::open(&global, &a, &profile(), &original, false).unwrap();
        let first_pointer = pointer(&a);
        let (binary, native_manifest, metadata) = base_files(&global, &first_pointer);
        let original_files = [
            fs::read(&binary).unwrap(),
            fs::read(&native_manifest).unwrap(),
            fs::read(&metadata).unwrap(),
        ];
        let other = vec![
            row(101, "a", &[1.0, 0.0]),
            row(102, "b", &[0.0, 1.0]),
            row(103, "d", &[1.0, 1.0]),
            row(104, "d", &[1.0, 1.0]),
        ];
        let second = SharedIndex::open(&global, &b, &profile(), &other, false).unwrap();
        let second_pointer = pointer(&b);
        assert_eq!(first_pointer.base_id, second_pointer.base_id);
        assert_eq!(second.delta.len(), 1);
        assert_eq!(second.len(), 4);
        assert_eq!(ids(&second, &[1.0, 1.0]), [103, 104, 101, 102]);
        assert_ne!(first.fingerprint(), second.fingerprint());
        assert_eq!(pointer(&a).fingerprint, first.fingerprint());
        assert_eq!(ids(&first, &[1.0, 0.0]), [10, 20, 30]);

        // Replacement removes the old occurrence binding; deletion cannot fan out
        // to the previous occurrence even when the embedding survives in the base.
        let edited = vec![
            row(10, "edit", &[1.0, 1.0]),
            row(20, "b", &[0.0, 1.0]),
            row(40, "c", &[-1.0, 0.0]),
        ];
        let changed = SharedIndex::open(&global, &a, &profile(), &edited, false).unwrap();
        assert_eq!(pointer(&a).base_id, first_pointer.base_id);
        assert_eq!(changed.delta.len(), 1);
        assert_eq!(ids(&changed, &[1.0, 0.0]), [10, 20, 40]);
        assert_ne!(changed.fingerprint(), first.fingerprint());
        let restored = SharedIndex::open(&global, &a, &profile(), &original, false).unwrap();
        assert_eq!(restored.fingerprint(), first.fingerprint());
        assert!(restored.delta.is_empty());
        assert_eq!(
            [
                fs::read(&binary).unwrap(),
                fs::read(&native_manifest).unwrap(),
                fs::read(&metadata).unwrap()
            ],
            original_files
        );

        let pointer_time = mark_old(&a);
        let binary_time = mark_old(&binary);
        let mut reordered = original.clone();
        reordered.reverse();
        let reopened = SharedIndex::open(&global, &a, &profile(), &reordered, false).unwrap();
        assert_eq!(reopened.fingerprint(), first.fingerprint());
        assert_eq!(fs::metadata(&a).unwrap().modified().unwrap(), pointer_time);
        assert_eq!(
            fs::metadata(&binary).unwrap().modified().unwrap(),
            binary_time
        );
        let value: Value = serde_json::from_slice(&fs::read(&b).unwrap()).unwrap();
        assert!(value.get("vectors").is_none());
        assert_eq!(value["delta"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn delta_fanout_filters_before_limit_and_handles_reserved_occurrence() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("global");
        let path = dir.path().join("pointer");
        let base = vec![
            row(10, "a", &[1.0, 0.0]),
            row(20, "b", &[0.0, 1.0]),
            row(30, "c", &[-1.0, 0.0]),
        ];
        SharedIndex::open(&global, &path, &profile(), &base, false).unwrap();
        let current = vec![
            row(99, "a", &[1.0, 0.0]),
            row(2, "a", &[1.0, 0.0]),
            row(3, "b", &[0.0, 1.0]),
            row(9, "delta", &[1.0, 1.0]),
            row(4, "delta", &[1.0, 1.0]),
            row(u64::MAX, "delta", &[1.0, 1.0]),
        ];
        let index = SharedIndex::open(&global, &path, &profile(), &current, false).unwrap();
        assert_eq!(index.base.len(), 3);
        assert_eq!(index.delta.len(), 1);
        assert_eq!(index.len(), 6);
        assert_eq!(ids(&index, &[1.0, 1.0]), [4, 9, u64::MAX, 2, 3, 99]);
        assert_eq!(
            index
                .search_filtered(&[1.0, 1.0], 2, |id| id != 4)
                .unwrap()
                .iter()
                .map(|row| row.0)
                .collect::<Vec<_>>(),
            [9, u64::MAX]
        );
        let restrictive = index
            .search_filtered(&[1.0, 0.0], 10, |id| id == 3 || id == u64::MAX)
            .unwrap();
        assert_eq!(
            restrictive.iter().map(|row| row.0).collect::<Vec<_>>(),
            [u64::MAX, 3]
        );
        assert_eq!(restrictive[0].1, 1.0 / 2.0_f64.sqrt());
        assert_eq!(
            index
                .search_filtered(&[1.0, 0.0], 1, |id| id == 99)
                .unwrap(),
            [(99, 1.0)]
        );
        assert!(
            index
                .search_filtered(&[1.0, 0.0], 100, |_| false)
                .unwrap()
                .is_empty()
        );
        assert!(
            index
                .search_filtered(&[1.0, 0.0], 0, |_| panic!("zero limit predicate"))
                .unwrap()
                .is_empty()
        );
        assert!(index.search_filtered(&[0.0, 0.0], 0, |_| true).is_err());
    }

    #[test]
    fn sparse_occurrence_and_cross_file_filters_find_distant_base_vectors() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("global");
        let path = dir.path().join("pointer");
        let mut rows: Vec<_> = (0..128)
            .map(|id| row(id, &format!("distractor-{id}"), &[1.0, id as f32 / 1000.0]))
            .collect();
        rows.extend([
            row(1000, "distant", &[-1.0, 0.0]),
            row(1001, "orthogonal", &[0.0, -1.0]),
            row(1002, "distant", &[-1.0, 0.0]),
        ]);
        SharedIndex::open(&global, &path, &profile(), &rows, false).unwrap();
        for readonly in [false, true] {
            let index = SharedIndex::open(&global, &path, &profile(), &rows, readonly).unwrap();
            assert_eq!(index.base.len(), 130);
            // Only the second occurrence of a distant shared embedding is allowed.
            assert_eq!(
                index
                    .search_filtered(&[1.0, 0.0], 1, |id| id == 1002)
                    .unwrap(),
                [(1002, -1.0)]
            );
            assert_eq!(
                index
                    .search_filtered(&[1.0, 0.0], 10, |id| id >= 1000)
                    .unwrap(),
                [(1001, 0.0), (1000, -1.0), (1002, -1.0)]
            );
        }

        rows.extend([
            row(2000, "allowed-delta", &[0.0, -1.0]),
            row(2001, "excluded-delta", &[1.0, 0.0]),
        ]);
        let files: BTreeMap<_, _> = rows
            .iter()
            .map(|&(id, _, _)| {
                let file = if matches!(id, 1001 | 1002 | 2000) {
                    "other.rs"
                } else {
                    "source.rs"
                };
                (id, file)
            })
            .collect();
        for readonly in [false, true] {
            let index = SharedIndex::open(&global, &path, &profile(), &rows, readonly).unwrap();
            assert_eq!(index.delta.len(), 2);
            let expected = [(1001, 0.0), (2000, 0.0), (1002, -1.0)];
            for limit in [1, 2, 10] {
                assert_eq!(
                    index
                        .search_filtered(&[1.0, 0.0], limit, |id| files[&id] != files[&0])
                        .unwrap(),
                    expected[..limit.min(expected.len())]
                );
            }
            assert_eq!(
                index
                    .search_filtered(&[1.0, 0.0], 1, |id| id == 2000)
                    .unwrap(),
                [(2000, 0.0)]
            );
        }
    }

    #[test]
    fn compaction_thresholds_and_profile_isolation() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("global");
        let path = dir.path().join("pointer");
        let original: Vec<_> = (0..8)
            .map(|id| row(id, &format!("original-{id}"), &[1.0, id as f32]))
            .collect();
        SharedIndex::open(&global, &path, &profile(), &original, false).unwrap();
        let first = pointer(&path);
        let half = original[..4].to_vec();
        SharedIndex::open(&global, &path, &profile(), &half, false).unwrap();
        assert_eq!(pointer(&path).base_id, first.base_id); // Exactly 50% is retained.
        let reduced = SharedIndex::open(&global, &path, &profile(), &half[..3], false).unwrap();
        let compacted = pointer(&path);
        assert_ne!(compacted.base_id, first.base_id);
        assert_eq!(reduced.base.len(), 3);
        let mut grown = half[..3].to_vec();
        grown.extend((100..164).map(|id| row(id, &format!("new-{id}"), &[1.0, id as f32])));
        let exact = SharedIndex::open(&global, &path, &profile(), &grown, false).unwrap();
        assert_eq!(pointer(&path).base_id, compacted.base_id);
        assert_eq!(exact.delta.len(), 64);
        grown.push(row(164, "new-164", &[1.0, 164.0]));
        let larger = SharedIndex::open(&global, &path, &profile(), &grown, false).unwrap();
        assert!(larger.delta.is_empty());
        assert_eq!(larger.base.len(), grown.len());
        assert_ne!(pointer(&path).base_id, compacted.base_id);

        let mut other_profile = profile();
        other_profile["model"] = json!("two");
        let isolated = SharedIndex::open(&global, &path, &other_profile, &original, false).unwrap();
        assert_ne!(pointer(&path).contract, first.contract);
        assert_ne!(isolated.fingerprint(), first.fingerprint);
        assert!(isolated.delta.is_empty());
        let (_, _, original_metadata) = base_files(&global, &first);
        assert!(original_metadata.exists());
    }

    #[test]
    fn registry_chooses_most_overlapping_suitable_base() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("global");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let c = dir.path().join("c");
        let small: Vec<_> = (0..4)
            .map(|id| row(id, &format!("key-{id}"), &[1.0, id as f32]))
            .collect();
        SharedIndex::open(&global, &a, &profile(), &small, false).unwrap();
        let mut large = small.clone();
        large.extend((10..75).map(|id| row(id, &format!("key-{id}"), &[1.0, id as f32])));
        SharedIndex::open(&global, &b, &profile(), &large, false).unwrap();
        assert_ne!(pointer(&a).base_id, pointer(&b).base_id);
        let current = large[..50].to_vec();
        let index = SharedIndex::open(&global, &c, &profile(), &current, false).unwrap();
        assert_eq!(pointer(&c).base_id, pointer(&b).base_id);
        assert!(index.delta.is_empty());
        assert_eq!(index.len(), 50);
    }

    #[test]
    fn readonly_missing_and_corrupt_caches_never_write_and_writers_recover() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("missing/global");
        let path = dir.path().join("missing/worktree/pointer");
        let vectors = vec![row(1, "a", &[1.0, 0.0]), row(2, "b", &[0.0, 1.0])];
        let fallback = SharedIndex::open(&global, &path, &profile(), &vectors, true).unwrap();
        assert_eq!(ids(&fallback, &[1.0, 0.0]), [1, 2]);
        assert!(!dir.path().join("missing").exists());
        let written = SharedIndex::open(&global, &path, &profile(), &vectors, false).unwrap();
        assert_eq!(written.fingerprint(), fallback.fingerprint());
        let saved_pointer = fs::read(&path).unwrap();
        let p = pointer(&path);
        let (binary, native, metadata) = base_files(&global, &p);

        // A pointerless readonly worktree can reuse the shared base and keep its
        // small exact delta in memory without creating a workspace directory.
        let other_path = dir.path().join("readonly/pointer");
        let mut edited = vectors.clone();
        edited.push(row(3, "delta", &[1.0, 1.0]));
        let shared = SharedIndex::open(&global, &other_path, &profile(), &edited, true).unwrap();
        assert_eq!(shared.delta.len(), 1);
        assert!(!dir.path().join("readonly").exists());

        for target in [&binary, &native, &metadata] {
            fs::write(target, b"corrupt").unwrap();
            let readonly = SharedIndex::open(&global, &path, &profile(), &vectors, true).unwrap();
            assert_eq!(ids(&readonly, &[1.0, 0.0]), [1, 2]);
            assert_eq!(fs::read(target).unwrap(), b"corrupt");
            assert_eq!(fs::read(&path).unwrap(), saved_pointer);
            let recovered = SharedIndex::open(&global, &path, &profile(), &vectors, false).unwrap();
            assert_eq!(ids(&recovered, &[1.0, 0.0]), [1, 2]);
            assert_eq!(pointer(&path).base_id, p.base_id);
            assert!(
                load_base(
                    &global.join(&p.contract),
                    &read_base(&global.join(&p.contract), &p.base_id, &p.contract, 2).unwrap()
                )
                .is_ok()
            );
        }
        fs::write(&path, b"broken pointer").unwrap();
        SharedIndex::open(&global, &path, &profile(), &vectors, true).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"broken pointer");
        SharedIndex::open(&global, &path, &profile(), &vectors, false).unwrap();
        assert_eq!(pointer(&path).fingerprint, written.fingerprint());

        // A corrupt overlapping base cannot be reconstructed from a subset;
        // build a new full-current base instead, preserving the old immutable ID.
        fs::write(&binary, b"corrupt").unwrap();
        SharedIndex::open(&global, &other_path, &profile(), &edited, false).unwrap();
        assert_ne!(pointer(&other_path).base_id, p.base_id);
        assert_eq!(fs::read(&binary).unwrap(), b"corrupt");
    }

    #[test]
    fn invalid_inputs_and_key_collisions_do_not_publish() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("global");
        let path = dir.path().join("pointer");
        let vectors = vec![row(1, "a", &[1.0, 0.0]), row(2, "b", &[0.0, 1.0])];
        SharedIndex::open(&global, &path, &profile(), &vectors, false).unwrap();
        let saved = fs::read(&path).unwrap();
        for bad in [
            vec![row(1, "a", &[0.0, 1.0])], // Cross-open content collision.
            vec![row(1, "a", &[1.0, 0.0]), row(3, "a", &[0.0, 1.0])],
            vec![row(1, "a", &[1.0, 0.0]), row(1, "b", &[0.0, 1.0])],
            vec![row(1, "x", &[0.0, 0.0])],
            vec![row(1, "x", &[f32::NAN, 1.0])],
            vec![row(1, "x", &[f32::INFINITY, 1.0])],
            vec![row(1, "x", &[1.0])],
            vec![(1, "truncated".to_owned(), vec![1.0, 0.0])],
        ] {
            for readonly in [false, true] {
                assert!(SharedIndex::open(&global, &path, &profile(), &bad, readonly).is_err());
            }
            assert_eq!(fs::read(&path).unwrap(), saved);
        }
        // Registry discovery also checks full content, independently of pointers.
        assert!(
            SharedIndex::open(
                &global,
                &dir.path().join("new"),
                &profile(),
                &[row(9, "a", &[0.0, 1.0])],
                false
            )
            .is_err()
        );
        let mut delta = vectors.clone();
        delta.push(row(3, "delta", &[1.0, 1.0]));
        SharedIndex::open(&global, &path, &profile(), &delta, false).unwrap();
        delta[2].2 = vec![1.0, -1.0];
        assert!(SharedIndex::open(&global, &path, &profile(), &delta, false).is_err());
    }

    #[test]
    fn empty_snapshots_and_dimensions() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("global");
        let path = dir.path().join("pointer");
        let empty = SharedIndex::open(&global, &path, &profile(), &[], false).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert!(
            empty
                .search_filtered(&[1.0, 0.0], 10, |_| true)
                .unwrap()
                .is_empty()
        );
        assert!(empty.search_filtered(&[0.0, 0.0], 0, |_| true).is_err());
        let added = SharedIndex::open(
            &global,
            &path,
            &profile(),
            &[row(0, "a", &[1.0, 0.0])],
            false,
        )
        .unwrap();
        assert_eq!(ids(&added, &[1.0, 0.0]), [0]);
        let deleted = SharedIndex::open(&global, &path, &profile(), &[], false).unwrap();
        assert_eq!(deleted.fingerprint(), empty.fingerprint());
        assert!(SharedIndex::open(&global, &path, &json!({}), &[], true).is_err());
        assert!(SharedIndex::open(&global, &path, &json!({"dimensions": 0}), &[], true).is_err());
        assert!(SharedIndex::open(&global, &path, &json!({"dimensions": "2"}), &[], true).is_err());
        let inferred = SharedIndex::open(
            &global,
            &path,
            &json!({"model": "inferred"}),
            &[row(0, "a", &[1.0, 0.0])],
            true,
        )
        .unwrap();
        assert_eq!(ids(&inferred, &[1.0, 0.0]), [0]);
    }

    #[test]
    fn native_ids_are_full_key_sorted_and_mapping_is_checked() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("global");
        let path = dir.path().join("pointer");
        // The keys share the entire truncated prefix, but remain distinct.
        let key_a = format!("{}0", "a".repeat(63));
        let key_b = format!("{}1", "a".repeat(63));
        let vectors = vec![
            (u64::MAX, key_b.clone(), vec![0.0, 1.0]),
            (50, key_a.clone(), vec![1.0, 0.0]),
            (3, key_a.clone(), vec![1.0, 0.0]),
        ];
        let index = SharedIndex::open(&global, &path, &profile(), &vectors, false).unwrap();
        assert_eq!(index.base.len(), 2);
        let p = pointer(&path);
        let directory = global.join(&p.contract);
        let base = read_base(&directory, &p.base_id, &p.contract, 2).unwrap();
        assert_eq!(base.embeddings[&key_a].native_id, 0);
        assert_eq!(base.embeddings[&key_b].native_id, 1);
        assert_eq!(ids(&index, &[1.0, 0.0]), [3, 50, u64::MAX]);
        // A valid native manifest with wrong content mapping must be rejected too.
        let binary = base_path(&directory, &p.base_id);
        let mut native: Manifest =
            serde_json::from_slice(&fs::read(manifest_path(&binary)).unwrap()).unwrap();
        native.vectors.insert(0, crate::hash("wrong"));
        native.fingerprint = native.compute_fingerprint().unwrap();
        atomic_write(
            &manifest_path(&binary),
            &serde_json::to_vec(&native).unwrap(),
        )
        .unwrap();
        assert!(load_base(&directory, &base).is_err());
        SharedIndex::open(&global, &path, &profile(), &vectors, false).unwrap();
        assert!(load_base(&directory, &base).is_ok());
    }

    #[test]
    fn f64_scores_and_finite_extremes_match_primitive_in_base_and_delta() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("global");
        let path = dir.path().join("pointer");
        let query = [1.0, 2.0, 2.0];
        let vectors = vec![
            row(1, "one", &query),
            row(2, "two", &[-2.0, 1.0, 2.0]),
            row(3, "three", &[2.0, -2.0, 1.0]),
        ];
        let profile = json!({"dimensions": 3});
        SharedIndex::open(&global, &path, &profile, &vectors, false).unwrap();
        let mut current = vectors.clone();
        current.extend([
            row(4, "four", &[2.0, -1.0, -2.0]),
            row(5, "five", &[-1.0, -2.0, -2.0]),
            row(6, "huge", &[f32::MAX, f32::MAX, 0.0]),
            row(7, "tiny", &[f32::from_bits(1), 0.0, 0.0]),
        ]);
        for readonly in [false, true] {
            let index = SharedIndex::open(&global, &path, &profile, &current, readonly).unwrap();
            assert_eq!(index.delta.len(), 4);
            let results = index.search_filtered(&query, 5, |id| id <= 5).unwrap();
            for ((_, actual), expected) in
                results.iter().zip([1.0, 4.0 / 9.0, 0.0, -4.0 / 9.0, -1.0])
            {
                assert!((actual - expected).abs() < 1e-14, "{actual} != {expected}");
            }
            for (id, _, vector) in &current {
                let score = index.search_filtered(vector, 1, |key| key == *id).unwrap()[0].1;
                assert!((score - 1.0).abs() < 1e-14);
            }
            for exponent in [-80, 0, 80] {
                let scaled = query.map(|v| v * 2.0_f32.powi(exponent));
                assert_eq!(
                    index.search_filtered(&scaled, 5, |id| id <= 5).unwrap(),
                    results
                );
            }
        }
    }

    #[test]
    fn concurrent_publishers_share_one_complete_base_without_lifetime_locks() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("global");
        let vectors = vec![row(1, "a", &[1.0, 0.0]), row(2, "b", &[0.0, 1.0])];
        let barrier = std::sync::Barrier::new(4);
        let indexes = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|id| {
                    let global = &global;
                    let vectors = &vectors;
                    let barrier = &barrier;
                    let path = dir.path().join(format!("worktree-{id}"));
                    scope.spawn(move || {
                        barrier.wait();
                        SharedIndex::open(global, &path, &profile(), vectors, false).unwrap()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        for index in &indexes {
            assert_eq!(index.fingerprint(), indexes[0].fingerprint());
            assert_eq!(ids(index, &[1.0, 0.0]), [1, 2]);
        }
        let p = pointer(&dir.path().join("worktree-0"));
        let bases = fs::read_dir(global.join(&p.contract))
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".base.json")
            })
            .count();
        assert_eq!(bases, 1);
        // Kept-open indexes do not prevent publishing another base.
        SharedIndex::open(
            &global,
            &dir.path().join("disjoint"),
            &profile(),
            &[row(3, "new", &[-1.0, 0.0])],
            false,
        )
        .unwrap();
    }

    #[test]
    fn boundary_ties_use_occurrence_order_across_more_than_64_embeddings() {
        let dir = tempdir().unwrap();
        let rows: Vec<_> = (0..150)
            .map(|id| row(id, &format!("key-{id}"), &[1.0, 0.0]))
            .collect();
        let index = SharedIndex::open(
            &dir.path().join("global"),
            &dir.path().join("pointer"),
            &profile(),
            &rows,
            false,
        )
        .unwrap();
        assert_eq!(
            index.search_filtered(&[1.0, 0.0], 3, |_| true).unwrap(),
            [(0, 1.0), (1, 1.0), (2, 1.0)]
        );
        assert_eq!(
            index
                .search_filtered(&[1.0, 0.0], 2, |id| id >= 40)
                .unwrap(),
            [(40, 1.0), (41, 1.0)]
        );
    }
}
