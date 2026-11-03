//! Terminal presentation shared by commands and background provider work.
//! Results stay on stdout; interactive rendering is confined to terminal stderr.

use std::{
    cell::RefCell,
    fmt::Display,
    io::{self, IsTerminal, Write},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::Result;

struct Active {
    id: Arc<()>,
    label: String,
    display: Option<TerminalDisplay>,
    state: ProgressState,
}

struct TerminalDisplay {
    group: cliclack::MultiProgress,
    bar: cliclack::ProgressBar,
    indicator: Option<Indicator>,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);
const TEXT_DELAY: Duration = Duration::from_millis(200);
const BAR_THRESHOLD: Duration = Duration::from_secs(1);
const REFRESH_INTERVAL: Duration = Duration::from_millis(25);
const PROGRESS_TEMPLATE: &str = "{msg} {bar:30.magenta} ETA {eta}";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Indicator {
    Spinner,
    Progress,
}

// Keep the last settled text while a replacement is pending. Frequent short
// updates (such as filenames) must not reset the age of the enclosing task.
struct DelayedText {
    text: String,
    since: Instant,
    visible: Option<String>,
}

impl DelayedText {
    fn new(text: String) -> Self {
        Self {
            text,
            since: Instant::now(),
            visible: None,
        }
    }

    fn set(&mut self, text: String) {
        if self.text != text {
            self.text = text;
            self.since = Instant::now();
        }
    }

    fn get(&mut self, now: Instant) -> Option<&str> {
        if now.saturating_duration_since(self.since) >= TEXT_DELAY {
            self.visible = Some(self.text.clone());
        }
        self.visible.as_deref()
    }
}

// Tokens belong to a stage, rather than a stack index or display generation:
// a late increment/drop cannot affect another stage or a later CLI operation.
type ScopeId = StageId;

struct Count {
    label: String,
    completed: usize,
    total: usize,
}

impl Count {
    fn message(&self, success: bool) -> String {
        let percent = if self.total == 0 {
            if success { 100 } else { 0 }
        } else {
            self.completed as u128 * 100 / self.total as u128
        };
        format!(
            "{} {percent}% ({}/{})",
            self.label, self.completed, self.total
        )
    }
}

struct Scope {
    id: ScopeId,
    count: Count,
    detail: Option<DelayedText>,
    since: Instant,
    bar_visible: bool,
}

impl Scope {
    fn render(&self, detail: Option<&str>) -> Render {
        let mut message = self.count.message(false);
        if let Some(detail) = detail {
            message.push_str(" — ");
            message.push_str(detail);
        }
        Render {
            message,
            counts: Some((self.count.completed, self.count.total)),
            show_bar: false,
        }
    }

    fn visible(&mut self, now: Instant) -> Option<Render> {
        let elapsed = now.saturating_duration_since(self.since);
        if elapsed < TEXT_DELAY {
            return None;
        }
        // With no samples, wait until the task actually exceeds a second.
        // Once shown, keep the bar stable even if later samples are faster.
        if self.count.completed < self.count.total
            && (elapsed > BAR_THRESHOLD
                || (self.count.completed > 0
                    && elapsed.as_secs_f64() * self.count.total as f64
                        / self.count.completed as f64
                        > BAR_THRESHOLD.as_secs_f64()))
        {
            self.bar_visible = true;
        }
        let detail = self
            .detail
            .as_mut()
            .and_then(|detail| detail.get(now))
            .map(str::to_owned);
        let mut render = self.render(detail.as_deref());
        render.show_bar = self.bar_visible;
        Some(render)
    }
}

struct ProgressState {
    spinner: DelayedText,
    scopes: Vec<Scope>,
    last: Option<(Count, bool)>,
}

struct Render {
    message: String,
    // None selects the unknown-size spinner template.
    counts: Option<(usize, usize)>,
    show_bar: bool,
}

impl ProgressState {
    fn new(label: &str) -> Self {
        Self {
            spinner: DelayedText::new(label.to_owned()),
            scopes: Vec::new(),
            last: None,
        }
    }

    #[cfg(test)]
    fn push(&mut self, label: String, total: usize) -> ScopeId {
        let id = StageId::next();
        self.push_stage(id, label, total);
        id
    }

    fn push_stage(&mut self, id: ScopeId, label: String, total: usize) {
        if self.scopes.is_empty() {
            self.last = None;
        }
        self.scopes.push(Scope {
            id,
            count: Count {
                label,
                completed: 0,
                total,
            },
            detail: None,
            since: Instant::now(),
            bar_visible: false,
        });
    }

    #[cfg(test)]
    fn inc(&mut self, id: &ScopeId, delta: usize) {
        if let Some(scope) = self.scopes.iter_mut().find(|s| s.id == *id) {
            scope.count.completed = scope
                .count
                .completed
                .saturating_add(delta)
                .min(scope.count.total);
        }
    }

    fn set_completed(&mut self, id: &ScopeId, completed: usize) {
        if let Some(scope) = self.scopes.iter_mut().find(|s| s.id == *id) {
            scope.count.completed = completed.min(scope.count.total);
        }
    }

    fn end(&mut self, id: &ScopeId, success: bool) {
        if let Some(index) = self.scopes.iter().position(|s| s.id == *id) {
            let scope = self.scopes.remove(index);
            // Only retain the outer task's summary. Nested completion restores
            // its parent without printing or accumulating a row per child.
            if index == 0 {
                self.last = Some((scope.count, success));
            }
            if self.scopes.is_empty() {
                // Returning to an unknown-size stage is itself a replacement.
                self.spinner.since = Instant::now();
                self.spinner.visible = None;
            }
        }
    }

    fn message(&mut self, message: String) {
        if let Some(scope) = self.scopes.last_mut() {
            if let Some(detail) = &mut scope.detail {
                detail.set(message);
            } else {
                scope.detail = Some(DelayedText::new(message));
            }
        } else {
            self.spinner.set(message);
            self.last = None;
        }
    }

    #[cfg(test)]
    fn render(&self) -> Render {
        if let Some(scope) = self.scopes.last() {
            scope.render(scope.detail.as_ref().map(|detail| detail.text.as_str()))
        } else {
            Render {
                message: self.spinner.text.clone(),
                counts: None,
                show_bar: false,
            }
        }
    }

    fn visible(&mut self, now: Instant) -> Option<Render> {
        if self.scopes.is_empty() {
            return self.spinner.get(now).map(|message| Render {
                message: message.to_owned(),
                counts: None,
                show_bar: false,
            });
        }
        // A short-lived child never displaces its already-visible parent.
        let mut rows = self
            .scopes
            .iter_mut()
            .filter_map(|scope| scope.visible(now))
            .collect::<Vec<_>>();
        let mut active = rows.pop()?;
        if !rows.is_empty() {
            active.message = format!(
                "{}\n│    {}",
                rows.into_iter()
                    .map(|render| render.message)
                    .collect::<Vec<_>>()
                    .join("\n│    "),
                active.message
            );
        }
        Some(active)
    }

    fn completion(&self, success: bool) -> String {
        // Successful outer scopes already emitted their completed-count line.
        // Keep the final command row from repeating the last stage's summary.
        if success && self.scopes.is_empty() && self.last.as_ref().is_some_and(|(_, done)| *done) {
            return "Done".into();
        }
        let status = if success { "Done" } else { "Failed" };
        let summary = if let Some(scope) = self.scopes.first() {
            Some(scope.count.message(false))
        } else {
            self.last
                .as_ref()
                .map(|(count, finished)| count.message(success && *finished))
        };
        match summary {
            Some(summary) => format!("{status} — {summary}"),
            None => status.to_owned(),
        }
    }
}

impl Active {
    fn render(&mut self) {
        let Some(render) = self.state.visible(Instant::now()) else {
            return;
        };
        if let Some(display) = &mut self.display {
            display.render(render);
        } else {
            let group = cliclack::multi_progress(&self.label);
            let indicator = render.indicator();
            let bar = match (indicator, render.counts) {
                (Indicator::Progress, Some((_, total))) => group.add(
                    cliclack::progress_bar(progress_value(total)).with_template(PROGRESS_TEMPLATE),
                ),
                _ => group.add(cliclack::spinner()),
            };
            let mut display = TerminalDisplay {
                group,
                bar,
                indicator: None,
            };
            display.render(render);
            self.display = Some(display);
        }
    }

    fn println(&self, message: impl Display) {
        if let Some(display) = &self.display {
            display.group.println(message);
        } else {
            let _ = cliclack::log::info(message);
        }
    }
}

impl Render {
    fn indicator(&self) -> Indicator {
        if self.show_bar && self.counts.is_some() {
            Indicator::Progress
        } else {
            Indicator::Spinner
        }
    }
}

impl TerminalDisplay {
    fn render(&mut self, render: Render) {
        let indicator = render.indicator();
        let changed = self.indicator != Some(indicator);
        if changed {
            match indicator {
                Indicator::Spinner => {
                    self.bar.clone().with_spinner_template();
                }
                Indicator::Progress => {
                    self.bar.clone().with_template(PROGRESS_TEMPLATE);
                }
            }
            self.indicator = Some(indicator);
        }
        if let Some((completed, total)) = render.counts {
            self.bar.set_length(progress_value(total));
            self.bar.set_position(progress_value(completed));
        }
        if changed {
            self.bar.start(render.message);
        } else {
            self.bar.set_message(render.message);
        }
    }
}

fn progress_value(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Identifies one counted stage reported to a [`ProgressReporter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StageId(u64);

impl StageId {
    fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Receives progress and diagnostics from engine work. Inject one with
/// [`crate::Engine::set_progress`]. Progress methods default to doing
/// nothing; `info` and `warning` default to writing to stderr.
///
/// Stages nest: a stage started while another is running is its child.
/// Methods may be called from worker threads. Text may contain untrusted
/// repository content (paths, parser messages); sanitize before rendering.
pub trait ProgressReporter: Send + Sync {
    /// A free-form status update, such as the file currently being indexed.
    /// The text may contain untrusted repository content.
    fn message(&self, _text: &str) {}
    /// A counted stage of `total` units started.
    fn stage_started(&self, _stage: StageId, _label: &str, _total: usize) {}
    /// A counted stage has now completed `completed` of `total` units.
    fn stage_advanced(&self, _stage: StageId, _completed: usize, _total: usize) {}
    /// A counted stage ended; `success` is false when it was abandoned.
    fn stage_finished(&self, _stage: StageId, _success: bool) {}
    /// An informational notice, such as a model download starting.
    fn info(&self, text: &str) {
        log(Level::Info, text);
    }
    /// A recoverable problem: skipped files, parse errors, an unavailable
    /// remote cache, provider retries.
    fn warning(&self, text: &str) {
        log(Level::Warning, text);
    }
}

/// The default reporter: discards progress; notices still go to stderr.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoProgress;

impl ProgressReporter for NoProgress {}

thread_local! {
    static CURRENT: RefCell<Option<Arc<dyn ProgressReporter>>> = const { RefCell::new(None) };
}

/// Route progress reported on this thread to `reporter` while `work` runs.
/// Engine entry points install their injected reporter here, so deeper code
/// (index construction, parsing) need not thread it through every call.
pub(crate) fn with_reporter<T>(
    reporter: &Arc<dyn ProgressReporter>,
    work: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<Arc<dyn ProgressReporter>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            CURRENT.with(|current| *current.borrow_mut() = previous);
        }
    }
    let previous = CURRENT.with(|current| current.borrow_mut().replace(reporter.clone()));
    let _restore = Restore(previous);
    work()
}

/// The reporter installed on this thread, if any. Capture it before spawning
/// workers and reinstall it there with [`inherit`].
pub(crate) fn current() -> Option<Arc<dyn ProgressReporter>> {
    CURRENT.with(|current| current.borrow().clone())
}

/// Run `work` under a reporter captured on another thread with [`current`].
pub(crate) fn inherit<T>(
    reporter: Option<&Arc<dyn ProgressReporter>>,
    work: impl FnOnce() -> T,
) -> T {
    match reporter {
        Some(reporter) => with_reporter(reporter, work),
        None => work(),
    }
}

/// The CLI's reporter: renders into the terminal display owned by [`spin`].
/// Silent when no spin owns the display (or stderr is not a terminal).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TerminalProgress;

impl TerminalProgress {
    pub(crate) fn shared() -> Arc<dyn ProgressReporter> {
        Arc::new(Self)
    }
}

impl ProgressReporter for TerminalProgress {
    fn message(&self, text: &str) {
        if let Some(active) = ACTIVE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            active.state.message(sanitize_terminal(text.to_owned()));
            active.render();
        }
    }

    fn stage_started(&self, stage: StageId, label: &str, total: usize) {
        if let Some(active) = ACTIVE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            active
                .state
                .push_stage(stage, sanitize_terminal(label.to_owned()), total);
            active.render();
        }
    }

    fn stage_advanced(&self, stage: StageId, completed: usize, _total: usize) {
        if let Some(active) = ACTIVE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            active.state.set_completed(&stage, completed);
            active.render();
        }
    }

    fn stage_finished(&self, stage: StageId, success: bool) {
        if let Some(active) = ACTIVE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            if success
                && let Some(scope) = active.state.scopes.first()
                && scope.id == stage
            {
                // A fast subsequent stage can otherwise replace the last
                // frame before the terminal ever displays its completion.
                active.println(scope.count.message(true));
            }
            active.state.end(&stage, success);
            active.render();
        }
    }
}

/// A counted stage token for the reporter current when it was created.
/// Only inc records completed work; finish and Drop never fill missing counts.
pub(crate) struct Progress {
    stage: Option<(Arc<dyn ProgressReporter>, StageId)>,
    completed: AtomicUsize,
    total: usize,
}

pub(crate) fn counted(label: impl Display, total: usize) -> Progress {
    let stage = current().map(|reporter| {
        let id = StageId::next();
        reporter.stage_started(id, &label.to_string(), total);
        (reporter, id)
    });
    Progress {
        stage,
        completed: AtomicUsize::new(0),
        total,
    }
}

impl Progress {
    pub(crate) fn inc(&self, delta: usize) {
        if let Some((reporter, id)) = &self.stage {
            let completed = self
                .completed
                .fetch_add(delta, Ordering::Relaxed)
                .saturating_add(delta)
                .min(self.total);
            reporter.stage_advanced(*id, completed, self.total);
        }
    }

    pub(crate) fn finish(mut self) {
        self.end(true);
    }

    fn end(&mut self, success: bool) {
        if let Some((reporter, id)) = self.stage.take() {
            reporter.stage_finished(id, success);
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.end(false);
    }
}

pub(crate) fn terminal() -> bool {
    io::stderr().is_terminal() && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
}

/// Owns a display only when no enclosing operation already owns it.
struct DisplayGuard(Option<(mpsc::Sender<()>, JoinHandle<()>)>);

impl DisplayGuard {
    fn start(label: &str) -> Self {
        if !terminal() {
            return Self(None);
        }
        let mut active = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
        if active.is_some() {
            return Self(None);
        }
        let id = Arc::new(());
        *active = Some(Active {
            id: id.clone(),
            label: label.to_owned(),
            display: None,
            state: ProgressState::new(label),
        });
        let (stop, receiver) = mpsc::channel();
        // Rendering must wake even while the main thread is blocked on I/O.
        let worker = thread::spawn(move || {
            while let Err(mpsc::RecvTimeoutError::Timeout) = receiver.recv_timeout(REFRESH_INTERVAL)
            {
                let mut active = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(active) = active.as_mut()
                    && Arc::ptr_eq(&active.id, &id)
                {
                    active.render();
                } else {
                    break;
                }
            }
        });
        Self(Some((stop, worker)))
    }

    fn finish(&mut self, success: bool) {
        let Some((stop, worker)) = self.0.take() else {
            return;
        };
        let _ = stop.send(());
        let mut active = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(active) = active.take()
            && let Some(display) = active.display
        {
            let message = active.state.completion(success);
            if success {
                display.bar.stop(message);
                display.group.stop();
            } else {
                display.bar.error(message);
                display.group.error("Failed");
            }
        }
        drop(active);
        let _ = worker.join();
    }
}

impl Drop for DisplayGuard {
    fn drop(&mut self) {
        // Also stop the tick thread if an operation unwinds.
        self.finish(false);
    }
}

pub(crate) fn spin<T>(label: &str, work: impl FnOnce() -> Result<T>) -> Result<T> {
    let mut display = DisplayGuard::start(label);
    let result = match current() {
        Some(_) => work(),
        None => with_reporter(&TerminalProgress::shared(), work),
    };
    display.finish(result.is_ok());
    result
}

/// Messages can embed repository file paths and other untrusted text; on a
/// terminal, control characters (ESC above all) must not reach it raw.
fn sanitize_terminal(message: String) -> String {
    message
        .chars()
        .filter(|ch| *ch == '\n' || *ch == '\t' || !ch.is_control())
        .collect()
}

/// Report a status message to the current reporter, if any.
pub(crate) fn progress(message: impl Display) {
    if let Some(reporter) = current() {
        reporter.message(&message.to_string());
    }
}

enum Level {
    Info,
    Warning,
    Error,
}

fn log(level: Level, message: impl Display) {
    let message = message.to_string();
    let active = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(display) = active.as_ref().and_then(|active| active.display.as_ref()) {
        // MultiProgress serializes notices from HTTP workers above the animation.
        let message = sanitize_terminal(message);
        display.group.println(&message);
    } else if terminal() {
        let message = sanitize_terminal(message);
        let _ = match level {
            Level::Info => cliclack::log::info(&message),
            Level::Warning => cliclack::log::warning(&message),
            Level::Error => cliclack::log::error(&message),
        };
    } else {
        let _ = writeln!(io::stderr().lock(), "{message}");
    }
}

/// Report a notice to the current reporter, or to stderr without one.
pub(crate) fn info(message: impl Display) {
    match current() {
        Some(reporter) => reporter.info(&message.to_string()),
        None => log(Level::Info, message),
    }
}

/// Report a warning to the current reporter, or to stderr without one.
pub(crate) fn warning(message: impl Display) {
    match current() {
        Some(reporter) => reporter.warning(&message.to_string()),
        None => log(Level::Warning, message),
    }
}

pub(crate) fn error(message: impl Display) {
    log(Level::Error, message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Notices(Mutex<Vec<String>>);

    impl ProgressReporter for Notices {
        fn info(&self, text: &str) {
            self.0.lock().unwrap().push(format!("info {text}"));
        }

        fn warning(&self, text: &str) {
            self.0.lock().unwrap().push(format!("warning {text}"));
        }
    }

    #[test]
    fn notices_route_to_the_installed_reporter_and_restore_on_exit() {
        let outer = Arc::new(Notices::default());
        let inner = Arc::new(Notices::default());
        let (outer_dyn, inner_dyn): (Arc<dyn ProgressReporter>, Arc<dyn ProgressReporter>) =
            (outer.clone(), inner.clone());
        with_reporter(&outer_dyn, || {
            warning("first");
            with_reporter(&inner_dyn, || info("nested"));
            warning(format_args!("after {}", 1));
        });
        assert_eq!(
            *outer.0.lock().unwrap(),
            ["warning first", "warning after 1"]
        );
        assert_eq!(*inner.0.lock().unwrap(), ["info nested"]);
        assert!(current().is_none());
    }

    #[test]
    fn inherit_carries_a_captured_reporter_to_worker_threads() {
        let notices = Arc::new(Notices::default());
        let reporter: Arc<dyn ProgressReporter> = notices.clone();
        with_reporter(&reporter, || {
            let captured = current();
            thread::scope(|scope| {
                scope.spawn(|| warning("uninherited")).join().unwrap();
                scope
                    .spawn(|| inherit(captured.as_ref(), || warning("inherited")))
                    .join()
                    .unwrap();
            });
        });
        assert_eq!(*notices.0.lock().unwrap(), ["warning inherited"]);
    }

    fn visible_message(state: &mut ProgressState, now: Instant) -> Option<String> {
        state.visible(now).map(|render| render.message)
    }

    #[test]
    fn transient_text_waits_for_each_replacement_to_settle() {
        let mut state = ProgressState::new("Opening index");
        let start = state.spinner.since;
        assert_eq!(
            visible_message(&mut state, start + TEXT_DELAY - Duration::from_nanos(1)),
            None
        );
        assert_eq!(
            visible_message(&mut state, start + TEXT_DELAY),
            Some("Opening index".into())
        );

        state.message("Scanning files".into());
        let start = state.spinner.since;
        assert_eq!(
            visible_message(&mut state, start),
            Some("Opening index".into())
        );
        // Repeated reports of the same stage do not postpone its appearance.
        state.message("Scanning files".into());
        assert_eq!(state.spinner.since, start);
        assert_eq!(
            visible_message(&mut state, start + TEXT_DELAY),
            Some("Scanning files".into())
        );

        state.message("Short-lived stage".into());
        state.message("Saving index".into());
        let start = state.spinner.since;
        assert_eq!(
            visible_message(&mut state, start),
            Some("Scanning files".into())
        );
        assert_eq!(
            visible_message(&mut state, start + TEXT_DELAY),
            Some("Saving index".into())
        );
    }

    #[test]
    fn bars_require_more_than_one_second_of_expected_work() {
        let mut state = ProgressState::new("Indexing");
        let files = state.push("Files".into(), 5);
        let start = state.scopes[0].since;
        state.inc(&files, 1);
        assert_eq!(
            visible_message(&mut state, start + Duration::from_millis(199)),
            None
        );
        // 200 ms for one of five items predicts exactly one second: no bar.
        let render = state.visible(start + TEXT_DELAY).unwrap();
        assert_eq!(render.message, "Files 20% (1/5)");
        assert!(!render.show_bar);
        let render = state.visible(start + Duration::from_millis(201)).unwrap();
        assert_eq!(render.message, "Files 20% (1/5)");
        assert!(render.show_bar);
        // A revised estimate must not make an already-visible bar flicker off.
        state.inc(&files, 3);
        assert!(
            state
                .visible(start + Duration::from_millis(202))
                .unwrap()
                .show_bar
        );
    }

    #[test]
    fn slow_estimates_still_observe_the_minimum_text_delay() {
        let mut state = ProgressState::new("Indexing");
        let files = state.push("Files".into(), 100);
        let start = state.scopes[0].since;
        state.inc(&files, 1);
        assert_eq!(
            visible_message(&mut state, start + Duration::from_millis(199)),
            None
        );
        assert!(state.visible(start + TEXT_DELAY).unwrap().show_bar);
    }

    #[test]
    fn unsampled_work_waits_one_second_and_finished_work_never_introduces_a_bar() {
        let mut state = ProgressState::new("Indexing");
        state.push("Files".into(), 4);
        let start = state.scopes[0].since;
        let render = state.visible(start + BAR_THRESHOLD).unwrap();
        assert_eq!(render.message, "Files 0% (0/4)");
        assert!(!render.show_bar);
        assert!(
            state
                .visible(start + BAR_THRESHOLD + REFRESH_INTERVAL)
                .unwrap()
                .show_bar
        );

        let mut state = ProgressState::new("Indexing");
        let files = state.push("Files".into(), 4);
        let start = state.scopes[0].since;
        state.inc(&files, 3);
        let render = state.visible(start + TEXT_DELAY).unwrap();
        assert_eq!(render.message, "Files 75% (3/4)");
        assert!(!render.show_bar);
        state.inc(&files, 1);
        let render = state.visible(start + BAR_THRESHOLD * 2).unwrap();
        assert_eq!(render.message, "Files 100% (4/4)");
        assert!(!render.show_bar);
    }

    #[test]
    fn fast_children_and_details_do_not_replace_visible_parent() {
        let mut state = ProgressState::new("Indexing");
        let files = state.push("Files".into(), 10);
        state.scopes[0].since = Instant::now() - TEXT_DELAY;
        let parent = state.visible(Instant::now()).unwrap().message;
        state.message("short-lived.rs".into());
        let child = state.push("Embeddings".into(), 1);
        assert_eq!(
            visible_message(&mut state, Instant::now()),
            Some(parent.clone())
        );
        state.end(&child, true);
        state.message("long-lived.rs".into());
        let start = state.scopes[0].detail.as_ref().unwrap().since;
        assert_eq!(visible_message(&mut state, start), Some(parent));
        assert!(
            state
                .visible(start + TEXT_DELAY)
                .unwrap()
                .message
                .contains(" — long-lived.rs")
        );

        state.end(&files, true);
        let start = state.spinner.since;
        assert_eq!(visible_message(&mut state, start), None);
        state.message("Saving snapshot".into());
        let start = state.spinner.since;
        assert_eq!(visible_message(&mut state, start), None);
        assert_eq!(
            visible_message(&mut state, start + TEXT_DELAY),
            Some("Saving snapshot".into())
        );
    }

    #[test]
    fn native_bar_and_numeric_counts_share_one_snapshot() {
        let mut state = ProgressState::new("Indexing");
        let files = state.push("Files".into(), 2);
        state.inc(&files, 1);
        let embeddings = state.push("Embeddings".into(), 5);
        let visible_since = Instant::now() - BAR_THRESHOLD - REFRESH_INTERVAL;
        state.scopes[0].since = visible_since;
        state.scopes[1].since = visible_since;
        let render = state.visible(Instant::now()).unwrap();
        assert_eq!(render.message, "Files 50% (1/2)\n│    Embeddings 0% (0/5)");
        assert_eq!(render.counts, Some((0, 5)));
        assert!(matches!(render.indicator(), Indicator::Progress));
        state.inc(&embeddings, 2);
        let render = state.visible(Instant::now()).unwrap();
        assert_eq!(render.message, "Files 50% (1/2)\n│    Embeddings 40% (2/5)");
        assert_eq!(render.counts, Some((2, 5)));
        state.inc(&embeddings, 3);
        let render = state.visible(Instant::now()).unwrap();
        assert_eq!(
            render.message,
            "Files 50% (1/2)\n│    Embeddings 100% (5/5)"
        );
        assert_eq!(render.counts, Some((5, 5)));
        state.end(&embeddings, true);
        assert_eq!(state.render().message, "Files 50% (1/2)");
        assert!(PROGRESS_TEMPLATE.contains("{bar:"));
        assert!(PROGRESS_TEMPLATE.contains("{eta}"));
    }

    #[test]
    fn unknown_size_stage_does_not_reuse_previous_stage_completion() {
        let mut state = ProgressState::new("Searching");
        let queries = state.push("Queries".into(), 1);
        state.inc(&queries, 1);
        state.end(&queries, true);
        state.message("Generating explanation".into());
        assert_eq!(state.render().message, "Generating explanation");
        assert_eq!(state.completion(false), "Failed");
    }

    #[test]
    fn counted_lifecycle_preserves_final_percentage_and_counts() {
        let mut state = ProgressState::new("Indexing");
        assert_eq!(state.render().counts, None);
        let files = state.push("Files".into(), 4);
        assert_eq!(state.render().message, "Files 0% (0/4)");
        assert_eq!(state.render().counts, Some((0, 4)));
        state.inc(&files, 1);
        assert_eq!(state.render().message, "Files 25% (1/4)");
        state.inc(&files, 3);
        assert_eq!(state.render().message, "Files 100% (4/4)");
        state.end(&files, true);
        assert_eq!(state.render().counts, None);
        assert_eq!(state.render().message, "Indexing");
        // The completed outer scope's log preserves counts; the final command
        // row must not duplicate that summary.
        assert_eq!(
            state.last.as_ref().unwrap().0.message(true),
            "Files 100% (4/4)"
        );
        assert_eq!(state.completion(true), "Done");
    }

    #[test]
    fn nested_scopes_restore_parent_counts_label_and_detail() {
        let mut state = ProgressState::new("Indexing");
        let files = state.push("Files".into(), 3);
        state.inc(&files, 1);
        state.message("src/ui.rs".into());
        let embeddings = state.push("Embeddings".into(), 10);
        state.inc(&embeddings, 5);
        state.message("Retrying request".into());
        assert_eq!(
            state.render().message,
            "Embeddings 50% (5/10) — Retrying request"
        );
        // An update to a suspended parent must not advance the visible child.
        state.inc(&files, 1);
        assert_eq!(state.render().counts, Some((5, 10)));
        state.inc(&embeddings, 5);
        state.end(&embeddings, true);
        assert_eq!(state.render().message, "Files 66% (2/3) — src/ui.rs");
        assert_eq!(state.render().counts, Some((2, 3)));
        state.inc(&files, 1);
        state.end(&files, true);
        assert_eq!(
            state.last.as_ref().unwrap().0.message(true),
            "Files 100% (3/3)"
        );
        assert_eq!(state.completion(true), "Done");
    }

    #[test]
    fn failed_scopes_restore_parent_without_fabricating_work() {
        let mut state = ProgressState::new("Indexing");
        let files = state.push("Files".into(), 4);
        state.inc(&files, 1);
        let embeddings = state.push("Embeddings".into(), 8);
        state.inc(&embeddings, 3);
        // Drop uses this same unsuccessful end transition, including unwind.
        state.end(&embeddings, false);
        assert_eq!(state.render().message, "Files 25% (1/4)");
        state.end(&files, false);
        assert_eq!(state.completion(false), "Failed — Files 25% (1/4)");
        assert!(state.scopes.is_empty());
    }

    #[test]
    fn finish_does_not_invent_unreported_completions() {
        let mut state = ProgressState::new("Indexing");
        let files = state.push("Files".into(), 4);
        state.inc(&files, 1);
        assert_eq!(state.completion(false), "Failed — Files 25% (1/4)");
        state.end(&files, true);
        assert_eq!(
            state.last.as_ref().unwrap().0.message(true),
            "Files 25% (1/4)"
        );
        assert_eq!(state.completion(true), "Done");
    }

    #[test]
    fn empty_work_is_complete_only_after_success() {
        let mut state = ProgressState::new("Indexing");
        let files = state.push("Files".into(), 0);
        state.inc(&files, usize::MAX);
        assert_eq!(state.render().message, "Files 0% (0/0)");
        assert_eq!(state.completion(false), "Failed — Files 0% (0/0)");
        state.end(&files, false);
        assert_eq!(state.completion(true), "Done — Files 0% (0/0)");
        let files = state.push("Files".into(), 0);
        state.end(&files, true);
        assert_eq!(
            state.last.as_ref().unwrap().0.message(true),
            "Files 100% (0/0)"
        );
        assert_eq!(state.completion(true), "Done");
        assert_eq!(state.completion(false), "Failed — Files 0% (0/0)");
    }

    #[test]
    fn increments_saturate_and_percentages_do_not_overflow() {
        let mut state = ProgressState::new("Indexing");
        let files = state.push("Files".into(), usize::MAX);
        state.inc(&files, usize::MAX - 1);
        assert!(state.render().message.contains("99%"));
        state.inc(&files, usize::MAX);
        assert_eq!(state.render().counts, Some((usize::MAX, usize::MAX)));
        assert!(state.render().message.contains("100%"));
    }

    #[test]
    fn repeated_scopes_and_stale_handles_do_not_accumulate_state() {
        let mut state = ProgressState::new("Indexing");
        let stale = state.push("Old task".into(), 1);
        state.end(&stale, true);
        let files = state.push("Files".into(), 100);
        for _ in 0..100 {
            let child = state.push("Embeddings".into(), 1);
            state.inc(&child, 1);
            state.end(&child, true);
            state.inc(&files, 1);
            state.inc(&stale, usize::MAX);
            state.end(&stale, false);
            assert_eq!(state.scopes.len(), 1);
            assert!(state.last.is_none());
        }
        state.end(&files, true);
        assert!(state.scopes.is_empty());
        assert_eq!(
            state.last.as_ref().unwrap().0.message(true),
            "Files 100% (100/100)"
        );
        assert_eq!(state.completion(true), "Done");

        let mut next_display = ProgressState::new("Next operation");
        let next = next_display.push("New task".into(), 10);
        next_display.inc(&files, 10);
        next_display.end(&files, false);
        assert_eq!(next_display.render().message, "New task 0% (0/10)");
        next_display.end(&next, true);
        next_display.message("Waiting for provider".into());
        assert_eq!(next_display.render().message, "Waiting for provider");
        assert_eq!(next_display.render().counts, None);
    }
}
