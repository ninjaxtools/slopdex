pub mod cli;
pub mod engine;
pub mod models;
pub mod parser;
pub mod providers;
pub mod storage;
pub mod vectors;

pub fn hash(input: impl AsRef<[u8]>) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(input.as_ref()))
}
