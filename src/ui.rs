//! Terminal presentation shared by commands and background provider work.
//! Results stay on stdout; interactive rendering is confined to terminal stderr.

use std::{
    fmt::Display,
    io::{self, IsTerminal, Write},
    sync::Mutex,
};

use anyhow::Result;

struct Active {
    group: cliclack::MultiProgress,
    bar: cliclack::ProgressBar,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

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
        *active = Some(Active { group, bar });
        Self(true)
    }

    fn finish(&mut self, success: bool) {
        if !self.0 {
            return;
        }
        self.0 = false;
        let mut active = ACTIVE.lock().unwrap();
        if let Some(display) = active.take() {
            if success {
                display.bar.stop("Done");
                display.group.stop();
            } else {
                display.bar.error("Failed");
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
    if let Some(active) = ACTIVE.lock().unwrap().as_ref() {
        active.bar.set_message(message);
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
