//! Semantic answer reuse, independent of the code-search snapshot generation.
use super::*;
use crate::storage::FaqEntry;

pub(super) const SYSTEM: &str = "Explain the existing code and documentation relevant to the user's task using only the supplied search context. Cite paths and symbols. Do not propose an implementation. If there is insufficient context, say so.";

pub(crate) fn threshold(config: &Value, options: &Value) -> Result<f64> {
    let value = options
        .get("faqThreshold")
        .or_else(|| config.get("faqThreshold"));
    match value {
        None => Ok(0.9),
        Some(value) => value
            .as_f64()
            .filter(|n| n.is_finite() && (-1.0..=1.0).contains(n))
            .context("faqThreshold must be a finite number between -1 and 1"),
    }
}

impl Engine {
    /// Answer a question, reusing the best valid question/answer match above the cutoff.
    pub fn faq(&self, query: &str, options: &Value) -> Result<Value> {
        let cutoff = threshold(&self.config, options)?;
        ensure!(!query.trim().is_empty(), "FAQ question must not be empty");
        ensure!(
            query.len() + 80 <= FAQ_PROMPT_BYTES,
            "FAQ question exceeds the prompt size limit"
        );
        // Validate context options even on a hit, before any provider requests.
        let expand_callers = options["expandCallers"].as_u64().unwrap_or(2) as usize;
        let expand_callees = options["expandCallees"].as_u64().unwrap_or(2) as usize;
        crate::map::reference_mode(
            options,
            (options["callers"].as_u64().unwrap_or(0) as usize).max(expand_callers),
            (options["callees"].as_u64().unwrap_or(0) as usize).max(expand_callees),
        )?;
        Selection::compile(options)?;
        ensure!(
            !self.readonly && !self.structural_only,
            "FAQ requires a writable search engine"
        );
        let mut context = options.clone();
        context
            .as_object_mut()
            .context("FAQ options must be an object")?
            .remove("faqThreshold");
        context["limit"] = limits::resolve(&self.config, options)?.value();
        let contract = hash(
            json!([
                "faq-v1",
                SYSTEM,
                STRUCTURE_PARSER_VERSION,
                self.providers.vector().profile(),
                self.description_generation_profile(),
                self.config["include"],
                self.config["exclude"],
                self.config["maxFileSize"],
                self.config["rerankingEnabled"],
                self.config["rerankerProvider"],
                self.config["rerankerModel"],
                self.config["rerankerBaseUrl"],
                self.config["rerankerCandidates"],
                context
            ])
            .to_string(),
        );
        let question_embedding = self.embed_one(query, true)?;
        let query_vector = self.vector(&question_embedding)?;
        // Retain stale entries as question anchors, never as authoritative answers.
        // A paraphrase can refresh the same question instead of changing the LLM request.
        let entries = self.db.faq_entries(&contract)?;
        ui::progress("Searching FAQ answers");
        let index = self.faq_index(&entries)?;
        // Both occurrences point at the same entry: the best of either embedding wins.
        let matched = index
            .search_filtered(&query_vector, 1, |_| true)?
            .first()
            .filter(|(_, similarity)| *similarity > cutoff)
            .map(|(id, _)| {
                entries
                    .iter()
                    .find(|entry| entry.id == id / 2)
                    .context("Missing FAQ entry")
            })
            .transpose()?;
        if let Some(entry) = matched
            && self.faq_files_current(&entry.files, &mut HashMap::new())
        {
            let mut result = entry.result.clone();
            result["query"] = json!(query);
            ui::progress("Reusing FAQ answer");
            return Ok(result);
        }

        let question = matched.map_or(query, |entry| entry.question.as_str());
        let question_embedding =
            matched.map_or(question_embedding, |entry| entry.question_embedding.clone());
        let (mut result, files) =
            self.answer_faq(question, options, matched.map(|entry| &entry.files))?;
        // A saved snapshot may deliberately differ from disk under --no-reindex.
        // It can still answer, but must never publish a stale semantic cache entry.
        if !files.is_empty() && self.faq_files_current(&files, &mut HashMap::new()) {
            let answer = result["description"]
                .as_str()
                .context("Missing FAQ answer")?;
            let answer_embedding = self.embed_one(answer, false)?;
            // Provider work may have overlapped an editor write; validate again.
            if self.faq_files_current(&files, &mut HashMap::new()) {
                let entry = FaqEntry {
                    id: 0,
                    question: question.into(),
                    result: result.clone(),
                    files,
                    question_embedding,
                    answer_embedding,
                };
                self.try_write_index(|| self.db.put_faq(&contract, &entry))?;
                self.faq_index(&self.db.faq_entries(&contract)?)?;
            }
        }
        result["query"] = json!(query);
        Ok(result)
    }

    pub(super) fn faq_files_current(
        &self,
        files: &BTreeMap<String, String>,
        checked: &mut HashMap<String, Option<String>>,
    ) -> bool {
        files.iter().all(|(path, expected)| {
            self.files
                .get(path)
                .is_some_and(|file| file.hash == *expected)
                && checked
                    .entry(path.clone())
                    .or_insert_with(|| {
                        let path = self.root.join(path);
                        let metadata = fs::symlink_metadata(&path).ok()?;
                        if !metadata.is_file() {
                            return None;
                        }
                        fs::read(path).ok().map(hash)
                    })
                    .as_ref()
                    == Some(expected)
        })
    }

    fn faq_index(&self, entries: &[FaqEntry]) -> Result<SharedIndex> {
        let mut vectors = Vec::with_capacity(entries.len() * 2);
        for entry in entries {
            let id = entry.id.checked_mul(2).context("FAQ vector id overflow")?;
            vectors.push((
                id,
                entry.question_embedding.clone(),
                self.vector(&entry.question_embedding)?,
            ));
            vectors.push((
                id.checked_add(1).context("FAQ vector id overflow")?,
                entry.answer_embedding.clone(),
                self.vector(&entry.answer_embedding)?,
            ));
        }
        let mut profile = self.providers.vector().profile();
        profile["dimensions"] = json!(self.providers.vector().dimensions());
        profile["indexChannel"] = json!("faq");
        let mut path = self.db.path.as_os_str().to_os_string();
        path.push(".faq.shared.json");
        let prepared = SharedIndex::prepare(
            &self.global_indexes(),
            &PathBuf::from(path),
            &profile,
            &vectors,
            false,
        )?;
        if prepared.needs_publication() {
            self.try_write_index(|| prepared.publish_pointer())?;
        }
        Ok(prepared.into_index())
    }
}
