//! Per-command handlers over an opened, refreshed [`Session`].

use super::args::{
    Command, CrossArgs, FaqArgs, Format, GenerateAction, IndexAction, LinesArgs, RefactorArgs,
};
use super::config::{effective_config, mark_repository_config};
use super::io::print_json;
use super::output::{
    CrossOutput, Presentation, print_cross, print_errors, print_limit_notice, print_search,
};
use super::session::Session;
use super::workspace::{absolute, config_path, repository_config_selected, same_path};
use super::{MapOutput, emit_map, warn_errors};
use crate::engine::Engine;
use crate::{limits, lines, ui};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;

pub(super) fn dispatch(
    session: &mut Session,
    out: &mut impl Write,
    map_output: &mut MapOutput,
    line_files: &mut Vec<(PathBuf, usize)>,
) -> Result<()> {
    match &session.cli.command {
        Command::Refactor(args) => refactor(session, args, out),
        Command::Lines(args) => lines(session, args, line_files),
        Command::Map(_) => map(session, out, map_output),
        Command::Search(_)
        | Command::SearchCode(_)
        | Command::SearchDescriptions(_)
        | Command::SearchMd(_)
        | Command::SearchSymbols(_) => search(session, out),
        Command::Faq(args) => faq(session, args, out),
        Command::CrossSearch(args) => cross_search(session, args, out),
        Command::Status => print_json(out, &session.engine().status()?),
        Command::Index {
            action: IndexAction::Errors,
        } => print_errors(out, &session.engine().errors()?, session.format()),
        Command::Index {
            action: IndexAction::List(_),
        } => unreachable!(),
        Command::Update(_) => {
            let fallback = json!({"refreshed": false, "noReindex": true});
            print_json(out, session.refreshed.as_ref().unwrap_or(&fallback))
        }
        Command::Generate {
            action: GenerateAction::Descriptions,
        } => {
            let engine = session.engine_mut();
            let result = ui::spin("Generating file and callable descriptions", || {
                engine.generate_descriptions()
            })?;
            print_json(out, &result)
        }
        Command::Cache { .. } | Command::Config { .. } | Command::Help { .. } => unreachable!(),
    }
}

fn refactor(session: &mut Session, args: &RefactorArgs, out: &mut impl Write) -> Result<()> {
    let format = session.format();
    let plan = session.engine().plan_refactor(
        &args.source,
        &args.destination,
        &std::env::current_dir()?,
    )?;
    if !args.dry_run {
        plan.apply()?;
        // Persist only structure, never embeddings or generated descriptions.
        if let Err(error) = session.engine_mut().refresh_structure() {
            ui::warning(format!(
                "slopdex: source refactored, but index refresh failed: {error:#}; run update"
            ));
        }
    }
    if format == Format::Json {
        print_json(out, &plan.report(args.dry_run))?;
    } else {
        if args.dry_run {
            plan.print_diff(out)?;
        }
        writeln!(
            out,
            "{} {} file(s)",
            if args.dry_run {
                "Would change"
            } else {
                "Changed"
            },
            plan.changes.len()
        )?;
    }
    Ok(())
}

fn lines(
    session: &mut Session,
    args: &LinesArgs,
    line_files: &mut Vec<(PathBuf, usize)>,
) -> Result<()> {
    let root = session
        .root()
        .canonicalize()
        .context("Repository root does not exist")?;
    let Some(options) = args.existing_options(&root, &session.workspace.paths)? else {
        return Ok(());
    };
    let files = session.run_at(&root, "Counting source lines", |engine| {
        lines::indexed(engine, &root, &options)
    })?;
    line_files.extend(files);
    Ok(())
}

fn map(session: &mut Session, out: &mut impl Write, map_output: &mut MapOutput) -> Result<()> {
    let format = session.format();
    if let Some(options) = session.map_options.take() {
        let (rows, selection) = session.run("Mapping repository structure", |engine| {
            Ok((
                engine.map_limited(&options, format != Format::Json)?,
                engine.selection(&options)?,
            ))
        })?;
        emit_map(
            session.cli,
            out,
            &rows,
            Some(session.engine()),
            Some(&selection),
            map_output,
        )?;
    }
    Ok(())
}

fn search(session: &mut Session, out: &mut impl Write) -> Result<()> {
    let cli = session.cli;
    let format = session.format();
    let (args, kind, options, descriptions) = cli.command.search_request().unwrap();
    let mut queries = vec![args.query.clone()];
    if let Command::Search(search) = &cli.command {
        queries.extend(search.additional_queries.iter().cloned());
    }
    let rows = session.run("Searching index", |engine| {
        engine.search_queries_limited(&queries, kind, &options)
    })?;
    print_search(
        out,
        &rows.rows,
        format,
        cli.global.detail,
        descriptions,
        &mut Presentation::with_calls(
            session.engine(),
            args.calls.resolve(cli.global.detail),
            cli.global.expand_code_threshold,
        )?,
    )?;
    print_limit_notice(out, format, rows.omitted, rows.limit)
}

fn faq(session: &mut Session, args: &FaqArgs, out: &mut impl Write) -> Result<()> {
    let cli = session.cli;
    let format = session.format();
    let mut options = args.query.filters.options();
    let calls = args.query.calls.resolve(cli.global.detail);
    let context = args.query.calls.resolve_faq_context();
    options["callers"] = json!(context.callers);
    options["callees"] = json!(context.callees);
    options["expandCallers"] = json!(context.expand_callers);
    options["expandCallees"] = json!(context.expand_callees);
    options["refs"] = json!(context.refs);
    options["expandCodeThreshold"] = json!(cli.global.expand_code_threshold);
    if let Some(threshold) = args.faq_threshold {
        options["faqThreshold"] = json!(threshold);
    }
    let engine = session.engine();
    let mut result = ui::spin("Answering FAQ", || engine.faq(&args.query.query, &options))?;
    // A semantic hit returns the original references, never a fresh search.
    let references: Vec<Value> = serde_json::from_value(result["searchResults"].clone())?;
    if format == Format::Json {
        if calls.enabled() {
            let presentation =
                Presentation::with_calls(engine, calls, cli.global.expand_code_threshold)?;
            if let Some(functions) = result["functions"].as_array_mut() {
                for function in functions {
                    let related = presentation.related_json(function);
                    let outgoing = presentation.outgoing_json(function);
                    function["relatedCallables"] = json!(related);
                    function["references"] = json!(outgoing);
                }
            }
        }
        print_json(out, &result)?;
    } else {
        writeln!(
            out,
            "Explanation\n{}",
            result["description"].as_str().unwrap_or("")
        )?;
        if !references.is_empty() {
            writeln!(out, "\nReferences")?;
            print_search(
                out,
                &references,
                format,
                cli.global.detail,
                false,
                &mut Presentation::with_calls(engine, calls, cli.global.expand_code_threshold)?,
            )?;
        }
    }
    print_limit_notice(
        out,
        format,
        result["omitted"].as_bool().unwrap_or(false),
        result["limit"].as_u64().map(|n| n as usize),
    )
}

fn cross_search(session: &mut Session, args: &CrossArgs, out: &mut impl Write) -> Result<()> {
    let cli = session.cli;
    let mut target = None;
    if let (Some(target_root), Some(target_index)) = (&args.target_root, &args.target_index) {
        let target_root = absolute(target_root)?;
        let target_index = absolute(target_index)?;
        if same_path(&session.index, &target_index)? {
            ensure!(
                same_path(session.root(), &target_root)?,
                "source and target use the same index but different repository roots"
            );
        } else {
            let target_path = config_path(&target_root, args.target_config.as_deref())?;
            let mut target_config = effective_config(&cli.global, &target_path)?;
            if repository_config_selected(
                &target_root,
                args.target_config.as_deref(),
                &target_path,
            )? {
                mark_repository_config(&mut target_config, &cli.global)?;
            }
            target_config["indexPath"] = json!(target_index);
            let mut opened = ui::spin("Opening target index", || {
                Engine::open(&target_root, &target_index, target_config.clone())
            })?;
            if !cli.global.no_reindex {
                ui::spin("Refreshing target index", || opened.refresh())?;
            }
            warn_errors(
                &opened,
                target_config["ignoreErrors"].as_bool().unwrap_or(false),
            )?;
            target = Some(opened);
        }
    }
    let mut options = args.options();
    if let Some(path) = session.workspace.paths.first() {
        options["sourcePath"] = json!(path);
    }
    let engine = session.engine();
    let rows = ui::spin("Comparing indexed functions", || {
        engine.cross_search(target.as_ref(), &options)
    })?;
    let format = cli.global.format.unwrap_or(if args.cohesion {
        Format::Summary
    } else {
        Format::Clusters
    });
    let limit = limits::resolve(&session.config, &args.filters.options())?.count();
    let omitted = print_cross(
        out,
        rows,
        CrossOutput::new(
            format,
            target.is_none(),
            args.cohesion,
            limit,
            cli.global.detail,
        ),
        &mut Presentation::with_calls(
            engine,
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
    print_limit_notice(out, format, omitted, limit)
}
