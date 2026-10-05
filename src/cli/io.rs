//! Stdout error boundary and shared JSON writing.

use anyhow::Result;
use std::io::{self, Write};

#[derive(Debug)]
struct ClosedStdout;

impl std::fmt::Display for ClosedStdout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stdout pipe closed")
    }
}

impl std::error::Error for ClosedStdout {}

#[derive(Debug)]
struct StdoutBrokenPipe(io::Error);

impl std::fmt::Display for StdoutBrokenPipe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for StdoutBrokenPipe {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Both io::Error and serde_json::Error forward the inner error's source.
        // Keep a stdout-specific marker visible through either wrapper.
        Some(&ClosedStdout)
    }
}

pub(super) struct StdoutWriter<W>(W);

fn stdout_error(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::BrokenPipe {
        io::Error::new(io::ErrorKind::BrokenPipe, StdoutBrokenPipe(error))
    } else {
        error
    }
}

impl<W: Write> Write for StdoutWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes).map_err(stdout_error)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush().map_err(stdout_error)
    }
}

pub(super) fn with_stdout<W: Write>(
    writer: W,
    run: impl FnOnce(&mut StdoutWriter<W>) -> Result<()>,
) -> Result<()> {
    let mut out = StdoutWriter(writer);
    let result = run(&mut out).and_then(|()| out.flush().map_err(Into::into));
    match result {
        Err(error) if error.chain().any(|cause| cause.is::<ClosedStdout>()) => Ok(()),
        result => result,
    }
}

pub(super) fn print_json(out: &mut impl Write, value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer_pretty(&mut *out, value)?;
    writeln!(out)?;
    Ok(())
}

#[cfg(test)]
#[path = "io_tests.rs"]
mod tests;
