//! Search regressions use durable, deterministic vectors and make no model calls.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use slopdex::{
    cache::Artifacts,
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
        let temp = tempfile::tempdir()?;
        let index = temp.path().join("index.sqlite");
        let config = test_config(&temp);
        let mut db = Database::open(&index, temp.path(), &Value::Null, false)?;
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
        let db = Database::open(&self.index, self.temp.path(), &Value::Null, false)?;
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
fn default_search_unions_all_indexes_and_explicit_selectors_agree() -> Result<()> {
    let fixture = Fixture::new(true)?;
    let engine = fixture.engine()?;
    let options = json!({"minSimilarity":-1});
    let all = engine.search("copper token", "search", &options)?;
    let mut union = BTreeSet::new();
    for (selector, command) in [
        ("code", "search-code"),
        ("descriptions", "search-descriptions"),
        ("md", "search-md"),
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
    assert!(
        !fixture
            .index
            .with_extension("sqlite.combined.usearch")
            .exists()
    );
    Ok(())
}

#[test]
fn description_similarity_is_independent_of_code_and_file_prose() -> Result<()> {
    let fixture = Fixture::new(true)?;
    let db = Database::open_readonly(&fixture.index, fixture.temp.path())?;
    let item = db
        .items()?
        .into_iter()
        .find(|item| item.kind == "function")
        .unwrap();
    let description_index = fixture.index.with_extension("sqlite.descriptions.usearch");
    // A disposable index from the old callable+file concatenation is rebuilt.
    slopdex::vectors::VectorIndex::open(
        &description_index,
        4,
        db.generation()?,
        &[(item.id, vec![1.0, 0.0, 0.0, 1.0])],
    )?;
    drop(db);
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
    let manifest: Value = serde_json::from_str(&fs::read_to_string(
        description_index.with_extension("usearch.manifest.json"),
    )?)?;
    assert_eq!(manifest["dimensions"], 2);
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
    let db = Database::open(&fixture.index, fixture.temp.path(), &Value::Null, false)?;
    // Embeddings are immutable; replace the fixture's uniform vectors before opening.
    db.conn.execute("DELETE FROM embeddings", [])?;
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
    let db = Database::open(&fixture.index, fixture.temp.path(), &Value::Null, false)?;
    assert!(
        db.items()?
            .iter()
            .all(|item| item.description_embedding.is_none())
    );
    let generation = db.generation()?;
    db.conn.execute(
        "UPDATE descriptions SET text='silver token' WHERE scope='callable' AND path='a.rs'
         AND identity=(SELECT identity FROM search_units WHERE path='a.rs' AND kind='function')",
        [],
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
    let temp = tempfile::tempdir()?;
    fs::create_dir(temp.path().join(".slopdex"))?;
    let index = temp.path().join(".slopdex/index.sqlite");
    let config = test_config(&temp);
    let profile = Providers::new(&config)?.embedding_profile();
    let db = Database::open(&index, temp.path(), &Value::Null, false)?;
    // Keep vectors out of the workspace DB so refresh must prepare the new
    // records and hydrate their content-addressed vectors from the local cache.
    let artifacts = Artifacts::open(&config);
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
            artifacts.put_embedding(
                &db,
                &Database::embedding_key(&profile, false, input),
                &[0.0, 1.0],
            );
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
            artifacts.put_embedding(
                &db,
                &Database::embedding_key(&profile, false, text),
                &vector,
            );
        }
    }
    artifacts.put_embedding(
        &db,
        &Database::embedding_key(&profile, true, "copper token"),
        &[1.0, 0.0],
    );
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
    assert_eq!(refreshed["filesPrepared"], 3);
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
