//! Structure, symbol, and source presentation over committed records.

use super::discovery::*;
use super::*;

pub(super) struct SymbolIndex {
    pub(super) names: BTreeMap<u64, String>,
    pub(super) index: SharedIndex,
}

impl Engine {
    pub fn map(&self, options: &Value) -> Result<Vec<Value>> {
        let mut options = options.clone();
        options["limit"] = limits::resolve(&self.config, &options)?.value();
        crate::map::query(self, &self.root, &options)
    }

    pub(crate) fn map_prepared(
        &self,
        options: &Value,
        label_invocations: bool,
    ) -> Result<(crate::map::PreparedMap, Selection)> {
        let mut options = options.clone();
        options["limit"] = limits::resolve(&self.config, &options)?.value();
        crate::map::prepare(self, &self.root, &options, label_invocations)
    }

    /// Resolve name and description selectors against the current structural snapshot.
    /// SQLite's symbols and content-addressed embeddings are authoritative; the
    /// separate USearch vocabulary index is a disposable materialized view.
    pub(crate) fn selection(&self, options: &Value) -> Result<Selection> {
        let selection = Selection::compile(options)?;
        let queries = crate::filter::strings(options, "symbolQuery")?;
        if queries.is_empty() {
            return Ok(selection);
        }
        let vector = self.providers.symbol_vector()?;
        let profile = vector.profile();
        let queries: Vec<_> = queries
            .iter()
            .map(|query| symbols::normalize(query))
            .collect();
        let threshold = options["symbolThreshold"].as_f64().unwrap_or(0.5);
        // Map can resolve source or generated prose with the symbol provider,
        // even when the content indexes have never been prepared.
        let structures = self.symbol_structures()?;
        let mut descriptions = Vec::new();
        for (path, structure) in &structures {
            let stored = self.db.symbol_descriptions(path)?;
            for node in &structure.nodes {
                if let Some(text) = stored.get(&node.id).or(node.description.as_ref()) {
                    descriptions.push((path.clone(), Some(node.id), text.clone()));
                }
            }
            if let Some(text) = self
                .files
                .get(path)
                .and_then(|file| file.description.as_ref())
            {
                descriptions.push((path.clone(), None, text.clone()));
            }
        }
        descriptions.retain(|(_, _, text)| !symbols::normalize(text).is_empty());
        let cache_key = || -> Result<String> {
            Ok(hash(
                json!([
                    "symbol-selection-v2-descriptions",
                    self.db.meta("snapshot")?,
                    self.db.generation()?,
                    self.index_fingerprint("symbols"),
                    profile,
                    queries,
                    threshold,
                    descriptions
                ])
                .to_string(),
            ))
        };
        let key = cache_key()?;
        if let Some(cached) = self.db.search_cache(&key)? {
            let cached = cached
                .first()
                .context("Invalid cached semantic selection")?;
            return Ok(selection.with_semantic_matches(
                serde_json::from_value(cached["names"].clone())?,
                serde_json::from_value(cached["symbols"].clone())?,
                serde_json::from_value(cached["paths"].clone())?,
            ));
        }
        let symbols = self.symbol_index()?;
        self.ensure_embedding_groups_with(vector, vec![queries.clone()], true)?;
        self.ensure_embedding_groups_with(
            vector,
            vec![
                descriptions
                    .iter()
                    .map(|(_, _, text)| text.clone())
                    .collect(),
            ],
            false,
        )?;
        let description_vectors = descriptions
            .iter()
            .map(|(path, id, text)| {
                let embedding = self
                    .db
                    .embedding(&Database::embedding_key(&profile, false, text))?
                    .context("Missing selector description embedding")?;
                Ok((path, id, embedding))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut matched = HashSet::new();
        let mut matched_symbols = HashSet::new();
        let mut matched_paths = HashSet::new();
        for query in &queries {
            let query = self
                .db
                .embedding(&Database::embedding_key(&profile, true, query))?
                .context("Missing symbol query embedding")?;
            for (id, similarity) in
                symbols
                    .index
                    .search_filtered(&query, symbols.names.len(), |_| true)?
            {
                if similarity >= threshold {
                    matched.insert(symbols.names[&id].clone());
                }
            }
            for (path, id, description) in &description_vectors {
                if cosine(&query, description) >= threshold {
                    if let Some(id) = id {
                        matched_symbols.insert(((*path).clone(), *id));
                    } else {
                        matched_paths.insert((*path).clone());
                    }
                }
            }
        }
        if !self.readonly {
            self.try_write_index(|| {
                self.db.put_search_cache(
                    &cache_key()?,
                    &[json!({"names":matched, "symbols":matched_symbols, "paths":matched_paths})],
                )
            })?;
        }
        Ok(selection.with_semantic_matches(matched, matched_symbols, matched_paths))
    }

    fn symbol_structures(&self) -> Result<BTreeMap<String, Arc<parse::FileStructure>>> {
        self.preload_structures()?;
        self.db
            .paths()?
            .into_iter()
            .map(|path| {
                let structure = self.cached_structure(&path)?.with_context(|| {
                    format!("Structure for {path} requires refresh; run without --no-reindex")
                })?;
                Ok((path, structure))
            })
            .collect()
    }

    /// Fill the per-file structure cache with one bulk load. Whole-snapshot
    /// consumers (semantic selection, the call graph) would otherwise issue
    /// several statements per file. The index lock held for the engine's
    /// lifetime keeps the files table in step with the loaded snapshot.
    pub(super) fn preload_structures(&self) -> Result<()> {
        if self.structures.borrow().len() >= self.files.len() {
            return Ok(());
        }
        let mut all = self.db.all_structures(STRUCTURE_PARSER_VERSION)?;
        let mut cache = self.structures.borrow_mut();
        for path in self.files.keys() {
            cache
                .entry(path.clone())
                .or_insert_with(|| all.remove(path).map(Arc::new));
        }
        Ok(())
    }

    /// One vocabulary index per loaded snapshot: selectors and symbol results in
    /// the same command share it instead of reopening the shared base.
    fn symbol_index(&self) -> Result<Ref<'_, SymbolIndex>> {
        if self.symbol_index.borrow().is_none() {
            let index = self.open_symbol_index()?;
            *self.symbol_index.borrow_mut() = Some(index);
        }
        Ok(Ref::map(self.symbol_index.borrow(), |index| {
            index.as_ref().expect("initialized symbol index")
        }))
    }

    fn open_symbol_index(&self) -> Result<SymbolIndex> {
        let vector = self.providers.symbol_vector()?;
        let profile = vector.profile();
        let names: std::collections::BTreeSet<_> = self
            .symbol_structures()?
            .values()
            .flat_map(|structure| &structure.nodes)
            .flat_map(|node| std::iter::once(&node.name).chain(&node.names))
            .map(|name| symbols::normalize(name))
            .filter(|name| !name.is_empty())
            .collect();
        let names: Vec<_> = names.into_iter().collect();
        self.ensure_embedding_groups_with(vector, vec![names.clone()], false)?;
        // Name hashes keep unchanged graph entries stable when vocabulary order
        // changes. Reject collisions rather than silently dropping a name.
        let name_count = names.len();
        let names: BTreeMap<_, _> = names
            .into_iter()
            .map(|name| Ok((u64::from_str_radix(&hash(&name)[..16], 16)?, name)))
            .collect::<Result<_>>()?;
        ensure!(names.len() == name_count, "Symbol name hash collision");
        let vectors: Vec<_> = names
            .iter()
            .map(|(&id, name)| {
                let key = Database::embedding_key(&profile, false, name);
                Ok((
                    id,
                    key.clone(),
                    self.db
                        .embedding(&key)?
                        .context("Missing symbol embedding")?,
                ))
            })
            .collect::<Result<_>>()?;
        let mut path = self.db.path.as_os_str().to_os_string();
        path.push(".symbols.shared.json");
        let path = PathBuf::from(path);
        let mut profile = profile;
        profile["dimensions"] = json!(vector.dimensions());
        profile["indexChannel"] = json!("symbols");
        let prepared = SharedIndex::prepare(
            &self.global_indexes(),
            &path,
            &profile,
            &vectors,
            self.readonly,
        )?;
        if !self.readonly
            && (prepared.needs_publication()
                || self.db.meta("index_state_symbols")?.as_deref() != Some(prepared.fingerprint()))
        {
            self.try_write_index(|| {
                prepared.publish_pointer()?;
                self.db
                    .set_meta("index_state_symbols", prepared.fingerprint())
            })?;
        }
        let index = prepared.into_index();
        Ok(SymbolIndex { names, index })
    }

    pub(super) fn symbol_results(
        &self,
        query: &str,
        selection: &Selection,
        options: &Value,
    ) -> Result<Vec<Value>> {
        let query = symbols::normalize(query);
        ensure!(
            !query.is_empty(),
            "Symbol query must contain letters or numbers"
        );
        let symbols = self.symbol_index()?;
        let vector = self.providers.symbol_vector()?;
        let profile = vector.profile();
        self.ensure_embedding_groups_with(vector, vec![vec![query.clone()]], true)?;
        let query = self
            .db
            .embedding(&Database::embedding_key(&profile, true, &query))?
            .context("Missing symbol query embedding")?;
        let scores: HashMap<_, _> = symbols
            .index
            .search_filtered(&query, symbols.names.len(), |_| true)?
            .into_iter()
            .map(|(id, score)| (symbols.names[&id].clone(), score))
            .collect();
        let min = options["minSimilarity"].as_f64().unwrap_or(0.3);
        let max = options["maxSimilarity"].as_f64().unwrap_or(f64::INFINITY);
        let mut results = Vec::new();
        for (path, structure) in self.symbol_structures()? {
            if !selection.path_matches(&path) {
                continue;
            }
            let source = selection
                .has_regex()
                .then(|| self.source(&path))
                .transpose()?;
            for node in &structure.nodes {
                if !selection.kind_matches(&node.kind)
                    || !selection.symbol_matches_in(&path, node, source)
                {
                    continue;
                }
                let similarity = std::iter::once(&node.name)
                    .chain(&node.names)
                    .filter_map(|name| scores.get(&symbols::normalize(name)).copied())
                    .max_by(f64::total_cmp);
                if let Some(similarity) = similarity.filter(|score| *score >= min && *score < max) {
                    let mut symbol = serde_json::to_value(node)?;
                    symbol.as_object_mut().unwrap().remove("calls");
                    symbol["path"] = json!(path);
                    symbol["sourceMode"] = json!(self.files[&path].source_mode);
                    results.push(
                        json!({"type":"symbol", "symbol":symbol, "similarity":similarity,
                        "symbolSimilarity":similarity}),
                    );
                }
            }
        }
        Ok(results)
    }

    /// Search presentation uses the same indexed declarations as map. Older
    /// offline indexes can still return hits without current structure; callers
    /// can fall back to the search unit's declaration metadata in that case.
    pub fn presentation_structure(&self, path: &str) -> Result<Option<parse::FileStructure>> {
        Ok(self
            .cached_structure(path)?
            .map(|structure| (*structure).clone()))
    }

    /// Plan source edits against current indexed structure. CLI callers refresh
    /// first; library callers get an error if source bytes have become stale.
    pub fn plan_refactor(
        &self,
        source: &str,
        destination: &str,
        cwd: &Path,
    ) -> Result<crate::refactor::Plan> {
        let mut files = BTreeMap::new();
        for (path, file) in &self.files {
            if !crate::refactor::supported(path)
                || !crate::refactor::same_family(
                    source.split_once("::").map_or(source, |(path, _)| path),
                    path,
                )
            {
                continue;
            }
            ensure!(
                file.errors.is_empty(),
                "Cannot refactor with indexing errors in {path}"
            );
            let text = read_source(
                &self.root,
                path,
                self.config["maxFileSize"].as_u64().unwrap_or(1_048_576),
            )?;
            ensure!(
                hash(&text) == file.hash,
                "Stale source for {path}; refresh structure before refactoring"
            );
            let structure = self.presentation_structure(path)?.with_context(|| {
                format!(
                    "Missing current structure for {path}; refresh structure before refactoring"
                )
            })?;
            files.insert(path.clone(), (text, structure));
        }
        crate::refactor::plan(&self.root, cwd, files, source, destination)
    }

    /// Committed structure for one file, loaded at most once per snapshot load.
    pub(super) fn cached_structure(&self, path: &str) -> Result<Option<Arc<parse::FileStructure>>> {
        if let Some(cached) = self.structures.borrow().get(path) {
            return Ok(cached.clone());
        }
        let structure = match self.files.get(path) {
            Some(file)
                if self
                    .db
                    .structure_current(path, &file.hash, STRUCTURE_PARSER_VERSION)? =>
            {
                // Currency is verified above; skip the checked accessor's
                // second existence query.
                Some(Arc::new(self.db.read_structure(path)?))
            }
            _ => None,
        };
        self.structures
            .borrow_mut()
            .insert(path.to_owned(), structure.clone());
        Ok(structure)
    }

    pub fn call_graph(&self) -> Result<crate::callgraph::CallGraph> {
        ensure!(
            self.db
                .missing_structure_paths(STRUCTURE_PARSER_VERSION)?
                .is_empty(),
            "Call graph requires current structure; run without --no-reindex"
        );
        let key = hash(
            json!([
                "callgraph-v2",
                STRUCTURE_PARSER_VERSION,
                self.db.meta("snapshot")?
            ])
            .to_string(),
        );
        if let Some(cached) = self.db.cache("callgraph", &key)? {
            return Ok(serde_json::from_str(&cached)?);
        }
        // Build from the engine's (bulk-loaded) structures instead of
        // re-reading every file's structure from the database.
        let graph = crate::callgraph::CallGraph::from_structures(
            self.symbol_structures()?,
            crate::callgraph::CallGraph::indexed_keys(&self.db)?,
        );
        if !self.readonly {
            self.db
                .cache_put("callgraph", &key, &serde_json::to_string(&graph)?)?;
        }
        Ok(graph)
    }

    pub fn presentation_source(&self, path: &str) -> Option<&str> {
        self.source(path).ok()
    }

    pub(super) fn cached_line_totals(&self, path: &str) -> Result<crate::lines::Totals> {
        let file = self.files.get(path).context("Missing indexed file")?;
        let key = crate::lines::cache_key(
            &file.language,
            &file.hash,
            parse::line_exclusions::is_test_file(path),
        );
        if let Some(cached) = self.db.cache(crate::lines::CACHE_KIND, &key)? {
            return serde_json::from_str(&cached)
                .with_context(|| format!("Invalid cached line totals for {path}"));
        }
        if self.readonly {
            return Err(NeedsWrite.into());
        }
        let _lock = registry::artifact_lock(&self.db, crate::lines::CACHE_KIND, &key)?;
        if let Some(cached) = self.db.cache(crate::lines::CACHE_KIND, &key)? {
            return serde_json::from_str(&cached)
                .with_context(|| format!("Invalid cached line totals for {path}"));
        }
        let totals = crate::lines::Totals::compute(path, self.source(path)?)?;
        self.db.cache_put(
            crate::lines::CACHE_KIND,
            &key,
            &serde_json::to_string(&totals)?,
        )?;
        Ok(totals)
    }

    pub(super) fn source(&self, path: &str) -> Result<&str> {
        let file = self.files.get(path).context("Missing indexed file")?;
        let source = self
            .sources
            .get(path)
            .context("Missing indexed source binding")?;
        if source.get().is_none() {
            let _ = source.set(self.db.source(&file.hash)?);
        }
        Ok(source.get().expect("initialized source").as_str())
    }

    pub fn presentation_file_description(&self, path: &str) -> Option<&str> {
        self.files
            .get(path)
            .and_then(|file| file.description.as_deref())
    }

    pub fn presentation_symbol_descriptions(&self, path: &str) -> Result<HashMap<usize, String>> {
        self.db.symbol_descriptions(path)
    }

    pub(super) fn parsed(&self, path: &str, source: &str) -> Result<parse::ParsedFile> {
        let key = hash(
            json!([
                STRUCTURE_PARSER_VERSION,
                parse::language_for_path(path),
                hash(source)
            ])
            .to_string(),
        );
        if let Some(cached) = self.db.cache("parse", &key)? {
            return Ok(serde_json::from_str(&cached)?);
        }
        let _lock = registry::artifact_lock(&self.db, "parse", &key)?;
        if let Some(cached) = self.db.cache("parse", &key)? {
            return Ok(serde_json::from_str(&cached)?);
        }
        let parsed = parse::parse(path, source)?;
        self.db
            .cache_put("parse", &key, &serde_json::to_string(&parsed)?)?;
        Ok(parsed)
    }
}
