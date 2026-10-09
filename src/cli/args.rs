//! Command-line declarations, parsing, and argument validation.

use crate::{filter, map, ui};
use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(
    name = "slopdex",
    version,
    disable_help_subcommand = true,
    about = "Semantic code, documentation, and configuration search",
    after_help = "Examples:\n  slopdex update\n  slopdex generate descriptions\n  slopdex search \"validate an authenticated session\"\n  slopdex search-symbols \"validate session\"\n  slopdex search \"validate session\" --code --symbols\n  slopdex cross-search --cross-file-only --lines 4 --threshold 0.85-0.9\n  slopdex describe \"I want to implement a new rpc endpoint\"\n  slopdex config\n\nSearch and cross-search require an existing index; run update first. Plain search includes all indexes; selectors restrict it to explicit indexes. Descriptions are generated only by generate descriptions. Pure symbol searches refresh only structure. --no-reindex reuses saved snapshots; queries and explicit generation may call providers. Map without -q parses directly when no index exists."
)]
pub(super) struct Cli {
    #[command(flatten)]
    pub(super) global: Global,
    #[command(subcommand)]
    pub(super) command: Command,
}

#[derive(Debug, Args)]
pub(super) struct Global {
    /// Repository root (defaults to the current directory)
    #[arg(long, global = true)]
    pub(super) root: Option<PathBuf>,
    /// Config file (default: <root>/.slopdex/config.json); explicit paths are relative to cwd
    #[arg(long, global = true)]
    pub(super) config: Option<PathBuf>,
    /// Index file (default: XDG cache per workspace); overrides config indexPath
    #[arg(long, global = true)]
    pub(super) index: Option<PathBuf>,
    /// Embedding provider (default: openai)
    #[arg(long, id = "embedding_provider", global = true, value_parser = ["openai", "jina"])]
    pub(super) provider: Option<String>,
    /// Embedding model (default: text-embedding-3-large / jina-embeddings-v4)
    #[arg(long, id = "embedding_model", global = true, value_parser = nonempty)]
    pub(super) model: Option<String>,
    /// Embedding dimensions (default: OpenAI 3072 / Jina 1024)
    #[arg(long, global = true, value_parser = positive)]
    pub(super) dimensions: Option<usize>,
    /// Description provider (default: openai, or the saved profile)
    #[arg(long, global = true, value_parser = ["openai", "opencode", "opencode-go"])]
    pub(super) description_provider: Option<String>,
    #[arg(long, global = true, value_parser = nonempty)]
    pub(super) description_model: Option<String>,
    /// Optional same-provider fallback description model
    #[arg(long, global = true, value_parser = nonempty)]
    pub(super) description_fallback_model: Option<String>,
    /// OpenAI reranker candidate count (1..=100; default: 10)
    #[arg(long, global = true, value_parser = candidates)]
    pub(super) reranker_candidates: Option<usize>,
    /// Text excerpts by default; cross-search uses clusters, cohesion uses source/match groups; cross JSON is JSONL
    #[arg(long, global = true, value_enum)]
    pub(super) format: Option<Format>,
    /// Compact by default; expanded includes descriptions and high-similarity callable code
    #[arg(long, global = true, value_enum, default_value = "compact")]
    pub(super) detail: Detail,
    /// Show full callable code in expanded text output when similarity is strictly above this value
    #[arg(long, global = true, default_value = "0.9", allow_hyphen_values = true, value_parser = similarity)]
    pub(super) expand_code_threshold: f64,
    /// Reuse the saved index without refresh; queries and explicit generation may call providers
    #[arg(long, global = true, conflicts_with = "force_reindex")]
    pub(super) no_reindex: bool,
    /// Reset live index state while preserving reusable caches
    #[arg(long, global = true, requires = "yes_really_rebuild_the_index")]
    pub(super) force_reindex: bool,
    /// Permit refresh after Git history divergence
    #[arg(long, global = true, requires = "yes_really_rebuild_the_index")]
    pub(super) rebuild_on_divergence: bool,
    /// Confirm an explicitly requested index rebuild
    #[arg(long, global = true)]
    pub(super) yes_really_rebuild_the_index: bool,
    /// Suppress saved indexing-error warnings
    #[arg(long, global = true)]
    pub(super) ignore_errors: bool,
    /// Report every external model request on stderr
    #[arg(long, global = true)]
    pub(super) verbose: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(super) enum Format {
    #[value(name = "text", alias = "summary")]
    Summary,
    Json,
    Clusters,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(super) enum Detail {
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
pub(super) enum Command {
    /// Search all indexes: code, descriptions, Markdown/documents, and symbols
    Search(SearchArgs),
    /// Search only the callable code index
    SearchCode(QueryArgs),
    /// Search only the generated callable/file description index
    SearchDescriptions(QueryArgs),
    /// Search only the heading-aware Markdown content index
    SearchMd(QueryArgs),
    /// Search only the symbol-name/alias and heading-title index
    SearchSymbols(QueryArgs),
    /// Explain existing code relevant to a task using the description model
    Describe(DescribeArgs),
    /// Compare functions; clusters are connected components of observed matches
    CrossSearch(CrossArgs),
    /// Show code declarations and Markdown headings; -q matches symbols, descriptions, and heading titles
    Map(MapArgs),
    /// Refresh and show index metadata, counts, and profiles as JSON
    Status,
    /// Inspect indexed data
    Index {
        #[command(subcommand)]
        action: IndexAction,
    },
    /// Explicitly generate indexed data with configured providers
    Generate {
        #[command(subcommand)]
        action: GenerateAction,
    },
    /// Refresh the current working tree and Git HEAD
    Update(UpdateArgs),
    /// Show command help or fetch published model catalogs
    Help {
        #[command(subcommand)]
        topic: Option<HelpTopic>,
    },
    /// Edit configuration without opening an index; optional prefix filters interactive prompts
    ///
    /// Use `config set <key> <value>` or `config exclude-cross-search <file>:<symbol> ...`.
    Config { args: Vec<String> },
}

#[derive(Debug, Subcommand)]
pub(super) enum IndexAction {
    /// Refresh and inspect saved file/function indexing failures
    Errors,
}

#[derive(Debug, Subcommand)]
pub(super) enum GenerateAction {
    /// Generate missing or stale file and callable descriptions from indexed source
    Descriptions,
}

#[derive(Debug, Subcommand)]
pub(super) enum HelpTopic {
    /// Fetch published OpenCode model catalogs without opening an index
    Models {
        #[arg(value_parser = ["opencode", "opencode-go"])]
        provider: Option<String>,
    },
}

#[derive(Debug, Args)]
pub(super) struct Filters {
    /// Inclusive minimum, or inclusive-min/exclusive-max range; scores must be in [-1,1]
    #[arg(long, default_value = "0.3", allow_hyphen_values = true, value_parser = threshold)]
    pub(super) threshold: Threshold,
    /// Positive output limit; default: unlimited. Cross-search limits clusters or matched sources
    #[arg(long, value_parser = positive)]
    pub(super) limit: Option<usize>,
    #[command(flatten)]
    pub(super) selection: SelectionArgs,
}

impl Filters {
    pub(super) fn options(&self) -> Value {
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
pub(super) struct SelectionArgs {
    /// Repository-relative glob; repeat in order, ! excludes, last match wins
    #[arg(short = 'g', long, value_parser = valid_glob)]
    pub(super) glob: Vec<String>,
    /// Qualified-name or heading-path regex; repeat for OR; cross-search selects sources
    #[arg(short = 'e', long, alias = "regex", value_parser = valid_regex)]
    pub(super) regexp: Vec<String>,
    /// Semantic query over symbols, descriptions, and heading titles; repeat for OR; cross-search selects sources
    #[arg(short = 'q', long, value_parser = valid_symbol_query)]
    pub(super) symbol_query: Vec<String>,
    /// Minimum semantic selector similarity in [-1,1], independent of --threshold (default: 0.5)
    #[arg(long, allow_hyphen_values = true, value_parser = similarity)]
    pub(super) symbol_threshold: Option<f64>,
    /// Match regexes case-insensitively
    #[arg(short = 'i', long)]
    pub(super) ignore_case: bool,
}

impl SelectionArgs {
    pub(super) fn options(&self) -> Value {
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
        if !self.symbol_query.is_empty() {
            value["symbolQuery"] = json!(self.symbol_query);
        }
        if let Some(threshold) = self.symbol_threshold {
            value["symbolThreshold"] = json!(threshold);
        }
        value
    }
}

#[derive(Debug, Args)]
pub(super) struct MapArgs {
    /// Files or recursive directories; external paths use their own repository and index
    pub(super) paths: Vec<PathBuf>,
    #[command(flatten)]
    pub(super) selection: SelectionArgs,
    /// Kinds, comma-separated or repeated (functions includes methods; types groups type declarations)
    #[arg(short = 'k', long = "kind", alias = "kinds", value_delimiter = ',', value_parser = valid_kind)]
    pub(super) kinds: Vec<String>,
    /// Include private and unexported symbols
    #[arg(long)]
    pub(super) private: bool,
    #[command(flatten)]
    pub(super) calls: CallArgs,
}

#[derive(Clone, Copy, Debug, Default, Args)]
pub(super) struct CallArgs {
    /// Include this many levels of functions calling selected callables (expanded default: 1)
    #[arg(long)]
    pub(super) callers: Option<usize>,
    /// Include this many levels of functions called by selected callables (expanded default: 1)
    #[arg(long)]
    pub(super) callees: Option<usize>,
    /// Include this many caller levels with full indexed code in text output
    #[arg(long)]
    pub(super) expand_callers: Option<usize>,
    /// Include this many callee levels with full indexed code in text output
    #[arg(long, visible_alias = "expand-callables")]
    pub(super) expand_callees: Option<usize>,
}

impl CallArgs {
    pub(super) fn resolve(self, detail: Detail) -> CallDepths {
        let default = usize::from(detail == Detail::Expanded);
        let expand_callers = self.expand_callers.unwrap_or(0);
        let expand_callees = self.expand_callees.unwrap_or(0);
        CallDepths {
            callers: self.callers.unwrap_or(default).max(expand_callers),
            callees: self.callees.unwrap_or(default).max(expand_callees),
            expand_callers,
            expand_callees,
        }
    }

    pub(super) fn resolve_describe_context(self) -> CallDepths {
        Self {
            expand_callers: Some(self.expand_callers.unwrap_or(2)),
            expand_callees: Some(self.expand_callees.unwrap_or(2)),
            ..self
        }
        .resolve(Detail::Expanded)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CallDepths {
    pub(super) callers: usize,
    pub(super) callees: usize,
    pub(super) expand_callers: usize,
    pub(super) expand_callees: usize,
}

impl CallDepths {
    pub(super) fn enabled(self) -> bool {
        self.callers > 0 || self.callees > 0
    }
}

impl MapArgs {
    #[cfg(test)]
    pub(super) fn options(&self) -> Value {
        self.options_for(Detail::Compact)
    }

    pub(super) fn options_for(&self, detail: Detail) -> Value {
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
        let calls = self.calls.resolve(detail);
        if calls.enabled() {
            value["callers"] = json!(calls.callers);
            value["callees"] = json!(calls.callees);
        }
        if calls.expand_callers > 0 {
            value["expandCallers"] = json!(calls.expand_callers);
        }
        if calls.expand_callees > 0 {
            value["expandCallees"] = json!(calls.expand_callees);
        }
        value
    }

    pub(super) fn existing_options(
        &self,
        root: &Path,
        paths: &[PathBuf],
        detail: Detail,
    ) -> Result<Option<Value>> {
        if paths.is_empty() {
            return Ok(Some(self.options_for(detail)));
        }
        let mut existing = Vec::new();
        for path in paths {
            let source = if path.is_absolute() {
                path.to_owned()
            } else {
                root.join(path)
            };
            if source
                .try_exists()
                .with_context(|| format!("Cannot inspect map path {}", path.display()))?
            {
                existing.push(path);
            } else {
                ui::warning(format!(
                    "slopdex: warning: map path does not exist; ignoring: {}",
                    path.display()
                ));
            }
        }
        if existing.is_empty() {
            return Ok(None);
        }
        let mut options = self.options_for(detail);
        options["paths"] = json!(existing);
        Ok(Some(options))
    }
}

#[derive(Debug, Args)]
pub(super) struct QueryArgs {
    #[arg(value_parser = nonempty)]
    pub(super) query: String,
    #[command(flatten)]
    pub(super) filters: Filters,
    #[command(flatten)]
    pub(super) calls: CallArgs,
}

#[derive(Debug, Args)]
pub(super) struct SearchArgs {
    #[command(flatten)]
    pub(super) query: QueryArgs,
    /// Select the callable code index; any selector omits unselected indexes
    #[arg(long)]
    pub(super) code: bool,
    /// Select the generated callable/file description index
    #[arg(long)]
    pub(super) descriptions: bool,
    /// Select the Markdown content index
    #[arg(long)]
    pub(super) md: bool,
    /// Select the symbol-name/alias and heading-title index
    #[arg(long)]
    pub(super) symbols: bool,
}

impl SearchArgs {
    pub(super) fn options(&self) -> Value {
        let mut options = self.query.filters.options();
        if self.code || self.descriptions || self.md || self.symbols {
            options["code"] = json!(self.code);
            options["descriptions"] = json!(self.descriptions);
            options["md"] = json!(self.md);
            options["symbols"] = json!(self.symbols);
        }
        options
    }

    pub(super) fn symbols_only(&self) -> bool {
        self.symbols && !self.code && !self.descriptions && !self.md
    }
}

impl Command {
    pub(super) fn search_request(&self) -> Option<(&QueryArgs, &str, Value, bool)> {
        let (args, kind) = match self {
            Self::Search(args) => {
                return Some((&args.query, "search", args.options(), args.descriptions));
            }
            Self::SearchCode(args) => (args, "search-code"),
            Self::SearchDescriptions(args) => (args, "search-descriptions"),
            Self::SearchMd(args) => (args, "search-md"),
            Self::SearchSymbols(args) => (args, "search-symbols"),
            _ => return None,
        };
        Some((
            args,
            kind,
            args.filters.options(),
            kind == "search-descriptions",
        ))
    }
}

#[derive(Debug, Args)]
pub(super) struct DescribeArgs {
    #[command(flatten)]
    pub(super) query: QueryArgs,
}

#[derive(Debug, Args)]
#[command(mut_arg("threshold", |arg| arg.default_value("0.8")))]
pub(super) struct CrossArgs {
    #[command(flatten)]
    pub(super) filters: Filters,
    #[command(flatten)]
    pub(super) calls: CallArgs,
    /// Matches kept per source function
    #[arg(long, default_value = "5", value_parser = positive)]
    pub(super) matches: usize,
    /// Inclusive minimum, or inclusive-min/exclusive-max line count for sources and candidates
    #[arg(
        long,
        visible_alias = "min-lines",
        value_name = "N[-N]",
        default_value = "2",
        value_parser = line_range
    )]
    pub(super) lines: LineRange,
    /// Source file or recursive directory; external paths use their own repository and index
    #[arg(long)]
    pub(super) source_path: Option<PathBuf>,
    /// Use changed functions since this Git ancestor as sources
    #[arg(long, value_parser = nonempty)]
    pub(super) changed_since: Option<String>,
    /// Use staged, unstaged, and untracked working-tree functions as sources
    #[arg(long)]
    pub(super) uncommitted: bool,
    /// Exclude matches from the same physical file
    #[arg(long)]
    pub(super) cross_file_only: bool,
    /// Keep both directions of same-index pairs
    #[arg(long)]
    pub(super) include_symmetric_duplicates: bool,
    /// Order each source's matches by descending filesystem distance; summary by default
    #[arg(long)]
    pub(super) cohesion: bool,
    #[arg(long, requires = "target_index")]
    pub(super) target_root: Option<PathBuf>,
    #[arg(long, requires = "target_root")]
    pub(super) target_index: Option<PathBuf>,
    #[arg(long, requires_all = ["target_root", "target_index"])]
    pub(super) target_config: Option<PathBuf>,
}

impl CrossArgs {
    pub(super) fn options(&self) -> Value {
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
pub(super) struct UpdateArgs {
    /// This update command currently supports HEAD only
    #[arg(long, default_value = "HEAD", value_parser = ["HEAD"])]
    pub(super) target: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Threshold {
    pub(super) min: f64,
    pub(super) max: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LineRange {
    pub(super) min: usize,
    pub(super) max: Option<usize>,
}

fn nonempty(input: &str) -> std::result::Result<String, String> {
    if input.trim().is_empty() {
        Err("must not be empty".into())
    } else {
        Ok(input.to_owned())
    }
}

fn valid_symbol_query(input: &str) -> std::result::Result<String, String> {
    if crate::symbols::normalize(input).is_empty() {
        Err("symbol query must contain letters or numbers".into())
    } else {
        Ok(input.to_owned())
    }
}

pub(super) fn positive(input: &str) -> std::result::Result<usize, String> {
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
    pub(super) fn validate(&self) -> Result<()> {
        if self.global.format == Some(Format::Clusters) {
            ensure!(
                matches!(&self.command, Command::CrossSearch(args) if !args.cohesion),
                "clusters format is only available for cross-search without --cohesion; use summary or json"
            );
        }
        if let Command::Help {
            topic: Some(HelpTopic::Models { provider }),
        } = &self.command
        {
            self.models_provider(provider.as_deref())?;
        }
        Ok(())
    }

    pub(super) fn models_provider<'a>(
        &'a self,
        positional: Option<&'a str>,
    ) -> Result<Option<&'a str>> {
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

#[cfg(test)]
#[path = "args_tests.rs"]
mod tests;
