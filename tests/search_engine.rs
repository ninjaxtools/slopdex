//! Search regressions use durable, deterministic vectors and make no model calls.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use slopdex::{
    engine::Engine,
    hash, parse,
    providers::Providers,
    storage::{Database, File},
};
use std::{collections::BTreeSet, fs, path::PathBuf};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    index: PathBuf,
    config: Value,
}

fn test_config(temp: &TempDir) -> Value {
    json!({
        "embeddingProvider":"openai", "embeddingModel":"search-fixture",
        "embeddingDimensions":2, "symbolDimensions":2,
        "embeddingBaseUrl":"http://127.0.0.1:1/v1", "embeddingApiKey":"fixture",
        "descriptionProvider":"openai", "descriptionModel":"source-only-no-llm",
        "descriptionBaseUrl":"http://127.0.0.1:1/v1", "descriptionApiKey":"fixture",
        "providerMaxRetries":0, "artifactCachePath":temp.path().join("artifacts.sqlite")
    })
}

impl Fixture {
    fn new(descriptions: bool) -> Result<Self> {
        Self::with_sources(
            descriptions,
            &[
                (
                    "a.rs",
                    "//! file overview\n\n/// copper token\npub fn run() {\n    let value = 1;\n    println!(\"{value}\");\n}\n/// copper token\npub const LIMIT: usize = 1;\n",
                ),
                (
                    "b.rs",
                    "pub fn run() {\n    let value = 1;\n    println!(\"{value}\");\n}\n",
                ),
                (
                    "guide.md",
                    "# Guide\n\nInstructions.\n## Setup\n\nMore instructions.\n",
                ),
                (
                    "settings.rs",
                    "//! file overview\n\npub const SETTING: usize = 2;\n",
                ),
            ],
        )
    }

    fn with_sources(descriptions: bool, sources: &[(&str, &str)]) -> Result<Self> {
        // Database roots must match Engine's canonical paths, including symlinked TMPDIRs.
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize()?)?;
        let index = temp.path().join("index.sqlite");
        let config = test_config(&temp);
        let mut db = Database::open_with_config(&index, temp.path(), &config, false)?;
        let mut records = Vec::new();
        for &(path, source) in sources {
            fs::write(temp.path().join(path), source)?;
            let parsed = parse::parse(path, source)?;
            records.push((
                File {
                    path: path.into(),
                    hash: hash(source),
                    source: source.into(),
                    language: parse::language_for_path(path).unwrap().into(),
                    source_mode: "working-tree".into(),
                    description: None,
                    description_hash: None,
                    description_embedding: None,
                    errors: Vec::new(),
                },
                parsed,
            ));
        }
        db.apply_structure(&records, &[], None)?;
        let profile = Providers::new(&config)?.embedding_profile();
        for item in db.items()? {
            if let Some(input) = item.data["embeddingInput"].as_str() {
                db.put_embedding(
                    &Database::embedding_key(&profile, false, input),
                    &[0.0, 1.0],
                )?;
            }
            if descriptions && let Some(text) = item.data["description"].as_str() {
                db.put_embedding(&Database::embedding_key(&profile, false, text), &[1.0, 0.0])?;
            }
        }
        if descriptions {
            for file in db.files()? {
                if let Some(text) = file.description {
                    db.put_embedding(
                        &Database::embedding_key(&profile, false, &text),
                        &[0.0, 1.0],
                    )?;
                }
            }
        }
        db.put_embedding(
            &Database::embedding_key(&profile, true, "copper token"),
            &[1.0, 0.0],
        )?;
        let fixture = Self {
            temp,
            index,
            config,
        };
        fixture.seed_symbols(false)?;
        Ok(fixture)
    }

    fn seed_symbols(&self, structural: bool) -> Result<()> {
        // open_map deliberately constructs default providers; symbol maps through
        // the CLI use open_symbol_map and the configured profile.
        let mut config = if structural {
            json!({})
        } else {
            self.config.clone()
        };
        let dimensions = if structural { 256 } else { 2 };
        config["dimensions"] = json!(dimensions);
        let mut profile = Providers::new(&config)?.embedding_profile();
        profile["symbolNormalizationVersion"] = json!("symbols-v1");
        let db = Database::open_with_config(&self.index, self.temp.path(), &self.config, false)?;
        for (input, query, axis) in [
            ("run", false, 1),
            ("limit", false, 1),
            ("kind", false, 1),
            ("setting", false, 1),
            ("guide", false, 1),
            ("setup", false, 1),
            ("copper token", false, 0),
            ("silver token", false, 1),
            ("file overview", false, 1),
            ("copper token", true, 0),
        ] {
            let mut vector = vec![0.0; dimensions];
            vector[axis] = 1.0;
            db.put_embedding(&Database::embedding_key(&profile, query, input), &vector)?;
        }
        Ok(())
    }

    fn engine(&self) -> Result<Engine> {
        Engine::open(self.temp.path(), &self.index, self.config.clone())
    }

    fn seed_query(&self, query: &str, vector: &[f32]) -> Result<()> {
        let db = Database::open_with_config(&self.index, self.temp.path(), &self.config, false)?;
        let providers = Providers::new(&self.config)?;
        db.put_embedding(
            &Database::embedding_key(&providers.embedding_profile(), true, query),
            vector,
        )?;
        let mut config = self.config.clone();
        config["dimensions"] = self.config["symbolDimensions"].clone();
        let mut profile = Providers::new(&config)?.embedding_profile();
        profile["symbolNormalizationVersion"] = json!("symbols-v1");
        db.put_embedding(&Database::embedding_key(&profile, true, query), vector)?;
        Ok(())
    }

    fn source_hash(&self, path: &str) -> Result<String> {
        let parsed = parse::parse(path, &fs::read_to_string(self.temp.path().join(path))?)?;
        Ok(parsed.callables[0].source_hash.clone())
    }

    fn seed_code_vectors(&self, vectors: &[(&str, [f32; 2])]) -> Result<()> {
        let db = Database::open_with_config(&self.index, self.temp.path(), &self.config, false)?;
        // Replace the fixture's uniform vectors before opening any search indexes.
        db.conn.execute("DELETE FROM global.embeddings", [])?;
        let profile = Providers::new(&self.config)?.embedding_profile();
        for item in db.items()? {
            let (_, vector) = vectors.iter().find(|(path, _)| *path == item.path).unwrap();
            db.put_embedding(
                &Database::embedding_key(
                    &profile,
                    false,
                    item.data["embeddingInput"].as_str().unwrap(),
                ),
                vector,
            )?;
        }
        db.put_embedding(
            &Database::embedding_key(&profile, true, "copper token"),
            &[1.0, 0.0],
        )?;
        Ok(())
    }
}

fn hit_keys(rows: &[Value]) -> BTreeSet<String> {
    rows.iter()
        .map(|row| {
            let item = match row["type"].as_str().unwrap() {
                "function" => &row["function"],
                "symbol" => &row["symbol"],
                "file" => &row["file"],
                _ => &row["chunk"],
            };
            format!(
                "{}:{}:{}:{}",
                row["type"], item["path"], item["name"], item["startLine"]
            )
        })
        .collect()
}

#[test]
fn multiple_queries_keep_best_scores_across_code_docs_and_symbols() -> Result<()> {
    let fixture = Fixture::new(true)?;
    fixture.seed_query("silver token", &[0.6, 0.8])?;
    let engine = fixture.engine()?;
    let queries: Vec<String> = vec!["copper token".into(), "silver token".into()];
    let options = json!({"minSimilarity":-1, "limit":"none"});
    for kind in ["search-code", "search-docs", "search-symbols"] {
        let copper = engine.search(&queries[0], kind, &options)?;
        let silver = engine.search(&queries[1], kind, &options)?;
        assert!(!copper.is_empty(), "{kind}");
        assert_eq!(hit_keys(&copper), hit_keys(&silver), "{kind}");
        let combined = engine.search_queries(&queries, kind, &options)?;
        assert_eq!(combined.len(), copper.len(), "{kind}: {combined:?}");
        assert_eq!(hit_keys(&combined), hit_keys(&copper), "{kind}");
        for row in &combined {
            let key = hit_keys(std::slice::from_ref(row));
            let best = copper
                .iter()
                .chain(&silver)
                .filter(|candidate| hit_keys(std::slice::from_ref(*candidate)) == key)
                .max_by(|a, b| {
                    a["similarity"]
                        .as_f64()
                        .unwrap()
                        .total_cmp(&b["similarity"].as_f64().unwrap())
                })
                .unwrap();
            assert_eq!(row, best, "{kind}");
        }
        assert_eq!(combined, engine.search_queries(&queries, kind, &options)?);
        assert_eq!(
            combined,
            engine.search_queries(
                &[queries[1].clone(), queries[0].clone(), queries[1].clone()],
                kind,
                &options,
            )?
        );
        assert_eq!(
            copper,
            engine.search_queries(&queries[..1], kind, &options)?
        );
        assert_eq!(
            copper,
            engine.search_queries(&[queries[0].clone(), queries[0].clone()], kind, &options)?
        );
    }
    Ok(())
}

#[test]
fn multiple_queries_or_thresholds_and_apply_one_global_limit() -> Result<()> {
    let mut fixture = Fixture::with_sources(
        false,
        &[
            ("a.rs", "pub fn run() { println!(\"a\"); }\n"),
            ("b.rs", "pub fn run() { println!(\"b\"); }\n"),
            ("c.rs", "pub fn run() { println!(\"c\"); }\n"),
        ],
    )?;
    fixture.seed_code_vectors(&[
        ("a.rs", [1.0, 0.0]),
        ("b.rs", [0.8, 0.6]),
        ("c.rs", [0.0, 1.0]),
    ])?;
    fixture.seed_query("silver token", &[0.0, 1.0])?;
    fixture.config["defaultLimit"] = json!(1);
    let engine = fixture.engine()?;
    let queries: Vec<String> = vec!["copper token".into(), "silver token".into()];
    let unlimited = json!({"minSimilarity":0.7, "limit":"none"});
    let rows = engine.search_queries(&queries, "search-code", &unlimited)?;
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(hit_keys(&rows).len(), 3);
    assert_eq!(rows[0]["function"]["path"], "a.rs");
    assert_eq!(rows[1]["function"]["path"], "c.rs");
    assert_eq!(rows[2]["function"]["path"], "b.rs");
    assert!((rows[2]["similarity"].as_f64().unwrap() - 0.8).abs() < 0.001);
    assert_eq!(
        engine.search_queries(&queries, "search-code", &json!({"minSimilarity":0.7}))?,
        rows[..1]
    );
    for count in [1, 2, 3, 4] {
        let options = json!({"minSimilarity":0.7, "limit":count});
        assert_eq!(
            engine.search_queries(&queries, "search-code", &options)?,
            rows[..count.min(rows.len())]
        );
    }
    assert!(
        engine
            .search_queries(&[], "search-code", &unlimited)?
            .is_empty()
    );
    assert!(
        engine
            .search_queries(&queries, "search-code", &json!({"limit":0}))
            .is_err()
    );
    Ok(())
}

#[test]
fn multiple_queries_preserve_different_locations_and_result_streams() -> Result<()> {
    let fixture = Fixture::new(true)?;
    fixture.seed_query("silver token", &[0.6, 0.8])?;
    let engine = fixture.engine()?;
    let queries: Vec<String> = vec!["copper token".into(), "silver token".into()];
    let options = json!({"minSimilarity":-1, "limit":"none"});
    let rows = engine.search_queries(&queries, "search", &options)?;
    for path in ["a.rs", "b.rs"] {
        for (kind, field) in [("function", "function"), ("symbol", "symbol")] {
            assert!(
                rows.iter().any(|row| {
                    row["type"] == kind && row[field]["path"] == path && row[field]["name"] == "run"
                }),
                "missing {kind} in {path}: {rows:?}"
            );
        }
    }
    assert!(rows.iter().any(|row| row["type"] == "file"));
    assert!(rows.iter().any(|row| row["type"] == "markdown"));
    assert_eq!(
        rows,
        engine.search_queries(
            &[queries[1].clone(), queries[0].clone()],
            "search",
            &options
        )?
    );
    Ok(())
}

#[test]
fn multiple_queries_choose_and_sort_best_rerank_scores_without_provider_calls() -> Result<()> {
    let mut fixture = Fixture::with_sources(
        false,
        &[
            ("a.rs", "pub fn run() { println!(\"a\"); }\n"),
            ("b.rs", "pub fn run() { println!(\"b\"); }\n"),
        ],
    )?;
    fixture.seed_code_vectors(&[("a.rs", [1.0, 0.0]), ("b.rs", [0.0, 1.0])])?;
    fixture.seed_query("silver token", &[0.0, 1.0])?;
    let queries: Vec<String> = vec!["copper token".into(), "silver token".into()];
    let options = json!({"minSimilarity":-1, "limit":"none"});
    let baseline = fixture.engine()?;
    let documents = queries
        .iter()
        .map(|query| baseline.search(query, "search-code", &options))
        .collect::<Result<Vec<_>>>()?;
    drop(baseline);
    fixture.config["rerankingEnabled"] = json!(true);
    fixture.config["rerankerProvider"] = json!("openai");
    fixture.config["rerankerModel"] = json!("rerank-fixture");
    fixture.config["rerankerBaseUrl"] = json!("http://127.0.0.1:1/v1");
    fixture.config["rerankerApiKey"] = json!("fixture");
    let db =
        Database::open_with_config(&fixture.index, fixture.temp.path(), &fixture.config, false)?;
    for (query, rows) in queries.iter().zip(&documents) {
        let documents: Vec<_> = rows.iter().map(Value::to_string).collect();
        let key = hash(
            json!([
                "rerank-v1",
                fixture.config["rerankerProvider"],
                fixture.config["rerankerModel"],
                fixture.config["rerankerBaseUrl"],
                query,
                documents
            ])
            .to_string(),
        );
        let mut ranking: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let score = match (query.as_str(), row["function"]["path"].as_str().unwrap()) {
                    ("copper token", "a.rs") => 0.1,
                    ("copper token", "b.rs") => 0.7,
                    ("silver token", "a.rs") => 0.9,
                    ("silver token", "b.rs") => 0.2,
                    _ => unreachable!(),
                };
                (index, score)
            })
            .collect();
        ranking.sort_by(|a, b| f64::total_cmp(&b.1, &a.1));
        db.cache_put("rerank", &key, &serde_json::to_string(&ranking)?)?;
    }
    drop(db);
    let engine = fixture.engine()?;
    let rows = engine.search_queries(&queries, "search-code", &options)?;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["function"]["path"], "a.rs");
    assert_eq!(rows[0]["rerankScore"], 0.9);
    assert_eq!(rows[0]["similarity"], 0.0);
    assert_eq!(rows[1]["function"]["path"], "b.rs");
    assert_eq!(rows[1]["rerankScore"], 0.7);
    assert_eq!(rows[1]["similarity"], 0.0);
    assert_eq!(
        rows,
        engine.search_queries(
            &[queries[1].clone(), queries[0].clone()],
            "search-code",
            &options
        )?
    );
    assert_eq!(
        engine.search_queries(
            &queries,
            "search-code",
            &json!({"minSimilarity":-1,"limit":1})
        )?,
        rows[..1]
    );
    Ok(())
}

#[test]
fn default_result_limits_apply_to_search_and_map_and_can_be_disabled() -> Result<()> {
    // Repeated callable inputs share one vector with distinct occurrences, so
    // this checks output limits without depending on approximate-search recall.
    let source = "pub fn item() {}\n".repeat(4097);
    let mut fixture = Fixture::with_sources(false, &[("many.rs", &source)])?;
    let options = json!({"minSimilarity":-1});
    let engine = fixture.engine()?;
    assert_eq!(
        engine
            .search("copper token", "search-code", &options)?
            .len(),
        4096
    );
    assert_eq!(
        engine.map(&json!({}))?[0]["nodes"]
            .as_array()
            .unwrap()
            .len(),
        4096
    );
    let unlimited = json!({"minSimilarity":-1, "limit":"none"});
    assert_eq!(
        engine
            .search("copper token", "search-code", &unlimited)?
            .len(),
        4097
    );
    assert_eq!(
        engine.map(&json!({"limit":"none"}))?[0]["nodes"]
            .as_array()
            .unwrap()
            .len(),
        4097
    );
    drop(engine);

    fixture.config["defaultLimit"] = json!(2);
    let engine = fixture.engine()?;
    let selected = engine.search("copper token", "search-code", &options)?;
    assert_eq!(selected.len(), 2);
    assert_eq!(
        engine.search("copper token", "search-code", &options)?,
        selected
    );
    assert_eq!(
        engine.map(&json!({}))?[0]["nodes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    for count in [1, 5] {
        assert_eq!(
            engine
                .search(
                    "copper token",
                    "search-code",
                    &json!({"minSimilarity":-1, "limit":count})
                )?
                .len(),
            count
        );
        assert_eq!(
            engine.map(&json!({"limit":count}))?[0]["nodes"]
                .as_array()
                .unwrap()
                .len(),
            count
        );
    }
    assert_eq!(
        engine
            .search("copper token", "search-code", &unlimited)?
            .len(),
        4097
    );
    drop(engine);

    fixture.config["defaultLimit"] = json!("none");
    let engine = fixture.engine()?;
    assert_eq!(
        engine
            .search("copper token", "search-code", &options)?
            .len(),
        4097
    );
    assert_eq!(
        engine.map(&json!({}))?[0]["nodes"]
            .as_array()
            .unwrap()
            .len(),
        4097
    );
    Ok(())
}

fn configuration_fixture() -> Result<Fixture> {
    let fixture = Fixture::with_sources(
        false,
        &[
            ("a.JSON", "{\"limit\":\"json configuration\"}\n"),
            ("b.YaMl", "limit: yaml configuration\n"),
            ("c.yML", "limit: yml configuration\n"),
            ("d.ToMl", "limit = \"toml configuration\"\n"),
            ("e.Tf", "limit = \"terraform configuration\"\n"),
            ("f.TfVaRs", "limit = \"variables configuration\"\n"),
            ("g.HcL", "limit = \"hcl configuration\"\n"),
            ("z.html", "<run>html content</run>\n"),
            ("z.xml", "<run>xml content</run>\n"),
            ("z.css", ".run { run: visible; }\n"),
            ("z.md", "# run\n\nMarkdown content.\n"),
            ("z.rs", "pub fn run() {\n    println!(\"rust\");\n}\n"),
        ],
    )?;
    let mut db =
        Database::open_with_config(&fixture.index, fixture.temp.path(), &fixture.config, false)?;
    let mut records = Vec::new();
    for file in db.files()? {
        let mut parsed = parse::parse(&file.path, &file.source)?;
        assert!(
            parsed.errors.is_empty(),
            "{}: {:?}",
            file.path,
            parsed.errors
        );
        // Seed prose directly rather than generating descriptions through a provider.
        let prose = if file.path.starts_with("z.") {
            "silver token"
        } else {
            "copper token"
        };
        parsed.description = Some(prose.into());
        for node in &mut parsed.structure.nodes {
            node.description = Some(prose.into());
        }
        for callable in &mut parsed.callables {
            callable.description = Some(prose.into());
        }
        records.push((file, parsed));
    }
    db.apply_structure(&records, &[], None)?;
    db.conn.execute("DELETE FROM global.embeddings", [])?;
    let profile = Providers::new(&fixture.config)?.embedding_profile();
    for item in db.items()? {
        let vector = if item.path.starts_with("z.") {
            [0.8, 0.6]
        } else {
            [1.0, 0.0]
        };
        if let Some(input) = item.data["embeddingInput"].as_str() {
            db.put_embedding(&Database::embedding_key(&profile, false, input), &vector)?;
        }
    }
    for (text, vector) in [("copper token", [1.0, 0.0]), ("silver token", [0.8, 0.6])] {
        db.put_embedding(&Database::embedding_key(&profile, false, text), &vector)?;
    }
    db.put_embedding(
        &Database::embedding_key(&profile, true, "copper token"),
        &[1.0, 0.0],
    )?;
    let mut symbol_profile = profile;
    symbol_profile["symbolNormalizationVersion"] = json!("symbols-v1");
    for (name, vector) in [("limit", [1.0, 0.0]), ("run", [0.8, 0.6])] {
        db.put_embedding(
            &Database::embedding_key(&symbol_profile, false, name),
            &vector,
        )?;
    }
    db.put_embedding(
        &Database::embedding_key(&symbol_profile, true, "copper token"),
        &[1.0, 0.0],
    )?;
    Ok(fixture)
}

fn result_path(row: &Value) -> &str {
    let item = match row["type"].as_str().unwrap() {
        "function" => &row["function"],
        "symbol" => &row["symbol"],
        "file" => &row["file"],
        _ => &row["chunk"],
    };
    item["path"].as_str().unwrap()
}

#[test]
fn configuration_and_markup_remain_indexed_but_map_requires_opt_in() -> Result<()> {
    let fixture = configuration_fixture()?;
    let engine = fixture.engine()?;
    let defaults = json!({"private":true});
    let rows = engine.map(&defaults)?;
    let visible = BTreeSet::from(["z.md", "z.rs"]);
    assert_eq!(
        rows.iter()
            .map(|row| row["path"].as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        visible
    );
    let options = json!({"private":true, "formats":"all"});
    let all = engine.map(&options)?;
    let db =
        Database::open_with_config(&fixture.index, fixture.temp.path(), &fixture.config, false)?;
    let indexed: BTreeSet<_> = db.files()?.into_iter().map(|file| file.path).collect();
    assert_eq!(indexed.len(), 12);
    assert_eq!(
        all.iter()
            .map(|row| row["path"].as_str().unwrap().to_owned())
            .collect::<BTreeSet<_>>(),
        indexed
    );
    for path in indexed.iter().filter(|path| !path.starts_with("z.")) {
        assert!(
            db.items()?
                .iter()
                .any(|item| item.path == *path && item.kind == "document")
        );
    }
    assert_eq!(all, engine.map(&options)?);
    assert_eq!(rows, engine.map(&defaults)?);
    assert!(
        engine
            .map(&json!({"glob":"*.html", "private":true}))?
            .is_empty()
    );
    assert_eq!(
        engine
            .map(&json!({"glob":"*.html", "private":true, "formats":"markup"}))?
            .len(),
        1
    );
    assert!(
        engine
            .map(&json!({"glob":"*.JSON", "private":true}))?
            .is_empty()
    );
    assert_eq!(
        engine
            .map(&json!({"glob":"*.JSON", "private":true, "formats":["config"]}))?
            .len(),
        1
    );
    Ok(())
}

#[test]
fn configuration_and_markup_search_filter_every_stream_before_limits_and_cache_opt_in() -> Result<()>
{
    let fixture = configuration_fixture()?;
    let engine = fixture.engine()?;
    for (command, selector) in [
        ("search", None),
        ("search-code", Some("code")),
        ("search-descriptions", Some("descriptions")),
        ("search-docs", Some("docs")),
        ("search-md", Some("md")),
        ("search-symbols", Some("symbols")),
    ] {
        let defaults = json!({"minSimilarity":-1});
        let rows = engine.search("copper token", command, &defaults)?;
        assert!(!rows.is_empty(), "{command}");
        assert!(
            rows.iter()
                .all(|row| matches!(result_path(row), "z.rs" | "z.md")),
            "{command}: {rows:?}"
        );
        let options = json!({"minSimilarity":-1, "formats":"all"});
        let all = engine.search("copper token", command, &options)?;
        let includes_configs =
            matches!(command, "search" | "search-descriptions" | "search-symbols");
        if includes_configs {
            let configs: BTreeSet<_> = all
                .iter()
                .map(result_path)
                .filter(|path| !path.starts_with("z."))
                .collect();
            assert_eq!(
                configs,
                BTreeSet::from([
                    "a.JSON", "b.YaMl", "c.yML", "d.ToMl", "e.Tf", "f.TfVaRs", "g.HcL"
                ]),
                "{command}: {all:?}"
            );
        } else {
            assert_eq!(all, rows, "{command}");
            let expected = if command == "search-code" {
                "z.rs"
            } else {
                "z.md"
            };
            assert!(
                all.iter().all(|row| result_path(row) == expected),
                "{command}: {all:?}"
            );
        }
        assert_eq!(all, engine.search("copper token", command, &options)?);
        assert_eq!(rows, engine.search("copper token", command, &defaults)?);
        if let Some(selector) = selector {
            let mut explicit = options.clone();
            explicit[selector] = json!(true);
            assert_eq!(
                hit_keys(&all),
                hit_keys(&engine.search("copper token", "search", &explicit)?)
            );
        }
        let limited = engine.search(
            "copper token",
            command,
            &json!({"minSimilarity":-1, "limit":2}),
        )?;
        assert_eq!(limited.len(), rows.len().min(2), "{command}: {limited:?}");
        assert!(
            limited
                .iter()
                .all(|row| matches!(result_path(row), "z.rs" | "z.md"))
        );
        if includes_configs {
            let top = engine.search(
                "copper token",
                command,
                &json!({"minSimilarity":-1, "limit":2, "formats":"all"}),
            )?;
            assert_eq!(top.len(), 2);
            assert!(
                top.iter().all(|row| !result_path(row).starts_with("z.")),
                "{command}: {top:?}"
            );
        }
    }
    let db =
        Database::open_with_config(&fixture.index, fixture.temp.path(), &fixture.config, false)?;
    let items = db.items()?;
    let mut contracts = BTreeSet::new();
    for (group, paths) in [
        ("code", vec!["z.rs"]),
        ("docs", vec!["z.md"]),
        (
            "config",
            vec![
                "a.JSON", "b.YaMl", "c.yML", "d.ToMl", "e.Tf", "f.TfVaRs", "g.HcL",
            ],
        ),
        ("markup", vec!["z.html", "z.xml", "z.css"]),
    ] {
        let pointer: Value = serde_json::from_slice(&fs::read(
            fixture
                .index
                .with_extension(format!("sqlite.{group}.shared.json")),
        )?)?;
        let membership = pointer["membership"]
            .as_object()
            .context("missing content membership")?;
        let expected: BTreeSet<_> = items
            .iter()
            .filter(|item| item.kind != "symbol-description" && paths.contains(&item.path.as_str()))
            .map(|item| item.id.to_string())
            .collect();
        assert!(!expected.is_empty(), "{group}");
        assert_eq!(
            membership.keys().cloned().collect::<BTreeSet<_>>(),
            expected,
            "{group}"
        );
        contracts.insert(
            pointer["contract"]
                .as_str()
                .context("missing content contract")?
                .to_owned(),
        );
        assert!(
            !fixture
                .index
                .with_extension(format!("sqlite.{group}.usearch"))
                .exists()
        );
    }
    assert_eq!(
        contracts.len(),
        4,
        "content groups must have separate graph namespaces"
    );
    assert!(
        !fixture
            .index
            .with_extension("sqlite.markdown.shared.json")
            .exists()
    );
    Ok(())
}

#[test]
fn selected_content_queries_ignore_missing_or_corrupt_excluded_group_vectors() -> Result<()> {
    for corrupt in [false, true] {
        let fixture = Fixture::with_sources(
            false,
            &[
                ("source.rs", "pub fn run() {\n    println!(\"code\");\n}\n"),
                ("guide.md", "# Guide\n\nDocumentation.\n"),
                ("hidden.json", "{\"run\":\"configuration\"}\n"),
                ("hidden.html", "<run>markup</run>\n"),
            ],
        )?;
        let db = Database::open_with_config(
            &fixture.index,
            fixture.temp.path(),
            &fixture.config,
            false,
        )?;
        let profile = Providers::new(&fixture.config)?.embedding_profile();
        for item in db
            .items_for_profile(&profile)?
            .iter()
            .filter(|item| item.path.starts_with("hidden."))
        {
            let sql = if corrupt {
                "UPDATE global.embeddings SET vector=x'ff' WHERE key=?"
            } else {
                "DELETE FROM global.embeddings WHERE key=?"
            };
            db.conn.execute(sql, [&item.embedding])?;
        }
        let engine = fixture.engine()?;
        let options = json!({"formats":"all", "minSimilarity":-1});
        for (command, path) in [("search-code", "source.rs"), ("search-docs", "guide.md")] {
            let rows = engine.search("copper token", command, &options)?;
            assert_eq!(rows.len(), 1, "corrupt={corrupt}: {command}: {rows:?}");
            assert_eq!(result_path(&rows[0]), path);
            assert_eq!(rows, engine.search("copper token", command, &options)?);
        }
        for group in ["config", "markup"] {
            assert!(
                engine
                    .search("copper token", "search", &json!({"formats":[group]}))
                    .is_err(),
                "corrupt={corrupt}: {group}"
            );
        }
    }
    Ok(())
}

#[test]
fn identical_content_inputs_reuse_embeddings_but_not_format_index_contracts() -> Result<()> {
    let fixture = Fixture::with_sources(
        false,
        &[
            ("guide.md", "# Guide\n\nDocumentation.\n"),
            ("settings.json", "{\"run\":\"configuration\"}\n"),
        ],
    )?;
    let mut db =
        Database::open_with_config(&fixture.index, fixture.temp.path(), &fixture.config, false)?;
    let mut records = Vec::new();
    for file in db.files()? {
        let mut parsed = parse::parse(&file.path, &file.source)?;
        assert_eq!(parsed.chunks.len(), 1);
        parsed.chunks[0].embedding_input = "identical content input".into();
        records.push((file, parsed));
    }
    db.apply_structure(&records, &[], None)?;
    let profile = Providers::new(&fixture.config)?.embedding_profile();
    let shared_key = Database::embedding_key(&profile, false, "identical content input");
    db.put_embedding(&shared_key, &[0.0, 1.0])?;
    let engine = fixture.engine()?;
    let rows = engine.search(
        "copper token",
        "search",
        &json!({"formats":["docs", "config"], "minSimilarity":-1}),
    )?;
    assert!(rows.iter().any(|row| row["type"] == "markdown"));
    assert!(rows.iter().any(|row| row["type"] == "document"));
    let mut contracts = BTreeSet::new();
    for group in ["docs", "config"] {
        let pointer: Value = serde_json::from_slice(&fs::read(
            fixture
                .index
                .with_extension(format!("sqlite.{group}.shared.json")),
        )?)?;
        let membership = pointer["membership"].as_object().unwrap();
        assert_eq!(membership.len(), 1);
        assert_eq!(membership.values().next().unwrap(), &json!(shared_key));
        contracts.insert(pointer["contract"].as_str().unwrap().to_owned());
    }
    assert_eq!(contracts.len(), 2);
    let count: i64 = db.conn.query_row(
        "SELECT count(*) FROM global.embeddings WHERE key=?",
        [&shared_key],
        |row| row.get(0),
    )?;
    assert_eq!(count, 1, "format indexes must reuse paid embeddings");
    assert_eq!(db.embedding(&shared_key)?, Some(vec![0.0, 1.0]));
    Ok(())
}

#[test]
fn format_groups_filter_map_and_shared_search_channels_and_validate_options() -> Result<()> {
    let fixture = configuration_fixture()?;
    let engine = fixture.engine()?;
    let configs = [
        "a.JSON", "b.YaMl", "c.yML", "d.ToMl", "e.Tf", "f.TfVaRs", "g.HcL",
    ];
    for (formats, paths) in [
        (json!(null), vec!["z.rs", "z.md"]),
        (json!(["code"]), vec!["z.rs"]),
        (json!(["docs"]), vec!["z.md"]),
        (json!(["markup"]), vec!["z.html", "z.xml", "z.css"]),
        (json!(["config"]), configs.to_vec()),
        (
            json!(["code", "config"]),
            [configs.to_vec(), vec!["z.rs"]].concat(),
        ),
        (
            json!("all"),
            [
                configs.to_vec(),
                vec!["z.rs", "z.md", "z.html", "z.xml", "z.css"],
            ]
            .concat(),
        ),
    ] {
        let expected: BTreeSet<_> = paths.into_iter().collect();
        let options = json!({"formats":formats, "private":true, "minSimilarity":-1});
        let map = engine.map(&options)?;
        assert_eq!(
            map.iter()
                .map(|row| row["path"].as_str().unwrap())
                .collect::<BTreeSet<_>>(),
            expected,
            "{options:?}"
        );
        for command in ["search", "search-descriptions", "search-symbols"] {
            let rows = engine.search("copper token", command, &options)?;
            assert_eq!(
                rows.iter().map(result_path).collect::<BTreeSet<_>>(),
                expected,
                "{command}: {options:?}"
            );
            assert_eq!(rows, engine.search("copper token", command, &options)?);
            let mut limited = options.clone();
            limited["limit"] = json!(2);
            let top = engine.search("copper token", command, &limited)?;
            assert_eq!(top.len(), rows.len().min(2));
            assert!(top.iter().all(|row| expected.contains(result_path(row))));
        }
    }
    for formats in [json!("unknown"), json!(true), json!(["code", 1]), json!("")] {
        let options = json!({"formats":formats});
        assert!(engine.map(&options).is_err(), "{options:?}");
        for command in [
            "search",
            "search-docs",
            "search-md",
            "search-symbols",
            "search-descriptions",
        ] {
            assert!(
                engine.search("copper token", command, &options).is_err(),
                "{command}: {options:?}"
            );
        }
        assert!(engine.cross_search(None, &options).is_err(), "{options:?}");
    }
    Ok(())
}

#[test]
fn cross_search_is_code_only_and_keeps_target_globs_and_names_source_only() -> Result<()> {
    let source = Fixture::with_sources(
        false,
        &[
            (
                "source.rs",
                "pub fn source() {\n    println!(\"source\");\n}\n",
            ),
            ("source.JSON", "{\"limit\":1}\n"),
        ],
    )?;
    let target = Fixture::with_sources(
        false,
        &[
            (
                "target.rs",
                "pub fn target() {\n    println!(\"target\");\n}\n",
            ),
            ("target.TfVaRs", "limit = 1\n"),
        ],
    )?;
    for (fixture, config_path) in [(&source, "source.JSON"), (&target, "target.TfVaRs")] {
        let mut db = Database::open_with_config(
            &fixture.index,
            fixture.temp.path(),
            &fixture.config,
            false,
        )?;
        let file = db
            .files()?
            .into_iter()
            .find(|file| file.path == config_path)
            .unwrap();
        // Config parsers have no callables. Publish a synthetic legacy callable
        // to exercise the query-layer gate independently of parser behavior.
        let parsed = parse::parse(
            "synthetic.rs",
            "pub fn run() {\n    println!(\"configuration callable\");\n}\n",
        )?;
        db.apply_structure(&[(file, parsed)], &[], None)?;
    }
    source.seed_code_vectors(&[("source.rs", [1.0, 0.0]), ("source.JSON", [1.0, 0.0])])?;
    target.seed_code_vectors(&[("target.rs", [0.8, 0.6]), ("target.TfVaRs", [1.0, 0.0])])?;
    for fixture in [&source, &target] {
        fixture.seed_symbols(false)?;
        let db = Database::open_with_config(
            &fixture.index,
            fixture.temp.path(),
            &fixture.config,
            false,
        )?;
        let mut profile = Providers::new(&fixture.config)?.embedding_profile();
        profile["symbolNormalizationVersion"] = json!("symbols-v1");
        for name in ["source", "target"] {
            db.put_embedding(&Database::embedding_key(&profile, false, name), &[0.0, 1.0])?;
        }
    }
    let source_engine = source.engine()?;
    let target_engine = target.engine()?;
    let mut options =
        json!({"glob":"source.rs", "regexp":"^source$", "minSimilarity":0, "matches":1});
    let rows = source_engine.cross_search(Some(&target_engine), &options)?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["source"]["path"], "source.rs");
    assert_eq!(rows[0]["matches"][0]["function"]["path"], "target.rs");
    options["formats"] = json!("all");
    let all = source_engine.cross_search(Some(&target_engine), &options)?;
    assert_eq!(all, rows, "config callables must never enter cross-search");
    assert_eq!(
        all,
        source_engine.cross_search(Some(&target_engine), &options)?
    );
    options = json!({"sourcePath":"source.JSON", "minSimilarity":0, "matches":1});
    assert!(
        source_engine
            .cross_search(Some(&target_engine), &options)?
            .is_empty()
    );
    options["formats"] = json!("all");
    let configs = source_engine.cross_search(Some(&target_engine), &options)?;
    assert!(configs.is_empty(), "{configs:?}");
    for formats in [json!(["config"]), json!(["docs", "markup"])] {
        assert!(
            source_engine
                .cross_search(
                    Some(&target_engine),
                    &json!({"formats":formats, "minSimilarity":0, "symbolQuery":"never embedded"})
                )?
                .is_empty()
        );
    }
    assert!(
        source_engine
            .search(
                "copper token",
                "search-docs",
                &json!({"formats":"config", "symbolQuery":"never embedded"})
            )?
            .is_empty()
    );
    drop(source_engine);
    drop(target_engine);
    for fixture in [&source, &target] {
        let engine = fixture.engine()?;
        let defaults = engine.search(
            "copper token",
            "search-code",
            &json!({"minSimilarity":-1, "limit":1}),
        )?;
        assert_eq!(defaults.len(), 1);
        assert!(result_path(&defaults[0]).ends_with(".rs"));
        let all = engine.search(
            "copper token",
            "search-code",
            &json!({"minSimilarity":-1, "formats":"all"}),
        )?;
        assert_eq!(all, defaults);
        let configs = engine.search(
            "copper token",
            "search",
            &json!({"minSimilarity":-1, "formats":["config"]}),
        )?;
        assert!(
            configs.iter().any(|row| row["type"] == "function"),
            "{configs:?}"
        );
        assert!(
            configs.iter().all(|row| !result_path(row).ends_with(".rs")),
            "{configs:?}"
        );
        let db = Database::open_with_config(
            &fixture.index,
            fixture.temp.path(),
            &fixture.config,
            false,
        )?;
        for group in ["code", "config"] {
            let pointer: Value = serde_json::from_slice(&fs::read(
                fixture
                    .index
                    .with_extension(format!("sqlite.{group}.shared.json")),
            )?)?;
            let expected: BTreeSet<_> = db
                .items()?
                .iter()
                .filter(|item| {
                    item.kind == "function" && item.path.ends_with(".rs") == (group == "code")
                })
                .map(|item| item.id.to_string())
                .collect();
            assert_eq!(
                pointer["membership"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>(),
                expected,
                "{group}"
            );
        }
    }
    Ok(())
}

#[test]
fn default_search_unions_all_indexes_and_explicit_selectors_agree() -> Result<()> {
    let fixture = Fixture::new(true)?;
    let engine = fixture.engine()?;
    let options = json!({"minSimilarity":-1});
    let all = engine.search("copper token", "search", &options)?;
    let mut union = BTreeSet::new();
    for (selector, command) in [
        ("code", "search-code"),
        ("descriptions", "search-descriptions"),
        ("docs", "search-docs"),
        ("symbols", "search-symbols"),
    ] {
        let direct = engine.search("copper token", command, &options)?;
        let mut explicit = options.clone();
        explicit[selector] = json!(true);
        assert_eq!(
            hit_keys(&direct),
            hit_keys(&engine.search("copper token", "search", &explicit)?)
        );
        union.extend(hit_keys(&direct));
    }
    assert_eq!(hit_keys(&all), union);
    let docs = engine.search("copper token", "search-docs", &options)?;
    assert_eq!(docs, engine.search("copper token", "search-md", &options)?);
    assert_eq!(
        hit_keys(&docs),
        hit_keys(&engine.search(
            "copper token",
            "search",
            &json!({"minSimilarity":-1, "md":true})
        )?)
    );
    assert!(all.iter().any(|row| row["type"] == "file"));
    assert!(
        all.iter()
            .any(|row| row["type"] == "file" && row["file"]["path"] == "settings.rs")
    );
    assert!(
        all.iter()
            .any(|row| row["type"] == "symbol" && row["symbol"]["name"] == "LIMIT")
    );
    assert!(
        all.iter()
            .any(|row| row["type"] == "function" && row["function"]["path"] == "b.rs")
    );
    assert!(all.iter().any(|row| row["type"] == "markdown"));
    for kind in ["code", "docs", "descriptions", "symbols"] {
        let pointer: Value = serde_json::from_slice(&fs::read(
            fixture
                .index
                .with_extension(format!("sqlite.{kind}.shared.json")),
        )?)?;
        assert_eq!(pointer["version"], 1);
        assert!(!pointer["membership"].as_object().unwrap().is_empty());
        assert_eq!(pointer["fingerprint"].as_str().unwrap().len(), 64);
        assert!(
            !fixture
                .index
                .with_extension(format!("sqlite.{kind}.usearch"))
                .exists()
        );
    }
    assert!(
        !fixture
            .index
            .with_extension("sqlite.combined.shared.json")
            .exists()
    );
    Ok(())
}

#[test]
fn description_similarity_is_independent_of_code_and_file_prose() -> Result<()> {
    let fixture = Fixture::new(true)?;
    let description_index = fixture
        .index
        .with_extension("sqlite.descriptions.shared.json");
    // An incompatible shared pointer must be replaced using durable vectors.
    fs::write(
        &description_index,
        br#"{"version":0,"contract":"old-concatenated-descriptions"}"#,
    )?;
    let engine = fixture.engine()?;
    let rows = engine.search(
        "copper token",
        "search-descriptions",
        &json!({"minSimilarity":0.9}),
    )?;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let function = rows.iter().find(|row| row["type"] == "function").unwrap();
    assert!(function["similarity"].as_f64().unwrap() > 0.99);
    assert_eq!(function["codeSimilarity"], 0.0);
    assert_eq!(function["functionDescriptionSimilarity"], 1.0);
    assert_eq!(function["fileDescriptionSimilarity"], 0.0);
    let symbol = rows.iter().find(|row| row["type"] == "symbol").unwrap();
    assert_eq!(symbol["symbol"]["name"], "LIMIT");
    assert!(symbol.get("codeSimilarity").is_none());
    // File descriptions are scored independently without synthetic item IDs.
    let all = engine.search(
        "copper token",
        "search-descriptions",
        &json!({"minSimilarity":-1}),
    )?;
    assert_eq!(all.len(), 4);
    assert_eq!(all.iter().filter(|row| row["type"] == "file").count(), 2);
    let manifest: Value = serde_json::from_str(&fs::read_to_string(&description_index)?)?;
    assert_eq!(manifest["membership"].as_object().unwrap().len(), 2);
    assert!(manifest["delta"].as_object().unwrap().is_empty());
    let base: Value = serde_json::from_slice(&fs::read(
        PathBuf::from(format!(
            "{}.indexes",
            fixture.config["artifactCachePath"].as_str().unwrap()
        ))
        .join(manifest["contract"].as_str().unwrap())
        .join(format!(
            "{}.base.json",
            manifest["base_id"].as_str().unwrap()
        )),
    )?)?;
    assert_eq!(base["dimensions"], 2);
    assert_eq!(
        base["embeddings"].as_object().unwrap().len(),
        1,
        "identical callable and symbol prose is stored once despite two live occurrences"
    );
    Ok(())
}

#[test]
fn cross_search_description_scores_do_not_affect_ranking_limits_or_thresholds() -> Result<()> {
    let fixture = Fixture::with_sources(
        true,
        &[
            (
                "source.rs",
                "//! source overview\n/// source description\npub fn source() {\n    println!(\"source\");\n}\n",
            ),
            (
                "high.rs",
                "//! high overview\n/// high description\npub fn high() {\n    println!(\"high\");\n}\n",
            ),
            (
                "middle.rs",
                "//! middle overview\n/// middle description\npub fn middle() {\n    println!(\"middle\");\n}\n",
            ),
            (
                "low.rs",
                "//! low overview\n/// low description\npub fn low() {\n    println!(\"low\");\n}\n",
            ),
        ],
    )?;
    let db =
        Database::open_with_config(&fixture.index, fixture.temp.path(), &fixture.config, false)?;
    // Embeddings are immutable; replace the fixture's uniform vectors before opening.
    db.conn.execute("DELETE FROM global.embeddings", [])?;
    let profile = Providers::new(&fixture.config)?.embedding_profile();
    let files = db.files()?;
    for item in db.items()? {
        let (code, description) = match item.path.as_str() {
            "source.rs" => ([1.0, 0.0], [1.0, 0.0]),
            "high.rs" => ([1.0, 0.0], [-1.0, 0.0]),
            "middle.rs" => ([0.8, 0.6], [0.0, 1.0]),
            "low.rs" => ([0.6, 0.8], [1.0, 0.0]),
            path => panic!("Unexpected fixture path: {path}"),
        };
        let file = files.iter().find(|file| file.path == item.path).unwrap();
        for (text, vector) in [
            (item.data["embeddingInput"].as_str().unwrap(), code),
            (item.data["description"].as_str().unwrap(), description),
            (file.description.as_deref().unwrap(), description),
        ] {
            db.put_embedding(&Database::embedding_key(&profile, false, text), &vector)?;
        }
    }
    drop(db);
    let engine = fixture.engine()?;
    let options = json!({"sourcePath":"source.rs", "crossFileOnly":true, "minSimilarity":0});
    let rows = engine.cross_search(None, &options)?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["scoring"],
        json!({
            "similarityMode":"code",
            "similarityWeights":{"code":1,"description":0,"fileDescription":0}
        })
    );
    let matches = rows[0]["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 3);
    // Description rankings are the reverse of code rankings for both scopes.
    for (row, (name, code, description)) in
        matches
            .iter()
            .zip([("high", 1.0, -1.0), ("middle", 0.8, 0.0), ("low", 0.6, 1.0)])
    {
        assert_eq!(row["function"]["name"], name);
        for key in ["similarity", "codeSimilarity"] {
            assert!((row[key].as_f64().unwrap() - code).abs() < 1e-6, "{row:?}");
        }
        for key in [
            "descriptionSimilarity",
            "functionDescriptionSimilarity",
            "fileDescriptionSimilarity",
        ] {
            assert_eq!(row[key], description, "{row:?}");
        }
    }
    assert_eq!(rows, engine.cross_search(None, &options)?);
    for (filters, expected) in [
        (json!({"matches":1}), vec!["high"]),
        (json!({"minSimilarity":0.7}), vec!["high", "middle"]),
        (json!({"minSimilarity":0.9}), vec!["high"]),
        (
            json!({"minSimilarity":0.7,"maxSimilarity":0.9}),
            vec!["middle"],
        ),
    ] {
        let mut options = options.clone();
        options
            .as_object_mut()
            .unwrap()
            .extend(filters.as_object().unwrap().clone());
        let rows = engine.cross_search(None, &options)?;
        assert_eq!(rows.len(), 1, "{rows:?}");
        let names: Vec<_> = rows[0]["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, expected, "{options:?}: {rows:?}");
    }
    Ok(())
}

#[test]
fn cross_search_works_with_existing_unembedded_descriptions() -> Result<()> {
    let fixture = Fixture::new(false)?;
    let engine = fixture.engine()?;
    let rows = engine.cross_search(None, &json!({"minSimilarity":0.9, "crossFileOnly":true}))?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["scoring"]["similarityMode"], "code");
    assert!(rows[0]["matches"][0]["similarity"].as_f64().unwrap() > 0.99);
    assert!(
        rows[0]["matches"][0]
            .get("functionDescriptionSimilarity")
            .is_none()
    );
    assert!(
        rows[0]["matches"][0]
            .get("fileDescriptionSimilarity")
            .is_none()
    );
    Ok(())
}

#[test]
fn cross_search_exclusions_filter_same_index_hashes_before_limits_and_preserve_search() -> Result<()>
{
    let excluded_source = "pub fn duplicate() {\n    println!(\"old\");\n}\n";
    let mut fixture = Fixture::with_sources(
        false,
        &[
            (
                "source.rs",
                "pub fn source() {\n    println!(\"source\");\n}\n",
            ),
            ("a.rs", excluded_source),
            ("b.rs", excluded_source),
            (
                "changed.rs",
                "pub fn duplicate() {\n    println!(\"new\");\n}\n",
            ),
            (
                "fallback.rs",
                "pub fn fallback() {\n    println!(\"fallback\");\n}\n",
            ),
        ],
    )?;
    fixture.seed_code_vectors(&[
        ("source.rs", [1.0, 0.0]),
        ("a.rs", [1.0, 0.0]),
        ("b.rs", [1.0, 0.0]),
        ("changed.rs", [0.8, 0.6]),
        ("fallback.rs", [0.6, 0.8]),
    ])?;
    let excluded_hash = fixture.source_hash("a.rs")?;
    assert_eq!(excluded_hash, fixture.source_hash("b.rs")?);
    assert_ne!(excluded_hash, fixture.source_hash("changed.rs")?);
    let options = json!({
        "crossFileOnly":true, "includeSymmetricDuplicates":true,
        "minSimilarity":0, "matches":1
    });
    let mut selected = options.clone();
    selected["sourcePath"] = json!("source.rs");
    let query_options = json!({"minSimilarity":-1});
    let engine = fixture.engine()?;
    let baseline = engine.cross_search(None, &options)?;
    let selected_baseline = engine.cross_search(None, &selected)?;
    let search_baseline = engine.search("copper token", "search-code", &query_options)?;
    assert_eq!(baseline.len(), 5, "{baseline:?}");
    assert_eq!(search_baseline.len(), 5);
    assert_eq!(selected_baseline.len(), 1);
    assert!(
        ["a.rs", "b.rs"]
            .iter()
            .any(|path| { selected_baseline[0]["matches"][0]["function"]["path"] == *path })
    );
    assert_eq!(baseline, engine.cross_search(None, &options)?);
    drop(engine);

    fixture.config["crossSearchExclusions"] = json!([excluded_hash]);
    let engine = fixture.engine()?;
    let rows = engine.cross_search(None, &options)?;
    let paths: BTreeSet<_> = rows
        .iter()
        .map(|row| row["source"]["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        BTreeSet::from(["source.rs", "changed.rs", "fallback.rs"])
    );
    for row in &rows {
        let matches = row["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1, "{row:?}");
        assert!(
            matches.iter().all(|hit| {
                hit["function"]["path"] != "a.rs" && hit["function"]["path"] != "b.rs"
            })
        );
    }
    let selected_rows = engine.cross_search(None, &selected)?;
    assert_eq!(selected_rows.len(), 1, "{selected_rows:?}");
    assert_eq!(selected_rows[0]["matches"].as_array().unwrap().len(), 1);
    assert_eq!(
        selected_rows[0]["matches"][0]["function"]["path"],
        "changed.rs"
    );
    assert_eq!(rows, engine.cross_search(None, &options)?);
    assert_eq!(
        search_baseline,
        engine.search("copper token", "search-code", &query_options)?
    );
    drop(engine);

    fixture
        .config
        .as_object_mut()
        .unwrap()
        .remove("crossSearchExclusions");
    let engine = fixture.engine()?;
    assert_eq!(baseline, engine.cross_search(None, &options)?);
    assert_eq!(selected_baseline, engine.cross_search(None, &selected)?);
    Ok(())
}

#[test]
fn cross_search_unions_external_configs_and_tracks_both_exclusions_in_cache() -> Result<()> {
    let source_excluded = "pub fn source_excluded() {\n    println!(\"source exclusion\");\n}\n";
    let target_excluded = "pub fn target_excluded() {\n    println!(\"target exclusion\");\n}\n";
    let mut source_fixture = Fixture::with_sources(
        false,
        &[
            ("keep.rs", "pub fn keep() {\n    println!(\"keep\");\n}\n"),
            ("source_excluded.rs", source_excluded),
            ("target_excluded.rs", target_excluded),
        ],
    )?;
    let mut target_fixture = Fixture::with_sources(
        false,
        &[
            ("source_copy.rs", source_excluded),
            ("target_copy.rs", target_excluded),
            (
                "fallback.rs",
                "pub fn fallback() {\n    println!(\"fallback\");\n}\n",
            ),
        ],
    )?;
    source_fixture.seed_code_vectors(&[
        ("keep.rs", [1.0, 0.0]),
        ("source_excluded.rs", [1.0, 0.0]),
        ("target_excluded.rs", [1.0, 0.0]),
    ])?;
    target_fixture.seed_code_vectors(&[
        ("source_copy.rs", [1.0, 0.0]),
        ("target_copy.rs", [0.8, 0.6]),
        ("fallback.rs", [0.6, 0.8]),
    ])?;
    let source_hash = source_fixture.source_hash("source_excluded.rs")?;
    let target_hash = target_fixture.source_hash("target_copy.rs")?;
    assert_eq!(source_hash, target_fixture.source_hash("source_copy.rs")?);
    assert_eq!(
        target_hash,
        source_fixture.source_hash("target_excluded.rs")?
    );
    let options = json!({"crossFileOnly":true, "minSimilarity":0, "matches":1});
    let source = source_fixture.engine()?;
    let target = target_fixture.engine()?;
    let baseline = source.cross_search(Some(&target), &options)?;
    assert_eq!(baseline.len(), 3, "{baseline:?}");
    assert!(
        baseline
            .iter()
            .all(|row| row["matches"][0]["function"]["path"] == "source_copy.rs")
    );
    assert_eq!(baseline, source.cross_search(Some(&target), &options)?);
    drop(source);

    source_fixture.config["crossSearchExclusions"] = json!([source_hash]);
    let source = source_fixture.engine()?;
    let source_only = source.cross_search(Some(&target), &options)?;
    assert_eq!(source_only.len(), 2, "{source_only:?}");
    assert!(source_only.iter().all(|row| {
        row["source"]["path"] != "source_excluded.rs"
            && row["matches"].as_array().unwrap().len() == 1
            && row["matches"][0]["function"]["path"] == "target_copy.rs"
    }));
    drop(target);

    target_fixture.config["crossSearchExclusions"] = json!([target_hash]);
    let target = target_fixture.engine()?;
    let both = source.cross_search(Some(&target), &options)?;
    assert_eq!(both.len(), 1, "{both:?}");
    assert_eq!(both[0]["source"]["path"], "keep.rs");
    assert_eq!(both[0]["matches"].as_array().unwrap().len(), 1);
    assert_eq!(both[0]["matches"][0]["function"]["path"], "fallback.rs");
    assert_eq!(both, source.cross_search(Some(&target), &options)?);
    drop(source);

    source_fixture.config["crossSearchExclusions"] = json!([]);
    let source = source_fixture.engine()?;
    let target_only = source.cross_search(Some(&target), &options)?;
    assert_eq!(target_only.len(), 2, "{target_only:?}");
    assert!(target_only.iter().all(|row| {
        row["source"]["path"] != "target_excluded.rs"
            && row["matches"][0]["function"]["path"] == "source_copy.rs"
    }));
    drop(target);

    target_fixture.config["crossSearchExclusions"] = json!([]);
    let target = target_fixture.engine()?;
    assert_eq!(baseline, source.cross_search(Some(&target), &options)?);
    Ok(())
}

#[test]
fn cross_search_excluded_function_returns_after_its_source_hash_changes() -> Result<()> {
    let original = "pub fn duplicate() {\n    println!(\"old\");\n}\n";
    let changed = "pub fn duplicate() {\n    println!(\"new\");\n}\n";
    let mut fixture = Fixture::with_sources(
        false,
        &[
            (
                "source.rs",
                "pub fn source() {\n    println!(\"source\");\n}\n",
            ),
            ("edited.rs", original),
            ("unchanged.rs", original),
        ],
    )?;
    let excluded_hash = fixture.source_hash("edited.rs")?;
    fixture.config["crossSearchExclusions"] = json!([excluded_hash]);
    let options = json!({"sourcePath":"source.rs", "crossFileOnly":true, "matches":1});
    let engine = fixture.engine()?;
    assert!(engine.cross_search(None, &options)?.is_empty());
    assert!(engine.cross_search(None, &options)?.is_empty());
    drop(engine);

    fs::write(fixture.temp.path().join("edited.rs"), changed)?;
    let parsed = parse::parse("edited.rs", changed)?;
    assert_ne!(parsed.callables[0].source_hash, excluded_hash);
    let mut db =
        Database::open_with_config(&fixture.index, fixture.temp.path(), &fixture.config, false)?;
    let mut file = db
        .files()?
        .into_iter()
        .find(|file| file.path == "edited.rs")
        .unwrap();
    file.hash = hash(changed);
    file.source = changed.into();
    let profile = Providers::new(&fixture.config)?.embedding_profile();
    db.put_embedding(
        &Database::embedding_key(&profile, false, &parsed.callables[0].embedding_input),
        &[0.0, 1.0],
    )?;
    db.apply_structure(&[(file, parsed)], &[], None)?;
    drop(db);

    let engine = fixture.engine()?;
    let rows = engine.cross_search(None, &options)?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["matches"].as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["matches"][0]["function"]["path"], "edited.rs");
    assert_eq!(rows, engine.cross_search(None, &options)?);
    Ok(())
}

#[test]
fn code_search_description_selectors_are_precise_and_union_regex() -> Result<()> {
    let fixture = Fixture::new(false)?;
    let engine = fixture.engine()?;
    let mut options = json!({
        "symbolQuery":"copper token", "symbolThreshold":0.9, "minSimilarity":-1
    });
    let rows = engine.search("copper token", "search-code", &options)?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["function"]["path"], "a.rs");
    assert_eq!(
        rows,
        engine.search("copper token", "search-code", &options)?
    );
    options["regexp"] = json!("^run$");
    let union = engine.search("copper token", "search-code", &options)?;
    assert_eq!(union.len(), 2, "{union:?}");
    Ok(())
}

#[test]
fn structural_map_description_queries_select_precise_declarations() -> Result<()> {
    let fixture = Fixture::new(false)?;
    fixture.seed_symbols(true)?;
    let engine = Engine::open_map(fixture.temp.path(), &fixture.index, fixture.config.clone())?;
    let options = json!({"symbolQuery":"copper token", "symbolThreshold":0.9, "private":true});
    let rows = engine.map(&options)?;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["path"], "a.rs");
    assert_eq!(rows[0]["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(rows, engine.map(&options)?);
    let db =
        Database::open_with_config(&fixture.index, fixture.temp.path(), &fixture.config, false)?;
    assert!(
        db.items()?
            .iter()
            .all(|item| item.description_embedding.is_none())
    );
    let generation = db.generation()?;
    db.conn.execute(
        "INSERT OR IGNORE INTO global.description_content(hash,content) VALUES(?,'silver token')",
        [hash("silver token")],
    )?;
    db.conn.execute(
        "UPDATE descriptions SET content_hash=? WHERE scope='callable' AND path='a.rs'
         AND identity=(SELECT identity FROM search_units WHERE path='a.rs' AND kind='function')",
        [hash("silver token")],
    )?;
    assert_eq!(db.generation()?, generation);
    let changed = engine.map(&options)?;
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0]["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(changed[0]["nodes"][0]["name"], "LIMIT");
    Ok(())
}

#[test]
fn refresh_and_generate_preserve_source_comments_without_llm_requests() -> Result<()> {
    let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize()?)?;
    fs::create_dir(temp.path().join(".slopdex"))?;
    let index = temp.path().join(".slopdex/index.sqlite");
    let config = test_config(&temp);
    let profile = Providers::new(&config)?.embedding_profile();
    let db = Database::open_with_config(&index, temp.path(), &config, false)?;
    // Seed the configured global store before publishing workspace bindings.
    for (path, source) in [
        (
            "source.rs",
            "//! file overview\n\n/// copper token\npub fn run() { println!(\"hello\"); }\n/// copper token\npub const LIMIT: usize = 1;\n/// silver token\npub type Kind = usize;\n",
        ),
        (
            "source.ts",
            "// file overview\n\n// copper token\nexport function run() { return 1; }\n// copper token\nexport const LIMIT = 1;\n",
        ),
        (
            "guide.md",
            "<!-- file overview -->\n\n<!-- copper token -->\n# Guide\n\nInstructions.\n",
        ),
    ] {
        fs::write(temp.path().join(path), source)?;
        let parsed = parse::parse(path, source)?;
        assert_eq!(
            parsed.description.as_deref(),
            Some("file overview"),
            "{path}"
        );
        for input in parsed
            .callables
            .iter()
            .map(|callable| &callable.embedding_input)
            .chain(parsed.chunks.iter().map(|chunk| &chunk.embedding_input))
        {
            db.put_embedding(
                &Database::embedding_key(&profile, false, input),
                &[0.0, 1.0],
            )?;
        }
        for text in parsed
            .structure
            .nodes
            .iter()
            .filter_map(|node| node.description.as_ref())
            .chain(
                parsed
                    .callables
                    .iter()
                    .filter_map(|callable| callable.description.as_ref()),
            )
            .chain(parsed.description.as_ref())
        {
            let vector = if text == "copper token" {
                [1.0, 0.0]
            } else {
                [0.0, 1.0]
            };
            db.put_embedding(&Database::embedding_key(&profile, false, text), &vector)?;
        }
    }
    db.put_embedding(
        &Database::embedding_key(&profile, true, "copper token"),
        &[1.0, 0.0],
    )?;
    drop(db);
    let fixture = Fixture {
        temp,
        index,
        config,
    };
    fixture.seed_symbols(false)?;
    let mut engine = fixture.engine()?;
    let refreshed = engine.refresh().context("Refresh source-only fixture")?;
    assert_eq!(refreshed["filesUpdated"], 3);
    assert_eq!(
        refreshed["filesPrepared"], 0,
        "all vectors were seeded globally"
    );
    assert!(engine.errors()?.is_empty());
    let options = json!({"minSimilarity":0.9});
    let before = engine
        .search("copper token", "search-descriptions", &options)
        .context("Search prepared source descriptions")?;
    assert_eq!(before.len(), 5, "{before:?}");
    assert_eq!(
        before
            .iter()
            .filter(|row| row["type"] == "function")
            .count(),
        2
    );
    assert_eq!(
        before.iter().filter(|row| row["type"] == "symbol").count(),
        3
    );
    for row in &before {
        let item = if row["type"] == "function" {
            &row["function"]
        } else {
            &row["symbol"]
        };
        assert_eq!(item["description"], "copper token");
        assert_eq!(item["sourceDescription"], true);
    }
    let selected = engine
        .map(&json!({
            "symbolQuery":"copper token", "symbolThreshold":0.9, "glob":"*.md", "private":true
        }))
        .context("Select source-described heading")?;
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0]["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(selected[0]["nodes"][0]["name"], "Guide");
    let generated = engine
        .generate_descriptions()
        .context("Generate from source comments")?;
    assert_eq!(generated["filesPrepared"], 0);
    assert_eq!(generated["fileDescriptionCount"], 3);
    assert_eq!(
        before,
        engine
            .search("copper token", "search-descriptions", &options)
            .context("Search after generate")?
    );
    assert_eq!(
        engine.refresh().context("Repeat source-only refresh")?["filesPrepared"],
        0
    );
    drop(engine);
    let db = Database::open_readonly(&fixture.index, fixture.temp.path())?;
    for item in db.items_for_profile(&profile)? {
        if item.data["description"].is_string() {
            assert_eq!(item.data["sourceDescription"], true);
            assert!(item.description_embedding.is_some());
        }
        if item.kind == "symbol-description" {
            assert!(item.embedding.is_empty());
        }
    }
    assert!(db.files_for_profile(&profile)?.iter().all(|file| {
        file.description.as_deref() == Some("file overview")
            && file
                .description_hash
                .as_deref()
                .is_some_and(|hash| hash.starts_with("source:"))
            && file.description_embedding.is_some()
    }));
    drop(db);
    assert_eq!(
        before,
        fixture
            .engine()?
            .search("copper token", "search-descriptions", &options)
            .context("Search reopened fixture")?
    );
    Ok(())
}
