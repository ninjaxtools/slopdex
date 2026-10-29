//! Cross-index function comparison.

use super::discovery::*;
use super::*;

impl Engine {
    pub fn cross_search(&self, target: Option<&Engine>, options: &Value) -> Result<Vec<Value>> {
        // Cross-search needs a more selective floor than natural-language
        // queries: weak edges can otherwise join every connected component.
        // Include the effective default in the cache key as well as filtering.
        let mut options = options.clone();
        if options["minSimilarity"].as_f64().is_none() {
            options["minSimilarity"] = json!(0.8);
        }
        let options = &options;
        let target = target.unwrap_or(self);
        if !Selection::compile(options)?.includes_group(FormatGroup::Code) {
            return Ok(Vec::new());
        }
        let mut exclusions = cross_search_exclusions(&self.config)?;
        exclusions.extend(cross_search_exclusions(&target.config)?);
        ensure!(
            !self.structural_only
                && !target.structural_only
                && [self, target]
                    .iter()
                    .all(|engine| engine.items.iter().all(|item| {
                        content_group(item) != Some(FormatGroup::Code) || !item.embedding.is_empty()
                    })),
            "Semantic index is incomplete for the configured profiles; run without --no-reindex to prepare it"
        );
        ensure!(
            self.providers.vector().profile() == target.providers.vector().profile(),
            "Cross-search requires identical embedding profiles"
        );
        let same = self.db.path.canonicalize()? == target.db.path.canonicalize()?;
        let kind = "code";
        let base = options["changedSince"]
            .as_str()
            .map(|reference| {
                git::resolve_base(
                    &self.root,
                    reference,
                    self.db.meta("checkpoint")?.as_deref(),
                )
            })
            .transpose()?;
        let cache_key = || -> Result<String> {
            Ok(hash(
                json!([
                    "cross-v8-callable-source-regex",
                    crate::formats::VERSION,
                    self.db.meta("snapshot")?,
                    self.db.generation()?,
                    target.db.path.canonicalize()?,
                    target.db.meta("incarnation")?,
                    target.db.meta("snapshot")?,
                    target.db.generation()?,
                    target.index_fingerprint("code"),
                    self.providers.vector().profile(),
                    kind,
                    base,
                    self.db.meta("checkpoint")?,
                    exclusions,
                    options
                ])
                .to_string(),
            ))
        };
        let key = cache_key()?;
        if let Some(results) = self.db.search_cache(&key)? {
            return Ok(results);
        }
        let selection = self.selection(options)?;
        let stored_vectors =
            self.vectors(|path| FormatGroup::for_path(path) == Some(FormatGroup::Code))?;
        let min_lines = options["minLines"].as_u64().unwrap_or(2);
        let max_lines = options["maxLines"].as_u64();
        let lines_match = |item: &Item| {
            let lines = item.data["lineCount"].as_u64().unwrap_or(0);
            lines >= min_lines && max_lines.is_none_or(|max| lines < max)
        };
        let excluded = |item: &Item| {
            item.data["sourceHash"]
                .as_str()
                .is_some_and(|hash| exclusions.contains(hash))
        };
        let source_path = options["sourcePath"]
            .as_str()
            .map(|path| self.normalize_path(path))
            .transpose()?;
        let changed = base
            .as_deref()
            .map(|reference| git::changed_identities(&self.root, reference, &self.items))
            .transpose()?;
        let mut seen = HashSet::new();
        let mut results = Vec::new();
        let eligible: HashMap<_, _> = target
            .items
            .iter()
            .filter(|i| {
                content_group(i) == Some(FormatGroup::Code)
                    && lines_match(i)
                    && !excluded(i)
                    && selection.format_matches(&i.path)
            })
            .map(|i| (i.id, target.root.join(&i.path)))
            .collect();
        let mut sources = Vec::new();
        for source in &self.items {
            // Structure is only consulted for units whose path is selected.
            if content_group(source) == Some(FormatGroup::Code)
                && lines_match(source)
                && !excluded(source)
                && selection.path_matches(&source.path)
                && selection.unit_matches(
                    &source.path,
                    &source.data,
                    self.cached_structure(&source.path)?.as_deref(),
                    selection
                        .has_regex()
                        .then(|| self.source(&source.path))
                        .transpose()?,
                )
                && source_path.as_ref().is_none_or(|p| under(&source.path, p))
                && (!flag(options, "uncommitted") || source.data["sourceMode"] == "working-tree")
                && changed.as_ref().is_none_or(|ids| ids.contains(&source.id))
            {
                sources.push(source);
            }
        }
        let searching = ui::counted("Searching source functions", sources.len());
        for source in sources {
            let source_file = self.root.join(&source.path);
            let cross_file = flag(options, "crossFileOnly");
            let allowed = |id| {
                eligible.get(&id).is_some_and(|path| {
                    !(same && id == source.id || cross_file && *path == source_file)
                })
            };
            let query = self.item_vector(source, kind)?;
            let neighbors = target.filtered_neighbors(
                kind,
                &query,
                eligible.len(),
                allowed,
                Some(options["matches"].as_u64().unwrap_or(5) as usize),
                options,
            )?;
            let mut matches = Vec::new();
            for (id, similarity) in neighbors {
                if same
                    && !flag(options, "includeSymmetricDuplicates")
                    && !seen.insert((source.id.min(id), source.id.max(id)))
                {
                    continue;
                }
                let item = target.item(id)?;
                let mut row = json!({"function":item.data,"similarity":similarity});
                let other = Some((
                    source
                        .description_embedding
                        .as_ref()
                        .and_then(|key| stored_vectors.get(key))
                        .map(Vec::as_slice),
                    self.files
                        .get(&source.path)
                        .and_then(|file| file.description_embedding.as_ref())
                        .and_then(|key| stored_vectors.get(key))
                        .map(Vec::as_slice),
                ));
                target.add_scores(&mut row, item, &stored_vectors[&source.embedding], other)?;
                if flag(options, "cohesion") {
                    row["physicalDistance"] = json!(distance(
                        &self.root.join(&source.path),
                        &target.root.join(&item.path)
                    ));
                }
                matches.push(row);
            }
            if flag(options, "cohesion") {
                matches.sort_by(|a, b| {
                    b["physicalDistance"]
                        .as_u64()
                        .cmp(&a["physicalDistance"].as_u64())
                        .then_with(|| score(b, "similarity").total_cmp(&score(a, "similarity")))
                });
            }
            if !matches.is_empty() {
                results.push(json!({"source":source.data,"matches":matches,"scoring":{
                    "similarityMode":"code", "similarityWeights":{"code":1,"description":0,"fileDescription":0}
                }}));
            }
            searching.inc(1);
        }
        searching.finish();
        self.try_write_index(|| self.db.put_search_cache(&cache_key()?, &results))?;
        Ok(results)
    }
}

pub(crate) fn cross_search_exclusions(config: &Value) -> Result<BTreeSet<&str>> {
    let Some(value) = config.get("crossSearchExclusions") else {
        return Ok(BTreeSet::new());
    };
    let values = value
        .as_array()
        .context("crossSearchExclusions must be an array of SHA-256 hashes")?;
    values
        .iter()
        .map(|value| {
            let hash = value
                .as_str()
                .context("crossSearchExclusions must contain SHA-256 hash strings")?;
            ensure!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "crossSearchExclusions must contain lowercase SHA-256 hashes"
            );
            Ok(hash)
        })
        .collect()
}

fn distance(left: &Path, right: &Path) -> usize {
    if left == right {
        return 0;
    }
    let a: Vec<_> = left.parent().unwrap_or(left).components().collect();
    let b: Vec<_> = right.parent().unwrap_or(right).components().collect();
    let common = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    1 + a.len() + b.len() - 2 * common
}
