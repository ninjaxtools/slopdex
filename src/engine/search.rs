//! Vector search over committed index records.

use super::*;

struct SearchPlan {
    groups: Vec<FormatGroup>,
    descriptions: bool,
    symbols: bool,
}

impl SearchPlan {
    fn compile(kind: &str, options: &Value, selection: &Selection) -> Self {
        let explicit = ["code", "descriptions", "docs", "md", "symbols"]
            .iter()
            .any(|channel| flag(options, channel));
        let selected = |channel| kind == "search" && (!explicit || flag(options, channel));
        let groups = FormatGroup::ALL
            .into_iter()
            .filter(|group| {
                selection.includes_group(*group)
                    && match group {
                        FormatGroup::Code => kind == "search-code" || selected("code"),
                        FormatGroup::Docs => {
                            matches!(kind, "search-docs" | "search-md")
                                || selected("docs")
                                || selected("md")
                        }
                        FormatGroup::Config | FormatGroup::Markup => kind == "search" && !explicit,
                    }
            })
            .collect();
        Self {
            groups,
            descriptions: kind == "search-descriptions" || selected("descriptions"),
            symbols: kind == "search-symbols" || selected("symbols"),
        }
    }

    fn roles(&self) -> impl Iterator<Item = &str> {
        self.groups
            .iter()
            .map(|group| group.as_str())
            .chain(self.descriptions.then_some("descriptions"))
            .chain(self.symbols.then_some("symbols"))
    }
}

type DescriptionQueries<'a> = (Option<&'a [f32]>, Option<&'a [f32]>);

impl Engine {
    pub(super) fn vector(&self, key: &str) -> Result<Vec<f32>> {
        if let Some(vector) = self.vectors.borrow().get(key) {
            return Ok(vector.clone());
        }
        let vector = self
            .db
            .embedding(key)?
            .context("Missing indexed embedding")?;
        self.vectors.borrow_mut().insert(key.into(), vector.clone());
        Ok(vector)
    }

    pub(super) fn vectors(
        &self,
        matches: impl Fn(&str) -> bool,
    ) -> Result<HashMap<String, Vec<f32>>> {
        let keys: HashSet<_> = self
            .items
            .iter()
            .filter(|item| matches(&item.path))
            .flat_map(|i| {
                std::iter::once(i.embedding.clone()).chain(i.description_embedding.clone())
            })
            .chain(
                self.files
                    .values()
                    .filter(|file| matches(&file.path))
                    .filter_map(|f| f.description_embedding.clone()),
            )
            .filter(|key| !key.is_empty())
            .collect();
        let loading = ui::counted("Loading embeddings", keys.len());
        let mut vectors = HashMap::new();
        for key in keys {
            vectors.insert(key.clone(), self.vector(&key)?);
            loading.inc(1);
        }
        loading.finish();
        Ok(vectors)
    }

    fn ensure_index(&self, kind: &str) -> Result<()> {
        if self.indexes.borrow().contains_key(kind) {
            return Ok(());
        }
        let items: Vec<_> = self
            .items
            .iter()
            .filter(|i| {
                if kind == "descriptions" {
                    i.data["description"].is_string() && i.description_embedding.is_some()
                } else {
                    content_group(i).is_some_and(|group| group.as_str() == kind)
                        && !i.embedding.is_empty()
                }
            })
            .collect();
        let preparing = ui::counted(format_args!("Preparing {kind} vectors"), items.len());
        let mut vectors = Vec::with_capacity(items.len());
        for item in items {
            let key = if kind == "descriptions" {
                item.description_embedding
                    .as_ref()
                    .context("Missing description")?
            } else {
                &item.embedding
            };
            vectors.push((item.id, key.clone(), self.item_vector(item, kind)?));
            preparing.inc(1);
        }
        preparing.finish();
        let mut path = self.db.path.as_os_str().to_os_string();
        path.push(format!(".{kind}.shared.json"));
        ui::progress(format_args!("Loading {kind} vector index"));
        let mut profile = self.providers.vector().profile();
        profile["dimensions"] = json!(self.providers.vector().dimensions());
        profile["indexChannel"] = json!(if kind == "descriptions" {
            "descriptions"
        } else {
            "content"
        });
        if let Ok(group) = FormatGroup::from_name(kind) {
            profile["formatGroup"] = json!(group);
        }
        let prepared = SharedIndex::prepare(
            &self.global_indexes(),
            &PathBuf::from(path),
            &profile,
            &vectors,
            self.readonly,
        )?;
        let state = format!("index_state_{kind}");
        if !self.readonly
            && (prepared.needs_publication()
                || self.db.meta(&state)?.as_deref() != Some(prepared.fingerprint()))
        {
            self.try_write_index(|| {
                prepared.publish_pointer()?;
                self.db.set_meta(&state, prepared.fingerprint())
            })?;
        }
        let index = prepared.into_index();
        self.indexes.borrow_mut().insert(kind.into(), index);
        Ok(())
    }

    pub(super) fn global_indexes(&self) -> PathBuf {
        let mut path = self.db.global_path().as_os_str().to_os_string();
        path.push(".indexes");
        PathBuf::from(path)
    }

    pub(super) fn item_vector(&self, item: &Item, kind: &str) -> Result<Vec<f32>> {
        let key = if kind == "descriptions" {
            item.description_embedding
                .as_ref()
                .context("Missing description")?
        } else {
            &item.embedding
        };
        self.vector(key)
    }

    pub fn search(&self, query: &str, kind: &str, options: &Value) -> Result<Vec<Value>> {
        Ok(self.search_limited(query, kind, options)?.rows)
    }

    /// Search the union of queries, retaining each match's best-ranked result.
    pub fn search_queries(
        &self,
        queries: &[String],
        kind: &str,
        options: &Value,
    ) -> Result<Vec<Value>> {
        Ok(self.search_queries_limited(queries, kind, options)?.rows)
    }

    pub(crate) fn search_queries_limited(
        &self,
        queries: &[String],
        kind: &str,
        options: &Value,
    ) -> Result<LimitedResults<Value>> {
        let queries: BTreeSet<_> = queries.iter().collect();
        if queries.len() == 1 {
            return self.search_limited(queries.first().unwrap(), kind, options);
        }
        let limit = limits::resolve(&self.config, options)?;
        let ranking_score = if flag(&self.config, "rerankingEnabled") {
            "rerankScore"
        } else {
            "similarity"
        };
        let order = |a: &Value, b: &Value| {
            score(b, ranking_score)
                .total_cmp(&score(a, ranking_score))
                .then_with(|| a.to_string().cmp(&b.to_string()))
        };
        // Scores belong to the query, not the match. Include full item data so
        // names shared by locations or streams stay distinct.
        let identity = |row: &Value| {
            json!([
                row["type"],
                row["function"],
                row["symbol"],
                row["chunk"],
                row["file"]
            ])
            .to_string()
        };
        let mut matches: BTreeMap<String, Value> = BTreeMap::new();
        let mut capped = false;
        for query in queries {
            let mut query_options = options.clone();
            query_options["limit"] = limit.value();
            let rows = loop {
                // A query's best count + 1 distinct matches suffice for both the
                // global top count and omission detection, even if queries overlap.
                // Keep using search_rows so individual queries reuse their caches.
                let mut rows = self.search_rows(query, kind, &query_options)?;
                capped |= take_omission_marker(&mut rows);
                let Some(count) = limit.count() else {
                    break rows;
                };
                let probe = query_options["limit"].as_u64().unwrap() as usize;
                let identities: HashSet<_> = rows.iter().map(&identity).collect();
                if identities.len() > count || rows.len() <= probe {
                    break rows;
                }
                // Duplicate streams can consume the spare candidate. Widen only
                // in that case, rather than misreporting an exactly full union.
                let wider = probe.saturating_mul(2);
                if wider == probe {
                    break rows;
                }
                query_options["limit"] = json!(wider);
            };
            for row in rows {
                matches
                    .entry(identity(&row))
                    .and_modify(|existing| {
                        if order(&row, existing).is_lt() {
                            *existing = row.clone();
                        }
                    })
                    .or_insert(row);
            }
        }
        let mut rows: Vec<_> = matches.into_values().collect();
        rows.sort_by(order);
        let omitted = limit
            .count()
            .is_some_and(|count| capped || rows.len() > count);
        if let Some(count) = limit.count() {
            rows.truncate(count);
        }
        Ok(LimitedResults {
            rows,
            omitted,
            limit: limit.count(),
        })
    }

    pub(crate) fn search_limited(
        &self,
        query: &str,
        kind: &str,
        options: &Value,
    ) -> Result<LimitedResults<Value>> {
        let mut options = options.clone();
        let limit = limits::resolve(&self.config, &options)?;
        options["limit"] = limit.value();
        let mut rows = self.search_rows(query, kind, &options)?;
        let capped = take_omission_marker(&mut rows);
        let omitted = limit
            .count()
            .is_some_and(|count| capped || rows.len() > count);
        if let Some(count) = limit.count() {
            rows.truncate(count);
        }
        Ok(LimitedResults {
            rows,
            omitted,
            limit: limit.count(),
        })
    }

    /// Ranked rows plus at most one probe beyond the output limit. When a
    /// reranker's candidate cap hides further candidates that the row count
    /// cannot reveal, the last row is an omission marker instead; callers must
    /// remove it with `take_omission_marker`.
    fn search_rows(&self, query: &str, kind: &str, options: &Value) -> Result<Vec<Value>> {
        let formats = Selection::compile(options)?;
        let SearchPlan {
            groups,
            descriptions,
            symbols,
        } = SearchPlan::compile(kind, options, &formats);
        let content = !groups.is_empty() || descriptions;
        if !content && !symbols {
            return Ok(Vec::new());
        }
        ensure!(
            (!content || !self.structural_only)
                && self.items.iter().all(|item| {
                    content_group(item).is_none_or(|group| !groups.contains(&group))
                        || !item.embedding.is_empty()
                })
                && (!descriptions
                    || (self
                        .items
                        .iter()
                        .filter(|item| formats.format_matches(&item.path))
                        .all(|item| !item.data["description"].is_string()
                            || item.description_embedding.is_some())
                        && self
                            .files
                            .values()
                            .filter(|file| formats.format_matches(&file.path))
                            .all(|file| file.description.is_none()
                                || file.description_embedding.is_some()))),
            "Semantic index is incomplete for the configured profiles; run without --no-reindex to prepare it"
        );
        let key = self.query_key(query, kind, options)?;
        if let Some(results) = self.db.search_cache(&key)? {
            return Ok(results);
        }
        if self.readonly {
            return Err(NeedsWrite.into());
        }
        let selection = self.selection(options)?;
        let vector = if content {
            let embedding_key = self.embed_one(query, true)?;
            self.db
                .embedding(&embedding_key)?
                .context("Missing query vector")?
        } else {
            Vec::new()
        };
        let rerank = flag(&self.config, "rerankingEnabled");
        let limit = options["limit"].as_u64().map(|v| v as usize);
        let candidate_limit = if rerank {
            if let Some(maximum) = self.providers.reranker()?.candidate_limit() {
                Some(
                    limit
                        .unwrap_or(maximum)
                        .max(self.config["rerankerCandidates"].as_u64().unwrap_or(10) as usize)
                        .min(maximum),
                )
            } else {
                limit.map(|l| l.saturating_mul(5))
            }
        } else {
            // Retrieve one extra threshold-passing hit to distinguish actual
            // omission from exactly filling the requested output limit.
            limit.map(|count| count.saturating_add(1))
        };
        // A reranker only ever sees `candidate_limit` documents. Retrieve one
        // more, never sent to it, to learn whether that cap hid any candidates.
        let fetch_limit = if rerank {
            candidate_limit.map(|count| count.saturating_add(1))
        } else {
            candidate_limit
        };
        let mut results = Vec::new();
        let searching = ui::counted(
            "Searching vector indexes",
            groups.len() + usize::from(descriptions) + usize::from(symbols),
        );
        let mut item_results: HashMap<u64, Value> = HashMap::new();
        for index_kind in groups
            .iter()
            .map(|group| group.as_str())
            .chain(descriptions.then_some("descriptions"))
        {
            ui::progress(format_args!("Searching {index_kind} index"));
            let mut allowed = HashSet::new();
            for i in &self.items {
                let in_channel = if index_kind == "descriptions" {
                    i.data["description"].is_string() && i.description_embedding.is_some()
                } else {
                    content_group(i).is_some_and(|group| group.as_str() == index_kind)
                };
                // Structure is only consulted for units whose path is selected.
                if in_channel
                    && selection.path_matches(&i.path)
                    && selection.unit_matches(
                        &i.path,
                        &i.data,
                        self.cached_structure(&i.path)?.as_deref(),
                        (selection.has_regex()
                            && matches!(i.kind.as_str(), "function" | "symbol-description"))
                        .then(|| self.source(&i.path))
                        .transpose()?,
                    )
                {
                    allowed.insert(i.id);
                }
            }
            for (id, similarity) in
                self.neighbors(index_kind, &vector, &allowed, fetch_limit, options)?
            {
                let item = self.item(id)?;
                let mut result = if item.kind == "symbol-description" {
                    let mut symbol = item.data.clone();
                    symbol.as_object_mut().unwrap().remove("calls");
                    json!({"type":"symbol","symbol":symbol,"similarity":similarity})
                } else if item.kind == "function" {
                    json!({"type":"function","function":item.data,"similarity":similarity})
                } else {
                    json!({"type":item.kind,"chunk":item.data,"similarity":similarity})
                };
                if matches!(item.kind.as_str(), "function" | "symbol-description") {
                    self.add_scores(&mut result, item, &vector, None)?;
                }
                item_results
                    .entry(id)
                    .and_modify(|existing| {
                        if similarity > score(existing, "similarity") {
                            *existing = result.clone();
                        }
                    })
                    .or_insert(result);
            }
            searching.inc(1);
        }
        results.extend(item_results.into_values());
        if descriptions {
            // Files are independent description hits, including files with no
            // callable units. Direct comparison avoids synthetic item IDs.
            let min = options["minSimilarity"].as_f64().unwrap_or(0.3);
            let max = options["maxSimilarity"].as_f64().unwrap_or(f64::INFINITY);
            for file in self
                .files
                .values()
                .filter(|file| selection.file_matches(&file.path))
            {
                let Some(key) = file.description_embedding.as_ref() else {
                    continue;
                };
                let similarity = cosine(&vector, &self.vector(key)?);
                if similarity >= min && similarity < max {
                    results.push(json!({"type":"file", "file":{
                        "path":file.path, "description":file.description,
                        "sourceMode":file.source_mode, "language":file.language
                    }, "similarity":similarity, "fileDescriptionSimilarity":similarity}));
                }
            }
        }
        if symbols {
            ui::progress("Searching symbol index");
            results.extend(self.symbol_results(query, &selection, options)?);
            searching.inc(1);
        }
        searching.finish();
        results.sort_by(|a, b| {
            score(b, "similarity")
                .total_cmp(&score(a, "similarity"))
                .then_with(|| a.to_string().cmp(&b.to_string()))
        });
        let mut capped = false;
        if let Some(limit) = candidate_limit {
            capped = rerank && results.len() > limit;
            results.truncate(limit);
        }
        if rerank && !results.is_empty() {
            let documents: Vec<_> = results
                .iter()
                .map(|row| {
                    if row["type"] == "symbol" {
                        json!({"name": row["symbol"]["name"], "names": row["symbol"]["names"],
                            "description": row["symbol"]["description"]})
                        .to_string()
                    } else {
                        row.to_string()
                    }
                })
                .collect();
            let reranking = ui::counted("Reranking candidates", documents.len());
            let ranking_key = hash(
                json!([
                    "rerank-v1",
                    self.config["rerankerProvider"],
                    self.config["rerankerModel"],
                    self.config["rerankerBaseUrl"],
                    query,
                    documents
                ])
                .to_string(),
            );
            self.artifacts
                .hydrate_text(&self.db, "rerank", std::slice::from_ref(&ranking_key))?;
            let _lock = registry::artifact_lock(&self.db, "rerank", &ranking_key)?;
            let cached: Option<Vec<(usize, f64)>> = self
                .db
                .cache("rerank", &ranking_key)?
                .and_then(|text| serde_json::from_str(&text).ok())
                .filter(|rank: &Vec<(usize, f64)>| {
                    rank.len() == results.len()
                        && rank
                            .iter()
                            .all(|(id, score)| *id < results.len() && score.is_finite())
                        && rank.iter().map(|(id, _)| id).collect::<HashSet<_>>().len() == rank.len()
                });
            let ranking: Vec<(usize, f64)> = if let Some(cached) = cached {
                cached
            } else {
                let ranking = self.providers.reranker()?.rerank(query, &documents)?;
                let text = serde_json::to_string(&ranking)?;
                self.db.cache_put("rerank", &ranking_key, &text)?;
                self.artifacts
                    .put_text(&self.db, "rerank", &ranking_key, &text);
                ranking
            };
            results = ranking
                .into_iter()
                .map(|(index, score)| {
                    let mut row = results[index].clone();
                    row["rerankScore"] = json!(score);
                    row
                })
                .collect();
            reranking.inc(documents.len());
            reranking.finish();
        }
        if let Some(limit) = limit {
            results.truncate(limit.saturating_add(1));
            // Rows beyond the limit already prove omission. Otherwise record
            // that candidates beyond the reranker's cap were never ranked.
            if capped && results.len() <= limit {
                results.push(json!({"type": OMISSION_MARKER}));
            }
        }
        self.try_write_index(|| {
            self.db
                .put_search_cache(&self.query_key(query, kind, options)?, &results)
        })?;
        Ok(results)
    }

    fn query_key(&self, query: &str, kind: &str, options: &Value) -> Result<String> {
        let mut config = self.config.clone();
        if let Some(config) = config.as_object_mut() {
            config.remove("noReindex");
            config.remove("forceReindex");
        }
        let formats = Selection::compile(options)?;
        let plan = SearchPlan::compile(kind, options, &formats);
        let selector_symbols =
            !plan.symbols && !crate::filter::strings(options, "symbolQuery")?.is_empty();
        let roles: Vec<_> = plan
            .roles()
            .chain(selector_symbols.then_some("symbols"))
            .map(|role| (role, self.index_fingerprint(role)))
            .collect();
        Ok(hash(
            json!([
                "query-v10-callable-source-regex",
                crate::formats::VERSION,
                self.db.meta("snapshot")?,
                self.db.generation()?,
                config,
                self.providers.vector().profile(),
                kind,
                query,
                options,
                roles
            ])
            .to_string(),
        ))
    }

    pub(super) fn index_fingerprint(&self, role: &str) -> Option<String> {
        // A query can use a newly prepared plan without publishing its pointer
        // when other readers are active. Cache its actual plan, not the old one.
        if role == "symbols"
            && let Some(symbols) = self.symbol_index.borrow().as_ref()
        {
            return Some(symbols.index.fingerprint().to_owned());
        }
        if let Some(index) = self.indexes.borrow().get(role) {
            return Some(index.fingerprint().to_owned());
        }
        // The authoritative snapshot pins its plan independently of disposable
        // pointer/base files. A cached answer needs neither loading nor repair.
        self.db.meta(&format!("index_state_{role}")).ok().flatten()
    }

    fn neighbors(
        &self,
        kind: &str,
        query: &[f32],
        allowed: &HashSet<u64>,
        limit: Option<usize>,
        options: &Value,
    ) -> Result<Vec<(u64, f64)>> {
        self.filtered_neighbors(
            kind,
            query,
            allowed.len(),
            |key| allowed.contains(&key),
            limit,
            options,
        )
    }

    pub(super) fn filtered_neighbors(
        &self,
        kind: &str,
        query: &[f32],
        eligible_count: usize,
        allowed: impl Fn(u64) -> bool,
        limit: Option<usize>,
        options: &Value,
    ) -> Result<Vec<(u64, f64)>> {
        self.ensure_index(kind)?;
        let indexes = self.indexes.borrow();
        let index = indexes.get(kind).context("Search index unavailable")?;
        let wanted = limit.unwrap_or(eligible_count).min(eligible_count);
        if wanted == 0 {
            return Ok(Vec::new());
        }
        let min = options["minSimilarity"].as_f64().unwrap_or(0.3);
        let max = options["maxSimilarity"].as_f64().unwrap_or(f64::INFINITY);
        let mut width = wanted;
        loop {
            let found = index.search_filtered(query, width, &allowed)?;
            let total = found.len();
            let below_floor = found.last().is_some_and(|(_, score)| *score < min);
            let mut selected: Vec<_> = found
                .into_iter()
                .filter(|(_, s)| *s >= min && *s < max)
                .collect();
            if selected.len() >= wanted || width >= eligible_count || total < width || below_floor {
                selected.truncate(wanted);
                return Ok(selected);
            }
            width = width.saturating_mul(2).min(eligible_count);
        }
    }

    pub(super) fn item(&self, id: u64) -> Result<&Item> {
        self.items
            .binary_search_by_key(&id, |i| i.id)
            .map(|index| &self.items[index])
            .map_err(|_| anyhow::anyhow!("USearch returned an unknown key {id}"))
    }

    pub(super) fn add_scores(
        &self,
        result: &mut Value,
        item: &Item,
        code: &[f32],
        other: Option<DescriptionQueries<'_>>,
    ) -> Result<()> {
        if !item.embedding.is_empty() {
            result["codeSimilarity"] = json!(cosine(code, &self.vector(&item.embedding)?));
        }
        let (description, file) = other.unwrap_or((Some(code), Some(code)));
        if let Some((query, key)) = description.zip(item.description_embedding.as_ref()) {
            let similarity = cosine(query, &self.vector(key)?);
            result["descriptionSimilarity"] = json!(similarity);
            if item.kind == "function" {
                result["functionDescriptionSimilarity"] = json!(similarity);
            }
        }
        if let Some((query, key)) = file.zip(
            self.files
                .get(&item.path)
                .and_then(|file| file.description_embedding.as_ref()),
        ) {
            result["fileDescriptionSimilarity"] = json!(cosine(query, &self.vector(key)?));
        }
        Ok(())
    }
}

pub(super) const OMISSION_MARKER: &str = "omitted-candidates";

/// Remove the marker `search_rows` appends when a reranker's candidate cap hid
/// candidates that the number of returned rows cannot reveal.
pub(super) fn take_omission_marker(rows: &mut Vec<Value>) -> bool {
    let marked = rows
        .last()
        .is_some_and(|row| row["type"] == OMISSION_MARKER);
    if marked {
        rows.pop();
    }
    marked
}
