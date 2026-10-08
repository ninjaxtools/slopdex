//! CLI entry points, engine lifecycle, and command orchestration.

mod args;
mod config;
mod io;
mod output;
mod workspace;

pub(crate) use output::describe_search_context;

use self::{
    args::{Cli, Command, Format, GenerateAction, HelpTopic, IndexAction},
    config::{effective_config, run_config},
    io::{print_json, with_stdout},
    output::{CrossOutput, Presentation, print_cross, print_errors, print_map, print_search},
    workspace::{
        absolute, config_path, index_path, migrate_legacy_index, require_index, same_path,
    },
};
use crate::{engine::Engine, filter, map, providers::Providers, ui};
use anyhow::{Context, Result, ensure};
use clap::Parser;
use serde_json::{Value, json};
use std::{
    io::{self as stdio, Write},
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandMode {
    Content,
    Structure,
    Symbols,
}

impl CommandMode {
    fn for_command(command: &Command) -> Self {
        match command {
            Command::Map(args) if args.selection.symbol_query.is_empty() => Self::Structure,
            Command::Map(_) | Command::SearchSymbols(_) => Self::Symbols,
            Command::Search(args) if args.symbols_only() => Self::Symbols,
            _ => Self::Content,
        }
    }

    fn open(self, root: &Path, index: &Path, config: &Value, readonly: bool) -> Result<Engine> {
        match (self, readonly) {
            (Self::Symbols, _) => Engine::open_symbol_map(root, index, config.clone(), readonly),
            (Self::Structure, true) => Engine::open_map_readonly(root, index, config.clone()),
            (Self::Structure, false) => Engine::open_map(root, index, config.clone()),
            (Self::Content, true) => Engine::open_readonly(root, index, config.clone()),
            (Self::Content, false) => Engine::open(root, index, config.clone()),
        }
    }

    fn refresh(self, engine: &mut Engine) -> Result<Option<Value>> {
        match self {
            Self::Content => ui::spin("Refreshing index", || engine.refresh()).map(Some),
            Self::Structure | Self::Symbols => {
                ui::spin("Refreshing structure", || engine.refresh_structure())?;
                Ok(None)
            }
        }
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
    let stdout = stdio::stdout();
    with_stdout(stdout.lock(), |out| run_cli(&cli, out))
}

fn run_cli(cli: &Cli, mut out: &mut impl Write) -> Result<()> {
    if let Command::Help {
        topic: Some(HelpTopic::Models { provider }),
    } = &cli.command
    {
        let models = ui::spin("Fetching published models", || {
            Providers::models(cli.models_provider(provider.as_deref())?)
        })?;
        if cli.global.format == Some(Format::Json) {
            return print_json(&mut out, &models);
        }
        for model in models.as_array().into_iter().flatten() {
            writeln!(
                out,
                "{}/{}",
                model["provider"].as_str().unwrap_or(""),
                model["model"].as_str().unwrap_or("")
            )?;
        }
        return Ok(());
    }
    if let Command::Help { topic: None } = &cli.command {
        use clap::CommandFactory;
        Cli::command().write_long_help(&mut out)?;
        return Ok(());
    }
    let root = absolute(cli.global.root.as_deref().unwrap_or(Path::new(".")))?;
    let config_file = config_path(&root, cli.global.config.as_deref())?;
    if let Command::Config { args } = &cli.command {
        return run_config(&cli.global, &config_file, args, &mut out);
    }

    let config = effective_config(&cli.global, &config_file)?;
    let index = index_path(&root, cli.global.index.as_deref(), &config)?;
    if let Command::CrossSearch(args) = &cli.command
        && let Some(target_index) = &args.target_index
    {
        require_index(&absolute(target_index)?)?;
    }
    if cli.global.index.is_none() && config["indexPath"].is_null() {
        migrate_legacy_index(&root, &index)?;
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
        let format = cli.global.format.unwrap_or(Format::Summary);
        if !args.selection.symbol_query.is_empty() {
            writeln!(out, "slopdex: warning: no active index; -q is ignored.")?;
        }
        let Some(mut options) = args.existing_options(&root, cli.global.detail)? else {
            return print_map(&mut out, &[], format, cli.global.detail, None, None);
        };
        options.as_object_mut().unwrap().remove("symbolQuery");
        options.as_object_mut().unwrap().remove("symbolThreshold");
        let source = ui::spin("Parsing structure", || {
            map::Unindexed::parse(&root, &index, &config, &options)
        })?;
        let rows = map::query(&source, &root, &options)?;
        return print_map(
            &mut out,
            &rows,
            format,
            cli.global.detail,
            Some(&source),
            Some(&filter::Selection::compile(&options)?),
        );
    }
    let mode = CommandMode::for_command(&cli.command);
    let read_only_command = matches!(
        &cli.command,
        Command::Map(_)
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
    let mut engine = ui::spin("Opening index", || {
        mode.open(&root, &index, &config, reader)
    })?;
    let map_options = if let Command::Map(args) = &cli.command {
        args.existing_options(&root, cli.global.detail)?
    } else {
        None
    };
    let refreshed = if cli.global.no_reindex {
        None
    } else {
        match mode.refresh(&mut engine) {
            Err(error) if error.is::<crate::engine::NeedsWrite>() => {
                drop(engine);
                engine = ui::spin("Opening index for update", || {
                    mode.open(&root, &index, &config, false)
                })?;
                mode.refresh(&mut engine)?
            }
            result => result?,
        }
    };
    warn_errors(
        &engine,
        &index,
        config["ignoreErrors"].as_bool().unwrap_or(false),
    )?;
    let format = cli.global.format.unwrap_or(Format::Summary);
    match &cli.command {
        Command::Map(_) => {
            let (rows, selection) = if let Some(options) = map_options.as_ref() {
                let mapped = ui::spin("Mapping repository structure", || {
                    Ok((engine.map(options)?, engine.selection(options)?))
                });
                let (rows, selection) = match mapped {
                    Err(error) if error.is::<crate::engine::NeedsWrite>() => {
                        drop(engine);
                        engine = ui::spin("Opening index for update", || {
                            mode.open(&root, &index, &config, false)
                        })?;
                        if !cli.global.no_reindex {
                            mode.refresh(&mut engine)?;
                        }
                        ui::spin("Mapping repository structure", || {
                            Ok((engine.map(options)?, engine.selection(options)?))
                        })?
                    }
                    result => result?,
                };
                (rows, Some(selection))
            } else {
                (Vec::new(), None)
            };
            print_map(
                &mut out,
                &rows,
                format,
                cli.global.detail,
                Some(&engine),
                selection.as_ref(),
            )?;
        }
        Command::Search(_)
        | Command::SearchCode(_)
        | Command::SearchDescriptions(_)
        | Command::SearchMd(_)
        | Command::SearchSymbols(_) => {
            let (args, kind, options, descriptions) = cli.command.search_request().unwrap();
            let rows = match ui::spin("Searching index", || {
                engine.search(&args.query, kind, &options)
            }) {
                Err(error) if error.is::<crate::engine::NeedsWrite>() => {
                    drop(engine);
                    engine = ui::spin("Opening index for update", || {
                        mode.open(&root, &index, &config, false)
                    })?;
                    if !cli.global.no_reindex {
                        mode.refresh(&mut engine)?;
                    }
                    ui::spin("Searching index", || {
                        engine.search(&args.query, kind, &options)
                    })?
                }
                result => result?,
            };
            print_search(
                &mut out,
                &rows,
                format,
                cli.global.detail,
                descriptions,
                &mut Presentation::with_calls(
                    &engine,
                    args.calls.resolve(cli.global.detail),
                    cli.global.expand_code_threshold,
                )?,
            )?;
        }
        Command::Describe(args) => {
            let mut options = args.query.filters.options();
            let calls = args.query.calls.resolve(cli.global.detail);
            let context = args.query.calls.resolve_describe_context();
            options["callers"] = json!(context.callers);
            options["callees"] = json!(context.callees);
            options["expandCallers"] = json!(context.expand_callers);
            options["expandCallees"] = json!(context.expand_callees);
            options["expandCodeThreshold"] = json!(cli.global.expand_code_threshold);
            let mut result = ui::spin("Generating explanation", || {
                engine.describe(&args.query.query, &options)
            })?;
            if format == Format::Json {
                if calls.enabled() {
                    let presentation =
                        Presentation::with_calls(&engine, calls, cli.global.expand_code_threshold)?;
                    if let Some(functions) = result["functions"].as_array_mut() {
                        for function in functions {
                            let related = presentation.related_json(function);
                            let outgoing = presentation.outgoing_json(function);
                            function["relatedCallables"] = json!(related);
                            function["callees"] = json!(outgoing);
                        }
                    }
                }
                print_json(&mut out, &result)?;
            } else {
                writeln!(
                    out,
                    "Explanation\n{}",
                    result["description"].as_str().unwrap_or("")
                )?;
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
                        &mut Presentation::with_calls(
                            &engine,
                            calls,
                            cli.global.expand_code_threshold,
                        )?,
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
                &mut Presentation::with_calls(
                    &engine,
                    args.calls.resolve(cli.global.detail),
                    cli.global.expand_code_threshold,
                )?,
                target
                    .as_ref()
                    .map(|target| {
                        Presentation::with_calls(
                            target,
                            args.calls.resolve(cli.global.detail),
                            cli.global.expand_code_threshold,
                        )
                    })
                    .transpose()?
                    .as_mut(),
            )?;
        }
        Command::Status => print_json(&mut out, &engine.status()?)?,
        Command::Index {
            action: IndexAction::Errors,
        } => print_errors(&mut out, &engine.errors()?, format)?,
        Command::Update(_) => print_json(
            &mut out,
            &refreshed.unwrap_or(json!({"refreshed": false, "noReindex": true})),
        )?,
        Command::Generate {
            action: GenerateAction::Descriptions,
        } => {
            let result = ui::spin("Generating file and callable descriptions", || {
                engine.generate_descriptions()
            })?;
            print_json(&mut out, &result)?
        }
        Command::Config { .. } | Command::Help { .. } => unreachable!(),
    }
    Ok(())
}

fn warn_errors(engine: &Engine, index: &Path, ignore: bool) -> Result<()> {
    if !ignore {
        let errors = engine.errors()?;
        if !errors.is_empty() {
            ui::warning(format!(
                "slopdex: {} saved indexing error(s) in {}; inspect with index errors",
                errors.len(),
                index.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
