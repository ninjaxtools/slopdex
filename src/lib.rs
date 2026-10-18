pub mod cache;
pub mod callgraph;
pub mod cli;
pub mod engine;
pub mod filter;
pub mod formats;
pub mod git;
mod limits;
mod lines;
pub mod map;
pub mod models;
pub mod parse;
pub mod providers;
pub mod refactor;
mod registry;
pub mod storage;
mod symbols;
#[cfg(all(test, unix))]
mod test_support;
mod ui;
pub mod vectors;

pub fn hash(input: impl AsRef<[u8]>) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(input.as_ref()))
}
