pub mod cli;
pub mod engine;
pub mod git;
pub mod models;
pub mod parse;
pub mod providers;
pub mod storage;
mod ui;
pub mod vectors;

pub fn hash(input: impl AsRef<[u8]>) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(input.as_ref()))
}
