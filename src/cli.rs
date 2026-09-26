//! Command-line parsing, configuration, and presentation. Engine operations live in engine.rs.

use crate::{engine::Engine, providers::Providers, ui};
use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{self, IsTerminal, Write},
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Parser)]
#[command(
    name = "slopdex",
    version,
    about = "Semantic code and Markdown search",
    after_help = "Examples:\n  slopdex search \"validate an authenticated session\"\n  slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.85-0.9\n  slopdex describe \"I want to implement a new rpc endpoint\"\n  slopdex config\n\nIndex commands refresh automatically. --no-reindex reuses the index offline."
)]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Args)]
struct Global {
    /// Repository root (defaults to the current directory)
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    /// Config file (default: <root>/.slopdex/config.json); explicit paths are relative to cwd
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Index file; overrides config indexPath; explicit paths are relative to cwd
    #[arg(long, global = true)]
    index: Option<PathBuf>,
    /// Embedding provider (default: openai)
    #[arg(long, id = "embedding_provider", global = true, value_parser = ["openai", "jina"])]
    provider: Option<String>,
    /// Embedding model (default: text-embedding-3-large / jina-embeddings-v4)
    #[arg(long, id = "embedding_model", global = true, value_parser = nonempty)]
    model: Option<String>,
    /// Embedding dimensions (default: OpenAI 3072 / Jina 1024)
    #[arg(long, global = true, value_parser = positive)]
    dimensions: Option<usize>,
    /// Description provider (default: openai, or the saved profile)
    #[arg(long, global = true, value_parser = ["openai", "opencode", "opencode-go"])]
    description_provider: Option<String>,
    #[arg(long, global = true, value_parser = nonempty)]
    description_model: Option<String>,
    /// Optional same-provider fallback description model
    #[arg(long, global = true, value_parser = nonempty)]
    description_fallback_model: Option<String>,
    /// OpenAI reranker candidate count (1..=100; default: 10)
    #[arg(long, global = true, value_parser = candidates)]
    reranker_candidates: Option<usize>,
    /// summary by default; cross-search uses clusters, cohesion uses summary; cross JSON is JSONL
    #[arg(long, global = true, value_enum)]
    format: Option<Format>,
    /// Reuse the existing index offline, without automatic refresh
    #[arg(long, global = true, conflicts_with = "force_reindex")]
    no_reindex: bool,
    /// Reset live index state while preserving reusable caches
    #[arg(long, global = true, requires = "yes_really_rebuild_the_index")]
    force_reindex: bool,
    /// Permit refresh after Git history divergence
    #[arg(long, global = true, requires = "yes_really_rebuild_the_index")]
    rebuild_on_divergence: bool,
    /// Confirm an explicitly requested index rebuild
    #[arg(long, global = true)]
    yes_really_rebuild_the_index: bool,
    /// Suppress saved indexing-error warnings
    #[arg(long, global = true)]
    ignore_errors: bool,
    /// Report every external model request on stderr
    #[arg(long, global = true)]
    verbose: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Format {
    Summary,
    Json,
    Clusters,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Search code, enabled descriptions, and Markdown together
    Search(SearchArgs),
    /// Search callable code only
    SearchCode(QueryArgs),
    /// Search enabled callable and file descriptions
    #[command(alias = "search-description")]
    SearchDescriptions(QueryArgs),
    /// Search heading-aware Markdown chunks
    SearchMd(QueryArgs),
    /// Explain existing code relevant to a task using the description model
    Describe(DescribeArgs),
    /// Compare functions; clusters are connected components of observed matches
    CrossSearch(CrossArgs),
    /// Refresh and show index metadata, counts, and profiles as JSON
    Status,
    /// Refresh and inspect saved file/function indexing failures
    IndexErrors,
    /// Refresh the current working tree and Git HEAD; alias: refresh
    #[command(alias = "refresh")]
    UpdateGit(UpdateArgs),
    /// Enable/disable generated descriptions; preserves cached descriptions
    Descriptions { action: Toggle },
    /// Regenerate stale file descriptions, optionally including their callables
    ReindexFiles {
        #[arg(long)]
        callables: bool,
    },
    /// Fetch published OpenCode model catalogs without opening an index
    Models {
        #[arg(value_parser = ["opencode", "opencode-go"])]
        provider: Option<String>,
    },
    /// Edit configuration without opening an index; no action starts interactive setup
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// Validate and save a published OpenCode description model (bare IDs must be unambiguous)
    Model { model: Option<String> },
    /// Validate and save a fallback model on the configured description provider
    FallbackModel { model: Option<String> },
    /// Set the description state applied by the next index command
    Descriptions { action: Toggle },
    /// Enable a hosted/LLM reranker or disable reranking
    Reranker {
        #[arg(value_parser = ["cohere", "jina", "openai", "disable"])]
        provider: String,
        #[arg(value_parser = nonempty)]
        model: Option<String>,
    },
    /// Set the positive concurrent provider-request limit
    Parallelism {
        #[arg(value_parser = positive)]
        count: usize,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Toggle {
    Enable,
    Disable,
}

impl Toggle {
    fn enabled(self) -> bool {
        matches!(self, Self::Enable)
    }
}

#[derive(Debug, Args)]
struct Filters {
    /// Inclusive minimum, or inclusive-min/exclusive-max range; scores must be in [-1,1]
    #[arg(long, default_value = "0.3", allow_hyphen_values = true, value_parser = threshold)]
    threshold: Threshold,
    /// Positive output limit; default: unlimited. Cross-search limits clusters or matched sources
    #[arg(long, value_parser = positive)]
    limit: Option<usize>,
    /// Case-sensitive qualified-name regex (Rust regex syntax); cross-search filters sources only
    #[arg(short = 'e', long, alias = "regex", value_parser = valid_regex)]
    regexp: Option<String>,
}

impl Filters {
    fn options(&self) -> Value {
        let mut value = json!({"minSimilarity": self.threshold.min});
        if let Some(max) = self.threshold.max {
            value["maxSimilarity"] = json!(max);
        }
        if let Some(limit) = self.limit {
            value["limit"] = json!(limit);
        }
        if let Some(regexp) = &self.regexp {
            value["regexp"] = json!(regexp);
        }
        value
    }
}

#[derive(Debug, Args)]
struct QueryArgs {
    #[arg(value_parser = nonempty)]
    query: String,
    #[command(flatten)]
    filters: Filters,
}

#[derive(Debug, Args)]
struct SearchArgs {
    #[command(flatten)]
    query: QueryArgs,
    /// Select code; if any selector is passed, unselected indexes are omitted
    #[arg(long)]
    code: bool,
    /// Select descriptions (requires enabled descriptions)
    #[arg(long)]
    descriptions: bool,
    /// Select Markdown
    #[arg(long)]
    md: bool,
}

#[derive(Debug, Args)]
struct DescribeArgs {
    #[command(flatten)]
    query: QueryArgs,
    /// Use complete indexed files strictly above this similarity
    #[arg(long, default_value = "0.8", allow_hyphen_values = true, value_parser = similarity)]
    describe_full_file_threshold: f64,
}

#[derive(Debug, Args)]
struct CrossArgs {
    #[command(flatten)]
    filters: Filters,
    /// Matches kept per source function
    #[arg(long, default_value = "5", value_parser = positive)]
    matches: usize,
    /// Minimum length of both source and candidate functions
    #[arg(long, default_value = "2", value_parser = positive)]
    min_lines: usize,
    /// Source file or recursive directory, repo-relative or absolute within the repository
    #[arg(long)]
    source_path: Option<PathBuf>,
    /// Use changed functions since this Git ancestor as sources
    #[arg(long, value_parser = nonempty)]
    changed_since: Option<String>,
    /// Use staged, unstaged, and untracked working-tree functions as sources
    #[arg(long)]
    uncommitted: bool,
    /// Exclude matches from the same physical file
    #[arg(long)]
    cross_file_only: bool,
    /// Keep both directions of same-index pairs
    #[arg(long)]
    include_symmetric_duplicates: bool,
    /// Order each source's matches by descending filesystem distance; summary by default
    #[arg(long)]
    cohesion: bool,
    #[arg(long, requires = "target_index")]
    target_root: Option<PathBuf>,
    #[arg(long, requires = "target_root")]
    target_index: Option<PathBuf>,
    #[arg(long, requires_all = ["target_root", "target_index"])]
    target_config: Option<PathBuf>,
}

impl CrossArgs {
    fn options(&self) -> Value {
        let mut value = self.filters.options();
        // The engine must scan all sources. Limit applies to emitted clusters/rows, never edges.
        value.as_object_mut().unwrap().remove("limit");
        value["matches"] = json!(self.matches);
        value["minLines"] = json!(self.min_lines);
        value["uncommitted"] = json!(self.uncommitted);
        value["crossFileOnly"] = json!(self.cross_file_only);
        value["includeSymmetricDuplicates"] = json!(self.include_symmetric_duplicates);
        value["cohesion"] = json!(self.cohesion);
        if let Some(path) = &self.source_path {
            value["sourcePath"] = json!(path);
        }
        if let Some(reference) = &self.changed_since {
            value["changedSince"] = json!(reference);
        }
        value
    }
}

#[derive(Debug, Args)]
struct UpdateArgs {
    /// This refresh alias currently supports HEAD only
    #[arg(long, default_value = "HEAD", value_parser = ["HEAD"])]
    target: String,
}

#[derive(Clone, Debug, PartialEq)]
struct Threshold {
    min: f64,
    max: Option<f64>,
}

fn nonempty(input: &str) -> std::result::Result<String, String> {
    if input.trim().is_empty() {
        Err("must not be empty".into())
    } else {
        Ok(input.to_owned())
    }
}

fn positive(input: &str) -> std::result::Result<usize, String> {
    input
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| "must be a positive integer".into())
}

fn candidates(input: &str) -> std::result::Result<usize, String> {
    positive(input).and_then(|n| {
        if n <= 100 {
            Ok(n)
        } else {
            Err("must be between 1 and 100".into())
        }
    })
}

fn similarity(input: &str) -> std::result::Result<f64, String> {
    input
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && (-1.0..=1.0).contains(n))
        .ok_or_else(|| "similarity must be finite and in [-1,1]".into())
}

fn threshold(input: &str) -> std::result::Result<Threshold, String> {
    let input = input.trim();
    if let Ok(min) = similarity(input) {
        return Ok(Threshold { min, max: None });
    }
    // Try separators rather than splitting on '-': negative endpoints and exponents both use it.
    for (offset, _) in input.match_indices('-').filter(|(offset, _)| *offset > 0) {
        if let (Ok(min), Ok(max)) = (
            similarity(input[..offset].trim()),
            similarity(input[offset + 1..].trim()),
        ) && min < max
        {
            return Ok(Threshold {
                min,
                max: Some(max),
            });
        }
    }
    Err(
        "threshold must be a number or min-max in [-1,1], with min < max (maximum exclusive)"
            .into(),
    )
}

fn valid_regex(input: &str) -> std::result::Result<String, String> {
    regex::Regex::new(input)
        .map(|_| input.to_owned())
        .map_err(|e| format!("invalid regex: {e}"))
}

impl Cli {
    fn validate(&self) -> Result<()> {
        if self.global.format == Some(Format::Clusters) {
            ensure!(
                matches!(&self.command, Command::CrossSearch(args) if !args.cohesion),
                "clusters format is only available for cross-search without --cohesion; use summary or json"
            );
        }
        match &self.command {
            Command::Config {
                action: Some(ConfigAction::Model { model }),
            } => {
                model_reference(model.as_deref(), self.global.description_model.as_deref())?;
            }
            Command::Config {
                action: Some(ConfigAction::FallbackModel { model }),
            } => {
                model_reference(
                    model.as_deref(),
                    self.global.description_fallback_model.as_deref(),
                )?;
            }
            Command::Config {
                action: Some(ConfigAction::Reranker { provider, model }),
            } => {
                ensure!(
                    provider != "disable" || model.is_none(),
                    "config reranker disable does not accept a model"
                );
                ensure!(
                    provider == "openai" || self.global.reranker_candidates.is_none(),
                    "--reranker-candidates requires config reranker openai"
                );
            }
            Command::Models { provider } => {
                self.models_provider(provider.as_deref())?;
            }
            _ => {}
        }
        Ok(())
    }

    fn models_provider<'a>(&'a self, positional: Option<&'a str>) -> Result<Option<&'a str>> {
        let option = self.global.description_provider.as_deref();
        ensure!(
            positional.is_none() || option.is_none() || positional == option,
            "models provider and --description-provider must match"
        );
        let provider = positional.or(option);
        ensure!(
            provider.is_none() || matches!(provider, Some("opencode" | "opencode-go")),
            "models provider must be opencode or opencode-go"
        );
        Ok(provider)
    }
}

/// Report a runtime error on stderr, using terminal styling when available.
pub fn report_error(error: &anyhow::Error) {
    ui::error(format!("slopdex: {error:#}"));
}

/// Execute the CLI; main owns reporting runtime errors and selecting the failure exit code.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    cli.validate()?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if let Command::Models { provider } = &cli.command {
        let models = ui::spin("Fetching published models", || {
            Providers::models(cli.models_provider(provider.as_deref())?)
        })?;
        if cli.global.format == Some(Format::Json) {
            return print_json(&mut out, &models);
        }
        for model in array(&models) {
            writeln!(out, "{}/{}", text(model, "provider"), text(model, "model"))?;
        }
        return Ok(());
    }
    let root = absolute(cli.global.root.as_deref().unwrap_or(Path::new(".")))?;
    let config_file = config_path(&root, cli.global.config.as_deref())?;
    if let Command::Config { action } = &cli.command {
        return run_config(&cli.global, &config_file, action.as_ref(), &mut out);
    }

    let mut config = effective_config(&cli.global, &config_file)?;
    let index = index_path(&root, cli.global.index.as_deref(), &config)?;
    if let Command::Descriptions { action } = &cli.command {
        config["descriptionsEnabled"] = json!(action.enabled());
    }
    let mut engine = ui::spin("Opening index", || {
        Engine::open(&root, &index, config.clone())
    })?;
    let refreshed = if cli.global.no_reindex {
        None
    } else {
        Some(ui::spin("Refreshing index", || engine.refresh())?)
    };
    warn_errors(
        &engine,
        &index,
        config["ignoreErrors"].as_bool().unwrap_or(false),
    )?;
    let format = cli.global.format.unwrap_or(Format::Summary);
    match &cli.command {
        Command::Search(args) => {
            let mut options = args.query.filters.options();
            if args.code || args.descriptions || args.md {
                options["code"] = json!(args.code);
                options["descriptions"] = json!(args.descriptions);
                options["md"] = json!(args.md);
            }
            let rows = ui::spin("Searching index", || {
                engine.search(&args.query.query, "search", &options)
            })?;
            print_search(&mut out, &rows, format, false)?;
        }
        Command::SearchCode(args) | Command::SearchDescriptions(args) | Command::SearchMd(args) => {
            let kind = match &cli.command {
                Command::SearchCode(_) => "search-code",
                Command::SearchDescriptions(_) => "search-descriptions",
                _ => "search-md",
            };
            let rows = ui::spin("Searching index", || {
                engine.search(&args.query, kind, &args.filters.options())
            })?;
            print_search(&mut out, &rows, format, kind == "search-descriptions")?;
        }
        Command::Describe(args) => {
            let mut options = args.query.filters.options();
            options["describeFullFileThreshold"] = json!(args.describe_full_file_threshold);
            let result = ui::spin("Generating explanation", || {
                engine.describe(&args.query.query, &options)
            })?;
            if format == Format::Json {
                print_json(&mut out, &result)?;
            } else {
                writeln!(out, "{}", text(&result, "description"))?;
            }
        }
        Command::CrossSearch(args) => {
            let mut target = None;
            if let (Some(target_root), Some(target_index)) = (&args.target_root, &args.target_index)
            {
                let target_root = absolute(target_root)?;
                let target_index = absolute(target_index)?;
                if same_path(&index, &target_index)? {
                    ensure!(
                        same_path(&root, &target_root)?,
                        "source and target use the same index but different repository roots"
                    );
                } else {
                    let target_path = config_path(&target_root, args.target_config.as_deref())?;
                    let mut target_config = effective_config(&cli.global, &target_path)?;
                    target_config["indexPath"] = json!(target_index);
                    let mut opened = ui::spin("Opening target index", || {
                        Engine::open(&target_root, &target_index, target_config.clone())
                    })?;
                    if !cli.global.no_reindex {
                        ui::spin("Refreshing target index", || opened.refresh())?;
                    }
                    warn_errors(
                        &opened,
                        &target_index,
                        target_config["ignoreErrors"].as_bool().unwrap_or(false),
                    )?;
                    target = Some(opened);
                }
            }
            let rows = ui::spin("Comparing indexed functions", || {
                engine.cross_search(target.as_ref(), &args.options())
            })?;
            let format = cli.global.format.unwrap_or(if args.cohesion {
                Format::Summary
            } else {
                Format::Clusters
            });
            print_cross(
                &mut out,
                rows,
                format,
                target.is_none(),
                args.cohesion,
                args.filters.limit,
            )?;
        }
        Command::Status => print_json(&mut out, &engine.status()?)?,
        Command::IndexErrors => print_errors(&mut out, &engine.errors()?, format)?,
        Command::UpdateGit(_) => print_json(
            &mut out,
            &refreshed.unwrap_or(json!({"refreshed": false, "noReindex": true})),
        )?,
        Command::Descriptions { action } => {
            let result = ui::spin("Updating descriptions", || {
                engine.set_descriptions(action.enabled())
            })?;
            let mut saved = read_config(&config_file)?;
            saved["descriptionsEnabled"] = json!(action.enabled());
            for key in [
                "descriptionProvider",
                "descriptionModel",
                "descriptionFallbackModel",
            ] {
                if let Some(value) = config.get(key) {
                    saved[key] = value.clone();
                }
            }
            write_config(&config_file, &saved)?;
            print_json(&mut out, &result)?;
        }
        Command::ReindexFiles { callables } => {
            let result = ui::spin("Regenerating file descriptions", || {
                engine.reindex_files(*callables)
            })?;
            print_json(&mut out, &result)?
        }
        Command::Config { .. } | Command::Models { .. } => unreachable!(),
    }
    Ok(())
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    // Canonicalize when comparing identities, not when interpreting repo-relative source filters.
    Ok(path)
}

fn config_path(root: &Path, explicit: Option<&Path>) -> Result<PathBuf> {
    absolute(
        &explicit
            .map(Path::to_owned)
            .unwrap_or_else(|| root.join(".slopdex/config.json")),
    )
}

fn index_path(root: &Path, explicit: Option<&Path>, config: &Value) -> Result<PathBuf> {
    absolute(
        &explicit
            .map(Path::to_owned)
            .or_else(|| config["indexPath"].as_str().map(PathBuf::from))
            .unwrap_or_else(|| root.join(".slopdex/index.sqlite")),
    )
}

fn canonical_identity(path: &Path) -> Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let path = absolute(path)?;
            let mut resolved = PathBuf::new();
            for component in path.components() {
                match component {
                    Component::CurDir => {}
                    Component::ParentDir => {
                        resolved.pop();
                    }
                    part => resolved.push(part.as_os_str()),
                }
                match fs::canonicalize(&resolved) {
                    Ok(canonical) => resolved = canonical,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("resolve {}", resolved.display()));
                    }
                }
            }
            Ok(resolved)
        }
        Err(error) => Err(error).with_context(|| format!("resolve {}", path.display())),
    }
}

fn same_path(left: &Path, right: &Path) -> Result<bool> {
    if canonical_identity(left)? == canonical_identity(right)? {
        return Ok(true);
    }
    // Hard links also identify the same database and must not acquire a second exclusive lock.
    #[cfg(unix)]
    if let (Ok(left), Ok(right)) = (fs::metadata(left), fs::metadata(right)) {
        use std::os::unix::fs::MetadataExt;
        return Ok(left.dev() == right.dev() && left.ino() == right.ino());
    }
    Ok(false)
}

fn read_config(path: &Path) -> Result<Value> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(json!({})),
        Err(error) => return Err(error).with_context(|| format!("read config {}", path.display())),
    };
    let mut value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse config {}", path.display()))?;
    ensure!(
        value.is_object(),
        "config must contain a JSON object: {}",
        path.display()
    );
    normalize_config_aliases(&mut value);
    Ok(value)
}

fn normalize_config_aliases(config: &mut Value) {
    let object = config.as_object_mut().expect("config must be an object");
    for (alias, canonical) in [
        ("embeddingProvider", "provider"),
        ("embeddingModel", "model"),
        ("embeddingDimensions", "dimensions"),
        ("fallbackModel", "descriptionFallbackModel"),
    ] {
        if let Some(value) = object.remove(alias).filter(|value| !value.is_null())
            && object.get(canonical).is_none_or(Value::is_null)
        {
            object.insert(canonical.into(), value);
        }
    }
}

fn effective_config(global: &Global, path: &Path) -> Result<Value> {
    let mut config = read_config(path)?;
    for (key, value) in [
        ("provider", &global.provider),
        ("model", &global.model),
        ("descriptionProvider", &global.description_provider),
        ("descriptionModel", &global.description_model),
        (
            "descriptionFallbackModel",
            &global.description_fallback_model,
        ),
    ] {
        if let Some(value) = value {
            config[key] = json!(value);
        }
    }
    if let Some(value) = global.dimensions {
        config["dimensions"] = json!(value);
    }
    if let Some(value) = global.reranker_candidates {
        config["rerankerCandidates"] = json!(value);
    }
    if let Some(value) = &global.index {
        config["indexPath"] = json!(absolute(value)?);
    }
    if global.verbose {
        config["verbose"] = json!(true);
    }
    config["noReindex"] = json!(global.no_reindex);
    config["forceReindex"] = json!(global.force_reindex);
    config["ignoreErrors"] = json!(global.ignore_errors);
    config["rebuildOnDivergence"] = json!(global.rebuild_on_divergence);
    validate_config(&config)?;
    Ok(config)
}

fn validate_config(config: &Value) -> Result<()> {
    ensure!(config.is_object(), "config must be a JSON object");
    for (key, allowed) in [
        ("provider", &["openai", "jina"][..]),
        (
            "descriptionProvider",
            &["openai", "opencode", "opencode-go"][..],
        ),
    ] {
        if let Some(value) = config.get(key) {
            ensure!(
                value.as_str().is_some_and(|value| allowed.contains(&value)),
                "unsupported {key}: {value}"
            );
        }
    }
    for key in [
        "model",
        "indexPath",
        "descriptionModel",
        "descriptionFallbackModel",
    ] {
        if let Some(value) = config.get(key) {
            ensure!(
                value.as_str().is_some_and(|s| !s.trim().is_empty()),
                "{key} must be a non-empty string"
            );
        }
    }
    for key in [
        "dimensions",
        "maxFileSize",
        "embeddingBatchSize",
        "parallelism",
    ] {
        if let Some(value) = config.get(key) {
            ensure!(
                value.as_u64().is_some_and(|n| n > 0),
                "{key} must be a positive integer"
            );
        }
    }
    for key in ["descriptionsEnabled", "rerankingEnabled", "verbose"] {
        if let Some(value) = config.get(key) {
            ensure!(value.is_boolean(), "{key} must be a boolean");
        }
    }
    for key in ["include", "exclude"] {
        if let Some(value) = config.get(key) {
            ensure!(
                value
                    .as_array()
                    .is_some_and(|a| a.iter().all(Value::is_string)),
                "{key} must be an array of strings"
            );
            for pattern in array(value) {
                globset::Glob::new(pattern.as_str().unwrap())
                    .with_context(|| format!("invalid {key} glob {pattern}"))?;
            }
        }
    }
    if config["rerankingEnabled"] == true {
        ensure!(
            matches!(
                config["rerankerProvider"].as_str(),
                Some("cohere" | "jina" | "openai")
            ),
            "unsupported rerankerProvider"
        );
        if let Some(model) = config.get("rerankerModel") {
            ensure!(
                model.as_str().is_some_and(|s| !s.trim().is_empty()),
                "rerankerModel must be a non-empty string"
            );
        }
        if config["rerankerProvider"] == "openai"
            && let Some(count) = config.get("rerankerCandidates")
        {
            ensure!(
                count.as_u64().is_some_and(|n| (1..=100).contains(&n)),
                "rerankerCandidates must be between 1 and 100"
            );
        }
    }
    Ok(())
}

fn write_config(path: &Path, config: &Value) -> Result<()> {
    validate_config(config)?;
    let parent = path.parent().context("config path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .context("config path has no filename")?
            .to_string_lossy(),
        std::process::id()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .with_context(|| format!("create temporary config {}", temp.display()))?;
    let result = (|| -> Result<()> {
        serde_json::to_writer_pretty(&mut file, config)?;
        writeln!(file)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("save config {}", path.display()))
}

fn model_reference<'a>(positional: Option<&'a str>, option: Option<&'a str>) -> Result<&'a str> {
    ensure!(
        positional.is_none() || option.is_none() || positional == option,
        "model argument and description-model option must match"
    );
    let reference = positional
        .or(option)
        .context("provide a model ID or provider/model reference")?;
    ensure!(!reference.trim().is_empty(), "model ID must not be empty");
    Ok(reference)
}

fn resolve_model(
    reference: &str,
    explicit: Option<&str>,
    default: Option<&str>,
) -> Result<(String, String)> {
    let (provider, model) = if let Some((provider, model)) = reference.split_once('/') {
        ensure!(
            explicit.is_none_or(|explicit| explicit == provider),
            "model reference and --description-provider must match"
        );
        (Some(provider), model)
    } else {
        (explicit.or(default), reference)
    };
    ensure!(!model.trim().is_empty(), "model ID must not be empty");
    ensure!(
        provider.is_none() || matches!(provider, Some("opencode" | "opencode-go")),
        "config model/fallback-model catalog provider must be opencode or opencode-go"
    );
    let catalog = ui::spin("Fetching published models", || Providers::models(provider))?;
    resolve_catalog_model(&catalog, provider, model)
}

fn resolve_catalog_model(
    catalog: &Value,
    provider: Option<&str>,
    model: &str,
) -> Result<(String, String)> {
    let matches: Vec<_> = array(catalog)
        .iter()
        .filter(|entry| {
            text(entry, "model") == model
                && provider.is_none_or(|provider| text(entry, "provider") == provider)
        })
        .collect();
    ensure!(
        !matches.is_empty(),
        "unknown published model: {}{model}",
        provider.map(|p| format!("{p}/")).unwrap_or_default()
    );
    ensure!(
        matches.len() == 1,
        "model {model} is available from multiple providers; use provider/model"
    );
    Ok((text(matches[0], "provider").to_owned(), model.to_owned()))
}

fn reranker_default(provider: &str) -> &'static str {
    match provider {
        "jina" => "jina-reranker-v3.5",
        "openai" => "gpt-5.6-luna",
        _ => "rerank-v4.0-pro",
    }
}

fn run_config(
    global: &Global,
    path: &Path,
    action: Option<&ConfigAction>,
    out: &mut impl Write,
) -> Result<()> {
    let mut config = read_config(path)?;
    let changed = match action {
        None => {
            ensure!(
                io::stdin().is_terminal() && ui::terminal(),
                "config without an action requires an interactive terminal"
            );
            cliclack::intro("slopdex configuration")?;
            if let Err(error) = configure_interactively(&mut config, &mut CliclackPrompts) {
                // cliclack 0.5.6 returns Interrupted for Esc/Ctrl-C; it does not exit.
                if error.downcast_ref::<io::Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::UnexpectedEof
                    )
                }) {
                    cliclack::outro_cancel("Configuration cancelled; no settings saved")?;
                    return Ok(());
                }
                cliclack::outro_cancel("Configuration failed; no settings saved")?;
                return Err(error);
            }
            config.clone()
        }
        Some(ConfigAction::Model { model }) => {
            let reference = model_reference(model.as_deref(), global.description_model.as_deref())?;
            let (provider, model) =
                resolve_model(reference, global.description_provider.as_deref(), None)?;
            if config["descriptionProvider"].as_str().unwrap_or("openai") != provider {
                config
                    .as_object_mut()
                    .unwrap()
                    .remove("descriptionFallbackModel");
            }
            config["descriptionProvider"] = json!(provider);
            config["descriptionModel"] = json!(model);
            json!({"descriptionProvider": provider, "descriptionModel": model})
        }
        Some(ConfigAction::FallbackModel { model }) => {
            let reference = model_reference(
                model.as_deref(),
                global.description_fallback_model.as_deref(),
            )?;
            let (provider, model) = resolve_model(
                reference,
                global.description_provider.as_deref(),
                config["descriptionProvider"].as_str(),
            )?;
            ensure!(
                config["descriptionProvider"]
                    .as_str()
                    .is_none_or(|saved| saved == provider),
                "fallback model provider must match the configured description provider"
            );
            config["descriptionProvider"] = json!(provider);
            config["descriptionFallbackModel"] = json!(model);
            json!({"descriptionProvider": provider, "descriptionFallbackModel": model})
        }
        Some(ConfigAction::Descriptions { action }) => {
            config["descriptionsEnabled"] = json!(action.enabled());
            json!({"descriptionsEnabled": action.enabled()})
        }
        Some(ConfigAction::Parallelism { count }) => {
            config["parallelism"] = json!(count);
            json!({"parallelism": count})
        }
        Some(ConfigAction::Reranker { provider, model }) => {
            config["rerankingEnabled"] = json!(provider != "disable");
            if provider == "disable" {
                json!({"rerankingEnabled": false})
            } else {
                let same = config["rerankerProvider"] == provider.as_str();
                let model = model
                    .as_deref()
                    .or_else(|| {
                        if same {
                            config["rerankerModel"].as_str()
                        } else {
                            None
                        }
                    })
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(reranker_default(provider))
                    .to_owned();
                let count = global
                    .reranker_candidates
                    .or_else(|| {
                        if same {
                            config["rerankerCandidates"].as_u64().map(|n| n as usize)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(10);
                config["rerankerProvider"] = json!(provider);
                config["rerankerModel"] = json!(model);
                let mut changed = json!({"rerankingEnabled": true, "rerankerProvider": provider, "rerankerModel": model});
                if provider == "openai" {
                    ensure!(
                        (1..=100).contains(&count),
                        "rerankerCandidates must be between 1 and 100"
                    );
                    config["rerankerCandidates"] = json!(count);
                    changed["rerankerCandidates"] = json!(count);
                } else {
                    config.as_object_mut().unwrap().remove("rerankerCandidates");
                }
                changed
            }
        }
    };
    write_config(path, &config)?;
    if action.is_none() {
        let summary = [
            "descriptionsEnabled",
            "descriptionProvider",
            "descriptionModel",
            "descriptionFallbackModel",
            "rerankingEnabled",
            "rerankerProvider",
            "rerankerModel",
            "rerankerCandidates",
            "provider",
            "model",
            "dimensions",
            "indexPath",
            "include",
            "exclude",
            "maxFileSize",
            "embeddingBatchSize",
            "parallelism",
            "verbose",
        ]
        .iter()
        .filter_map(|key| config.get(*key).map(|value| format!("{key}: {value}")))
        .collect::<Vec<_>>()
        .join("\n");
        cliclack::note("Saved settings", summary)?;
        cliclack::outro(format!("Updated {}", path.display()))?;
        if global.format != Some(Format::Json) {
            return Ok(());
        }
    }
    if global.format == Some(Format::Json) {
        let mut result = changed;
        result["configPath"] = json!(path);
        print_json(out, &result)
    } else {
        let settings = changed
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, value)| {
                let value = value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string());
                format!("{key}={value}")
            })
            .collect::<Vec<_>>()
            .join(" ");
        let message = format!("Updated {}: {settings}", path.display());
        if ui::terminal() && io::stdout().is_terminal() {
            cliclack::log::success(message)?;
        } else {
            writeln!(out, "{message}")?;
        }
        Ok(())
    }
}

trait Prompts {
    fn ask(&mut self, label: &str, default: &str) -> Result<String>;
    fn yes(&mut self, label: &str, default: bool) -> Result<bool>;
    fn number(&mut self, label: &str, default: u64, max: Option<usize>) -> Result<usize>;
    fn required(&mut self, label: &str, default: &str) -> Result<String>;
    fn catalog(&mut self, provider: &str) -> Result<Value>;
    fn select(
        &mut self,
        label: &str,
        default: &str,
        choices: &[&str],
        searchable: bool,
    ) -> Result<String>;

    fn choice(&mut self, label: &str, default: &str, choices: &[&str]) -> Result<String> {
        self.select(label, default, choices, false)
    }

    fn published(
        &mut self,
        catalog: &Value,
        provider: &str,
        label: &str,
        current: &str,
        excluded: Option<&str>,
    ) -> Result<String> {
        let models: Vec<&str> = array(catalog)
            .iter()
            .filter(|row| text(row, "provider") == provider)
            .map(|row| text(row, "model"))
            .filter(|model| !model.is_empty() && Some(*model) != excluded)
            .collect();
        ensure!(
            !models.is_empty(),
            "{provider} returned no selectable models"
        );
        let default = if models.contains(&current) {
            current
        } else {
            models[0]
        };
        self.select(label, default, &models, true)
    }
}

struct CliclackPrompts;

impl Prompts for CliclackPrompts {
    fn ask(&mut self, label: &str, default: &str) -> Result<String> {
        let value: String = cliclack::input(label)
            .required(false)
            .default_input(default)
            .interact()?;
        Ok(value.trim().to_owned())
    }

    fn select(
        &mut self,
        label: &str,
        default: &str,
        choices: &[&str],
        searchable: bool,
    ) -> Result<String> {
        let mut prompt = cliclack::select(label).max_rows(10);
        for choice in choices {
            prompt = prompt.item(choice.to_string(), choice, "");
        }
        prompt = prompt.initial_value(default.to_owned());
        if searchable {
            prompt = prompt.filter_mode();
        }
        Ok(prompt.interact()?)
    }

    fn yes(&mut self, label: &str, default: bool) -> Result<bool> {
        Ok(cliclack::confirm(label).initial_value(default).interact()?)
    }

    fn number(&mut self, label: &str, default: u64, max: Option<usize>) -> Result<usize> {
        Ok(cliclack::input(label)
            .default_input(&default.to_string())
            .validate(move |value: &String| {
                let n = positive(value)?;
                if let Some(max) = max
                    && n > max
                {
                    return Err(format!("Enter a positive integer no greater than {max}"));
                }
                Ok(())
            })
            .interact::<usize>()?)
    }

    fn required(&mut self, label: &str, default: &str) -> Result<String> {
        let value: String = cliclack::input(label)
            .default_input(default)
            .validate(|value: &String| {
                if value.trim().is_empty() {
                    Err("A value is required")
                } else {
                    Ok(())
                }
            })
            .interact()?;
        Ok(value.trim().to_owned())
    }

    fn catalog(&mut self, provider: &str) -> Result<Value> {
        crate::ui::spin("Fetching published models", || {
            Providers::models(Some(provider))
        })
    }
}

fn configure_interactively(saved: &mut Value, prompts: &mut impl Prompts) -> Result<()> {
    // Stage even alias migration locally so cancellation and validation failures
    // leave the caller's configuration untouched.
    let mut config = saved.clone();
    normalize_config_aliases(&mut config);
    let existing = config.clone();
    let enabled = prompts.yes(
        "Generate file and function descriptions with an LLM?",
        existing["descriptionsEnabled"].as_bool().unwrap_or(false),
    )?;
    config["descriptionsEnabled"] = json!(enabled);
    if enabled {
        let provider = prompts.choice(
            "Description provider",
            existing["descriptionProvider"]
                .as_str()
                .unwrap_or("opencode-go"),
            &["opencode-go", "opencode", "openai"],
        )?;
        let same = existing["descriptionProvider"] == provider;
        let current = if same {
            text(&existing, "descriptionModel")
        } else {
            ""
        };
        let catalog = if provider == "openai" {
            None
        } else {
            Some(prompts.catalog(&provider)?)
        };
        let model = if let Some(catalog) = &catalog {
            prompts.published(catalog, &provider, "Description model", current, None)?
        } else {
            prompts.required(
                "Description model",
                if current.is_empty() {
                    "gpt-5.6-luna"
                } else {
                    current
                },
            )?
        };
        config["descriptionProvider"] = json!(provider);
        config["descriptionModel"] = json!(model);
        if prompts.yes(
            "Configure a fallback description model?",
            existing.get("descriptionFallbackModel").is_some(),
        )? {
            let current = if same {
                text(&existing, "descriptionFallbackModel")
            } else {
                ""
            };
            let fallback = if let Some(catalog) = &catalog {
                prompts.published(
                    catalog,
                    &provider,
                    "Fallback description model",
                    current,
                    Some(&model),
                )?
            } else {
                prompts.required("Fallback description model", current)?
            };
            config["descriptionFallbackModel"] = json!(fallback);
        } else {
            config
                .as_object_mut()
                .unwrap()
                .remove("descriptionFallbackModel");
        }
    }
    let reranking = prompts.yes(
        "Enable second-stage reranking for searches?",
        existing["rerankingEnabled"].as_bool().unwrap_or(false),
    )?;
    config["rerankingEnabled"] = json!(reranking);
    if reranking {
        let provider = prompts.choice(
            "Reranker provider",
            existing["rerankerProvider"].as_str().unwrap_or("cohere"),
            &["cohere", "jina", "openai"],
        )?;
        let same = existing["rerankerProvider"] == provider;
        let default = if same {
            existing["rerankerModel"]
                .as_str()
                .unwrap_or(reranker_default(&provider))
        } else {
            reranker_default(&provider)
        };
        config["rerankerModel"] = json!(prompts.required("Reranker model", default)?);
        config["rerankerProvider"] = json!(provider);
        if provider == "openai" {
            let default = if same {
                existing["rerankerCandidates"].as_u64().unwrap_or(10)
            } else {
                10
            };
            config["rerankerCandidates"] = json!(prompts.number(
                "Embedding-ranked reranker candidates",
                default,
                Some(100)
            )?);
        } else {
            config.as_object_mut().unwrap().remove("rerankerCandidates");
        }
    }
    let provider = prompts.choice(
        "Embedding provider",
        existing["provider"].as_str().unwrap_or("openai"),
        &["openai", "jina"],
    )?;
    let same = existing["provider"].as_str().unwrap_or("openai") == provider;
    let (model, dimensions) = if provider == "jina" {
        ("jina-embeddings-v4", 1024)
    } else {
        ("text-embedding-3-large", 3072)
    };
    let model = if same {
        existing["model"].as_str().unwrap_or(model)
    } else {
        model
    };
    let dimensions = if same {
        existing["dimensions"].as_u64().unwrap_or(dimensions)
    } else {
        dimensions
    };
    config["provider"] = json!(provider);
    config["model"] = json!(prompts.required("Embedding model", model)?);
    config["dimensions"] = json!(prompts.number("Embedding dimensions", dimensions, None)?);
    let index = prompts.ask(
        "Index path (enter '-' for default)",
        text(&existing, "indexPath"),
    )?;
    if index.is_empty() || index == "-" {
        config.as_object_mut().unwrap().remove("indexPath");
    } else {
        config["indexPath"] = json!(index);
    }
    for (key, label) in [
        ("include", "Include globs"),
        ("exclude", "Additional exclude globs"),
    ] {
        let default = array(&existing[key])
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let value = prompts.ask(&format!("{label} (comma-separated; '-' clears)"), &default)?;
        let patterns: Vec<_> = value
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "-")
            .collect();
        config[key] = json!(patterns);
    }
    for (key, label, default) in [
        (
            "maxFileSize",
            "Maximum source file size in bytes",
            1_048_576,
        ),
        ("embeddingBatchSize", "Embedding batch size", 32),
        ("parallelism", "Concurrent provider request limit", 10),
    ] {
        config[key] =
            json!(prompts.number(label, existing[key].as_u64().unwrap_or(default), None)?);
    }
    config["verbose"] = json!(prompts.yes(
        "Log every external model request?",
        existing["verbose"].as_bool().unwrap_or(false)
    )?);
    validate_config(&config)?;
    *saved = config;
    Ok(())
}

fn warn_errors(engine: &Engine, index: &Path, ignore: bool) -> Result<()> {
    if !ignore {
        let errors = engine.errors()?;
        if !errors.is_empty() {
            ui::warning(format!(
                "slopdex: {} saved indexing error(s) in {}; inspect with index-errors",
                errors.len(),
                index.display()
            ));
        }
    }
    Ok(())
}

fn print_json(out: &mut impl Write, value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer_pretty(&mut *out, value)?;
    writeln!(out)?;
    Ok(())
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn number(value: &Value, key: &str) -> f64 {
    value[key].as_f64().unwrap_or(0.0)
}

fn function_name(function: &Value) -> String {
    let name = function["qualifiedName"]
        .as_str()
        .unwrap_or_else(|| text(function, "name"));
    format!("{} :: {name}", text(function, "path"))
}

fn function_location(function: &Value) -> String {
    let name = function["qualifiedName"]
        .as_str()
        .unwrap_or_else(|| text(function, "name"));
    format!(
        "{}:{}:{} :: {name}",
        text(function, "path"),
        function["startLine"].as_u64().unwrap_or(1),
        function["startColumn"].as_u64().unwrap_or(1)
    )
}

fn rank(row: &Value) -> String {
    let similarity = number(row, "similarity");
    if let Some(rerank) = row["rerankScore"].as_f64() {
        format!("{rerank:.4} rerank ({similarity:.4} similarity)")
    } else {
        format!("{similarity:.4}")
    }
}

fn score_details(row: &Value) -> String {
    match (
        row["descriptionSimilarity"].as_f64(),
        row["fileDescriptionSimilarity"].as_f64(),
    ) {
        (Some(description), Some(file)) => {
            if let Some(code) = row["codeSimilarity"].as_f64() {
                format!(
                    "  [combined thirds; code {code:.4}, description {description:.4}, file {file:.4}]"
                )
            } else {
                format!("  [combined 50/50; description {description:.4}, file {file:.4}]")
            }
        }
        _ => String::new(),
    }
}

fn print_search(
    out: &mut impl Write,
    rows: &[Value],
    format: Format,
    descriptions: bool,
) -> Result<()> {
    if format == Format::Json {
        return print_json(out, &rows);
    }
    if rows.is_empty() {
        writeln!(out, "No matches.")?;
    }
    for row in rows {
        if row["type"] == "markdown" {
            let chunk = &row["chunk"];
            let heading = array(&chunk["headingPath"])
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" > ");
            writeln!(
                out,
                "{}  {}:{}{}\n{}",
                rank(row),
                text(chunk, "path"),
                chunk["startLine"].as_u64().unwrap_or(1),
                if heading.is_empty() {
                    String::new()
                } else {
                    format!(" :: {heading}")
                },
                text(chunk, "content")
            )?;
        } else {
            writeln!(
                out,
                "{}  {}{}",
                rank(row),
                function_name(&row["function"]),
                score_details(row)
            )?;
            if descriptions
                && let Some(description) = row["function"]["description"]
                    .as_str()
                    .filter(|s| !s.is_empty())
            {
                writeln!(out, "{description}")?;
            }
        }
    }
    Ok(())
}

fn print_errors(out: &mut impl Write, errors: &[Value], format: Format) -> Result<()> {
    if format == Format::Json {
        return print_json(out, &errors);
    }
    if errors.is_empty() {
        writeln!(out, "No indexing errors.")?;
    }
    for error in errors {
        let path = error["path"]
            .as_str()
            .or_else(|| error["filePath"].as_str())
            .unwrap_or("(unknown file)");
        write!(out, "{path}")?;
        if let Some(line) = error["startLine"].as_u64() {
            write!(out, ":{line}")?;
        }
        if let Some(column) = error["startColumn"].as_u64() {
            write!(out, ":{column}")?;
        }
        if let Some(name) = error["qualifiedName"]
            .as_str()
            .or_else(|| error["name"].as_str())
        {
            write!(out, " :: {name}")?;
        }
        writeln!(out, "  {}", text(error, "message"))?;
    }
    Ok(())
}

fn print_cross(
    out: &mut impl Write,
    mut rows: Vec<Value>,
    format: Format,
    same_index: bool,
    cohesion: bool,
    limit: Option<usize>,
) -> Result<()> {
    rows.retain(|row| !array(&row["matches"]).is_empty());
    if format == Format::Clusters {
        return print_clusters(out, &rows, same_index, limit);
    }
    if cohesion {
        for row in &mut rows {
            if let Some(matches) = row["matches"].as_array_mut() {
                matches.sort_by(|a, b| {
                    number(b, "physicalDistance")
                        .total_cmp(&number(a, "physicalDistance"))
                        .then_with(|| number(b, "similarity").total_cmp(&number(a, "similarity")))
                });
            }
        }
    }
    if rows.is_empty() && format == Format::Summary {
        writeln!(out, "No matches.")?;
    }
    for row in rows.iter().take(limit.unwrap_or(usize::MAX)) {
        if format == Format::Json {
            serde_json::to_writer(&mut *out, row)?;
            writeln!(out)?;
        } else {
            writeln!(out, "{}", function_name(&row["source"]))?;
            for item in array(&row["matches"]) {
                let distance = item["physicalDistance"]
                    .as_f64()
                    .map(|d| format!("  [distance {d}]"))
                    .unwrap_or_default();
                writeln!(
                    out,
                    "  {}  {}{distance}{}",
                    rank(item),
                    function_name(&item["function"]),
                    score_details(item)
                )?;
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct Cluster {
    members: Vec<String>,
    min: f64,
    max: f64,
    combined: bool,
}

fn node_key(function: &Value, role: &str) -> String {
    let id = function
        .get("id")
        .filter(|id| !id.is_null())
        .map(Value::to_string)
        .unwrap_or_else(|| function_location(function));
    format!("{role}:{id}")
}

fn clusters(rows: &[Value], same_index: bool) -> Vec<Cluster> {
    let mut nodes = BTreeMap::<String, String>::new();
    let mut neighbors = HashMap::<String, Vec<(String, f64, bool)>>::new();
    for row in rows {
        let source = &row["source"];
        let left = node_key(source, if same_index { "index" } else { "source" });
        for item in array(&row["matches"]) {
            let function = &item["function"];
            let right = node_key(function, if same_index { "index" } else { "target" });
            if left == right {
                continue;
            }
            nodes.insert(
                left.clone(),
                format!(
                    "{}{}",
                    if same_index { "" } else { "[source] " },
                    function_location(source)
                ),
            );
            nodes.insert(
                right.clone(),
                format!(
                    "{}{}",
                    if same_index { "" } else { "[target] " },
                    function_location(function)
                ),
            );
            let score = number(item, "similarity");
            let combined = item["descriptionSimilarity"].is_number();
            neighbors
                .entry(left.clone())
                .or_default()
                .push((right.clone(), score, combined));
            neighbors
                .entry(right)
                .or_default()
                .push((left.clone(), score, combined));
        }
    }
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for start in nodes.keys() {
        if !seen.insert(start.clone()) {
            continue;
        }
        let mut pending = vec![start.clone()];
        let mut cluster = Cluster {
            members: Vec::new(),
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            combined: false,
        };
        while let Some(key) = pending.pop() {
            cluster.members.push(nodes[&key].clone());
            for (neighbor, score, combined) in &neighbors[&key] {
                cluster.min = cluster.min.min(*score);
                cluster.max = cluster.max.max(*score);
                cluster.combined |= combined;
                if seen.insert(neighbor.clone()) {
                    pending.push(neighbor.clone());
                }
            }
        }
        cluster.members.sort();
        result.push(cluster);
    }
    result.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then_with(|| a.members[0].cmp(&b.members[0]))
    });
    result
}

fn print_clusters(
    out: &mut impl Write,
    rows: &[Value],
    same_index: bool,
    limit: Option<usize>,
) -> Result<()> {
    let clusters = clusters(rows, same_index);
    if clusters.is_empty() {
        writeln!(out, "No clusters.")?;
    }
    for (index, cluster) in clusters
        .iter()
        .take(limit.unwrap_or(usize::MAX))
        .enumerate()
    {
        if index > 0 {
            writeln!(out)?;
        }
        let range = if cluster.min == cluster.max {
            format!("{:.4}", cluster.min)
        } else {
            format!("{:.4}-{:.4}", cluster.min, cluster.max)
        };
        writeln!(
            out,
            "Cluster {} ({} functions, similarity {range}{})",
            index + 1,
            cluster.members.len(),
            if cluster.combined {
                ", combined code + callable description + file description"
            } else {
                ""
            }
        )?;
        for member in &cluster.members {
            writeln!(out, "  {member}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        let cli =
            Cli::try_parse_from(std::iter::once("slopdex").chain(args.iter().copied())).unwrap();
        cli.validate().unwrap();
        cli
    }

    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn readme_commands_parse() {
        for args in [
            vec!["search", "validate an authenticated session"],
            vec!["search", "keep the repository index synchronized"],
            vec!["search-code", "configure the embedding provider"],
            vec!["search-md", "configure the embedding provider"],
            vec!["descriptions", "enable"],
            vec![
                "search-descriptions",
                "keep the repository index synchronized",
            ],
            vec!["models", "opencode-go"],
            vec!["config", "model", "opencode-go/gpt-5.6-luna"],
            vec![
                "config",
                "fallback-model",
                "opencode-go/muse-spark-1.3-contributor",
            ],
            vec!["config"],
            vec!["describe", "I want to implement a new rpc endpoint"],
            vec![
                "cross-search",
                "--cross-file-only",
                "--min-lines",
                "4",
                "--threshold",
                "0.9",
            ],
            vec![
                "cross-search",
                "--cross-file-only",
                "--min-lines",
                "4",
                "--threshold",
                "0.85-0.9",
            ],
            vec![
                "cross-search",
                "--uncommitted",
                "--cross-file-only",
                "--min-lines",
                "4",
                "--threshold",
                "0.9",
            ],
            vec![
                "cross-search",
                "--changed-since",
                "origin/main",
                "--threshold",
                "0.9",
            ],
            vec![
                "cross-search",
                "--source-path",
                "src/services",
                "-e",
                r"^UserService\.",
                "--threshold",
                "0.9",
            ],
            vec![
                "cross-search",
                "--cross-file-only",
                "--cohesion",
                "--threshold",
                "0.8",
            ],
            vec!["search", "...", "--threshold", "0.5"],
            vec!["cross-search", "--uncommitted", "--threshold", "0.8"],
            vec!["config", "reranker", "cohere"],
            vec!["config", "reranker", "jina"],
            vec!["config", "reranker", "openai"],
            vec![
                "cross-search",
                "--target-root",
                "/path/to/other/repo",
                "--target-index",
                "/path/to/other/repo/.slopdex/index.sqlite",
                "--threshold",
                "0.9",
            ],
            vec!["status"],
            vec!["index-errors", "--format", "summary"],
        ] {
            parse(&args);
        }
        assert_eq!(
            Cli::try_parse_from(["slopdex", "--version"])
                .unwrap_err()
                .kind(),
            clap::error::ErrorKind::DisplayVersion
        );
        assert_eq!(
            Cli::try_parse_from(["slopdex", "--help"])
                .unwrap_err()
                .kind(),
            clap::error::ErrorKind::DisplayHelp
        );
    }

    #[test]
    fn threshold_ranges_include_negative_endpoints_and_exponents() {
        for (value, min, max) in [
            ("0.3", 0.3, None),
            ("-1", -1.0, None),
            ("1", 1.0, None),
            ("0.85-0.9", 0.85, Some(0.9)),
            ("-0.8--0.2", -0.8, Some(-0.2)),
            ("-1-1", -1.0, Some(1.0)),
            ("1e-2-9e-1", 0.01, Some(0.9)),
            ("-1e-1--1e-2", -0.1, Some(-0.01)),
        ] {
            let cli = parse(&["cross-search", "--threshold", value]);
            let Command::CrossSearch(args) = cli.command else {
                panic!()
            };
            assert_eq!(args.filters.threshold, Threshold { min, max });
            let options = args.options();
            assert_eq!(options["minSimilarity"], json!(min));
            assert_eq!(options.get("maxSimilarity"), max.map(|n| json!(n)).as_ref());
        }
        for value in [
            "NaN",
            "inf",
            "-inf",
            "1.01",
            "-1.01",
            "0.9-0.9",
            "0.9-0.8",
            "-0.1--0.8",
            "0.2-NaN",
            "0.1-1.1",
            "0.1-",
            "",
        ] {
            assert!(
                Cli::try_parse_from(["slopdex", "search", "query", "--threshold", value]).is_err(),
                "accepted {value}"
            );
        }
    }

    #[test]
    fn options_defaults_and_limit_validation() {
        let cli = parse(&["cross-search"]);
        let Command::CrossSearch(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.matches, 5);
        assert_eq!(args.min_lines, 2);
        assert_eq!(args.filters.threshold.min, 0.3);
        assert!(args.filters.limit.is_none());
        let cli = parse(&["cross-search", "--limit", "1"]);
        let Command::CrossSearch(args) = cli.command else {
            panic!()
        };
        assert!(args.options().get("limit").is_none());
        for option in ["--limit", "--matches", "--min-lines"] {
            for value in ["0", "-1", "1.5", "NaN"] {
                assert!(Cli::try_parse_from(["slopdex", "cross-search", option, value]).is_err());
            }
        }
        for value in ["NaN", "1.1", "-1.1"] {
            assert!(
                Cli::try_parse_from([
                    "slopdex",
                    "describe",
                    "query",
                    "--describe-full-file-threshold",
                    value
                ])
                .is_err()
            );
        }
        parse(&[
            "describe",
            "query",
            "--describe-full-file-threshold",
            "-0.5",
        ]);
    }

    #[test]
    fn validates_combinations_before_opening_an_index() {
        for args in [
            vec!["cross-search", "--target-root", "other"],
            vec!["cross-search", "--target-index", "other.sqlite"],
            vec!["cross-search", "--target-config", "other.json"],
            vec!["status", "--force-reindex"],
            vec!["search", "query", "--regexp", "["],
        ] {
            assert!(Cli::try_parse_from(std::iter::once("slopdex").chain(args)).is_err());
        }
        for args in [
            vec!["search", "query", "--format", "clusters"],
            vec!["cross-search", "--cohesion", "--format", "clusters"],
            vec!["config", "model"],
            vec!["config", "fallback-model"],
            vec!["config", "reranker", "disable", "unexpected-model"],
        ] {
            let cli = Cli::try_parse_from(std::iter::once("slopdex").chain(args)).unwrap();
            assert!(cli.validate().is_err());
        }
        parse(&[
            "--root",
            "/tmp/repo",
            "search",
            "query",
            "--no-reindex",
            "--format",
            "json",
        ]);
        parse(&[
            "config",
            "model",
            "--description-provider",
            "opencode-go",
            "--description-model",
            "model",
        ]);
        parse(&[
            "config",
            "reranker",
            "openai",
            "--reranker-candidates",
            "100",
        ]);
        parse(&[
            "update-git",
            "--force-reindex",
            "--yes-really-rebuild-the-index",
        ]);
        parse(&["reindex-files", "--callables"]);
        parse(&["search", "query", "--code", "--md", "--regex", "foo"]);
    }

    fn function(id: &str) -> Value {
        json!({"id": id, "path": format!("src/{id}.rs"), "qualifiedName": id, "startLine": 1, "startColumn": 1})
    }

    fn edge(a: &str, b: &str, similarity: f64) -> Value {
        json!({"source": function(a), "matches": [{"function": function(b), "similarity": similarity}]})
    }

    #[test]
    fn clusters_are_transitive_and_limit_applies_after_components_form() {
        let rows = vec![
            edge("a", "b", 0.91),
            edge("c", "d", 0.95),
            edge("b", "c", 0.92),
            edge("x", "y", 0.99),
        ];
        let grouped = clusters(&rows, true);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].members.len(), 4);
        assert_eq!((grouped[0].min, grouped[0].max), (0.91, 0.95));
        let mut out = Vec::new();
        print_cross(&mut out, rows, Format::Clusters, true, false, Some(1)).unwrap();
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Cluster 1 (4 functions, similarity 0.9100-0.9500)"));
        assert!(output.contains("src/d.rs:1:1 :: d"));
        assert!(!output.contains("Cluster 2"));
        assert_eq!(clusters(&[edge("a", "a", 0.9)], false)[0].members.len(), 2);
    }

    #[test]
    fn cohesion_is_distance_first_and_json_is_one_object_per_source() {
        let rows = vec![
            json!({"source": function("a"), "matches": [
                {"function": function("b"), "similarity": 0.99, "physicalDistance": 1},
                {"function": function("c"), "similarity": 0.91, "physicalDistance": 4},
                {"function": function("d"), "similarity": 0.95, "physicalDistance": 4}
            ]}),
            edge("x", "y", 0.9),
        ];
        let mut out = Vec::new();
        print_cross(&mut out, rows, Format::Json, true, true, Some(1)).unwrap();
        let output = String::from_utf8(out).unwrap();
        assert_eq!(output.lines().count(), 1);
        let row: Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(row["matches"][0]["function"]["id"], "d");
        assert_eq!(row["matches"][1]["function"]["id"], "c");
    }

    #[test]
    fn catalog_resolution_rejects_unknown_and_ambiguous_models() {
        let catalog = json!([
            {"provider": "opencode", "model": "shared"},
            {"provider": "opencode-go", "model": "shared"},
            {"provider": "opencode-go", "model": "unique"}
        ]);
        assert!(resolve_catalog_model(&catalog, None, "shared").is_err());
        assert!(resolve_catalog_model(&catalog, None, "missing").is_err());
        assert_eq!(
            resolve_catalog_model(&catalog, None, "unique").unwrap(),
            ("opencode-go".into(), "unique".into())
        );
        assert_eq!(
            resolve_catalog_model(&catalog, Some("opencode"), "shared")
                .unwrap()
                .0,
            "opencode"
        );
    }

    #[test]
    fn config_round_trip_preserves_unknown_fields_and_resolves_cwd_paths() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested/config.json");
        let config = json!({"provider": "jina", "dimensions": 1024, "custom": {"keep": true}});
        write_config(&path, &config).unwrap();
        assert_eq!(read_config(&path).unwrap(), config);
        let root = temp.path().join("repo");
        assert_eq!(
            index_path(&root, Some(Path::new("custom.sqlite")), &config).unwrap(),
            std::env::current_dir().unwrap().join("custom.sqlite")
        );
        assert_eq!(
            index_path(&root, None, &config).unwrap(),
            root.join(".slopdex/index.sqlite")
        );
    }

    #[test]
    fn explicit_embedding_cli_options_override_config_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        let saved = json!({"embeddingProvider": "jina", "embeddingModel": "old-model",
            "embeddingDimensions": 1024, "custom": "keep"});
        fs::write(&path, saved.to_string()).unwrap();
        for (args, expected) in [
            (
                vec!["status"],
                json!({"provider": "jina", "model": "old-model", "dimensions": 1024}),
            ),
            (
                vec!["status", "--provider", "openai"],
                json!({"provider": "openai", "model": "old-model", "dimensions": 1024}),
            ),
            (
                vec!["status", "--model", "new-model"],
                json!({"provider": "jina", "model": "new-model", "dimensions": 1024}),
            ),
            (
                vec!["status", "--dimensions", "8"],
                json!({"provider": "jina", "model": "old-model", "dimensions": 8}),
            ),
            (
                vec![
                    "status",
                    "--provider",
                    "openai",
                    "--model",
                    "text-embedding-3-small",
                    "--dimensions",
                    "8",
                ],
                json!({"provider": "openai", "model": "text-embedding-3-small", "dimensions": 8}),
            ),
        ] {
            let config = effective_config(&parse(&args).global, &path).unwrap();
            let profile = Providers::new(&config).unwrap().embedding_profile();
            for key in ["provider", "model", "dimensions"] {
                assert_eq!(profile[key], expected[key], "{args:?}: {key}");
            }
            for alias in ["embeddingProvider", "embeddingModel", "embeddingDimensions"] {
                assert!(config.get(alias).is_none());
            }
            assert_eq!(config["custom"], "keep");
        }
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
            saved
        );
    }

    #[test]
    fn canonical_config_wins_conflicting_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        fs::write(
            &path,
            json!({"provider": "openai", "model": "new-model", "dimensions": 8,
            "embeddingProvider": "jina", "embeddingModel": "old-model", "embeddingDimensions": 1024,
            "descriptionFallbackModel": "new-backup", "fallbackModel": "old-backup"})
            .to_string(),
        )
        .unwrap();
        let config = effective_config(&parse(&["status"]).global, &path).unwrap();
        assert_eq!(config["provider"], "openai");
        assert_eq!(config["model"], "new-model");
        assert_eq!(config["dimensions"], 8);
        assert_eq!(config["descriptionFallbackModel"], "new-backup");
        assert!(config.get("fallbackModel").is_none());
    }

    #[test]
    fn identical_target_detection_handles_missing_paths_and_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        assert!(
            same_path(
                &temp.path().join(".slopdex/index.sqlite"),
                &temp.path().join("other/../.slopdex/index.sqlite")
            )
            .unwrap()
        );
        #[cfg(unix)]
        {
            let actual = temp.path().join("actual");
            fs::create_dir(&actual).unwrap();
            let alias = temp.path().join("alias");
            std::os::unix::fs::symlink(&actual, &alias).unwrap();
            assert!(same_path(&actual.join("index.sqlite"), &alias.join("index.sqlite")).unwrap());
        }
    }

    struct ScriptedSelection {
        default: String,
        choices: Vec<String>,
        searchable: bool,
    }

    struct ScriptedPrompts {
        input: io::Cursor<Vec<u8>>,
        output: Vec<u8>,
        catalog: Value,
        fetches: Vec<String>,
        selections: Vec<ScriptedSelection>,
    }

    impl ScriptedPrompts {
        fn new(answers: &str) -> Self {
            Self {
                input: io::Cursor::new(answers.as_bytes().to_vec()),
                output: Vec::new(),
                catalog: Value::Null,
                fetches: Vec::new(),
                selections: Vec::new(),
            }
        }
    }

    impl Prompts for ScriptedPrompts {
        fn ask(&mut self, label: &str, default: &str) -> Result<String> {
            use std::io::BufRead;

            writeln!(self.output, "{label} [{default}]")?;
            let mut line = String::new();
            if self.input.read_line(&mut line)? == 0 {
                return Err(io::Error::from(io::ErrorKind::Interrupted).into());
            }
            Ok(if line.trim().is_empty() {
                default.to_owned()
            } else {
                line.trim().to_owned()
            })
        }

        fn select(
            &mut self,
            label: &str,
            default: &str,
            choices: &[&str],
            searchable: bool,
        ) -> Result<String> {
            self.selections.push(ScriptedSelection {
                default: default.to_owned(),
                choices: choices.iter().map(|choice| (*choice).to_owned()).collect(),
                searchable,
            });
            let value = self.ask(label, default)?;
            ensure!(
                choices.contains(&value.as_str()),
                "invalid scripted selection: {value}"
            );
            Ok(value)
        }

        fn yes(&mut self, label: &str, default: bool) -> Result<bool> {
            match self
                .ask(label, if default { "yes" } else { "no" })?
                .as_str()
            {
                "yes" => Ok(true),
                "no" => Ok(false),
                value => anyhow::bail!("invalid scripted confirmation: {value}"),
            }
        }

        fn number(&mut self, label: &str, default: u64, max: Option<usize>) -> Result<usize> {
            let value = self.ask(label, &default.to_string())?;
            let n = positive(&value).map_err(anyhow::Error::msg)?;
            ensure!(
                max.is_none_or(|max| n <= max),
                "scripted number exceeds maximum"
            );
            Ok(n)
        }

        fn required(&mut self, label: &str, default: &str) -> Result<String> {
            let value = self.ask(label, default)?;
            ensure!(!value.is_empty(), "scripted value is required");
            Ok(value)
        }

        fn catalog(&mut self, provider: &str) -> Result<Value> {
            self.fetches.push(provider.to_owned());
            ensure!(!self.catalog.is_null(), "no scripted catalog supplied");
            Ok(self.catalog.clone())
        }
    }

    #[test]
    fn wizard_can_skip_llms_and_preserves_extensions() {
        let mut config = json!({"custom": "keep"});
        let mut prompts = ScriptedPrompts::new("no\nno\n\n\n\n\n\n\n\n\n\n\n");
        configure_interactively(&mut config, &mut prompts).unwrap();
        assert_eq!(config["descriptionsEnabled"], false);
        assert_eq!(config["model"], "text-embedding-3-large");
        assert_eq!(config["dimensions"], 3072);
        assert_eq!(config["custom"], "keep");
        assert!(prompts.fetches.is_empty());
        let output = String::from_utf8(prompts.output).unwrap();
        assert!(!output.contains("Description provider"));
        assert!(!output.contains("Description model"));
    }

    #[test]
    fn wizard_migrates_embedding_aliases_and_saves_selected_values() {
        for (answers, provider, model, dimensions) in [
            ("no\nno\n\n\n\n\n\n\n\n\n\n\n", "jina", "custom-jina", 16),
            (
                "no\nno\nopenai\ntext-embedding-3-small\n8\n\n\n\n\n\n\n\n",
                "openai",
                "text-embedding-3-small",
                8,
            ),
        ] {
            let mut config = json!({"embeddingProvider": "jina", "embeddingModel": "custom-jina",
                "embeddingDimensions": 16, "custom": "keep"});
            let mut prompts = ScriptedPrompts::new(answers);
            configure_interactively(&mut config, &mut prompts).unwrap();
            for alias in ["embeddingProvider", "embeddingModel", "embeddingDimensions"] {
                assert!(config.get(alias).is_none());
            }
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("config.json");
            write_config(&path, &config).unwrap();
            let saved = read_config(&path).unwrap();
            assert_eq!(saved["custom"], "keep");
            assert_eq!(
                Providers::new(&saved).unwrap().embedding_profile(),
                json!({
                    "provider": provider, "model": model, "dimensions": dimensions, "strategyVersion": "rust-v1"
                })
            );
        }
    }

    #[test]
    fn wizard_removes_fallback_alias_when_fallback_is_disabled() {
        let mut config = json!({"descriptionProvider": "openai", "descriptionModel": "primary",
            "fallbackModel": "backup"});
        let mut prompts = ScriptedPrompts::new("yes\n\n\nno\nno\n\n\n\n\n\n\n\n\n\n\n");
        configure_interactively(&mut config, &mut prompts).unwrap();
        assert!(config.get("fallbackModel").is_none());
        assert!(config.get("descriptionFallbackModel").is_none());
        assert_eq!(
            Providers::new(&config).unwrap().description_profile()["model"],
            "primary"
        );
    }

    #[test]
    fn wizard_published_models_are_searchable_scoped_and_defaulted() {
        let catalog = json!([
            {"provider": "opencode-go", "model": "first"},
            {"provider": "opencode-go", "model": "saved"},
            {"provider": "opencode", "model": "other-provider"}
        ]);
        let mut prompts = ScriptedPrompts::new("\n\n\n");
        assert_eq!(
            prompts
                .published(&catalog, "opencode-go", "Model", "saved", None)
                .unwrap(),
            "saved"
        );
        assert_eq!(
            prompts
                .published(&catalog, "opencode-go", "Fallback", "saved", Some("saved"))
                .unwrap(),
            "first"
        );
        assert_eq!(
            prompts
                .published(&catalog, "opencode-go", "Model", "retired", None)
                .unwrap(),
            "first"
        );
        assert!(
            prompts
                .selections
                .iter()
                .all(|selection| selection.searchable)
        );
        assert_eq!(prompts.selections[0].default, "saved");
        assert_eq!(prompts.selections[0].choices, ["first", "saved"]);
        assert_eq!(prompts.selections[1].choices, ["first"]);
        assert!(
            prompts
                .published(&catalog, "missing", "Model", "", None)
                .is_err()
        );
        assert!(
            prompts
                .published(&catalog, "opencode", "Fallback", "", Some("other-provider"))
                .is_err()
        );
    }

    #[test]
    fn wizard_fetches_one_catalog_and_reuses_it_for_fallback() {
        let mut config = json!({"descriptionProvider": "opencode-go", "descriptionModel": "primary",
            "descriptionFallbackModel": "backup", "descriptionsEnabled": true});
        let mut prompts = ScriptedPrompts::new(&"\n".repeat(16));
        prompts.catalog = json!([
            {"provider": "opencode-go", "model": "primary"},
            {"provider": "opencode-go", "model": "backup"}
        ]);
        configure_interactively(&mut config, &mut prompts).unwrap();
        assert_eq!(prompts.fetches, ["opencode-go"]);
        assert_eq!(config["descriptionModel"], "primary");
        assert_eq!(config["descriptionFallbackModel"], "backup");
    }

    #[test]
    fn wizard_cancellation_at_every_prompt_discards_all_changes() {
        let original = json!({"embeddingProvider": "jina", "embeddingModel": "custom",
            "embeddingDimensions": 16, "custom": "keep"});
        let answers = [
            "yes",
            "openai",
            "primary",
            "yes",
            "backup",
            "yes",
            "openai",
            "reranker",
            "100",
            "openai",
            "embedding",
            "8",
            "index.sqlite",
            "src/**",
            "-",
            "1024",
            "16",
            "4",
            "yes",
        ];
        for end in 0..answers.len() {
            let input = answers[..end]
                .iter()
                .map(|answer| format!("{answer}\n"))
                .collect::<String>();
            let mut config = original.clone();
            let error = configure_interactively(&mut config, &mut ScriptedPrompts::new(&input))
                .unwrap_err();
            assert_eq!(
                error.downcast_ref::<io::Error>().unwrap().kind(),
                io::ErrorKind::Interrupted
            );
            assert_eq!(config, original, "cancelled at prompt {end}");
        }
        let mut config = original;
        let mut prompts = ScriptedPrompts::new(&(answers.join("\n") + "\n"));
        configure_interactively(&mut config, &mut prompts).unwrap();
        assert_eq!(config["rerankerCandidates"], 100);
        assert_eq!(config["verbose"], true);
        assert!(prompts.fetches.is_empty());
    }

    #[test]
    fn wizard_catalog_and_validation_errors_discard_changes() {
        for answers in ["yes\nopencode-go\n", "no\nno\n\n\n\n\n[\n\n\n\n\n\n"] {
            let original = json!({"embeddingProvider": "jina", "custom": "keep"});
            let mut config = original.clone();
            assert!(
                configure_interactively(&mut config, &mut ScriptedPrompts::new(answers)).is_err()
            );
            assert_eq!(config, original);
        }
    }

    #[test]
    fn config_subcommands_keep_json_stdout_machine_readable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        write_config(&path, &json!({"custom": "keep"})).unwrap();
        for args in [
            vec!["config", "descriptions", "enable", "--format", "json"],
            vec!["config", "parallelism", "4", "--format", "json"],
            vec!["config", "reranker", "openai", "--format", "json"],
        ] {
            let cli = parse(&args);
            let Command::Config { action } = cli.command else {
                panic!()
            };
            let mut out = Vec::new();
            run_config(&cli.global, &path, action.as_ref(), &mut out).unwrap();
            let result: Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(result["configPath"], json!(path));
            assert_eq!(read_config(&path).unwrap()["custom"], "keep");
        }
    }
}
