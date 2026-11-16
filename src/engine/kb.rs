//! Knowledge-base retrieval uses ordinary documentation embeddings, not a side index.
use super::*;

impl Engine {
    /// Rank Markdown articles, ignoring those below the freshness cutoff.
    /// A miss returns an empty array; it never invokes the agent or changes Git.
    pub fn kb_query(&self, query: &str, options: &Value) -> Result<Vec<Value>> {
        self.reporting(|| {
            ensure!(!query.trim().is_empty(), "KB query must not be empty");
            let freshness =
                crate::kb::fraction(self.config.raw(), options, "kbQueryFreshness", 0.8)?;
            let threshold = options
                .get("kbThreshold")
                .or_else(|| self.config.raw().get("kbThreshold"))
                .map_or(Some(0.7), Value::as_f64)
                .filter(|n| n.is_finite() && (-1.0..=1.0).contains(n))
                .context("kbThreshold must be a finite number between -1 and 1")?;
            let limit = options
                .get("limit")
                .map(limits::ResultLimit::from_value)
                .transpose()?
                .unwrap_or(limits::ResultLimit::Count(1));
            let articles = crate::kb::articles(&self.root, self.config.raw())?;
            let mut candidates = BTreeMap::new();
            for article in articles {
                if !self
                    .files
                    .get(&article.path)
                    .is_some_and(|file| file.hash == hash(&article.markdown))
                {
                    continue;
                }
                let result = article.result(&self.root)?;
                if result["freshness"].as_f64().unwrap() >= freshness {
                    candidates.insert(article.path, result);
                }
            }
            if candidates.is_empty() {
                return Ok(Vec::new());
            }
            // Score the normal saved docs embeddings directly. Unlike a general
            // search, KB matching must not invoke a configured agent reranker or
            // lose eligible articles behind that reranker's candidate cap.
            let paths = candidates
                .keys()
                .map(|path| regex::escape(path))
                .collect::<Vec<_>>()
                .join("|");
            let search = json!({"query":query,"limit":"none","minSimilarity":threshold,
                "includeVectors":false,"regexp":format!("^(?:{paths})$")});
            let matches = self.index_entries_limited("docs", &search)?;
            let mut results: BTreeMap<String, Value> = BTreeMap::new();
            for row in matches.rows {
                let path = row["path"].as_str().unwrap_or("");
                let similarity = score(&row, "similarity");
                if similarity <= threshold {
                    continue;
                }
                if let Some(article) = candidates.get(path) {
                    let mut result = article.clone();
                    result["query"] = json!(query);
                    result["score"] = json!(similarity);
                    result["rankScore"] =
                        json!(0.9 * similarity + 0.1 * score(article, "freshness"));
                    let existing = results
                        .entry(path.to_owned())
                        .or_insert_with(|| result.clone());
                    if score(&result, "rankScore") > score(existing, "rankScore") {
                        *existing = result;
                    }
                }
            }
            let mut results: Vec<_> = results.into_values().collect();
            results.sort_by(|a, b| {
                score(b, "rankScore")
                    .total_cmp(&score(a, "rankScore"))
                    .then_with(|| a["path"].as_str().cmp(&b["path"].as_str()))
            });
            results.truncate(limit.count().unwrap_or(usize::MAX));
            Ok(results)
        })
    }
}
