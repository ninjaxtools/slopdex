//! Terminal presentation shared by commands and background provider work.
//! Results stay on stdout; interactive rendering is confined to terminal stderr.

use std::{
    fmt::Display,
    io::{self, IsTerminal, Write},
    sync::{Arc, Mutex},
};

use anyhow::Result;

struct Active {
    group: cliclack::MultiProgress,
    bar: cliclack::ProgressBar,
    state: ProgressState,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

// Tokens belong to a scope, rather than a stack index or display generation:
// a late increment/drop cannot affect another scope or a later CLI operation.
type ScopeId = Arc<()>;

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
    detail: Option<String>,
}

impl Scope {
    fn render(&self) -> Render {
        let mut message = self.count.message(false);
        if let Some(detail) = &self.detail {
            message.push_str(" — ");
            message.push_str(detail);
        }
        Render {
            message,
            counts: Some((self.count.completed, self.count.total)),
        }
    }
}

struct ProgressState {
    spinner: String,
    scopes: Vec<Scope>,
    last: Option<(Count, bool)>,
}

struct Render {
    message: String,
    // None selects the unknown-size spinner template.
    counts: Option<(usize, usize)>,
}

impl Render {
    fn display(&self) -> String {
        match self.counts {
            Some((completed, total)) => {
                let filled = if total == 0 {
                    0
                } else {
                    (completed as u128 * 20 / total as u128) as usize
                };
                format!(
                    "{} [{}{}]",
                    self.message,
                    "■".repeat(filled),
                    "□".repeat(20 - filled)
                )
            }
            None => self.message.clone(),
        }
    }
}

impl ProgressState {
    fn new(label: &str) -> Self {
        Self {
            spinner: label.to_owned(),
            scopes: Vec::new(),
            last: None,
        }
    }

    fn push(&mut self, label: String, total: usize) -> ScopeId {
        let id = Arc::new(());
        if self.scopes.is_empty() {
            self.last = None;
        }
        self.scopes.push(Scope {
            id: id.clone(),
            count: Count {
                label,
                completed: 0,
                total,
            },
            detail: None,
        });
        id
    }

    fn inc(&mut self, id: &ScopeId, delta: usize) {
        if let Some(scope) = self.scopes.iter_mut().find(|s| Arc::ptr_eq(&s.id, id)) {
            scope.count.completed = scope
                .count
                .completed
                .saturating_add(delta)
                .min(scope.count.total);
        }
    }

    fn end(&mut self, id: &ScopeId, success: bool) {
        if let Some(index) = self.scopes.iter().position(|s| Arc::ptr_eq(&s.id, id)) {
            let scope = self.scopes.remove(index);
            // Only retain the outer task's summary. Nested completion restores
            // its parent without printing or accumulating a row per child.
            if index == 0 {
                self.last = Some((scope.count, success));
            }
        }
    }

    fn message(&mut self, message: String) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.detail = Some(message);
        } else {
            self.spinner = message;
            self.last = None;
        }
    }

    fn render(&self) -> Render {
        if let Some(scope) = self.scopes.last() {
            scope.render()
        } else {
            Render {
                message: self.spinner.clone(),
                counts: None,
            }
        }
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
        let message = if self.state.scopes.is_empty() {
            self.state.render().display()
        } else {
            // Keep parent totals visible while nested work is running. A single
            // reusable multiline row avoids accumulating a row for every file.
            self.state
                .scopes
                .iter()
                .map(|s| s.render().display())
                .collect::<Vec<_>>()
                .join("\n│    ")
        };
        // Update bar, percentage, and counts atomically. Separate native length,
        // position, and message setters can draw mismatched nested-task states.
        self.bar.set_message(message);
    }
}

/// A counted scope is silent unless an enclosing CLI spin owns the display.
/// Only inc records completed work; finish and Drop never fill missing counts.
pub(crate) struct Progress {
    id: Option<ScopeId>,
}

pub(crate) fn counted(label: impl Display, total: usize) -> Progress {
    let mut active = ACTIVE.lock().unwrap();
    let id = active.as_mut().map(|active| {
        let id = active.state.push(label.to_string(), total);
        active.render();
        id
    });
    Progress { id }
}

impl Progress {
    pub(crate) fn inc(&self, delta: usize) {
        if let Some(id) = &self.id
            && let Some(active) = ACTIVE.lock().unwrap().as_mut()
        {
            active.state.inc(id, delta);
            active.render();
        }
    }

    pub(crate) fn finish(mut self) {
        self.end(true);
    }

    fn end(&mut self, success: bool) {
        if let Some(id) = self.id.take()
            && let Some(active) = ACTIVE.lock().unwrap().as_mut()
        {
            if success
                && let Some(scope) = active.state.scopes.first()
                && Arc::ptr_eq(&scope.id, &id)
            {
                // A fast subsequent stage can otherwise replace the last
                // frame before the terminal ever displays its completion.
                active.group.println(scope.count.message(true));
            }
            active.state.end(&id, success);
            active.render();
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
struct DisplayGuard(bool);

impl DisplayGuard {
    fn start(label: &str) -> Self {
        if !terminal() {
            return Self(false);
        }
        let mut active = ACTIVE.lock().unwrap();
        if active.is_some() {
            return Self(false);
        }
        let group = cliclack::multi_progress(label);
        let bar = group.add(cliclack::spinner());
        bar.start(label);
        *active = Some(Active {
            group,
            bar,
            state: ProgressState::new(label),
        });
        Self(true)
    }

    fn finish(&mut self, success: bool) {
        if !self.0 {
            return;
        }
        self.0 = false;
        let mut active = ACTIVE.lock().unwrap();
        if let Some(display) = active.take() {
            let message = display.state.completion(success);
            if success {
                display.bar.stop(message);
                display.group.stop();
            } else {
                display.bar.error(message);
                display.group.error("Failed");
            }
        }
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
    let result = work();
    display.finish(result.is_ok());
    result
}

/// Engine/library calls remain silent unless a CLI operation owns the display.
pub(crate) fn progress(message: impl Display) {
    if let Some(active) = ACTIVE.lock().unwrap().as_mut() {
        active.state.message(message.to_string());
        active.render();
    }
}

enum Level {
    Info,
    Warning,
    Error,
}

fn log(level: Level, message: impl Display) {
    let message = message.to_string();
    let active = ACTIVE.lock().unwrap();
    if let Some(active) = active.as_ref() {
        // MultiProgress serializes notices from HTTP workers above the animation.
        active.group.println(&message);
    } else if terminal() {
        let _ = match level {
            Level::Info => cliclack::log::info(&message),
            Level::Warning => cliclack::log::warning(&message),
            Level::Error => cliclack::log::error(&message),
        };
    } else {
        let _ = writeln!(io::stderr().lock(), "{message}");
    }
}

pub(crate) fn info(message: impl Display) {
    log(Level::Info, message);
}

pub(crate) fn warning(message: impl Display) {
    log(Level::Warning, message);
}

pub(crate) fn error(message: impl Display) {
    log(Level::Error, message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphical_bar_and_numeric_counts_share_one_snapshot() {
        let mut state = ProgressState::new("Indexing");
        let files = state.push("Files".into(), 2);
        state.inc(&files, 1);
        let embeddings = state.push("Embeddings".into(), 5);
        assert_eq!(
            state.render().display(),
            "Embeddings 0% (0/5) [□□□□□□□□□□□□□□□□□□□□]"
        );
        state.inc(&embeddings, 2);
        assert_eq!(
            state.render().display(),
            "Embeddings 40% (2/5) [■■■■■■■■□□□□□□□□□□□□]"
        );
        state.inc(&embeddings, 3);
        assert_eq!(
            state.render().display(),
            "Embeddings 100% (5/5) [■■■■■■■■■■■■■■■■■■■■]"
        );
        state.end(&embeddings, true);
        assert_eq!(
            state.render().display(),
            "Files 50% (1/2) [■■■■■■■■■■□□□□□□□□□□]"
        );
    }

    #[test]
    fn unknown_size_stage_does_not_reuse_previous_stage_completion() {
        let mut state = ProgressState::new("Searching");
        let queries = state.push("Queries".into(), 1);
        state.inc(&queries, 1);
        state.end(&queries, true);
        state.message("Generating explanation".into());
        assert_eq!(state.render().display(), "Generating explanation");
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
