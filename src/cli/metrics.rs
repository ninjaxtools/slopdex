//! Local, aggregate-only CLI usage. Deliberately independent of index/artifact
//! storage: observing usage must neither create an index nor share telemetry.

use super::args::{Cli, Command, Format, GenerateAction, IndexAction};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const HISTORY_DAYS: u64 = 90;
const MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Usage {
    results: Option<u64>,
    lines_counted: u64,
    faq_reused_answers: u64,
    pub(super) files_changed: u64,
}

impl Usage {
    pub(super) fn results(&mut self, count: u64) {
        *self.results.get_or_insert(0) += count;
    }

    pub(super) fn line_files(&mut self, files: &[(PathBuf, usize)]) {
        self.results(files.len() as u64);
        self.lines_counted += files.iter().map(|(_, count)| *count as u64).sum::<u64>();
    }

    pub(super) fn faq(&mut self, result: &Value, reused: u64) {
        self.results(result.as_array().map_or(1, Vec::len) as u64);
        self.faq_reused_answers += reused;
    }
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Counts {
    invocations: u64,
    succeeded: u64,
    failed: u64,
    duration_ms: u64,
    result_invocations: u64,
    empty_result_invocations: u64,
    results: u64,
    lines_counted: u64,
    faq_reused_answers: u64,
    files_changed: u64,
    saved_snapshot_invocations: u64,
    dry_runs: u64,
}

impl Counts {
    fn add(&mut self, other: &Self) {
        // Saturate rather than allowing a malformed counter to break commands.
        macro_rules! add {
            ($($field:ident),* $(,)?) => {$(
                self.$field = self.$field.saturating_add(other.$field);
            )*};
        }
        add!(
            invocations,
            succeeded,
            failed,
            duration_ms,
            result_invocations,
            empty_result_invocations,
            results,
            lines_counted,
            faq_reused_answers,
            files_changed,
            saved_snapshot_invocations,
            dry_runs
        );
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct History {
    version: u32,
    first_used_at_unix: Option<u64>,
    last_used_at_unix: Option<u64>,
    commands: BTreeMap<String, Counts>,
    /// UTC day start, in Unix seconds. Only recent daily buckets are retained.
    days: BTreeMap<u64, Counts>,
}

impl Default for History {
    fn default() -> Self {
        Self {
            version: 1,
            first_used_at_unix: None,
            last_used_at_unix: None,
            commands: BTreeMap::new(),
            days: BTreeMap::new(),
        }
    }
}

impl History {
    fn add(&mut self, command: &str, counts: &Counts, now: u64) {
        self.first_used_at_unix = Some(self.first_used_at_unix.map_or(now, |first| first.min(now)));
        self.last_used_at_unix = Some(self.last_used_at_unix.map_or(now, |last| last.max(now)));
        self.commands.entry(command.into()).or_default().add(counts);
        self.days
            .entry(now / 86400 * 86400)
            .or_default()
            .add(counts);
        self.prune_days(now);
    }

    fn prune_days(&mut self, now: u64) {
        let cutoff = (now / 86400).saturating_sub(HISTORY_DAYS - 1) * 86400;
        self.days.retain(|day, _| *day >= cutoff);
    }

    fn totals(&self) -> Counts {
        let mut counts = Counts::default();
        for command in self.commands.values() {
            counts.add(command);
        }
        counts
    }
}

fn command_name(command: &Command) -> Option<&'static str> {
    Some(match command {
        Command::Search(_) => "search",
        Command::SearchCode(_) => "search-code",
        Command::SearchDescriptions(_) => "search-descriptions",
        Command::SearchMd(_) => "search-docs",
        Command::SearchSymbols(_) => "search-symbols",
        Command::Faq(_) => "faq",
        Command::CrossSearch(_) => "cross-search",
        Command::Map(_) => "map",
        Command::Lines(_) => "lines",
        Command::Refactor(_) => "refactor",
        Command::Status => "status",
        Command::Update(_) => "update",
        Command::Index {
            action: IndexAction::Errors,
        } => "index errors",
        Command::Index {
            action: IndexAction::List(_),
        } => "index list",
        Command::Generate {
            action: GenerateAction::Descriptions,
        } => "generate descriptions",
        Command::Metrics { .. }
        | Command::Config { .. }
        | Command::Cache { .. }
        | Command::Help { .. } => return None,
    })
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn path(root: &Path) -> Result<PathBuf> {
    let root = root
        .canonicalize()
        .context("Repository root does not exist")?;
    ensure!(root.is_dir(), "Repository root is not a directory");
    Ok(crate::cache::directory()?
        .join("metrics-v1")
        .join(crate::hash(root.as_os_str().as_encoded_bytes()))
        .join("usage.json"))
}

/// A stable, separate lock inode protects read/modify/atomic-replace across
/// processes. Never unlink it: doing so would split waiting writers' locks.
fn lock(path: &Path) -> Result<File> {
    fs::create_dir_all(path.parent().context("Metrics path has no parent")?)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))?;
    let deadline = Instant::now() + Duration::from_secs(1);
    // The shared helper recognizes fs2's native contention code too: Windows
    // ERROR_LOCK_VIOLATION is not necessarily classified as WouldBlock.
    ensure!(
        crate::registry::lock_exclusive_until(&file, deadline)
            .context("Project metrics are busy or unavailable")?,
        "Project metrics are busy or unavailable"
    );
    Ok(file)
}

fn load(path: &Path) -> Result<History> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(History::default()),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(file, MAX_BYTES + 1), &mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "Metrics file exceeds size limit"
    );
    let history: History = serde_json::from_slice(&bytes)
        .context("Invalid metrics file; use metrics --reset to clear it")?;
    ensure!(
        history.version == 1,
        "Unsupported metrics version {}",
        history.version
    );
    Ok(history)
}

fn save(path: &Path, history: &History) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    // Serialize before writing to avoid thousands of tiny filesystem writes.
    file.as_file_mut()
        .write_all(&serde_json::to_vec(history)?)?;
    file.as_file_mut().sync_all()?;
    file.persist(path)?;
    Ok(())
}

pub(super) fn record(cli: &Cli, root: &Path, duration: Duration, success: bool, usage: Usage) {
    // In-process CLI unit tests must never touch the real user's metrics.
    // End-to-end tracking tests run the binary with a child-local cache home.
    if cfg!(test) {
        return;
    }
    let Some(command) = command_name(&cli.command) else {
        return;
    };
    if std::env::var_os("SLOPDEX_NO_METRICS").is_some_and(|value| value == "1") {
        return;
    }
    let counts = Counts {
        invocations: 1,
        succeeded: u64::from(success),
        failed: u64::from(!success),
        duration_ms: duration.as_millis().min(u64::MAX as u128) as u64,
        result_invocations: u64::from(success && usage.results.is_some()),
        empty_result_invocations: u64::from(success && usage.results == Some(0)),
        results: usage.results.unwrap_or(0),
        lines_counted: usage.lines_counted,
        faq_reused_answers: usage.faq_reused_answers,
        files_changed: usage.files_changed,
        saved_snapshot_invocations: u64::from(cli.global.no_reindex),
        dry_runs: u64::from(matches!(&cli.command, Command::Refactor(args) if args.dry_run)),
    };
    let update = || -> Result<()> {
        let path = path(root)?;
        let _lock = lock(&path)?;
        let mut history = load(&path)?;
        history.add(command, &counts, now()?);
        save(&path, &history)
    };
    if let Err(error) = update() {
        // Never replace a command's result or leak its arguments/error text.
        crate::ui::warning(format!(
            "slopdex: warning: could not record project metrics: {error}"
        ));
    }
}

/// A multi-workspace map/lines call still counts only once per canonical project.
/// Flush on error exits too, after workspace engines and locks are released.
pub(super) struct Tracker<'a> {
    cli: &'a Cli,
    projects: BTreeMap<PathBuf, (Duration, bool, Usage)>,
}

impl<'a> Tracker<'a> {
    pub(super) fn new(cli: &'a Cli) -> Self {
        Self {
            cli,
            projects: BTreeMap::new(),
        }
    }

    pub(super) fn observe(&mut self, root: &Path, duration: Duration, success: bool, usage: Usage) {
        if command_name(&self.cli.command).is_none() {
            return;
        }
        let root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
        let entry = self
            .projects
            .entry(root)
            .or_insert_with(|| (Duration::ZERO, true, Usage::default()));
        entry.0 = entry.0.saturating_add(duration);
        entry.1 &= success;
        if let Some(results) = usage.results {
            entry.2.results(results);
        }
        entry.2.lines_counted += usage.lines_counted;
        entry.2.faq_reused_answers += usage.faq_reused_answers;
        entry.2.files_changed += usage.files_changed;
    }
}

impl Drop for Tracker<'_> {
    fn drop(&mut self) {
        for (root, (duration, success, usage)) in std::mem::take(&mut self.projects) {
            record(self.cli, &root, duration, success, usage);
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    project: PathBuf,
    metrics_path: PathBuf,
    tracking_enabled: bool,
    totals: Counts,
    recent_active_days: usize,
    #[serde(flatten)]
    history: History,
}

pub(super) fn print(
    root: &Path,
    reset: bool,
    format: Option<Format>,
    out: &mut impl Write,
) -> Result<()> {
    let path = path(root)?;
    let mut history = if reset {
        let _lock = lock(&path)?;
        let history = History::default();
        save(&path, &history)?;
        history
    } else {
        // Readers need no lock: atomic replacement gives a complete snapshot.
        // An unused project's metrics command creates no files.
        load(&path)?
    };
    history.prune_days(now()?);
    let report = Report {
        project: root.canonicalize()?,
        metrics_path: path,
        tracking_enabled: !std::env::var_os("SLOPDEX_NO_METRICS").is_some_and(|value| value == "1"),
        totals: history.totals(),
        recent_active_days: history.days.len(),
        history,
    };
    if format == Some(Format::Json) {
        return super::io::print_json(out, &report);
    }
    writeln!(out, "Slopdex metrics: {}", report.project.display())?;
    if reset {
        writeln!(out, "Project usage history cleared.")?;
    }
    if !report.tracking_enabled {
        writeln!(out, "Tracking disabled (SLOPDEX_NO_METRICS=1).")?;
    }
    let totals = &report.totals;
    if totals.invocations == 0 {
        writeln!(
            out,
            "No usage recorded yet. Metrics start with this version; past usage is not backfilled."
        )?;
        return Ok(());
    }
    writeln!(
        out,
        "{} invocations: {} succeeded, {} failed; {} active UTC days in the last {HISTORY_DAYS} days",
        totals.invocations, totals.succeeded, totals.failed, report.recent_active_days
    )?;
    writeln!(
        out,
        "Total command time: {:.2}s (not time saved)",
        totals.duration_ms as f64 / 1000.0
    )?;
    writeln!(
        out,
        "\n{:<24} {:>8} {:>8} {:>8} {:>11} {:>10}",
        "Command", "Calls", "Success", "Failed", "Avg ms", "Results"
    )?;
    for (command, counts) in &report.history.commands {
        writeln!(
            out,
            "{command:<24} {:>8} {:>8} {:>8} {:>11.1} {:>10}",
            counts.invocations,
            counts.succeeded,
            counts.failed,
            counts.duration_ms as f64 / counts.invocations.max(1) as f64,
            if counts.result_invocations > 0 {
                counts.results.to_string()
            } else {
                "-".into()
            }
        )?;
    }
    writeln!(
        out,
        "\nResults: {} returned; {} of {} measured calls returned none",
        totals.results, totals.empty_result_invocations, totals.result_invocations
    )?;
    writeln!(
        out,
        "FAQ answers reused: {}; source lines counted: {}; files refactored: {}",
        totals.faq_reused_answers, totals.lines_counted, totals.files_changed
    )?;
    writeln!(
        out,
        "Saved-snapshot calls: {}; refactor dry runs: {}",
        totals.saved_snapshot_invocations, totals.dry_runs
    )?;
    writeln!(
        out,
        "\nLocal aggregates only; no queries or source stored. These measure usage, not usefulness or cost savings."
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
