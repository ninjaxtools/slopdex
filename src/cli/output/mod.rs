//! Result presentation shared by CLI commands and describe context generation.

mod clusters;
mod cross;
mod map;
mod render;
mod search;

pub(in crate::cli) use cross::{CrossOutput, print_cross};
pub(in crate::cli) use map::print_map;
pub(in crate::cli) use render::Presentation;
pub(crate) use search::describe_search_context;
pub(in crate::cli) use search::print_search;

use super::{args::Format, io::print_json};
use anyhow::Result;
use serde_json::Value;
use std::io::Write;

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn number(value: &Value, key: &str) -> f64 {
    value[key].as_f64().unwrap_or(0.0)
}

pub(in crate::cli) fn print_errors(
    out: &mut impl Write,
    errors: &[Value],
    format: Format,
) -> Result<()> {
    if format == Format::Json {
        return print_json(out, &errors);
    }
    if errors.is_empty() {
        writeln!(out, "No indexing errors.")?;
    }
    for error in errors {
        let path = error["path"]
            .as_str()
            .or_else(|| error["filePath"].as_str())
            .unwrap_or("(unknown file)");
        write!(out, "{path}")?;
        if let Some(line) = error["startLine"].as_u64() {
            write!(out, ":{line}")?;
        }
        if let Some(column) = error["startColumn"].as_u64() {
            write!(out, ":{column}")?;
        }
        if let Some(name) = error["qualifiedName"]
            .as_str()
            .or_else(|| error["name"].as_str())
        {
            write!(out, " :: {name}")?;
        }
        writeln!(out, "  {}", text(error, "message"))?;
    }
    Ok(())
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
