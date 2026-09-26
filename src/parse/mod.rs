//! Callable extraction and heading-aware Markdown chunking.
//!
//! Locations are one-based; end columns are exclusive, in UTF-8 bytes (the
//! tree-sitter convention). Syntax errors are recoverable diagnostics, while
//! parser initialization/failure is returned as an error.

mod code;
mod markdown;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Callable {
    pub language: String,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub signature: Option<String>,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub line_count: usize,
    pub source: String,
    pub source_hash: String,
    pub embedding_input: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownChunk {
    pub heading_path: Vec<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
    pub source_hash: String,
    pub embedding_input: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub message: String,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ParsedFile {
    pub callables: Vec<Callable>,
    pub chunks: Vec<MarkdownChunk>,
    pub errors: Vec<Diagnostic>,
}

pub fn language_for_path(path: &str) -> Option<&'static str> {
    let file = path.rsplit(['/', '\\']).next()?;
    let (stem, extension) = file.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    Some(match extension.to_ascii_lowercase().as_str() {
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "jsx",
        "py" | "pyw" => "python",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "md" | "markdown" => "markdown",
        _ => return None,
    })
}

pub fn parse(path: &str, source: &str) -> Result<ParsedFile> {
    match language_for_path(path) {
        Some("markdown") => Ok(ParsedFile {
            chunks: markdown::parse(source),
            ..ParsedFile::default()
        }),
        Some(language) => code::parse(language, path, source),
        None => Ok(ParsedFile::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_extensions_and_unsupported_files() {
        for (ext, language) in [
            ("ts", "typescript"),
            ("mts", "typescript"),
            ("cts", "typescript"),
            ("tsx", "tsx"),
            ("js", "javascript"),
            ("mjs", "javascript"),
            ("cjs", "javascript"),
            ("jsx", "jsx"),
            ("py", "python"),
            ("pyw", "python"),
            ("rs", "rust"),
            ("go", "go"),
            ("java", "java"),
            ("c", "c"),
            ("h", "c"),
            ("md", "markdown"),
            ("markdown", "markdown"),
        ] {
            assert_eq!(
                language_for_path(&format!("dir/source.{}", ext.to_uppercase())),
                Some(language)
            );
        }
        for path in [
            "file.cpp",
            "file.txt",
            ".ts",
            "dir.ts/file",
            "file",
            "file.",
        ] {
            assert_eq!(language_for_path(path), None, "{path}");
            assert!(
                parse(path, "function ignored() {}")
                    .unwrap()
                    .callables
                    .is_empty()
            );
        }
        assert_eq!(language_for_path("C:\\src\\source.TS"), Some("typescript"));
    }
}
