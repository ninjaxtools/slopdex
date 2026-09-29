//! Command-line parsing, configuration, and presentation. Engine operations live in engine.rs.

use crate::{
    cache,
    engine::Engine,
    filter, map,
    parse::{FileStructure, StructureNode},
    providers::Providers,
    ui,
};
use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags};
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
    after_help = "Examples:\n  slopdex search \"validate an authenticated session\"\n  slopdex cross-search --cross-file-only --lines 4 --threshold 0.85-0.9\n  slopdex describe \"I want to implement a new rpc endpoint\"\n  slopdex config\n\nIndex commands refresh automatically. --no-reindex reuses the index offline."
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
    /// Index file (default: XDG cache per workspace); overrides config indexPath
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
    /// Text excerpts by default; cross-search uses clusters, cohesion uses source/match groups; cross JSON is JSONL
    #[arg(long, global = true, value_enum)]
    format: Option<Format>,
    /// Compact by default; expanded includes description comments; explicit description searches show them at any detail
    #[arg(long, global = true, value_enum, default_value = "compact")]
    detail: Detail,
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
    #[value(name = "text", alias = "summary")]
    Summary,
    Json,
    Clusters,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Detail {
    Compact,
    Standard,
    Expanded,
}

impl From<Detail> for map::Detail {
    fn from(value: Detail) -> Self {
        match value {
            Detail::Compact => Self::Compact,
            Detail::Standard => Self::Standard,
            Detail::Expanded => Self::Expanded,
        }
    }
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
    /// Show code declarations and Markdown headings without calling providers
    Map(MapArgs),
    /// Refresh and show index metadata, counts, and profiles as JSON
    Status,
    /// Refresh and inspect saved file/function indexing failures
    IndexErrors,
    /// Refresh the current working tree and Git HEAD; alias: refresh
    #[command(alias = "refresh")]
    Update(UpdateArgs),
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
    #[command(flatten)]
    selection: SelectionArgs,
}

impl Filters {
    fn options(&self) -> Value {
        let mut value = self.selection.options();
        value["minSimilarity"] = json!(self.threshold.min);
        if let Some(max) = self.threshold.max {
            value["maxSimilarity"] = json!(max);
        }
        if let Some(limit) = self.limit {
            value["limit"] = json!(limit);
        }
        value
    }
}

#[derive(Debug, Args)]
struct SelectionArgs {
    /// Repository-relative glob; repeat in order, ! excludes, last match wins
    #[arg(short = 'g', long, value_parser = valid_glob)]
    glob: Vec<String>,
    /// Qualified-name or heading-path regex; repeat for OR; cross-search selects sources
    #[arg(short = 'e', long, alias = "regex", value_parser = valid_regex)]
    regexp: Vec<String>,
    /// Match regexes case-insensitively
    #[arg(short = 'i', long)]
    ignore_case: bool,
}

impl SelectionArgs {
    fn options(&self) -> Value {
        let mut value = json!({});
        if !self.glob.is_empty() {
            value["glob"] = json!(self.glob);
        }
        if !self.regexp.is_empty() {
            value["regexp"] = json!(self.regexp);
        }
        if self.ignore_case {
            value["ignoreCase"] = json!(true);
        }
        value
    }
}

#[derive(Debug, Args)]
struct MapArgs {
    /// Files or recursive directories, repository-relative or absolute within the root
    paths: Vec<PathBuf>,
    #[command(flatten)]
    selection: SelectionArgs,
    /// Kinds, comma-separated or repeated (functions includes methods; types groups type declarations)
    #[arg(short = 'k', long = "kind", alias = "kinds", value_delimiter = ',', value_parser = valid_kind)]
    kinds: Vec<String>,
    /// Include private and unexported symbols
    #[arg(long)]
    private: bool,
}

impl MapArgs {
    fn options(&self) -> Value {
        let mut value = self.selection.options();
        if !self.paths.is_empty() {
            value["paths"] = json!(self.paths);
        }
        if !self.kinds.is_empty() {
            value["kinds"] = json!(self.kinds);
        }
        if self.private {
            value["private"] = json!(true);
        }
        value
    }

    fn existing_options(&self, root: &Path) -> Result<Option<Value>> {
        if self.paths.is_empty() {
            return Ok(Some(self.options()));
        }
        let mut paths = Vec::new();
        for path in &self.paths {
            if !path.is_absolute() {
                ensure!(
                    !path.components().any(|c| matches!(c, Component::ParentDir)),
                    "Source path must remain inside the repository"
                );
            }
            let source = if path.is_absolute() {
                path.to_owned()
            } else {
                root.join(path)
            };
            if source
                .try_exists()
                .with_context(|| format!("Cannot inspect map path {}", path.display()))?
            {
                paths.push(path);
            } else {
                ui::warning(format!(
                    "slopdex: warning: map path does not exist; ignoring: {}",
                    path.display()
                ));
            }
        }
        if paths.is_empty() {
            return Ok(None);
        }
        let mut options = self.options();
        options["paths"] = json!(paths);
        Ok(Some(options))
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
#[command(mut_arg("threshold", |arg| arg.default_value("0.8")))]
struct CrossArgs {
    #[command(flatten)]
    filters: Filters,
    /// Matches kept per source function
    #[arg(long, default_value = "5", value_parser = positive)]
    matches: usize,
    /// Inclusive minimum, or inclusive-min/exclusive-max line count for sources and candidates
    #[arg(
        long,
        visible_alias = "min-lines",
        value_name = "N[-N]",
        default_value = "2",
        value_parser = line_range
    )]
    lines: LineRange,
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
        value["minLines"] = json!(self.lines.min);
        if let Some(max) = self.lines.max {
            value["maxLines"] = json!(max);
        }
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
    /// This update command currently supports HEAD only
    #[arg(long, default_value = "HEAD", value_parser = ["HEAD"])]
    target: String,
}

#[derive(Clone, Debug, PartialEq)]
struct Threshold {
    min: f64,
    max: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LineRange {
    min: usize,
    max: Option<usize>,
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

fn line_range(input: &str) -> std::result::Result<LineRange, String> {
    let input = input.trim();
    if let Ok(min) = positive(input) {
        return Ok(LineRange { min, max: None });
    }
    if let Some((min, max)) = input.split_once('-')
        && let (Ok(min), Ok(max)) = (positive(min.trim()), positive(max.trim()))
        && min < max
    {
        return Ok(LineRange {
            min,
            max: Some(max),
        });
    }
    Err("lines must be a positive integer or min-max, with min < max (maximum exclusive)".into())
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

fn valid_glob(input: &str) -> std::result::Result<String, String> {
    filter::Selection::compile(&json!({"glob": [input]}))
        .map(|_| input.to_owned())
        .map_err(|error| format!("{error:#}"))
}

fn valid_kind(input: &str) -> std::result::Result<String, String> {
    filter::normalize_kind(input).map_err(|error| error.to_string())
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
    if cli.global.index.is_none() && config["indexPath"].is_null() {
        migrate_legacy_index(&root, &index)?;
    }
    if let Command::Descriptions { action } = &cli.command {
        config["descriptionsEnabled"] = json!(action.enabled());
    }
    let is_map = matches!(&cli.command, Command::Map(_));
    let mut engine = ui::spin("Opening index", || {
        if is_map {
            Engine::open_map(&root, &index, config.clone())
        } else {
            Engine::open(&root, &index, config.clone())
        }
    })?;
    let map_options = if let Command::Map(args) = &cli.command {
        args.existing_options(&root)?
    } else {
        None
    };
    let refreshed = if cli.global.no_reindex {
        None
    } else if is_map {
        ui::spin("Refreshing structure", || engine.refresh_structure())?;
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
        Command::Map(_) => {
            let rows = if let Some(options) = map_options.as_ref() {
                ui::spin("Mapping repository structure", || engine.map(options))?
            } else {
                Vec::new()
            };
            print_map(&mut out, &rows, format, cli.global.detail, Some(&engine))?;
        }
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
            print_search(
                &mut out,
                &rows,
                format,
                cli.global.detail,
                args.descriptions,
                &mut Presentation::new(&engine),
            )?;
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
            print_search(
                &mut out,
                &rows,
                format,
                cli.global.detail,
                kind == "search-descriptions",
                &mut Presentation::new(&engine),
            )?;
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
                writeln!(out, "Explanation\n{}", text(&result, "description"))?;
                // Describe uses the same cached query results it used as LLM context.
                let references = engine.search(&args.query.query, "search", &options)?;
                if !references.is_empty() {
                    writeln!(out, "\nReferences")?;
                    print_search(
                        &mut out,
                        &references,
                        format,
                        cli.global.detail,
                        false,
                        &mut Presentation::new(&engine),
                    )?;
                }
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
                CrossOutput::new(
                    format,
                    target.is_none(),
                    args.cohesion,
                    args.filters.limit,
                    cli.global.detail,
                ),
                &mut Presentation::new(&engine),
                target.as_ref().map(Presentation::new).as_mut(),
            )?;
        }
        Command::Status => print_json(&mut out, &engine.status()?)?,
        Command::IndexErrors => print_errors(&mut out, &engine.errors()?, format)?,
        Command::Update(_) => print_json(
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
    if let Some(path) = explicit.or_else(|| config["indexPath"].as_str().map(Path::new)) {
        return absolute(path);
    }
    let root = root
        .canonicalize()
        .context("Repository root does not exist")?;
    Ok(cache::directory()?
        .join("workspaces")
        .join(crate::hash(root.as_os_str().as_encoded_bytes()))
        .join("index.sqlite"))
}

/// Migrate a compatible snapshot with SQLite backup so committed WAL data is included.
fn migrate_legacy_index(root: &Path, index: &Path) -> Result<()> {
    let old = root.join(".slopdex/index.sqlite");
    if index.exists() || !old.exists() || same_path(&old, index)? {
        return Ok(());
    }
    let parent = index.parent().context("Index path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut lock_path = index.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(PathBuf::from(lock_path))?;
    lock.lock_exclusive()?;
    if index.exists() {
        return Ok(());
    }
    let source = Connection::open_with_flags(&old, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let identity: String = match source.query_row(
        "SELECT value FROM metadata WHERE key='identity'",
        [],
        |row| row.get(0),
    ) {
        Ok(value) => value,
        Err(_) => return Ok(()),
    };
    if serde_json::from_str::<Value>(&identity)? != json!({"schema":3,"root":root.canonicalize()?})
    {
        return Ok(());
    }
    let temporary = parent.join(format!(".index.sqlite.migrate-{}", std::process::id()));
    let copied = (|| -> Result<()> {
        let mut target = Connection::open(&temporary)?;
        let backup = rusqlite::backup::Backup::new(&source, &mut target)?;
        backup.run_to_completion(256, std::time::Duration::from_millis(20), None)?;
        drop(backup);
        drop(target);
        fs::rename(&temporary, index)?;
        Ok(())
    })();
    if copied.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    copied
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
        "artifactCachePath",
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
    if let Some(s3) = config.get("artifactS3") {
        ensure!(s3.is_object(), "artifactS3 must be an object");
        for key in ["bucket", "endpoint", "region", "prefix"] {
            if let Some(value) = s3.get(key) {
                ensure!(
                    value.as_str().is_some_and(|text| !text.trim().is_empty()),
                    "artifactS3.{key} must be a non-empty string"
                );
            }
        }
        ensure!(s3["bucket"].is_string(), "artifactS3.bucket is required");
        if let Some(value) = s3.get("pathStyle") {
            ensure!(value.is_boolean(), "artifactS3.pathStyle must be a boolean");
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

fn print_map(
    out: &mut impl Write,
    rows: &[Value],
    format: Format,
    detail: Detail,
    engine: Option<&Engine>,
) -> Result<()> {
    if format == Format::Json {
        return print_json(out, &rows);
    }
    for (index, row) in rows.iter().enumerate() {
        let path = row["path"].as_str().context("map result is missing path")?;
        let nodes: Vec<StructureNode> = serde_json::from_value(row["nodes"].clone())
            .with_context(|| format!("decode map nodes for {path}"))?;
        let full = engine
            .map(|engine| engine.presentation_structure(path))
            .transpose()?
            .flatten();
        let symbols = if detail == Detail::Expanded {
            engine
                .map(|engine| engine.presentation_symbol_descriptions(path))
                .transpose()?
        } else {
            None
        };
        if index > 0 {
            writeln!(out)?;
        }
        write!(
            out,
            "{}",
            map::render_with_descriptions(
                &nodes,
                Some(path),
                detail.into(),
                engine.and_then(|engine| engine.presentation_source(path)),
                full.as_ref(),
                map::Descriptions {
                    file: if detail == Detail::Expanded {
                        engine.and_then(|engine| engine.presentation_file_description(path))
                    } else {
                        None
                    },
                    symbols: symbols.as_ref(),
                },
            )
        )?;
    }
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
        format!("score={rerank:.2} similarity={similarity:.2}")
    } else {
        format!("score={similarity:.2}")
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
                    "  [combined thirds; code {code:.2}, description {description:.2}, file {file:.2}]"
                )
            } else {
                format!("  [combined 50/50; description {description:.2}, file {file:.2}]")
            }
        }
        _ => String::new(),
    }
}

struct Presentation<'a> {
    engine: Option<&'a Engine>,
    structures: HashMap<String, Option<FileStructure>>,
}

impl<'a> Presentation<'a> {
    fn new(engine: &'a Engine) -> Self {
        Self {
            engine: Some(engine),
            structures: HashMap::new(),
        }
    }

    #[cfg(test)]
    fn empty() -> Self {
        Self {
            engine: None,
            structures: HashMap::new(),
        }
    }

    fn structure(&mut self, path: &str) -> Result<Option<&FileStructure>> {
        if !self.structures.contains_key(path) {
            self.structures.insert(
                path.to_owned(),
                self.engine
                    .map(|engine| engine.presentation_structure(path))
                    .transpose()?
                    .flatten(),
            );
        }
        Ok(self.structures[path].as_ref())
    }

    fn context(&mut self, function: &Value) -> Result<Vec<StructureNode>> {
        Ok(self
            .structure(text(function, "path"))?
            .and_then(|structure| {
                map::matching_node(structure, function).map(|node| map::ancestors(structure, node))
            })
            .unwrap_or_default())
    }

    fn markdown_context(&mut self, chunk: &Value) -> Result<Vec<StructureNode>> {
        Ok(self
            .structure(text(chunk, "path"))?
            .map(|structure| map::heading_context(structure, chunk))
            .unwrap_or_default())
    }
}

fn print_function(
    out: &mut impl Write,
    function: &Value,
    annotation: &str,
    detail: Detail,
    show_description: bool,
    presentation: &mut Presentation<'_>,
) -> Result<()> {
    print_function_with_header(
        out,
        function,
        annotation,
        detail,
        show_description,
        presentation,
        true,
    )
}

fn print_function_with_header(
    out: &mut impl Write,
    function: &Value,
    annotation: &str,
    detail: Detail,
    show_description: bool,
    presentation: &mut Presentation<'_>,
    file_header: bool,
) -> Result<()> {
    if file_header {
        write!(out, "{}", map::file_header(text(function, "path")))?;
        if (detail == Detail::Expanded || show_description)
            && let Some(description) = presentation
                .engine
                .and_then(|engine| engine.presentation_file_description(text(function, "path")))
                .filter(|text| !text.trim().is_empty())
        {
            writeln!(
                out,
                "{}",
                map::comment_block(text(function, "path"), description)
            )?;
        }
    }
    let nodes = presentation.context(function)?;
    let qualified = nodes
        .last()
        .filter(|node| nodes.len() == 1 && node.parent_id.is_some())
        .map(|node| node.qualified_name.as_str());
    let annotation = [Some(annotation), qualified]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let description = if detail == Detail::Expanded || show_description {
        function["description"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
    } else {
        None
    };
    if !nodes.is_empty() {
        write!(
            out,
            "{}",
            map::render_context_with_description(
                &nodes,
                Some(&annotation),
                detail.into(),
                description
            )
        )?;
    } else {
        let start = function["startLine"].as_u64().unwrap_or(1) as usize;
        let end = function["endLine"].as_u64().unwrap_or(start as u64) as usize;
        let name = function["qualifiedName"]
            .as_str()
            .unwrap_or_else(|| text(function, "name"));
        let suffix = map::inline_note(text(function, "path"), Some(&annotation), description);
        writeln!(out, "{}{name}{suffix}", map::hunk(start, end, None))?;
    }
    Ok(())
}

#[cfg(test)]
fn print_markdown(
    out: &mut impl Write,
    row: &Value,
    detail: Detail,
    show_description: bool,
    presentation: &mut Presentation<'_>,
) -> Result<()> {
    print_markdown_with_header(out, row, detail, show_description, presentation, true)
}

fn print_markdown_with_header(
    out: &mut impl Write,
    row: &Value,
    detail: Detail,
    show_description: bool,
    presentation: &mut Presentation<'_>,
    header: bool,
) -> Result<()> {
    let chunk = &row["chunk"];
    if header {
        write!(out, "{}", map::file_header(text(chunk, "path")))?;
    }
    if header
        && (detail == Detail::Expanded || show_description)
        && let Some(description) = presentation
            .engine
            .and_then(|engine| engine.presentation_file_description(text(chunk, "path")))
            .filter(|text| !text.trim().is_empty())
    {
        writeln!(
            out,
            "{}",
            map::comment_block(text(chunk, "path"), description)
        )?;
    }
    let start = chunk["startLine"].as_u64().unwrap_or(1) as usize;
    let end = chunk["endLine"].as_u64().unwrap_or(start as u64) as usize;
    write!(out, "{}", map::hunk(start, end, None))?;
    let nodes = presentation.markdown_context(chunk)?;
    let score = rank(row);
    let last_heading = if let Some(node) = nodes.last() {
        write!(
            out,
            "{}",
            map::render_declarations_with_annotation(&nodes, detail.into(), Some(&score))
        )?;
        Some(node.signature.clone())
    } else {
        let content = text(chunk, "content");
        let mut last = None;
        let headings: Vec<_> = array(&chunk["headingPath"])
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for (depth, name) in headings.iter().enumerate() {
            let heading = content
                .lines()
                .find(|line| line.starts_with('#') && line.trim_start_matches('#').trim() == *name)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{} {name}", "#".repeat(depth + 1)));
            let suffix = if depth + 1 == headings.len() {
                map::inline_note(text(chunk, "path"), Some(&score), None)
            } else {
                String::new()
            };
            writeln!(out, "{}{heading}{suffix}", "  ".repeat(depth))?;
            last = Some(heading);
        }
        if last.is_none() {
            writeln!(
                out,
                "{}{}",
                content
                    .lines()
                    .find(|line| !line.is_empty())
                    .unwrap_or("(untitled)"),
                map::inline_note(text(chunk, "path"), Some(&score), None)
            )?;
        }
        last
    };
    write!(
        out,
        "{}",
        markdown_extra(chunk, last_heading.as_deref(), detail)
    )?;
    Ok(())
}

fn markdown_extra(chunk: &Value, last_heading: Option<&str>, detail: Detail) -> String {
    let mut output = String::new();
    if detail != Detail::Compact {
        let content = text(chunk, "content");
        let body = if let Some(heading) = last_heading.filter(|heading| content.contains(*heading))
        {
            content
                .split_once(heading)
                .unwrap()
                .1
                .trim_start_matches('\n')
        } else if last_heading.is_none() {
            // Without a heading, compact output already displays the first line.
            content.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
        } else {
            content
        };
        if detail == Detail::Standard {
            let summary = body.split_whitespace().collect::<Vec<_>>().join(" ");
            if !summary.is_empty() {
                let preview = if summary.chars().count() > 160 {
                    format!("{}…", summary.chars().take(159).collect::<String>())
                } else {
                    summary
                };
                output.push_str(&format!("@ preview: {preview}\n"));
            }
        } else if !body.trim().is_empty() {
            output.push_str(body);
            output.push('\n');
        }
    }
    output
}

struct RankedHit<'a> {
    item: &'a Value,
    row: &'a Value,
    annotation: String,
    score: f64,
    markdown: bool,
    description: bool,
    target: bool,
}

fn print_ranked_files(
    out: &mut impl Write,
    hits: Vec<RankedHit<'_>>,
    detail: Detail,
    source: &mut Presentation<'_>,
    mut target: Option<&mut Presentation<'_>>,
) -> Result<()> {
    let mut files: BTreeMap<(bool, String), Vec<RankedHit<'_>>> = BTreeMap::new();
    for hit in hits {
        files
            .entry((hit.target, text(hit.item, "path").to_owned()))
            .or_default()
            .push(hit);
    }
    let mut files: Vec<_> = files.into_iter().collect();
    files.sort_by(|a, b| {
        let maximum = |hits: &Vec<RankedHit<'_>>| {
            hits.iter()
                .map(|hit| hit.score)
                .fold(f64::NEG_INFINITY, f64::max)
        };
        maximum(&b.1)
            .total_cmp(&maximum(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    for (index, ((is_target, path), hits)) in files.into_iter().enumerate() {
        if index > 0 {
            writeln!(out)?;
        }
        if is_target && target.is_some() {
            print_ranked_file(out, &path, &hits, detail, target.as_deref_mut().unwrap())?;
        } else {
            print_ranked_file(out, &path, &hits, detail, source)?;
        }
    }
    Ok(())
}

fn print_ranked_file(
    out: &mut impl Write,
    path: &str,
    hits: &[RankedHit<'_>],
    detail: Detail,
    presentation: &mut Presentation<'_>,
) -> Result<()> {
    let structure = presentation.structure(path)?.cloned();
    let mut selected = HashMap::<usize, StructureNode>::new();
    let mut annotations: HashMap<usize, String> = HashMap::new();
    let mut extras = HashMap::new();
    let mut descriptions = HashMap::new();
    let mut unmatched = Vec::new();
    for hit in hits {
        let chain = structure
            .as_ref()
            .map(|structure| {
                if hit.markdown {
                    map::heading_context(structure, hit.item)
                } else {
                    map::matching_node(structure, hit.item)
                        .map(|node| map::ancestors(structure, node))
                        .unwrap_or_default()
                }
            })
            .unwrap_or_default();
        if let Some(matched) = chain.last() {
            if let Some(previous) = annotations.get_mut(&matched.id) {
                if previous != &hit.annotation {
                    previous.push_str(" | ");
                    previous.push_str(&hit.annotation);
                }
                if hit.markdown {
                    extras
                        .entry(matched.id)
                        .or_insert_with(String::new)
                        .push_str(&markdown_extra(hit.item, Some(&matched.signature), detail));
                }
                continue;
            }
            annotations.insert(matched.id, hit.annotation.clone());
            if hit.markdown {
                extras.insert(
                    matched.id,
                    markdown_extra(hit.item, Some(&matched.signature), detail),
                );
            } else if (detail == Detail::Expanded || hit.description)
                && let Some(description) = hit.item["description"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
            {
                descriptions.insert(matched.id, description.to_owned());
            }
            for node in chain {
                selected.insert(node.id, node);
            }
        } else {
            unmatched.push(hit);
        }
    }
    let mut nodes: Vec<_> = selected.into_values().collect();
    nodes.sort_by_key(|node| (node.start_byte, node.id));
    unmatched.sort_by_key(|hit| {
        (
            hit.item["startLine"].as_u64().unwrap_or(1),
            hit.item["startColumn"].as_u64().unwrap_or(1),
        )
    });
    if !nodes.is_empty() {
        let file_description = (detail == Detail::Expanded
            || hits.iter().any(|hit| hit.description))
        .then(|| {
            presentation
                .engine
                .and_then(|engine| engine.presentation_file_description(path))
        })
        .flatten();
        write!(
            out,
            "{}",
            map::render_with_hits(
                &nodes,
                Some(path),
                detail.into(),
                presentation
                    .engine
                    .and_then(|engine| engine.presentation_source(path)),
                structure.as_ref(),
                map::Descriptions {
                    file: file_description,
                    symbols: Some(&descriptions)
                },
                map::HitDetails {
                    annotations: Some(&annotations),
                    extras: Some(&extras)
                },
            )
        )?;
    }
    for (fallback_index, hit) in unmatched.iter().enumerate() {
        if !nodes.is_empty() || fallback_index > 0 {
            writeln!(out)?;
        }
        if hit.markdown {
            print_markdown_with_header(
                out,
                hit.row,
                detail,
                hit.description,
                presentation,
                nodes.is_empty() && fallback_index == 0,
            )?;
        } else {
            print_function_with_header(
                out,
                hit.item,
                &hit.annotation,
                detail,
                hit.description,
                presentation,
                nodes.is_empty() && fallback_index == 0,
            )?;
        }
    }
    Ok(())
}

fn print_search(
    out: &mut impl Write,
    rows: &[Value],
    format: Format,
    detail: Detail,
    descriptions: bool,
    presentation: &mut Presentation<'_>,
) -> Result<()> {
    if format == Format::Json {
        return print_json(out, &rows);
    }
    if rows.is_empty() {
        writeln!(out, "No matches.")?;
    }
    print_ranked_files(
        out,
        rows.iter()
            .map(|row| {
                let markdown = row["type"] == "markdown";
                RankedHit {
                    item: if markdown {
                        &row["chunk"]
                    } else {
                        &row["function"]
                    },
                    row,
                    annotation: format!(
                        "{}{}",
                        rank(row),
                        if detail == Detail::Expanded {
                            score_details(row)
                        } else {
                            String::new()
                        }
                    ),
                    score: row["rerankScore"]
                        .as_f64()
                        .unwrap_or_else(|| number(row, "similarity")),
                    markdown,
                    description: descriptions,
                    target: false,
                }
            })
            .collect(),
        detail,
        presentation,
        None,
    )
}

/// Describe uses exactly the expanded text search presentation as its LLM context.
pub(crate) fn describe_search_context(engine: &Engine, rows: &[Value]) -> Result<String> {
    let mut out = Vec::new();
    print_search(
        &mut out,
        rows,
        Format::Summary,
        Detail::Expanded,
        false,
        &mut Presentation::new(engine),
    )?;
    Ok(String::from_utf8(out)?)
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

#[derive(Clone, Copy)]
struct CrossOutput {
    format: Format,
    same_index: bool,
    cohesion: bool,
    limit: Option<usize>,
    detail: Detail,
}

impl CrossOutput {
    fn new(
        format: Format,
        same_index: bool,
        cohesion: bool,
        limit: Option<usize>,
        detail: Detail,
    ) -> Self {
        Self {
            format,
            same_index,
            cohesion,
            limit,
            detail,
        }
    }
}

fn print_cross(
    out: &mut impl Write,
    mut rows: Vec<Value>,
    options: CrossOutput,
    source_presentation: &mut Presentation<'_>,
    mut target_presentation: Option<&mut Presentation<'_>>,
) -> Result<()> {
    let CrossOutput {
        format,
        same_index,
        cohesion,
        limit,
        detail,
    } = options;
    rows.retain(|row| !array(&row["matches"]).is_empty());
    if format == Format::Clusters {
        return print_clusters(
            out,
            &rows,
            same_index,
            limit,
            detail,
            source_presentation,
            target_presentation,
        );
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
    if format == Format::Summary && !rows.is_empty() {
        let mut hits = Vec::new();
        for row in rows.iter().take(limit.unwrap_or(usize::MAX)) {
            let matches = array(&row["matches"]);
            hits.push(RankedHit {
                item: &row["source"],
                row,
                annotation: "source".to_owned(),
                score: matches
                    .iter()
                    .map(|item| number(item, "similarity"))
                    .fold(f64::NEG_INFINITY, f64::max),
                markdown: false,
                description: false,
                target: false,
            });
            for item in matches {
                let distance = item["physicalDistance"]
                    .as_f64()
                    .map(|d| format!(" distance={d}"))
                    .unwrap_or_default();
                hits.push(RankedHit {
                    item: &item["function"],
                    row: item,
                    annotation: format!(
                        "{}{}{distance}{}",
                        if same_index { "" } else { "target " },
                        rank(item),
                        if detail == Detail::Expanded {
                            score_details(item)
                        } else {
                            String::new()
                        }
                    ),
                    score: number(item, "similarity"),
                    markdown: false,
                    description: false,
                    target: !same_index,
                });
            }
        }
        return print_ranked_files(out, hits, detail, source_presentation, target_presentation);
    }
    for (index, row) in rows.iter().take(limit.unwrap_or(usize::MAX)).enumerate() {
        if format == Format::Json {
            serde_json::to_writer(&mut *out, row)?;
            writeln!(out)?;
        } else {
            if index > 0 {
                writeln!(out)?;
            }
            writeln!(out, "Source")?;
            print_function(out, &row["source"], "", detail, false, source_presentation)?;
            for item in array(&row["matches"]) {
                let distance = item["physicalDistance"]
                    .as_f64()
                    .map(|d| format!(" distance={d}"))
                    .unwrap_or_default();
                writeln!(out)?;
                let annotation = format!(
                    "{}{}{distance}{}",
                    if same_index { "" } else { "target " },
                    rank(item),
                    if detail == Detail::Expanded {
                        score_details(item)
                    } else {
                        String::new()
                    }
                );
                if let Some(target) = target_presentation.as_deref_mut() {
                    print_function(out, &item["function"], &annotation, detail, false, target)?;
                } else {
                    print_function(
                        out,
                        &item["function"],
                        &annotation,
                        detail,
                        false,
                        source_presentation,
                    )?;
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct Cluster {
    members: Vec<ClusterMember>,
    edges: Vec<(String, String, f64)>,
    min: f64,
    max: f64,
    combined: bool,
}

#[derive(Clone, Debug)]
struct ClusterMember {
    function: Value,
    role: &'static str,
}

impl ClusterMember {
    fn label(&self) -> String {
        format!(
            "{}{}",
            if self.role == "index" {
                ""
            } else if self.role == "source" {
                "[source] "
            } else {
                "[target] "
            },
            function_location(&self.function)
        )
    }

    fn source_key(&self) -> (u8, &str, u64, u64, &str) {
        (
            u8::from(self.role == "target"),
            text(&self.function, "path"),
            self.function["startLine"].as_u64().unwrap_or(1),
            self.function["startColumn"].as_u64().unwrap_or(1),
            text(&self.function, "qualifiedName"),
        )
    }
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
    let mut nodes = BTreeMap::<String, ClusterMember>::new();
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
                ClusterMember {
                    function: source.clone(),
                    role: if same_index { "index" } else { "source" },
                },
            );
            nodes.insert(
                right.clone(),
                ClusterMember {
                    function: function.clone(),
                    role: if same_index { "index" } else { "target" },
                },
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
            edges: Vec::new(),
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            combined: false,
        };
        while let Some(key) = pending.pop() {
            cluster.members.push(nodes[&key].clone());
            for (neighbor, score, combined) in &neighbors[&key] {
                if key < *neighbor {
                    cluster
                        .edges
                        .push((nodes[&key].label(), nodes[neighbor].label(), *score));
                }
                cluster.min = cluster.min.min(*score);
                cluster.max = cluster.max.max(*score);
                cluster.combined |= combined;
                if seen.insert(neighbor.clone()) {
                    pending.push(neighbor.clone());
                }
            }
        }
        cluster
            .members
            .sort_by(|a, b| a.source_key().cmp(&b.source_key()));
        cluster.edges.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.total_cmp(&b.2))
        });
        cluster.edges.dedup();
        result.push(cluster);
    }
    result.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then_with(|| a.members[0].label().cmp(&b.members[0].label()))
    });
    result
}

fn print_clusters(
    out: &mut impl Write,
    rows: &[Value],
    same_index: bool,
    limit: Option<usize>,
    detail: Detail,
    source_presentation: &mut Presentation<'_>,
    mut target_presentation: Option<&mut Presentation<'_>>,
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
            format!("{:.2}", cluster.min)
        } else {
            format!("{:.2}-{:.2}", cluster.min, cluster.max)
        };
        writeln!(
            out,
            "Cluster {} · {} functions · similarity {range}{}",
            index + 1,
            cluster.members.len(),
            if cluster.combined && detail == Detail::Expanded {
                ", combined code + callable description + file description"
            } else {
                ""
            }
        )?;
        if detail == Detail::Expanded {
            for (left, right, similarity) in &cluster.edges {
                writeln!(out, "@ match: {left} ↔ {right} score={similarity:.2}")?;
            }
        }
        let mut previous_file = None;
        for member in &cluster.members {
            let file = (member.role, text(&member.function, "path"));
            let new_file = previous_file != Some(file);
            if new_file {
                writeln!(out)?;
            }
            let role = if same_index { "" } else { member.role };
            if member.role == "target"
                && let Some(target) = target_presentation.as_deref_mut()
            {
                print_function_with_header(
                    out,
                    &member.function,
                    role,
                    detail,
                    false,
                    target,
                    new_file,
                )?;
            } else {
                print_function_with_header(
                    out,
                    &member.function,
                    role,
                    detail,
                    false,
                    source_presentation,
                    new_file,
                )?;
            }
            previous_file = Some(file);
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
    fn shared_selection_arguments_reach_all_search_options() {
        for command in [
            "search",
            "search-code",
            "search-descriptions",
            "search-md",
            "describe",
            "cross-search",
        ] {
            let mut argv = vec![command];
            if command != "cross-search" {
                argv.push("query");
            }
            argv.extend([
                "-g",
                "*.rs",
                "--glob",
                "!src/generated/**",
                "-g",
                "src/generated/keep.rs",
                "-e",
                "^Service\\.",
                "--regex",
                "Setup",
                "--regexp",
                "Guide",
                "-i",
            ]);
            let cli = parse(&argv);
            let options = match cli.command {
                Command::Search(args) => args.query.filters.options(),
                Command::SearchCode(args)
                | Command::SearchDescriptions(args)
                | Command::SearchMd(args) => args.filters.options(),
                Command::Describe(args) => args.query.filters.options(),
                Command::CrossSearch(args) => args.options(),
                _ => unreachable!(),
            };
            assert_eq!(
                options["glob"],
                json!(["*.rs", "!src/generated/**", "src/generated/keep.rs"]),
                "{command}"
            );
            assert_eq!(
                options["regexp"],
                json!(["^Service\\.", "Setup", "Guide"]),
                "{command}"
            );
            assert_eq!(options["ignoreCase"], true, "{command}");
        }
    }

    #[test]
    fn map_paths_kinds_and_shared_selection_parse() {
        let cli = parse(&[
            "--root",
            "/repo",
            "map",
            "src",
            "/repo/docs",
            "-g",
            "*.rs",
            "-g",
            "!test.rs",
            "-e",
            "Service",
            "--regex",
            "Guide",
            "--ignore-case",
            "-k",
            "Functions,Types",
            "--kind",
            "Methods",
            "--kinds",
            "imports",
            "--private",
            "--format",
            "json",
            "--no-reindex",
        ]);
        assert_eq!(cli.global.format, Some(Format::Json));
        assert!(cli.global.no_reindex);
        let Command::Map(args) = cli.command else {
            panic!()
        };
        assert_eq!(
            args.options(),
            json!({
                "paths": ["src", "/repo/docs"],
                "glob": ["*.rs", "!test.rs"],
                "regexp": ["Service", "Guide"],
                "ignoreCase": true,
                "kinds": ["functions", "types", "method", "import"],
                "private": true
            })
        );
        let Command::Map(args) = parse(&["map"]).command else {
            panic!()
        };
        assert_eq!(args.options(), json!({}));
        for args in [
            vec!["map", "-k", "unknown"],
            vec!["map", "-k", "functions,"],
            vec!["map", "-g", "["],
            vec!["map", "-e", "["],
            vec!["map", "--threshold", "0.5"],
        ] {
            assert!(Cli::try_parse_from(std::iter::once("slopdex").chain(args)).is_err());
        }
    }

    #[test]
    fn map_json_is_a_full_array_and_summary_uses_structure_renderer() {
        let structure = crate::parse::parse(
            "example.rs",
            "pub struct Service { pub count: usize }\npub fn run() {}\n",
        )
        .unwrap()
        .structure;
        let rows = vec![json!({"path": "example.rs", "nodes": structure.nodes})];
        let mut json_output = Vec::new();
        print_map(&mut json_output, &rows, Format::Json, Detail::Compact, None).unwrap();
        let decoded: Value = serde_json::from_slice(&json_output).unwrap();
        assert_eq!(decoded, json!(rows));
        assert!(decoded[0]["nodes"][0].get("startByte").is_some());
        let mut summary = Vec::new();
        print_map(&mut summary, &rows, Format::Summary, Detail::Compact, None).unwrap();
        assert_eq!(
            String::from_utf8(summary).unwrap(),
            map::render_nodes(&structure.nodes, Some("example.rs"))
        );
        let mut empty = Vec::new();
        print_map(&mut empty, &[], Format::Json, Detail::Compact, None).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&empty).unwrap(), json!([]));
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
                "--lines",
                "4",
                "--threshold",
                "0.9",
            ],
            vec![
                "cross-search",
                "--cross-file-only",
                "--lines",
                "4",
                "--threshold",
                "0.85-0.9",
            ],
            vec![
                "cross-search",
                "--uncommitted",
                "--cross-file-only",
                "--lines",
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
    fn line_ranges_have_an_exclusive_maximum() {
        for (value, min, max) in [("1", 1, None), ("4", 4, None), ("4-10", 4, Some(10))] {
            let cli = parse(&["cross-search", "--lines", value]);
            let Command::CrossSearch(args) = cli.command else {
                panic!()
            };
            assert_eq!(args.lines, LineRange { min, max });
            let options = args.options();
            assert_eq!(options["minLines"], json!(min));
            assert_eq!(options.get("maxLines"), max.map(|n| json!(n)).as_ref());
        }
        for value in ["0", "-1", "1-1", "2-1", "1-", "1-2-3", "1.5", ""] {
            assert!(
                Cli::try_parse_from(["slopdex", "cross-search", "--lines", value]).is_err(),
                "accepted {value}"
            );
        }

        let Command::CrossSearch(args) = parse(&["cross-search", "--min-lines", "4-10"]).command
        else {
            panic!()
        };
        assert_eq!(
            args.lines,
            LineRange {
                min: 4,
                max: Some(10)
            }
        );
    }

    #[test]
    fn cross_search_has_a_selective_default_without_changing_query_defaults() {
        for command in [
            "search",
            "search-code",
            "search-descriptions",
            "search-md",
            "describe",
        ] {
            let cli = parse(&[command, "query"]);
            let filters = match cli.command {
                Command::Search(args) => args.query.filters,
                Command::SearchCode(args)
                | Command::SearchDescriptions(args)
                | Command::SearchMd(args) => args.filters,
                Command::Describe(args) => args.query.filters,
                _ => unreachable!(),
            };
            assert_eq!(filters.options()["minSimilarity"], 0.3, "{command}");
            let help = Cli::try_parse_from(["slopdex", command, "--help"])
                .unwrap_err()
                .to_string();
            assert!(help.contains("[default: 0.3]"), "{command}: {help}");
        }
        for args in [vec!["cross-search"], vec!["cross-search", "--cohesion"]] {
            let Command::CrossSearch(args) = parse(&args).command else {
                unreachable!()
            };
            assert_eq!(args.options()["minSimilarity"], 0.8);
        }
        let help = Cli::try_parse_from(["slopdex", "cross-search", "--help"])
            .unwrap_err()
            .to_string();
        assert!(help.contains("[default: 0.8]"), "{help}");
        assert!(!help.contains("[default: 0.3]"), "{help}");
    }

    #[test]
    fn options_defaults_and_limit_validation() {
        let cli = parse(&["cross-search"]);
        let Command::CrossSearch(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.matches, 5);
        assert_eq!(args.lines, LineRange { min: 2, max: None });
        assert_eq!(args.filters.threshold.min, 0.8);
        assert!(args.filters.limit.is_none());
        let cli = parse(&["cross-search", "--limit", "1"]);
        let Command::CrossSearch(args) = cli.command else {
            panic!()
        };
        assert!(args.options().get("limit").is_none());
        for option in ["--limit", "--matches", "--lines", "--min-lines"] {
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
            vec!["map", "--format", "clusters"],
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
            "update",
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
        print_cross(
            &mut out,
            rows,
            CrossOutput::new(Format::Clusters, true, false, Some(1), Detail::Compact),
            &mut Presentation::empty(),
            None,
        )
        .unwrap();
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("Cluster 1 · 4 functions · similarity 0.91-0.95"));
        assert!(output.contains("*** src/d.rs\n@@ 1 @@\nd"));
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
        print_cross(
            &mut out,
            rows,
            CrossOutput::new(Format::Json, true, true, Some(1), Detail::Compact),
            &mut Presentation::empty(),
            None,
        )
        .unwrap();
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
        fs::create_dir(&root).unwrap();
        assert_eq!(
            index_path(&root, Some(Path::new("custom.sqlite")), &config).unwrap(),
            std::env::current_dir().unwrap().join("custom.sqlite")
        );
        assert_eq!(
            index_path(&root, None, &config).unwrap(),
            cache::directory()
                .unwrap()
                .join("workspaces")
                .join(crate::hash(
                    root.canonicalize().unwrap().as_os_str().as_encoded_bytes()
                ))
                .join("index.sqlite")
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

    #[test]
    fn config_read_distinguishes_missing_files_from_invalid_or_unreadable_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        assert_eq!(read_config(&path).unwrap(), json!({}));
        for bytes in [
            b"".as_slice(),
            b"{\"provider\":",
            b"{} {}",
            b"{\"model\":\"\xff\"}",
            b"null",
            b"[]",
            b"true",
            b"42",
            b"\"openai\"",
        ] {
            fs::write(&path, bytes).unwrap();
            let error = read_config(&path).unwrap_err();
            assert!(format!("{error:#}").contains(&path.display().to_string()));
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        let error = read_config(temp.path()).unwrap_err();
        assert!(error.to_string().contains("read config"));
    }

    #[test]
    fn invalid_config_settings_cannot_partially_persist_a_config_action() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        let cli = parse(&["config", "descriptions", "enable", "--format", "json"]);
        let Command::Config { action } = cli.command else {
            panic!()
        };
        let mut cases = Vec::new();
        for key in ["provider", "descriptionProvider"] {
            for value in [Value::Null, json!(false), json!("unsupported")] {
                cases.push((key, value));
            }
        }
        for key in [
            "model",
            "indexPath",
            "descriptionModel",
            "descriptionFallbackModel",
        ] {
            for value in [Value::Null, json!(42), json!(" \t\n")] {
                cases.push((key, value));
            }
        }
        for key in [
            "dimensions",
            "maxFileSize",
            "embeddingBatchSize",
            "parallelism",
        ] {
            for value in [json!(0), json!(-1), json!(1.5), json!("4"), Value::Null] {
                cases.push((key, value));
            }
        }
        for key in ["rerankingEnabled", "verbose"] {
            for value in [json!("false"), json!(0), Value::Null] {
                cases.push((key, value));
            }
        }
        for key in ["include", "exclude"] {
            for value in [json!("src/**"), json!(["src/**", 1]), json!(["["])] {
                cases.push((key, value));
            }
        }
        cases.extend([
            ("rerankerProvider", json!("unknown")),
            ("rerankerModel", json!("  ")),
            ("rerankerCandidates", json!(0)),
            ("rerankerCandidates", json!(101)),
            ("rerankerCandidates", json!("10")),
        ]);
        for (key, value) in cases {
            let mut config = json!({"rerankingEnabled": true, "rerankerProvider": "openai",
                "custom": {"keep": [1, 2]}});
            config[key] = value;
            let original = serde_json::to_vec(&config).unwrap();
            fs::write(&path, &original).unwrap();
            let mut out = Vec::new();
            let error = run_config(&cli.global, &path, action.as_ref(), &mut out).unwrap_err();
            assert!(format!("{error:#}").contains(key), "{key}: {error:#}");
            assert!(out.is_empty(), "reported success for invalid {key}");
            assert_eq!(fs::read(&path).unwrap(), original, "modified invalid {key}");
        }
        let absent = temp.path().join("new/config.json");
        assert!(write_config(&absent, &json!({"dimensions": 0})).is_err());
        assert!(!absent.parent().unwrap().exists());
    }

    #[test]
    fn failed_config_replacement_preserves_destination_and_allows_retry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        // A nonempty directory forces failure after the temporary file is written.
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), b"original").unwrap();
        let config = json!({"parallelism": 3});
        let error = write_config(&path, &config).unwrap_err();
        assert!(error.to_string().contains("save config"));
        assert_eq!(fs::read(path.join("keep")).unwrap(), b"original");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
        fs::remove_file(path.join("keep")).unwrap();
        fs::remove_dir(&path).unwrap();
        write_config(&path, &config).unwrap();
        assert_eq!(read_config(&path).unwrap(), config);
        assert!(fs::read(&path).unwrap().ends_with(b"\n"));
    }

    fn config_action_json(path: &Path, args: &[&str]) -> Value {
        let cli = parse(&[args, &["--format", "json"]].concat());
        let Command::Config { action } = cli.command else {
            panic!()
        };
        let mut out = Vec::new();
        run_config(&cli.global, path, action.as_ref(), &mut out).unwrap();
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn unrelated_config_edits_durably_normalize_aliases_without_saving_global_overrides() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        fs::write(
            &path,
            json!({"provider": null, "embeddingProvider": "jina",
            "model": "canonical", "embeddingModel": "obsolete", "dimensions": null,
            "embeddingDimensions": 16, "descriptionFallbackModel": "backup", "fallbackModel": null,
            "custom": {"nested": [null, "λ"]}})
            .to_string(),
        )
        .unwrap();
        let result = config_action_json(
            &path,
            &[
                "config",
                "parallelism",
                "4",
                "--provider",
                "openai",
                "--model",
                "transient",
                "--dimensions",
                "8",
                "--no-reindex",
            ],
        );
        assert_eq!(result, json!({"configPath": path, "parallelism": 4}));
        let expected = json!({"provider": "jina", "model": "canonical", "dimensions": 16,
            "descriptionFallbackModel": "backup", "parallelism": 4,
            "custom": {"nested": [null, "λ"]}});
        // Inspect raw JSON: read_config would hide aliases accidentally left on disk.
        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved, expected);
        config_action_json(&path, &["config", "parallelism", "4"]);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
            expected
        );
        let effective = effective_config(&parse(&["status"]).global, &path).unwrap();
        assert_eq!(effective["provider"], "jina");
        assert_eq!(effective["dimensions"], 16);
        assert_eq!(effective["noReindex"], false);
    }

    #[test]
    fn reranker_config_transitions_retain_same_provider_settings_and_reset_on_switch() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        write_config(
            &path,
            &json!({"rerankingEnabled": true, "rerankerProvider": "openai",
            "rerankerModel": "custom-model", "rerankerCandidates": 37, "custom": "keep"}),
        )
        .unwrap();
        assert_eq!(
            config_action_json(&path, &["config", "reranker", "disable"]),
            json!({"configPath": path, "rerankingEnabled": false})
        );
        let disabled = read_config(&path).unwrap();
        assert_eq!(disabled["rerankerModel"], "custom-model");
        assert_eq!(disabled["rerankerCandidates"], 37);
        let enabled = config_action_json(&path, &["config", "reranker", "openai"]);
        assert_eq!(enabled["rerankingEnabled"], true);
        assert_eq!(enabled["rerankerModel"], "custom-model");
        assert_eq!(enabled["rerankerCandidates"], 37);
        config_action_json(&path, &["config", "reranker", "jina"]);
        let switched = read_config(&path).unwrap();
        assert_eq!(switched["rerankerModel"], "jina-reranker-v3.5");
        assert!(switched.get("rerankerCandidates").is_none());
        config_action_json(&path, &["config", "reranker", "openai", "replacement"]);
        let restored = read_config(&path).unwrap();
        assert_eq!(restored["rerankerModel"], "replacement");
        assert_eq!(restored["rerankerCandidates"], 10);
        assert_eq!(restored["custom"], "keep");
    }

    #[test]
    fn selected_root_does_not_rebase_explicit_config_or_saved_index_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo with spaces");
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            config_path(&root, None).unwrap(),
            root.join(".slopdex/config.json")
        );
        assert_eq!(
            config_path(&root, Some(Path::new("settings/custom.json"))).unwrap(),
            cwd.join("settings/custom.json")
        );
        let config_file = temp.path().join("elsewhere/config.json");
        write_config(&config_file, &json!({"indexPath": "indexes/saved.sqlite"})).unwrap();
        let config = read_config(&config_file).unwrap();
        assert_eq!(
            index_path(&root, None, &config).unwrap(),
            cwd.join("indexes/saved.sqlite")
        );
        let explicit = temp.path().join("override.sqlite");
        assert_eq!(
            index_path(&root, Some(&explicit), &config).unwrap(),
            explicit
        );
        assert_eq!(config_path(&root, Some(&config_file)).unwrap(), config_file);
        let cli = parse(&["status", "--index", "relative/override.sqlite"]);
        let effective = effective_config(&cli.global, &config_file).unwrap();
        assert_eq!(
            effective["indexPath"],
            json!(cwd.join("relative/override.sqlite"))
        );
        assert_eq!(read_config(&config_file).unwrap(), config);
    }

    #[cfg(unix)]
    #[test]
    fn path_identity_handles_hard_links_symlink_parents_and_resolution_failures() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("index.sqlite");
        let hard_link = temp.path().join("hard.sqlite");
        let different = temp.path().join("different.sqlite");
        fs::write(&original, b"same bytes").unwrap();
        fs::hard_link(&original, &hard_link).unwrap();
        fs::write(&different, b"same bytes").unwrap();
        assert!(same_path(&original, &hard_link).unwrap());
        assert!(!same_path(&original, &different).unwrap());
        let nested = temp.path().join("actual/nested");
        fs::create_dir_all(&nested).unwrap();
        let alias = temp.path().join("alias");
        symlink(&nested, &alias).unwrap();
        let via_parent = alias.join("../missing/index.sqlite");
        assert!(
            same_path(
                &via_parent,
                &temp.path().join("actual/missing/index.sqlite")
            )
            .unwrap()
        );
        assert!(!same_path(&via_parent, &temp.path().join("missing/index.sqlite")).unwrap());
        assert!(same_path(&original.join("child"), &different).is_err());
        let cycle = temp.path().join("cycle");
        symlink("cycle", &cycle).unwrap();
        assert!(same_path(&cycle, &original).is_err());
    }

    #[test]
    fn argument_errors_cover_global_constraints_and_conflicting_model_selections() {
        for args in [
            vec!["status", "--rebuild-on-divergence"],
            vec![
                "status",
                "--no-reindex",
                "--force-reindex",
                "--yes-really-rebuild-the-index",
            ],
            vec!["refresh", "--target", "main"],
            vec!["search", " \n\t"],
            vec!["status", "--model", " "],
            vec!["status", "--dimensions", "18446744073709551616"],
            vec!["config", "parallelism", "0"],
            vec![
                "config",
                "reranker",
                "openai",
                "--reranker-candidates",
                "101",
            ],
            vec!["search-code", "query", "--regexp", "(?=lookahead)"],
            vec!["search-code", "query", "--regexp", r"(a)\1"],
        ] {
            let error = Cli::try_parse_from(std::iter::once("slopdex").chain(args.iter().copied()))
                .unwrap_err();
            assert_eq!(error.exit_code(), 2, "{args:?}");
        }
        for (args, message) in [
            (
                vec![
                    "models",
                    "opencode",
                    "--description-provider",
                    "opencode-go",
                ],
                "must match",
            ),
            (
                vec!["models", "--description-provider", "openai"],
                "opencode or opencode-go",
            ),
            (
                vec!["config", "model", "first", "--description-model", "second"],
                "must match",
            ),
            (
                vec![
                    "config",
                    "fallback-model",
                    "first",
                    "--description-fallback-model",
                    "second",
                ],
                "must match",
            ),
            (vec!["config", "model", " \t"], "must not be empty"),
            (
                vec!["config", "reranker", "jina", "--reranker-candidates", "10"],
                "requires config reranker openai",
            ),
        ] {
            let cli = Cli::try_parse_from(std::iter::once("slopdex").chain(args.iter().copied()))
                .unwrap();
            assert!(
                cli.validate().unwrap_err().to_string().contains(message),
                "{args:?}"
            );
        }
        parse(&["models", "opencode", "--description-provider", "opencode"]);
        parse(&["config", "model", "same", "--description-model", "same"]);
        parse(&[
            "config",
            "fallback-model",
            "same",
            "--description-fallback-model",
            "same",
        ]);
        parse(&[
            "refresh",
            "--target",
            "HEAD",
            "--rebuild-on-divergence",
            "--yes-really-rebuild-the-index",
        ]);
        let cli = parse(&["search-description", "--", "--literal query"]);
        let Command::SearchDescriptions(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.query, "--literal query");
    }

    #[test]
    fn search_outputs_preserve_json_metadata_and_explain_summary_scores_and_content() {
        let rows = vec![
            json!({"type": "code", "function": {"path": "src/λ.rs", "name": "fallback",
                "qualifiedName": "Service.run", "description": "Purpose\nsecond line"},
                "similarity": 0.6, "rerankScore": 0.95, "codeSimilarity": 0.3,
                "descriptionSimilarity": 0.6, "fileDescriptionSimilarity": 0.9,
                "custom": {"escaped": "\"quoted\"\n"}}),
            json!({"type": "markdown", "similarity": 0.7, "chunk": {"path": "guide.md",
                "startLine": 12, "headingPath": ["Setup", "Credentials"], "content": "Use `KEY`.\nNext step."}}),
            json!({"type": "description", "similarity": 0.4, "descriptionSimilarity": 0.2,
                "fileDescriptionSimilarity": 0.6, "function": {"path": "other.rs", "name": "fallback"}}),
        ];
        let mut out = Vec::new();
        print_search(
            &mut out,
            &rows,
            Format::Json,
            Detail::Compact,
            false,
            &mut Presentation::empty(),
        )
        .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&out).unwrap(), json!(rows));
        assert!(out.ends_with(b"\n"));
        out.clear();
        print_search(
            &mut out,
            &rows,
            Format::Summary,
            Detail::Compact,
            false,
            &mut Presentation::empty(),
        )
        .unwrap();
        let summary = String::from_utf8(out).unwrap();
        assert_eq!(
            summary,
            concat!(
                "*** src/λ.rs\n@@ 1 @@\nService.run  // score=0.95 similarity=0.60\n",
                "\n*** guide.md\n@@ 12 @@\n# Setup\n  ## Credentials  <!-- score=0.70 -->\n",
                "\n*** other.rs\n@@ 1 @@\nfallback  // score=0.40\n"
            )
        );
        let mut out = Vec::new();
        print_search(
            &mut out,
            &rows,
            Format::Summary,
            Detail::Expanded,
            false,
            &mut Presentation::empty(),
        )
        .unwrap();
        let expanded = String::from_utf8(out).unwrap();
        assert!(
            expanded.contains("Service.run  // score=0.95"),
            "{expanded}"
        );
        assert!(expanded.contains(" | Purpose second line\n"), "{expanded}");
        assert!(expanded.contains("Use `KEY`.\nNext step."));
        assert!(summary.len() < expanded.len());
    }

    #[test]
    fn displayed_scores_round_to_two_places() {
        let row = json!({
            "similarity": 0.4639,
            "rerankScore": 0.5321,
            "codeSimilarity": 0.416,
            "descriptionSimilarity": 0.3925,
            "fileDescriptionSimilarity": 0.971
        });
        assert_eq!(rank(&row), "score=0.53 similarity=0.46");
        assert_eq!(
            score_details(&row),
            "  [combined thirds; code 0.42, description 0.39, file 0.97]"
        );
        assert_eq!(rank(&json!({"similarity": 0.996})), "score=1.00");
    }

    #[test]
    fn search_uses_indexed_declaration_instead_of_callable_source() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("api.rs"),
            "pub struct Api;\nimpl Api {\n  pub fn run(&self) { secret(); }\n}\n",
        )?;
        let index = dir.path().join("index.sqlite");
        let mut engine = Engine::open_map(dir.path(), &index, json!({}))?;
        engine.refresh_structure()?;
        let rows = vec![json!({"type":"function", "similarity":0.9, "function":{
            "path":"api.rs", "qualifiedName":"Api.run", "name":"run", "startLine":3,
            "endLine":3, "source":"pub fn run(&self) { secret(); }"}})];
        let mut output = Vec::new();
        print_search(
            &mut output,
            &rows,
            Format::Summary,
            Detail::Compact,
            false,
            &mut Presentation::new(&engine),
        )?;
        let output = String::from_utf8(output)?;
        assert_eq!(
            output,
            "*** api.rs\n\n@@ 2-4 @@\nimpl Api\n  pub fn run(&self)  // score=0.90\n"
        );
        assert!(!output.contains("secret") && !output.contains('{'));
        Ok(())
    }

    #[test]
    fn markdown_search_uses_last_heading_of_chunk() {
        let row = json!({"type":"markdown", "similarity":0.8, "chunk":{
            "path":"guide.md", "startLine":8, "endLine":14,
            "headingPath":["Guide", "Setup"], "content":"# Guide\n\n## Setup\n\nDetails."}});
        let mut output = Vec::new();
        print_markdown(
            &mut output,
            &row,
            Detail::Compact,
            false,
            &mut Presentation::empty(),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "*** guide.md\n@@ 8-14 @@\n# Guide\n  ## Setup  <!-- score=0.80 -->\n"
        );
    }

    #[test]
    fn standard_search_detail_previews_markdown_and_explicit_descriptions() -> Result<()> {
        let description = json!({"type":"function", "similarity":0.8,
            "function":{"path":"api.rs", "name":"run", "description":"Handles requests.\nChecks permissions."}});
        let mut output = Vec::new();
        print_search(
            &mut output,
            std::slice::from_ref(&description),
            Format::Summary,
            Detail::Standard,
            true,
            &mut Presentation::empty(),
        )?;
        assert!(
            String::from_utf8(output)?
                .contains("run  // score=0.80 | Handles requests. Checks permissions.\n")
        );
        let mut output = Vec::new();
        print_search(
            &mut output,
            &[description],
            Format::Summary,
            Detail::Standard,
            false,
            &mut Presentation::empty(),
        )?;
        assert!(!String::from_utf8(output)?.contains("// Handles requests."));

        let body = format!("{} extra material", "λ".repeat(170));
        let row = json!({"type":"markdown", "similarity":0.8, "chunk":{
            "path":"guide.md", "startLine":1, "endLine":8, "headingPath":["Guide"],
            "content":format!("# Guide\n\n{body}")}});
        let mut output = Vec::new();
        print_markdown(
            &mut output,
            &row,
            Detail::Standard,
            false,
            &mut Presentation::empty(),
        )?;
        let output = String::from_utf8(output)?;
        let preview = output
            .lines()
            .find_map(|line| line.strip_prefix("@ preview: "))
            .unwrap();
        assert_eq!(preview.chars().count(), 160);
        assert!(preview.ends_with('…') && !preview.contains("extra material"));
        let mut expanded = Vec::new();
        print_markdown(
            &mut expanded,
            &row,
            Detail::Expanded,
            false,
            &mut Presentation::empty(),
        )?;
        assert!(String::from_utf8(expanded)?.contains(&body));
        Ok(())
    }

    #[test]
    fn indexed_file_and_callable_descriptions_are_comments_at_the_requested_detail() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = "pub fn run() {}\n";
        fs::write(dir.path().join("api.rs"), source)?;
        let index = dir.path().join("index.sqlite");
        let mut engine = Engine::open_map(dir.path(), &index, json!({}))?;
        engine.refresh_structure()?;
        drop(engine);
        let db = rusqlite::Connection::open(&index)?;
        let (identity, data): (String, String) = db.query_row(
            "SELECT identity,data FROM search_units WHERE path='api.rs' AND kind='function'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let data: Value = serde_json::from_str(&data)?;
        db.execute("INSERT INTO descriptions(scope,path,identity,source_hash,text,embedding_key) VALUES('file','api.rs','',?,?,NULL)",
            rusqlite::params![crate::hash(source), "Handles API requests.\nIncludes validation."])?;
        db.execute("INSERT INTO descriptions(scope,path,identity,source_hash,text,embedding_key) VALUES('callable','api.rs',?,?,?,NULL)",
            rusqlite::params![identity, data["sourceHash"].as_str(), "Runs work.\nReturns a result."])?;
        drop(db);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let engine = loop {
            match Engine::open_map(dir.path(), &index, json!({})) {
                Err(error)
                    if error.to_string().starts_with("Index is in use")
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                result => break result?,
            }
        };
        let rows = engine.map(&json!({}))?;
        let mut compact = Vec::new();
        print_map(
            &mut compact,
            &rows,
            Format::Summary,
            Detail::Compact,
            Some(&engine),
        )?;
        assert!(!String::from_utf8(compact)?.contains("Handles API requests."));
        let mut expanded = Vec::new();
        print_map(
            &mut expanded,
            &rows,
            Format::Summary,
            Detail::Expanded,
            Some(&engine),
        )?;
        let expanded = String::from_utf8(expanded)?;
        assert!(
            expanded
                .starts_with("*** api.rs\n// Handles API requests.\n// Includes validation.\n\n@@"),
            "{expanded}"
        );
        assert!(
            expanded.contains("pub fn run()  // Runs work. Returns a result.\n"),
            "{expanded}"
        );

        let result = json!({"type":"function", "similarity":0.8, "function":{
            "path":"api.rs", "name":"run", "qualifiedName":"run", "startLine":1,"endLine":1,
            "description":"Runs work.\nReturns a result."}});
        let mut output = Vec::new();
        print_search(
            &mut output,
            std::slice::from_ref(&result),
            Format::Summary,
            Detail::Compact,
            true,
            &mut Presentation::new(&engine),
        )?;
        let output = String::from_utf8(output)?;
        assert!(
            output
                .starts_with("*** api.rs\n// Handles API requests.\n// Includes validation.\n\n@@"),
            "{output}"
        );
        assert!(
            output.contains("pub fn run()  // score=0.80 | Runs work. Returns a result.\n"),
            "{output}"
        );
        let mut output = Vec::new();
        print_search(
            &mut output,
            &[result],
            Format::Summary,
            Detail::Standard,
            false,
            &mut Presentation::new(&engine),
        )?;
        let output = String::from_utf8(output)?;
        assert!(
            !output.contains("Handles API requests.") && !output.contains("Runs work."),
            "{output}"
        );
        Ok(())
    }

    #[test]
    fn ranked_hits_group_files_and_emit_ancestors_once_in_source_order() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("api.rs"),
            "impl Api {\n  fn write(&self) { first(); }\n  fn flush(&self) { second(); }\n}\n",
        )?;
        fs::write(
            dir.path().join("guide.md"),
            "# Guide\n\n## Setup\nFirst.\n\n### Details\nSecond.\n",
        )?;
        let index = dir.path().join("index.sqlite");
        let mut engine = Engine::open_map(dir.path(), &index, json!({}))?;
        engine.refresh_structure()?;
        let rows = vec![
            json!({"type":"function", "similarity":0.9, "function":{"path":"api.rs", "qualifiedName":"Api.flush", "name":"flush", "startLine":3, "endLine":3}}),
            json!({"type":"markdown", "similarity":0.8, "chunk":{"path":"guide.md", "startLine":6, "endLine":7, "headingPath":["Guide", "Setup", "Details"], "content":"# Guide\n## Setup\n### Details\n\nSecond."}}),
            json!({"type":"function", "similarity":0.7, "function":{"path":"api.rs", "qualifiedName":"Api.write", "name":"write", "startLine":2, "endLine":2}}),
            json!({"type":"markdown", "similarity":0.6, "chunk":{"path":"guide.md", "startLine":3, "endLine":4, "headingPath":["Guide", "Setup"], "content":"# Guide\n## Setup\n\nFirst."}}),
        ];
        let mut output = Vec::new();
        print_search(
            &mut output,
            &rows,
            Format::Summary,
            Detail::Compact,
            false,
            &mut Presentation::new(&engine),
        )?;
        let output = String::from_utf8(output)?;
        assert_eq!(output.matches("*** api.rs").count(), 1);
        assert_eq!(output.matches("*** guide.md").count(), 1);
        assert!(output.find("*** api.rs").unwrap() < output.find("*** guide.md").unwrap());
        assert!(output.find("fn write(&self)").unwrap() < output.find("fn flush(&self)").unwrap());
        assert!(output.find("## Setup").unwrap() < output.find("### Details").unwrap());
        assert!(output.contains("## Setup  <!-- score=0.60 -->"), "{output}");
        assert!(
            output.contains("### Details  <!-- score=0.80 -->"),
            "{output}"
        );
        assert!(
            output.contains("@@ 1-4 @@\nimpl Api\n  fn write(&self)  // score=0.70\n  fn flush(&self)  // score=0.90\n"),
            "{output}"
        );
        assert_eq!(output.matches("impl Api").count(), 1);
        assert_eq!(output.matches("# Guide").count(), 1);
        assert!(!output.contains("first()") && !output.contains("second()"));
        Ok(())
    }

    #[test]
    fn search_orders_files_by_best_rerank_score_and_keeps_individual_scores() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("a.rs"),
            "fn first() {}\n\n\n\n\nfn second() {}\n",
        )?;
        fs::write(dir.path().join("b.rs"), "fn other() {}\n")?;
        let index = dir.path().join("index.sqlite");
        let mut engine = Engine::open_map(dir.path(), &index, json!({}))?;
        engine.refresh_structure()?;
        let rows = vec![
            json!({"type":"function", "similarity":0.8, "rerankScore":0.9, "function":{"path":"a.rs", "name":"second", "qualifiedName":"second", "startLine":6, "endLine":6}}),
            json!({"type":"function", "similarity":0.99, "rerankScore":0.7, "function":{"path":"b.rs", "name":"other", "qualifiedName":"other", "startLine":1, "endLine":1}}),
            json!({"type":"function", "similarity":0.4, "rerankScore":0.5, "function":{"path":"a.rs", "name":"first", "qualifiedName":"first", "startLine":1, "endLine":1}}),
        ];
        let mut output = Vec::new();
        print_search(
            &mut output,
            &rows,
            Format::Summary,
            Detail::Compact,
            false,
            &mut Presentation::new(&engine),
        )?;
        let output = String::from_utf8(output)?;
        assert!(
            output.find("*** a.rs").unwrap() < output.find("*** b.rs").unwrap(),
            "{output}"
        );
        assert!(
            output.find("fn first()").unwrap() < output.find("fn second()").unwrap(),
            "{output}"
        );
        assert!(
            output.contains("@@ 1-6 @@\nfn first()  // score=0.50 similarity=0.40\nfn second()  // score=0.90 similarity=0.80\n"),
            "{output}"
        );
        assert_eq!(output.matches("*** a.rs").count(), 1);
        assert!(output.contains("score=0.50 similarity=0.40"), "{output}");
        assert!(output.contains("score=0.90 similarity=0.80"), "{output}");
        Ok(())
    }

    #[test]
    fn cross_summary_groups_sources_and_matches_from_the_same_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("api.rs"),
            "impl Api {\n  fn first(&self) {}\n  fn second(&self) {}\n}\n",
        )?;
        let index = dir.path().join("index.sqlite");
        let mut engine = Engine::open_map(dir.path(), &index, json!({}))?;
        engine.refresh_structure()?;
        let first = json!({"path":"api.rs", "qualifiedName":"Api.first", "name":"first", "startLine":2,"endLine":2});
        let second = json!({"path":"api.rs", "qualifiedName":"Api.second", "name":"second", "startLine":3,"endLine":3});
        let rows =
            vec![json!({"source":second, "matches":[{"function":first, "similarity":0.91}]})];
        let mut output = Vec::new();
        print_cross(
            &mut output,
            rows,
            CrossOutput::new(Format::Summary, true, false, None, Detail::Compact),
            &mut Presentation::new(&engine),
            None,
        )?;
        let output = String::from_utf8(output)?;
        assert_eq!(output.matches("*** api.rs").count(), 1, "{output}");
        assert_eq!(output.matches("impl Api").count(), 1, "{output}");
        assert!(
            output.find("fn first(&self)").unwrap() < output.find("fn second(&self)").unwrap(),
            "{output}"
        );
        assert!(
            output.contains("fn first(&self)  // score=0.91"),
            "{output}"
        );
        assert!(output.contains("fn second(&self)  // source"), "{output}");
        Ok(())
    }

    #[test]
    fn empty_results_obey_array_jsonl_and_plain_output_contracts() {
        let mut out = Vec::new();
        print_search(
            &mut out,
            &[],
            Format::Json,
            Detail::Compact,
            false,
            &mut Presentation::empty(),
        )
        .unwrap();
        assert_eq!(out, b"[]\n");
        out.clear();
        print_errors(&mut out, &[], Format::Json).unwrap();
        assert_eq!(out, b"[]\n");
        out.clear();
        print_search(
            &mut out,
            &[],
            Format::Summary,
            Detail::Compact,
            false,
            &mut Presentation::empty(),
        )
        .unwrap();
        assert_eq!(out, b"No matches.\n");
        out.clear();
        print_errors(&mut out, &[], Format::Summary).unwrap();
        assert_eq!(out, b"No indexing errors.\n");
        for (format, expected) in [
            (Format::Json, ""),
            (Format::Summary, "No matches.\n"),
            (Format::Clusters, "No clusters.\n"),
        ] {
            out.clear();
            print_cross(
                &mut out,
                vec![json!({"source": function("a"), "matches": []})],
                CrossOutput::new(format, true, false, None, Detail::Compact),
                &mut Presentation::empty(),
                None,
            )
            .unwrap();
            assert_eq!(out, expected.as_bytes());
        }
    }

    #[test]
    fn indexing_error_output_retains_diagnostics_and_uses_available_locations() {
        let errors = vec![
            json!({"path": "src/a.rs", "filePath": "old.rs", "startLine": 7, "startColumn": 3,
                "qualifiedName": "Service.run", "name": "run", "message": "unexpected token",
                "sourceMode": "working-tree", "extra": ["kept"]}),
            json!({"filePath": "src/b.rs", "startLine": 2, "name": "fallback", "message": "bad UTF-8"}),
            json!({"message": "read failed\ncaused by: missing file"}),
        ];
        let mut out = Vec::new();
        print_errors(&mut out, &errors, Format::Json).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&out).unwrap(),
            json!(errors)
        );
        out.clear();
        print_errors(&mut out, &errors, Format::Summary).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                "src/a.rs:7:3 :: Service.run  unexpected token\n",
                "src/b.rs:2 :: fallback  bad UTF-8\n",
                "(unknown file)  read failed\ncaused by: missing file\n"
            )
        );
    }

    #[test]
    fn cross_jsonl_limits_matched_sources_without_truncating_edges_or_metadata() {
        let mut first = edge("a", "b", 0.8);
        first["source"]["description"] = json!("line one\n\"line two\" λ");
        first["scoring"] = json!({"mode": "combined", "weights": [1, 1, 1]});
        first["matches"]
            .as_array_mut()
            .unwrap()
            .push(json!({"function": function("c"),
            "similarity": 0.9, "descriptionSimilarity": 0.8, "fileDescriptionSimilarity": 1.0}));
        let rows = vec![
            json!({"source": function("empty"), "matches": []}),
            first.clone(),
            edge("x", "y", 0.7),
        ];
        let mut out = Vec::new();
        print_cross(
            &mut out,
            rows.clone(),
            CrossOutput::new(Format::Json, false, false, Some(1), Detail::Compact),
            &mut Presentation::empty(),
            None,
        )
        .unwrap();
        let output = String::from_utf8(out).unwrap();
        assert_eq!(output.lines().count(), 1);
        assert!(output.ends_with('\n'));
        assert_eq!(serde_json::from_str::<Value>(&output).unwrap(), first);
        let mut out = Vec::new();
        print_cross(
            &mut out,
            rows,
            CrossOutput::new(Format::Summary, false, false, Some(1), Detail::Compact),
            &mut Presentation::empty(),
            None,
        )
        .unwrap();
        let output = String::from_utf8(out).unwrap();
        assert!(
            output.starts_with("*** src/a.rs\n@@ 1 @@\na  // source\n"),
            "{output}"
        );
        assert!(
            output.contains("*** src/a.rs\n@@ 1 @@\na  // source\n"),
            "{output}"
        );
        assert!(output.contains("*** src/b.rs\n@@ 1 @@\nb  // target score=0.80"));
        assert!(output.contains("*** src/c.rs\n@@ 1 @@\nc  // target score=0.90"));
        assert!(!output.contains("src/x.rs"));
    }

    #[test]
    fn cross_repository_clusters_do_not_merge_swapped_or_identical_node_ids() {
        let rows = vec![edge("a", "b", 0.8), edge("b", "a", 0.9)];
        let grouped = clusters(&rows, false);
        assert_eq!(grouped.len(), 2);
        assert_eq!(
            grouped[0]
                .members
                .iter()
                .map(ClusterMember::label)
                .collect::<Vec<_>>(),
            ["[source] src/a.rs:1:1 :: a", "[target] src/b.rs:1:1 :: b"]
        );
        assert_eq!(
            grouped[1]
                .members
                .iter()
                .map(ClusterMember::label)
                .collect::<Vec<_>>(),
            ["[source] src/b.rs:1:1 :: b", "[target] src/a.rs:1:1 :: a"]
        );
        assert_eq!((grouped[0].min, grouped[0].max), (0.8, 0.8));
        assert_eq!((grouped[1].min, grouped[1].max), (0.9, 0.9));
        assert_eq!(clusters(&rows, true).len(), 1);
        let mut same_id = edge("a", "a", 0.7);
        same_id["source"]["id"] = json!(0);
        same_id["matches"][0]["function"]["id"] = json!(0);
        let mut out = Vec::new();
        print_cross(
            &mut out,
            vec![same_id],
            CrossOutput::new(Format::Clusters, false, false, None, Detail::Compact),
            &mut Presentation::empty(),
            None,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                "Cluster 1 · 2 functions · similarity 0.70\n",
                "\n*** src/a.rs\n@@ 1 @@\na  // source\n",
                "\n*** src/a.rs\n@@ 1 @@\na  // target\n"
            )
        );
    }

    #[test]
    fn clusters_distinguish_missing_id_locations_and_ignore_self_edges() {
        let a = json!({"path": "same.rs", "name": "overload", "startLine": 4, "startColumn": 1});
        let b = json!({"id": null, "path": "same.rs", "name": "overload", "startLine": 4, "startColumn": 9});
        let c = json!({"path": "same.rs", "name": "overload", "startLine": 8, "startColumn": 1});
        let rows = vec![
            json!({"source": a, "matches": [
                {"function": a, "similarity": 1.0}, {"function": b, "similarity": 0.6}]}),
            json!({"source": b, "matches": [
                {"function": a, "similarity": 0.6}, {"function": c, "similarity": 0.8, "descriptionSimilarity": 0.7}]}),
            edge("orphan", "orphan", 1.0),
        ];
        let grouped = clusters(&rows, true);
        assert_eq!(grouped.len(), 1);
        assert_eq!(
            grouped[0]
                .members
                .iter()
                .map(ClusterMember::label)
                .collect::<Vec<_>>(),
            [
                "same.rs:4:1 :: overload",
                "same.rs:4:9 :: overload",
                "same.rs:8:1 :: overload"
            ]
        );
        assert_eq!((grouped[0].min, grouped[0].max), (0.6, 0.8));
        let mut expected = Vec::new();
        print_cross(
            &mut expected,
            rows.clone(),
            CrossOutput::new(Format::Clusters, true, false, None, Detail::Expanded),
            &mut Presentation::empty(),
            None,
        )
        .unwrap();
        assert!(
            String::from_utf8_lossy(&expected)
                .contains("combined code + callable description + file description")
        );
        let mut reversed = rows;
        reversed.reverse();
        let mut actual = Vec::new();
        print_cross(
            &mut actual,
            reversed,
            CrossOutput::new(Format::Clusters, true, false, None, Detail::Expanded),
            &mut Presentation::empty(),
            None,
        )
        .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn cluster_members_in_one_file_follow_numeric_source_lines() {
        let first =
            json!({"id": 1, "path":"same.rs", "name":"first", "startLine":2, "startColumn":1});
        let second =
            json!({"id": 2, "path":"same.rs", "name":"second", "startLine":10, "startColumn":1});
        let rows =
            vec![json!({"source": second, "matches": [{"function": first, "similarity":0.9}]})];
        let cluster = clusters(&rows, true).remove(0);
        assert_eq!(cluster.members[0].function["startLine"], 2);
        assert_eq!(cluster.members[1].function["startLine"], 10);
        let mut output = Vec::new();
        print_cross(
            &mut output,
            rows,
            CrossOutput::new(Format::Clusters, true, false, None, Detail::Compact),
            &mut Presentation::empty(),
            None,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("*** same.rs").count(), 1, "{output}");
        assert!(
            output.contains("@@ 2 @@\nfirst\n@@ 10 @@\nsecond\n"),
            "{output}"
        );
    }

    #[test]
    fn output_failures_are_returned_for_json_summary_and_cluster_writers() {
        struct BrokenPipe;
        impl Write for BrokenPipe {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let rows = vec![edge("a", "b", 0.9)];
        for format in [Format::Json, Format::Summary] {
            assert!(
                print_search(
                    &mut BrokenPipe,
                    &[json!({"function": function("a")})],
                    format,
                    Detail::Compact,
                    false,
                    &mut Presentation::empty()
                )
                .is_err()
            );
            assert!(
                print_errors(&mut BrokenPipe, &[json!({"message": "failure"})], format).is_err()
            );
            assert!(
                print_cross(
                    &mut BrokenPipe,
                    rows.clone(),
                    CrossOutput::new(format, true, false, None, Detail::Compact),
                    &mut Presentation::empty(),
                    None
                )
                .is_err()
            );
        }
        assert!(
            print_cross(
                &mut BrokenPipe,
                rows,
                CrossOutput::new(Format::Clusters, true, false, None, Detail::Compact),
                &mut Presentation::empty(),
                None
            )
            .is_err()
        );
    }
}
