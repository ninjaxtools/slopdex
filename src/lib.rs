pub mod cache;
pub mod callgraph;
pub mod cli;
pub mod engine;
pub mod filter;
pub mod formats;
pub mod git;
mod limits;
pub mod map;
pub mod models;
pub mod parse;
pub mod providers;
mod registry;
pub mod storage;
mod symbols;
mod ui;
pub mod vectors;

pub fn hash(input: impl AsRef<[u8]>) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(input.as_ref()))
}
