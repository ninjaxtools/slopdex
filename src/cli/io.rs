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

pub(super) struct StdoutWriter<W> {
    inner: W,
    /// Strip terminal escape initiators from untrusted source text when the
    /// destination is a real terminal (JSON is unaffected: it escapes them).
    sanitize: bool,
}

fn stdout_error(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::BrokenPipe {
        io::Error::new(io::ErrorKind::BrokenPipe, StdoutBrokenPipe(error))
    } else {
        error
    }
}

impl<W: Write> Write for StdoutWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.sanitize {
            return self.inner.write(bytes).map_err(stdout_error);
        }
        // Byte-wise filtering is chunk-boundary safe: removing ESC and the
        // other C0 controls (keeping newline and tab) leaves any sequence
        // body as inert literal text.
        let clean: Vec<u8> = bytes
            .iter()
            .copied()
            .filter(|byte| *byte == b'\n' || *byte == b'\t' || (*byte >= 0x20 && *byte != 0x7f))
            .collect();
        self.inner.write_all(&clean).map_err(stdout_error)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush().map_err(stdout_error)
    }
}

pub(super) fn with_stdout<W: Write>(
    writer: W,
    sanitize: bool,
    run: impl FnOnce(&mut StdoutWriter<W>) -> Result<()>,
) -> Result<()> {
    let mut out = StdoutWriter {
        inner: writer,
        sanitize,
    };
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

/// Streams a pretty-printed JSON array element by element, byte-identical
/// to `print_json` of the collected rows, without retaining them.
pub(super) struct JsonArrayWriter {
    started: bool,
    first: bool,
}

impl JsonArrayWriter {
    pub(super) fn new() -> Self {
        Self {
            started: false,
            first: true,
        }
    }

    pub(super) fn row(&mut self, out: &mut impl Write, value: &serde_json::Value) -> Result<()> {
        if !self.started {
            writeln!(out, "[")?;
            self.started = true;
        }
        if !self.first {
            writeln!(out, ",")?;
        }
        self.first = false;
        // Pretty-printed elements nest one indent level inside the array;
        // structural newlines are the only raw ones in the serialization.
        let text = serde_json::to_string_pretty(value)?;
        let mut lines = text.lines();
        if let Some(first) = lines.next() {
            write!(out, "  {first}")?;
            for line in lines {
                write!(out, "\n  {line}")?;
            }
        }
        Ok(())
    }

    pub(super) fn finish(&mut self, out: &mut impl Write) -> Result<()> {
        if self.started {
            writeln!(out, "\n]")?;
        } else {
            writeln!(out, "[]")?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "io_tests.rs"]
mod tests;
