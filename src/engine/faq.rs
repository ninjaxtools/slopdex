//! Semantic answer reuse, independent of the code-search snapshot generation.
use super::*;
use crate::storage::FaqEntry;

pub(super) const SYSTEM: &str = "Explain the existing code and documentation relevant to the user's task using only the supplied search context. Do not propose an implementation. If there is insufficient context, say so in the description. Respond with a single JSON object and no other text, in exactly this shape: {\"description\": \"<free-text explanation>\", \"references\": [{\"path\": \"<path>\", \"symbol\": \"<qualified symbol name>\"}]}. List in references only the files and symbols from the supplied search context that are relevant to the task, using paths and symbol names exactly as shown there. Omit \"symbol\" for a whole-file reference. Do not invent paths, symbols, or line numbers; line numbers are added for you.";

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
                "faq-v3",
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

const FAQ_PROMPT_BYTES: usize = 128 * 1024;

impl Engine {
    fn answer_faq(
        &self,
        query: &str,
        options: &Value,
        previous_files: Option<&BTreeMap<String, String>>,
    ) -> Result<(Value, BTreeMap<String, String>)> {
        let expand_callers = options["expandCallers"].as_u64().unwrap_or(2) as usize;
        let expand_callees = options["expandCallees"].as_u64().unwrap_or(2) as usize;
        let callers = (options["callers"].as_u64().unwrap_or(0) as usize).max(expand_callers);
        let callees = (options["callees"].as_u64().unwrap_or(0) as usize).max(expand_callees);
        let refs = crate::map::reference_mode(options, callers, callees)?;
        let results = self.search_limited(query, "search", options)?;
        let matches = results.rows;
        let mut prompt = format!("Task: {query}\n\n");
        let marker = "\n[Search results truncated to fit the prompt size limit]\n";
        ensure!(
            prompt.len() + marker.len() <= FAQ_PROMPT_BYTES,
            "FAQ question exceeds the prompt size limit"
        );
        // The rendered search text is LLM context only: it is neither returned
        // nor cached with the answer. Its files/symbols bound what may be cited.
        // Reserve room for the truncation marker; whole trailing file sections
        // beyond the remaining budget are omitted during rendering instead.
        let search_budget = FAQ_PROMPT_BYTES - prompt.len() - marker.len();
        let (search, dependencies, symbols, omitted) = crate::cli::faq_search_context(
            self,
            &matches,
            (callers, callees, expand_callers, expand_callees),
            refs,
            options["expandCodeThreshold"].as_f64().unwrap_or(0.9),
            search_budget,
        )?;
        let context_files: BTreeMap<_, _> = dependencies
            .into_iter()
            .map(|path| {
                let file = self.files.get(&path).context("Missing FAQ context file")?;
                Ok((path, file.hash.clone()))
            })
            .collect::<Result<_>>()?;
        prompt.push_str(&search);
        if omitted {
            prompt.push_str(marker);
        }
        ui::progress("Generating explanation from search results");
        let system = faq::SYSTEM;
        let request = json!([
            "explanation-request-v1",
            self.providers.llm().profile(),
            hash(self.config["descriptionCommand"].to_string()),
            system,
            prompt
        ]);
        // Request identity is independent of FAQ file hashes, index generations,
        // and the incoming paraphrase. Successful paid results live in global.cache.
        let key = hash(request.to_string());
        self.artifacts
            .hydrate_text(&self.db, "explanation", std::slice::from_ref(&key))?;
        let _lock = registry::artifact_lock(&self.db, "explanation", &key)?;
        // The artifact cache holds the raw LLM response; parsing and reference
        // validation below are deterministic and re-run on every reuse.
        let response = if let Some(cached) = self.db.cache("explanation", &key)? {
            cached
        } else {
            // Promote old exact-request results when their old identity can
            // still be reconstructed. Never reuse one whose prompt differs.
            let mut legacy = request.clone();
            legacy[0] = json!("explanation-v2");
            let mut keys = vec![hash(legacy.to_string())];
            legacy[0] = json!("explanation-v3");
            for files in std::iter::once(&context_files).chain(previous_files) {
                let mut legacy = legacy.clone();
                legacy.as_array_mut().unwrap().push(json!(files));
                let key = hash(legacy.to_string());
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
            self.artifacts
                .hydrate_text(&self.db, "explanation", &keys)?;
            let mut cached = None;
            for key in keys {
                cached = self.db.cache("explanation", &key)?;
                if cached.is_some() {
                    break;
                }
            }
            let response = match cached {
                Some(cached) => cached,
                None => self.providers.llm().describe(system, &prompt)?,
            };
            self.db.cache_put("explanation", &key, &response)?;
            self.artifacts
                .put_text(&self.db, "explanation", &key, &response);
            response
        };
        let (description, cited) = parse_answer(&response);
        let (references, files) = match_references(&cited, &context_files, &symbols);
        Ok((
            json!({"query":query,"question":query,"description":description,"references":references,
            "omitted":results.omitted,"limit":results.limit}),
            files,
        ))
    }
}

/// One unvalidated reference exactly as cited by the LLM.
#[derive(Debug, PartialEq, Eq)]
struct Cited {
    path: String,
    symbol: Option<String>,
}

/// Split a raw LLM response into its free-text description and cited
/// references. Responses that are not the requested JSON object (including
/// legacy plain-text answers) keep their whole text as the description and
/// cite nothing, so a noncompliant model degrades to an uncached answer
/// instead of an error.
fn parse_answer(raw: &str) -> (String, Vec<Cited>) {
    let text = raw.trim();
    let unfenced = text
        .strip_prefix("```")
        .and_then(|rest| rest.split_once('\n').map(|(_, body)| body))
        .map(|body| body.strip_suffix("```").unwrap_or(body).trim())
        .unwrap_or(text);
    let value = serde_json::from_str::<Value>(unfenced).ok().or_else(|| {
        let start = unfenced.find('{')?;
        let end = unfenced.rfind('}')?;
        serde_json::from_str::<Value>(unfenced.get(start..=end)?).ok()
    });
    let Some(description) = value
        .as_ref()
        .and_then(|value| value["description"].as_str())
        .map(str::to_owned)
    else {
        return (text.to_owned(), Vec::new());
    };
    let references = value.unwrap()["references"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    if let Some(path) = entry.as_str() {
                        return Some(Cited {
                            path: path.into(),
                            symbol: None,
                        });
                    }
                    let path = entry["path"].as_str()?;
                    let symbol = ["symbol", "qualifiedName", "name"]
                        .iter()
                        .find_map(|key| entry[key].as_str())
                        .map(str::trim)
                        .filter(|symbol| !symbol.is_empty());
                    Some(Cited {
                        path: path.into(),
                        symbol: symbol.map(str::to_owned),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    (description, references)
}

/// Keep only citations present in the rendered search context, in cited
/// order and without duplicates. Symbol citations gain the rendered line
/// range and canonical qualified name; the returned file hashes (for exactly
/// the kept references) drive FAQ cache invalidation.
fn match_references(
    cited: &[Cited],
    context_files: &BTreeMap<String, String>,
    symbols: &[crate::cli::RenderedSymbol],
) -> (Vec<Value>, BTreeMap<String, String>) {
    let mut references: Vec<Value> = Vec::new();
    let mut files = BTreeMap::new();
    for citation in cited {
        let path = citation
            .path
            .trim()
            .strip_prefix("./")
            .unwrap_or_else(|| citation.path.trim());
        let Some(hash) = context_files.get(path) else {
            continue;
        };
        let reference = match &citation.symbol {
            None => json!({"path": path}),
            Some(symbol) => {
                let symbol = symbol.strip_suffix("()").unwrap_or(symbol);
                let Some(found) = symbols
                    .iter()
                    .filter(|candidate| {
                        candidate.path == path
                            && (candidate.qualified_name == symbol || candidate.name == symbol)
                    })
                    .min_by_key(|candidate| {
                        (
                            candidate.qualified_name != symbol,
                            candidate.start_line,
                            candidate.end_line,
                        )
                    })
                else {
                    continue;
                };
                json!({"path": path, "symbol": found.qualified_name,
                    "startLine": found.start_line, "endLine": found.end_line})
            }
        };
        if !references.contains(&reference) {
            files.insert(path.to_owned(), hash.clone());
            references.push(reference);
        }
    }
    (references, files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::RenderedSymbol;

    fn symbol(path: &str, qualified: &str, name: &str, start: usize, end: usize) -> RenderedSymbol {
        RenderedSymbol {
            path: path.into(),
            qualified_name: qualified.into(),
            name: name.into(),
            start_line: start,
            end_line: end,
        }
    }

    #[test]
    fn parse_answer_accepts_fenced_json_and_falls_back_to_plain_text() {
        let (description, cited) = parse_answer(
            "```json\n{\"description\":\"How it works.\",\"references\":[{\"path\":\"a.rs\",\"symbol\":\"run\"},{\"path\":\"b.md\"},\"c.rs\",{\"symbol\":\"orphan\"},{\"path\":\"d.rs\",\"symbol\":\"\"}]}\n```",
        );
        assert_eq!(description, "How it works.");
        assert_eq!(
            cited,
            [
                Cited {
                    path: "a.rs".into(),
                    symbol: Some("run".into())
                },
                Cited {
                    path: "b.md".into(),
                    symbol: None
                },
                Cited {
                    path: "c.rs".into(),
                    symbol: None
                },
                Cited {
                    path: "d.rs".into(),
                    symbol: None
                },
            ]
        );
        assert_eq!(
            parse_answer("plain legacy answer"),
            ("plain legacy answer".into(), Vec::new())
        );
        assert_eq!(
            parse_answer("prefix {\"description\":\"x\",\"references\":[]} suffix").0,
            "x"
        );
    }

    #[test]
    fn match_references_keeps_only_rendered_files_and_symbols_with_their_lines() {
        let files: BTreeMap<_, _> = [
            ("a.rs".to_owned(), "ha".to_owned()),
            ("b.md".to_owned(), "hb".to_owned()),
        ]
        .into();
        let symbols = [
            symbol("a.rs", "Api.run", "run", 3, 5),
            symbol("a.rs", "Api.run", "run", 9, 9),
            symbol("a.rs", "helper", "helper", 7, 8),
        ];
        let cited = [
            Cited {
                path: "a.rs".into(),
                symbol: Some("run()".into()),
            },
            Cited {
                path: "a.rs".into(),
                symbol: Some("missing".into()),
            },
            Cited {
                path: "gone.rs".into(),
                symbol: None,
            },
            Cited {
                path: "./b.md".into(),
                symbol: None,
            },
            Cited {
                path: "a.rs".into(),
                symbol: Some("Api.run".into()),
            },
            Cited {
                path: "b.md".into(),
                symbol: Some("run".into()),
            },
        ];
        let (references, tracked) = match_references(&cited, &files, &symbols);
        assert_eq!(
            references,
            json!([
                {"path":"a.rs","symbol":"Api.run","startLine":3,"endLine":5},
                {"path":"b.md"}
            ])
            .as_array()
            .unwrap()
            .clone()
        );
        assert_eq!(tracked, files);
    }
}
