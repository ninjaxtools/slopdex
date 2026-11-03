//! slopdex: a local code index with structural, semantic, and FAQ search.
//!
//! The crate exposes a deliberately small facade. Implementation modules are
//! private; everything a library or test caller needs is re-exported here.

mod cache;
mod callgraph;
mod cli;
mod config;
mod engine;
mod filter;
mod formats;
mod git;
mod limits;
mod lines;
mod map;
mod models;
mod parse;
mod providers;
mod refactor;
mod registry;
mod storage;
mod symbols;
#[cfg(all(test, unix))]
mod test_support;
mod ui;
mod vectors;

pub use cache::{Artifacts, store_path};
pub use cli::{report_error, run};
pub use config::Config;
pub use engine::{Access, Engine, OpenMode, OpenOptions};
pub use parse::{
    CallSite, Callable, Diagnostic, FileStructure, ImportBinding, LanguageId, MarkdownChunk,
    NodeKind, ParsedFile, ReferenceSite, StructureNode, language_for_path, parse,
};
pub use providers::{Providers, prepare_embeddings};
pub use refactor::{FileChange, Plan};
pub use storage::{Database, File, Item, STRUCTURE_PARSER_VERSION, UnitKind};
pub use ui::{NoProgress, ProgressReporter, StageId};

pub fn hash(input: impl AsRef<[u8]>) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(input.as_ref()))
}
#[cfg(test)]
#[path = "../tests/support/agent.rs"]
pub(crate) mod test_agent;
