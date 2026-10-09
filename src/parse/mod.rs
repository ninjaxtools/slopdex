//! Callable extraction and heading-aware Markdown chunking.
//!
//! Locations are one-based; end columns are exclusive, in UTF-8 bytes (the
//! tree-sitter convention). Syntax errors are recoverable diagnostics, while
//! parser initialization/failure is returned as an error.

mod code;
mod data;
mod descriptions;
mod imports;
mod markdown;
mod shell;
mod structure;
mod syntax;

pub use structure::{CallSite, FileStructure, ImportBinding, StructureNode};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::formats::FormatGroup;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Callable {
    pub language: String,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    /// Source comment prose and, for Python, a leading constant docstring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
    /// The first contiguous comment group, after any leading blank lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub callables: Vec<Callable>,
    pub chunks: Vec<MarkdownChunk>,
    pub errors: Vec<Diagnostic>,
    #[serde(default)]
    pub structure: FileStructure,
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
        "json" => "json",
        "tf" | "tfvars" | "hcl" => "terraform",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" | "svg" | "xsd" | "xsl" | "xslt" => "xml",
        "html" | "htm" => "html",
        "css" => "css",
        "sh" | "bash" | "zsh" => "bash",
        _ => return None,
    })
}

pub fn parse(path: &str, source: &str) -> Result<ParsedFile> {
    let mut descriptions = descriptions::SourceDescriptions::default();
    let mut parsed = match language_for_path(path) {
        Some(language) => match FormatGroup::for_language(language) {
            Some(FormatGroup::Docs) => markdown::parse(source, &mut descriptions),
            Some(FormatGroup::Config | FormatGroup::Markup) => {
                data::parse(language, path, source, &mut descriptions)
            }
            Some(FormatGroup::Code) => code::parse(language, path, source, &mut descriptions),
            None => Ok(ParsedFile::default()),
        },
        None => Ok(ParsedFile::default()),
    }?;
    descriptions.apply(source, &mut parsed);
    Ok(parsed)
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
            ("sh", "bash"),
            ("bash", "bash"),
            ("zsh", "bash"),
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

    #[test]
    fn format_groups_dispatch_to_their_parsers() {
        let code = parse("source.rs", "fn example() {}\n").unwrap();
        assert_eq!(code.callables.len(), 1);
        assert!(code.chunks.is_empty());
        assert!(code.errors.is_empty());
        for (path, source) in [
            ("readme.md", "# Overview\n\nA useful guide.\n"),
            ("settings.json", "{\"port\": 80}\n"),
            ("settings.yaml", "port: 80\n"),
            ("settings.toml", "port = 80\n"),
            ("settings.tf", "variable \"port\" { default = 80 }\n"),
            ("page.html", "<main>Hello</main>\n"),
            ("page.xml", "<settings><port>80</port></settings>\n"),
            ("page.css", ".main { color: red; }\n"),
        ] {
            let parsed = parse(path, source).unwrap();
            assert!(parsed.callables.is_empty(), "{path}");
            assert!(!parsed.chunks.is_empty(), "{path}");
            assert!(!parsed.structure.nodes.is_empty(), "{path}");
            assert!(parsed.errors.is_empty(), "{path}: {:?}", parsed.errors);
        }
    }

    #[test]
    fn empty_whitespace_and_comment_only_inputs_have_no_search_entries() {
        for (path, comment) in [
            ("empty.ts", "// function fake() {}"),
            ("empty.tsx", "/* const Fake = () => <div />; */"),
            ("empty.js", "/* function fake() {} */"),
            ("empty.jsx", "// const Fake = () => <div />;"),
            ("empty.py", "# def fake(): pass"),
            ("empty.rs", "// fn fake() {}"),
            ("empty.go", "// func fake() {}"),
            ("empty.java", "/* class Fake { void fake() {} } */"),
            ("empty.c", "/* int fake(void) { return 0; } */"),
            ("empty.sh", "# function fake() { :; }"),
            ("empty.md", "<!--\n# Fake\n```\n-->"),
        ] {
            for source in ["", " \t\r\n\n", comment] {
                let parsed = parse(path, source).unwrap();
                assert!(parsed.callables.is_empty(), "{path}: {source:?}");
                assert!(parsed.chunks.is_empty(), "{path}: {source:?}");
                assert!(parsed.errors.is_empty(), "{path}: {:?}", parsed.errors);
            }
        }
        let unsupported = parse("data.txt", "\0💥 function broken(").unwrap();
        assert!(unsupported.callables.is_empty());
        assert!(unsupported.chunks.is_empty());
        assert!(unsupported.errors.is_empty());
    }
}
