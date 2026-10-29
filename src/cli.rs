//! CLI entry points, engine lifecycle, and command orchestration.

mod args;
mod commands;
mod config;
mod io;
mod output;
mod session;
mod workspace;

#[cfg(test)]
use session::CommandMode;
use session::Session;

pub(crate) use output::{RenderedSymbol, faq_search_context};

use self::{
    args::{Cli, Command, Format},
    io::{print_json, with_stdout},
    output::{print_limit_notice, print_map},
    workspace::{SourceWorkspace, map_workspaces, root_path, source_workspace},
};
use crate::{
    engine::Engine,
    filter,
    limits::{self, LimitedResults, ResultLimit},
    lines, map, ui,
};
use anyhow::Result;
use clap::Parser;
use serde_json::Value;
use std::{
    io::{self as stdio, Write},
    path::PathBuf,
};

#[derive(Default)]
struct MapOutput {
    rows: Vec<Value>,
    limit: Option<ResultLimit>,
    remaining: Option<usize>,
    omitted: bool,
}

impl MapOutput {
    fn initialize(&mut self, config: &Value, options: &Value) -> Result<()> {
        if self.limit.is_none() {
            let limit = limits::resolve(config, options)?;
            self.limit = Some(limit);
            self.remaining = limit.count();
        }
        Ok(())
    }

    fn apply_limit(&self, options: &mut Value) {
        // A one-declaration probe after exhaustion distinguishes actual omission
        // from later workspaces with no matches, without permitting --limit 0.
        options["limit"] = self
            .remaining
            .map(|remaining| ResultLimit::Count(remaining.max(1)))
            .unwrap_or(ResultLimit::None)
            .value();
    }

    fn include(&mut self, results: &LimitedResults<Value>) -> bool {
        if self.remaining == Some(0) {
            self.omitted |= results.count > 0;
            return false;
        }
        self.remaining = self.remaining.map(|remaining| remaining - results.count);
        self.omitted |= results.omitted;
        true
    }
}

/// Report a runtime error on stderr, using terminal styling when available.
pub fn report_error(error: &anyhow::Error) {
    ui::error(format!("slopdex: {error:#}"));
}

/// Execute the CLI; main owns reporting runtime errors and selecting the failure exit code.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Err(error) = cli.validate() {
        // Post-parse argument problems are usage errors: report them like clap
        // (message plus usage on stderr) and exit with status 2.
        use clap::CommandFactory;
        Cli::command()
            .error(
                clap::error::ErrorKind::ValueValidation,
                format!("{error:#}"),
            )
            .exit();
    }
    let stdout = stdio::stdout();
    with_stdout(stdout.lock(), |out| run_cli(&cli, out))
}

fn run_cli(cli: &Cli, mut out: &mut impl Write) -> Result<()> {
    let root = root_path(cli.global.root.as_deref())?;
    let workspaces = match &cli.command {
        Command::Map(args) => map_workspaces(&root, &args.paths)?,
        Command::Lines(args) => map_workspaces(&root, &args.paths)?,
        Command::CrossSearch(args) if args.source_path.is_some() => {
            vec![source_workspace(&root, args.source_path.as_ref().unwrap())?]
        }
        _ => vec![SourceWorkspace {
            root,
            paths: Vec::new(),
            directory: None,
        }],
    };
    let mut map_output = MapOutput::default();
    let mut line_files = Vec::new();
    for workspace in workspaces {
        run_workspace(cli, &workspace, &mut out, &mut map_output, &mut line_files)?;
    }
    if let Command::Lines(args) = &cli.command {
        let limit = args.limit.and_then(ResultLimit::count);
        if cli.global.format == Some(Format::Files) {
            lines::print_files(&mut out, line_files, limit)?;
        } else {
            let tree = lines::Tree::new(line_files, limit);
            if cli.global.format == Some(Format::Json) {
                print_json(&mut out, &tree)?;
            } else {
                tree.print(&mut out)?;
            }
        }
    }
    if matches!(cli.command, Command::Map(_)) {
        let format = cli.global.format.unwrap_or(Format::Summary);
        if format == Format::Json {
            print_json(&mut out, &map_output.rows)?;
        }
        print_limit_notice(
            &mut out,
            format,
            map_output.omitted,
            map_output.limit.and_then(ResultLimit::count),
        )?;
    }
    Ok(())
}

fn emit_map(
    cli: &Cli,
    out: &mut impl Write,
    results: &LimitedResults<Value>,
    source: Option<&dyn map::StructureSource>,
    selection: Option<&filter::Selection>,
    collected: &mut MapOutput,
) -> Result<()> {
    if !collected.include(results) {
        return Ok(());
    }
    let format = cli.global.format.unwrap_or(Format::Summary);
    if cli.global.format == Some(Format::Json) {
        collected.rows.extend_from_slice(&results.rows);
    } else {
        print_map(
            out,
            &results.rows,
            format,
            cli.global.detail,
            source,
            selection,
        )?;
    }
    Ok(())
}

fn run_workspace(
    cli: &Cli,
    workspace: &SourceWorkspace,
    mut out: &mut impl Write,
    map_output: &mut MapOutput,
    line_files: &mut Vec<(PathBuf, usize)>,
) -> Result<()> {
    let Some(mut session) = Session::setup(cli, workspace, &mut out, map_output, line_files)?
    else {
        return Ok(());
    };
    session.refresh()?;
    warn_errors(
        session.engine(),
        session.config["ignoreErrors"].as_bool().unwrap_or(false),
    )?;
    commands::dispatch(&mut session, &mut out, map_output, line_files)
}

fn warn_errors(engine: &Engine, ignore: bool) -> Result<()> {
    if !ignore {
        let errors = engine.errors()?;
        if !errors.is_empty() {
            ui::warning(format!(
                "slopdex: {} saved indexing error(s); inspect with slopdex index errors",
                errors.len()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
