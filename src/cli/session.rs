//! One workspace's engine lifecycle: setup, opening, refresh, and the single
//! read-only→writable escalation path shared by read commands.

use super::args::{CacheAction, Cli, Command, Format, HelpTopic, IndexAction, help_command};
use super::config::{self, effective_config, mark_repository_config, run_config};
use super::io::print_json;
use super::output::{print_index_entries, print_indexes, print_limit_notice};
use super::workspace::{
    SourceWorkspace, absolute, config_path, index_path, repository_config_selected, require_index,
};
use super::{MapOutput, emit_map};
use crate::engine::Engine;
use crate::providers::Providers;
use crate::{filter, lines, map, ui};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CommandMode {
    Content,
    Structure,
    Symbols,
}

impl CommandMode {
    pub(super) fn for_command(command: &Command) -> Self {
        match command {
            Command::Map(args) if args.selection.symbol_query.is_empty() => Self::Structure,
            Command::Lines(args) if args.selection.symbol_query.is_empty() => Self::Structure,
            Command::Refactor(_) => Self::Structure,
            Command::Map(_) | Command::Lines(_) | Command::SearchSymbols(_) => Self::Symbols,
            Command::Search(args) if args.symbols_only() => Self::Symbols,
            _ => Self::Content,
        }
    }

    pub(super) fn open(
        self,
        root: &Path,
        index: &Path,
        config: &Value,
        readonly: bool,
    ) -> Result<Engine> {
        match (self, readonly) {
            (Self::Symbols, _) => Engine::open_symbol_map(root, index, config.clone(), readonly),
            (Self::Structure, true) => Engine::open_map_readonly(root, index, config.clone()),
            (Self::Structure, false) => Engine::open_map(root, index, config.clone()),
            (Self::Content, true) => Engine::open_readonly(root, index, config.clone()),
            (Self::Content, false) => Engine::open(root, index, config.clone()),
        }
    }

    pub(super) fn refresh(self, engine: &mut Engine) -> Result<Option<Value>> {
        match self {
            Self::Content => ui::spin("Refreshing index", || engine.refresh()).map(Some),
            Self::Structure | Self::Symbols => {
                ui::spin("Refreshing structure", || engine.refresh_structure())?;
                Ok(None)
            }
        }
    }
}

/// An opened workspace index plus the configuration it was opened with.
/// The engine is `None` only while reopening after a writable escalation.
pub(super) struct Session<'a> {
    pub(super) cli: &'a Cli,
    pub(super) workspace: &'a SourceWorkspace,
    pub(super) config: Value,
    pub(super) index: PathBuf,
    mode: CommandMode,
    engine: Option<Engine>,
    /// Map options are resolved before the initial refresh, as before.
    pub(super) map_options: Option<Value>,
    /// The initial refresh report, printed by `update`.
    pub(super) refreshed: Option<Value>,
}

impl<'a> Session<'a> {
    pub(super) fn root(&self) -> &Path {
        &self.workspace.root
    }

    pub(super) fn format(&self) -> Format {
        self.cli.global.format.unwrap_or(Format::Summary)
    }

    pub(super) fn engine(&self) -> &Engine {
        self.engine.as_ref().expect("session engine is open")
    }

    pub(super) fn engine_mut(&mut self) -> &mut Engine {
        self.engine.as_mut().expect("session engine is open")
    }

    /// Initial refresh; a read-only open escalates to writable on demand.
    pub(super) fn refresh(&mut self) -> Result<()> {
        if self.cli.global.no_reindex {
            return Ok(());
        }
        self.refreshed = match self.mode.refresh(self.engine_mut()) {
            Err(error) if error.is::<crate::engine::NeedsWrite>() => {
                let root = self.workspace.root.clone();
                self.reopen(&root)?;
                self.mode.refresh(self.engine_mut())?
            }
            result => result?,
        };
        Ok(())
    }

    /// Run one read against the engine, escalating at the workspace root.
    pub(super) fn run<T>(
        &mut self,
        label: &str,
        op: impl FnMut(&Engine) -> Result<T>,
    ) -> Result<T> {
        let root = self.workspace.root.clone();
        self.run_at(&root, label, op)
    }

    /// Run one read against the engine. If the read-only engine reports that
    /// it needs a write refresh, reopen writable at `root`, refresh (unless
    /// `--no-reindex`), and retry the read exactly once.
    pub(super) fn run_at<T>(
        &mut self,
        root: &Path,
        label: &str,
        mut op: impl FnMut(&Engine) -> Result<T>,
    ) -> Result<T> {
        match ui::spin(label, || op(self.engine())) {
            Err(error) if error.is::<crate::engine::NeedsWrite>() => {
                self.reopen(root)?;
                if !self.cli.global.no_reindex {
                    self.mode.refresh(self.engine_mut())?;
                }
                ui::spin(label, || op(self.engine()))
            }
            result => result,
        }
    }

    /// Release the shared lock before requesting the writable open.
    fn reopen(&mut self, root: &Path) -> Result<()> {
        self.engine = None;
        let engine = ui::spin("Opening index for update", || {
            self.mode.open(root, &self.index, &self.config, false)
        })?;
        self.engine = Some(engine);
        Ok(())
    }

    /// Handle commands that never open an engine and, for the rest, open the
    /// session engine. `None` means the command has been fully handled.
    pub(super) fn setup(
        cli: &'a Cli,
        workspace: &'a SourceWorkspace,
        mut out: &mut impl Write,
        map_output: &mut MapOutput,
        line_files: &mut Vec<(PathBuf, usize)>,
    ) -> Result<Option<Session<'a>>> {
        if let Command::Help {
            topic: Some(HelpTopic::Models { provider }),
        } = &cli.command
        {
            let models = ui::spin("Fetching published models", || {
                Providers::models(cli.models_provider(provider.as_deref())?)
            })?;
            if cli.global.format == Some(Format::Json) {
                print_json(&mut out, &models)?;
                return Ok(None);
            }
            for model in models.as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "{}/{}",
                    model["provider"].as_str().unwrap_or(""),
                    model["model"].as_str().unwrap_or("")
                )?;
            }
            return Ok(None);
        }
        if let Command::Help { topic: None } = &cli.command {
            use clap::CommandFactory;
            Cli::command().write_long_help(&mut out)?;
            return Ok(None);
        }
        if let Command::Help {
            topic: Some(HelpTopic::Command(path)),
        } = &cli.command
        {
            help_command(path)?.write_long_help(&mut out)?;
            return Ok(None);
        }
        let root = &workspace.root;
        let mut config_file = config_path(root, cli.global.config.as_deref())?;
        if let Command::Config { args } = &cli.command {
            run_config(&cli.global, &config_file, args, &mut out)?;
            return Ok(None);
        }

        if let Command::Update(args) = &cli.command
            && args.setup
        {
            let Some(path) = config::setup_embeddings(&cli.global, root, &config_file)? else {
                return Ok(None);
            };
            ui::info(format!(
                "slopdex: saved embedding settings to {}",
                path.display()
            ));
            config_file = path;
        }

        let mut config = effective_config(&cli.global, &config_file)?;
        // Settings committed to the repository are untrusted for provider policy.
        if repository_config_selected(root, cli.global.config.as_deref(), &config_file)? {
            mark_repository_config(&mut config, &cli.global)?;
        }
        if let Command::Map(args) = &cli.command {
            // The first selected workspace supplies the default for this command;
            // later workspace defaults cannot reset the shared declaration budget.
            map_output.initialize(&config, &args.options_for(cli.global.detail))?;
        }
        if let Some(directory) = &workspace.directory {
            for key in ["indexPath", "artifactCachePath"] {
                if let Some(path) = config[key]
                    .as_str()
                    .map(Path::new)
                    .filter(|path| path.is_relative())
                {
                    config[key] = json!(directory.join(path));
                }
            }
        }
        if let Command::Cache {
            action:
                CacheAction::Prune {
                    vacuum,
                    prune_vectors,
                    prune_descriptions,
                },
        } = &cli.command
        {
            let report = ui::spin("Pruning global artifact store", || {
                crate::cache::prune_with_options(
                    &config,
                    crate::cache::PruneOptions {
                        vacuum: *vacuum,
                        prune_vectors: *prune_vectors,
                        prune_descriptions: *prune_descriptions,
                    },
                )
            })?;
            print_json(&mut out, &serde_json::to_value(report)?)?;
            return Ok(None);
        }
        let index = index_path(root, cli.global.index.as_deref(), &config)?;
        if let Command::Lines(args) = &cli.command
            && !index.exists()
        {
            if args.selection.symbol_query.is_empty() {
                let root = root
                    .canonicalize()
                    .context("Repository root does not exist")?;
                ensure!(root.is_dir(), "Repository root is not a directory");
                let Some(options) = args.existing_options(&root, &workspace.paths)? else {
                    return Ok(None);
                };
                line_files.extend(ui::spin("Counting source lines", || {
                    lines::unindexed(&root, &index, &config, &options)
                })?);
                return Ok(None);
            }
            // Semantic selectors must not silently turn into whole-file counts.
            // A normal run can create a structure-only index on demand.
            if cli.global.no_reindex {
                require_index(&index)?;
            }
        }
        if let Command::CrossSearch(args) = &cli.command
            && let Some(target_index) = &args.target_index
        {
            require_index(&absolute(target_index)?)?;
        }
        if matches!(
            &cli.command,
            Command::Search(_)
                | Command::SearchCode(_)
                | Command::SearchDescriptions(_)
                | Command::SearchMd(_)
                | Command::SearchSymbols(_)
                | Command::CrossSearch(_)
        ) {
            require_index(&index)?;
        }
        if let Command::Map(args) = &cli.command
            && !index.exists()
        {
            let root = root
                .canonicalize()
                .context("Repository root does not exist")?;
            ensure!(root.is_dir(), "Repository root is not a directory");
            if !args.selection.symbol_query.is_empty() {
                // Diagnostics stay off stdout so `--output json` remains parseable.
                ui::warning("slopdex: warning: no active index; -q is ignored.");
            }
            let Some(mut options) =
                args.existing_options(&root, &workspace.paths, cli.global.detail)?
            else {
                return Ok(None);
            };
            options.as_object_mut().unwrap().remove("symbolQuery");
            options.as_object_mut().unwrap().remove("symbolThreshold");
            map_output.apply_limit(&mut options);
            let source = ui::spin("Parsing structure", || {
                map::Unindexed::parse(&root, &index, &config, &options)
            })?;
            let results = map::query_limited_with_reference_labels(
                &source,
                &root,
                &options,
                cli.global.format != Some(Format::Json),
            )?;
            emit_map(
                cli,
                &mut out,
                &results,
                Some(&source),
                Some(&filter::Selection::compile(&options)?),
                map_output,
            )?;
            return Ok(None);
        }
        let mode = CommandMode::for_command(&cli.command);
        if let Command::Index {
            action: IndexAction::List(args),
        } = &cli.command
        {
            require_index(&index)?;
            let engine = ui::spin("Opening saved index", || {
                Engine::open_symbol_map(root, &index, config.clone(), args.query.is_empty())
            })?;
            let format = cli.global.format.unwrap_or(Format::Summary);
            if let Some(name) = &args.name {
                let mut options = args.options();
                options["includeVectors"] = json!(matches!(format, Format::Json | Format::Csv));
                let results = ui::spin("Reading index entries", || {
                    engine.index_entries_limited(name, &options)
                })?;
                print_index_entries(&mut out, &results.rows, format, cli.global.detail)?;
                print_limit_notice(&mut out, format, results.omitted, results.limit)?;
            } else {
                print_indexes(&mut out, &engine.index_catalog()?, format)?;
            }
            return Ok(None);
        }
        let read_only_command = matches!(
            &cli.command,
            Command::Map(_)
                | Command::Lines(_)
                | Command::Status
                | Command::Index {
                    action: IndexAction::Errors
                }
                | Command::Search(_)
                | Command::SearchCode(_)
                | Command::SearchDescriptions(_)
                | Command::SearchMd(_)
                | Command::SearchSymbols(_)
        );
        let reader =
            read_only_command && index.exists() && config["forceReindex"].as_bool() != Some(true);
        let engine = ui::spin("Opening index", || mode.open(root, &index, &config, reader))?;
        let map_options = if let Command::Map(args) = &cli.command {
            let mut options = args.existing_options(root, &workspace.paths, cli.global.detail)?;
            if let Some(options) = &mut options {
                map_output.apply_limit(options);
            }
            options
        } else {
            None
        };
        Ok(Some(Session {
            cli,
            workspace,
            config,
            index,
            mode,
            engine: Some(engine),
            map_options,
            refreshed: None,
        }))
    }
}
