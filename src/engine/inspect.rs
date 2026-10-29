//! Saved-index inspection. Never refresh sources or generate missing indexed vectors.
use super::*;
use regex::{RegexSet, RegexSetBuilder};

const CHANNELS: &[&str] = &[
    "code",
    "docs",
    "config",
    "markup",
    "descriptions",
    "symbols",
    "faq",
];

struct Embedding {
    role: &'static str,
    key: String,
    present: bool,
    compatible: bool,
}

struct Record {
    id: String,
    kind: String,
    path: Option<String>,
    name: String,
    text: String,
    data: Value,
    embeddings: Vec<Embedding>,
}

impl Engine {
    /// Show all supported channels, including empty and not-yet-embedded ones.
    pub fn index_catalog(&self) -> Result<Vec<Value>> {
        CHANNELS.iter().map(|name| {
            let rows = self.inspection_records(name)?;
            let vectors = rows.iter().flat_map(|row| &row.embeddings).filter(|embedding| embedding.present).count();
            let pending = rows.iter().filter(|row| row.embeddings.iter().any(|embedding| !embedding.present)).count();
            Ok(json!({"name":name,"entryCount":rows.len(),"vectorCount":vectors,"pendingCount":pending,"exists":!rows.is_empty(),
                "state":if rows.is_empty() { "empty" } else if pending > 0 { "pending" } else { "ready" }}))
        }).collect()
    }

    /// List or rank saved entries, including their complete metadata and vector values.
    pub fn index_entries(&self, name: &str, options: &Value) -> Result<Vec<Value>> {
        Ok(self.index_entries_limited(name, options)?.rows)
    }

    pub(crate) fn index_entries_limited(
        &self,
        name: &str,
        options: &Value,
    ) -> Result<LimitedResults<Value>> {
        let name = if matches!(name, "md" | "markdown") {
            "docs"
        } else {
            name
        };
        ensure!(
            CHANNELS.contains(&name),
            "Unknown index {name:?}; use `slopdex index list` to list indexes"
        );
        let regex = inspection_regex(options)?;
        let include_vectors = match options.get("includeVectors") {
            None => true,
            Some(value) => value
                .as_bool()
                .context("includeVectors must be a boolean")?,
        };
        let queries = crate::filter::strings(options, "query")?;
        ensure!(
            queries.iter().all(|query| !query.trim().is_empty()),
            "Index queries must not be empty"
        );
        let mut limited = options.clone();
        if limited.get("limit").is_none() {
            limited["limit"] = json!("none");
        }
        let limit = limits::resolve(&json!({}), &limited)?.count();
        let min = inspection_bound(options, "minSimilarity", -1.0)?;
        let max = inspection_bound(options, "maxSimilarity", f64::INFINITY)?;
        ensure!(min <= max, "Index similarity bounds are reversed");
        let mut rows: Vec<_> = self
            .inspection_records(name)?
            .into_iter()
            .enumerate()
            .filter(|(_, row)| {
                regex.as_ref().is_none_or(|regex| {
                    regex.is_match(&format!(
                        "{}\n{}\n{}",
                        row.path.as_deref().unwrap_or(""),
                        row.name,
                        row.text
                    ))
                })
            })
            .map(|(order, row)| (order + 1, row, None))
            .collect();
        if !queries.is_empty() {
            rows.retain(|(_, row, _)| {
                row.embeddings
                    .iter()
                    .any(|embedding| embedding.present && embedding.compatible)
            });
        }
        if !queries.is_empty() && !rows.is_empty() {
            let vector = if name == "symbols" {
                self.providers.symbol_vector()?
            } else {
                self.providers.vector()
            };
            let queries: Vec<_> = queries
                .into_iter()
                .map(|query| {
                    if name == "symbols" {
                        symbols::normalize(query)
                    } else {
                        query.to_owned()
                    }
                })
                .collect();
            ensure!(
                queries.iter().all(|query| !query.is_empty()),
                "Symbol queries must contain letters or numbers"
            );
            self.ensure_embedding_groups_with(vector, vec![queries.clone()], true)?;
            let queries: Vec<_> = queries
                .iter()
                .map(|query| self.vector(&Database::embedding_key(&vector.profile(), true, query)))
                .collect::<Result<_>>()?;
            ensure!(
                queries
                    .iter()
                    .all(|query| query.len() == vector.dimensions()
                        && query.iter().any(|value| *value != 0.0)),
                "Invalid index query embedding"
            );
            for (_, row, similarity) in &mut rows {
                for embedding in row
                    .embeddings
                    .iter()
                    .filter(|embedding| embedding.present && embedding.compatible)
                {
                    let stored = self
                        .db
                        .embedding(&embedding.key)?
                        .context("Missing indexed embedding")?;
                    ensure!(
                        stored.len() == vector.dimensions()
                            && stored.iter().any(|value| *value != 0.0),
                        "Invalid indexed embedding for {}",
                        embedding.key
                    );
                    for query in &queries {
                        let score = cosine(query, &stored);
                        *similarity =
                            Some(similarity.map_or(score, |previous: f64| previous.max(score)));
                    }
                }
            }
            rows.retain(|(_, _, score)| score.is_some_and(|score| score >= min && score < max));
            rows.sort_by(|a, b| {
                b.2.unwrap()
                    .total_cmp(&a.2.unwrap())
                    .then_with(|| a.0.cmp(&b.0))
            });
        }
        let omitted = limit.is_some_and(|limit| rows.len() > limit);
        if let Some(limit) = limit {
            rows.truncate(limit);
        }
        let rows: Vec<_> = rows.into_iter().enumerate().map(|(rank, (order, row, similarity))| {
            let embeddings: Vec<_> = row.embeddings.into_iter().map(|embedding| {
                let vector = if embedding.present && include_vectors { Some(self.db.embedding(&embedding.key)?.context("Missing indexed embedding")?) } else { None };
                let dimensions = if let Some(vector) = &vector { vector.len() } else if embedding.present {
                    let bytes: i64 = self.db.conn.query_row("SELECT length(vector) FROM global.embeddings WHERE key=?", [&embedding.key], |row| row.get(0))?;
                    let bytes = usize::try_from(bytes).context("Invalid indexed embedding length")?;
                    ensure!(bytes > 0 && bytes.is_multiple_of(4), "Invalid indexed embedding for {}", embedding.key);
                    bytes / 4
                } else { 0 };
                let mut value = json!({"role":embedding.role,"key":embedding.key,"present":embedding.present,"compatible":embedding.compatible,"dimensions":dimensions});
                if include_vectors { value["vector"] = json!(vector); }
                Ok(value)
            }).collect::<Result<_>>()?;
            Ok(json!({"index":name,"id":row.id,"order":order,"rank":rank+1,"similarity":similarity,"kind":row.kind,"path":row.path,"name":row.name,"text":row.text,"data":row.data,"embeddings":embeddings}))
        }).collect::<Result<_>>()?;
        Ok(LimitedResults {
            rows,
            omitted,
            limit,
        })
    }

    fn inspection_embedding(
        &self,
        role: &'static str,
        key: String,
        compatible: bool,
    ) -> Result<Embedding> {
        Ok(Embedding {
            role,
            present: self.db.embedding_exists(&key)?,
            key,
            compatible,
        })
    }

    fn inspection_records(&self, name: &str) -> Result<Vec<Record>> {
        if name == "symbols" {
            return self.inspection_symbols();
        }
        let profile = self.providers.vector().profile();
        if name == "faq" {
            let mut checked = HashMap::new();
            return self.db.all_faq_entries()?.into_iter().map(|(contract, entry)| {
                let answer = entry.result["description"].as_str().context("Invalid FAQ answer")?;
                let compatible = entry.question_embedding == Database::embedding_key(&profile, true, &entry.question)
                    && entry.answer_embedding == Database::embedding_key(&profile, false, answer);
                let valid = self.faq_files_current(&entry.files, &mut checked);
                Ok(Record {
                    id:entry.id.to_string(),kind:"faq".into(),path:None,name:entry.question.clone(),text:format!("{}\n{}",entry.question,answer),
                    data:json!({"question":entry.question,"answer":answer,"result":entry.result,"files":entry.files,"contract":contract,"valid":valid}),
                    embeddings:vec![self.inspection_embedding("question",entry.question_embedding,compatible)?,self.inspection_embedding("answer",entry.answer_embedding,compatible)?],
                })
            }).collect();
        }
        let descriptions = name == "descriptions";
        let mut records = Vec::new();
        for item in self.db.items_for_profile(&profile)? {
            let text = if descriptions {
                item.data["description"].as_str()
            } else if content_group(&item).is_some_and(|group| group.as_str() == name) {
                item.data["embeddingInput"].as_str()
            } else {
                None
            };
            let Some(text) = text else {
                continue;
            };
            let key = Database::embedding_key(&profile, false, text);
            let record = Record {
                id: format!("unit:{}", item.id),
                kind: if descriptions {
                    "description".into()
                } else {
                    item.kind.clone()
                },
                path: Some(item.path.clone()),
                name: item.data["qualifiedName"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        item.data["headingPath"]
                            .as_array()
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join(" / ")
                            })
                            .unwrap_or_else(|| item.path.clone())
                    }),
                text: text.to_owned(),
                data: item.data,
                embeddings: vec![self.inspection_embedding(
                    if descriptions {
                        "description"
                    } else {
                        "content"
                    },
                    key,
                    true,
                )?],
            };
            records.push(record);
        }
        if descriptions {
            for file in self.db.file_records()? {
                let Some(text) = &file.description else {
                    continue;
                };
                let key = Database::embedding_key(&profile, false, text);
                records.push(Record { id:format!("file:{}",file.path),kind:"file-description".into(),path:Some(file.path.clone()),name:file.path.clone(),text:text.clone(),
                    data:json!({"path":file.path,"description":text,"sourceHash":file.hash,"descriptionHash":file.description_hash,"language":file.language,"sourceMode":file.source_mode}),
                    embeddings:vec![self.inspection_embedding("description",key,true)?] });
            }
        }
        Ok(records)
    }

    fn inspection_symbols(&self) -> Result<Vec<Record>> {
        let vector = self.providers.symbol_vector()?;
        let profile = vector.profile();
        let mut names = BTreeMap::<u64, (String, Vec<Value>)>::new();
        for path in self.db.paths()? {
            for node in self.db.structure(&path)?.nodes {
                let vocabulary: BTreeSet<_> = std::iter::once(&node.name)
                    .chain(&node.names)
                    .map(|name| symbols::normalize(name))
                    .filter(|name| !name.is_empty())
                    .collect();
                for name in vocabulary {
                    let id = u64::from_str_radix(&hash(&name)[..16], 16)?;
                    let (previous, locations) = names
                        .entry(id)
                        .or_insert_with(|| (name.clone(), Vec::new()));
                    ensure!(*previous == name, "Symbol name hash collision");
                    locations.push(json!({"path":path,"symbol":node}));
                }
            }
        }
        names
            .into_iter()
            .map(|(id, (name, locations))| {
                let key = Database::embedding_key(&profile, false, &name);
                Ok(Record {
                    id: id.to_string(),
                    kind: "symbol".into(),
                    path: None,
                    text: name.clone(),
                    name: name.clone(),
                    data: json!({"normalizedName":name,"locations":locations}),
                    embeddings: vec![self.inspection_embedding("symbol", key, true)?],
                })
            })
            .collect()
    }
}

fn inspection_regex(options: &Value) -> Result<Option<RegexSet>> {
    let patterns = crate::filter::strings(options, "regexp")?;
    if patterns.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        RegexSetBuilder::new(patterns)
            .case_insensitive(flag(options, "ignoreCase"))
            .multi_line(true)
            .build()
            .context("Invalid index regex")?,
    ))
}

fn inspection_bound(options: &Value, key: &str, default: f64) -> Result<f64> {
    match options.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_f64()
            .filter(|value| value.is_finite() && (-1.0..=1.0).contains(value))
            .with_context(|| format!("{key} must be finite and in [-1,1]")),
    }
}
