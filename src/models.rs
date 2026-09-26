//! Provider-independent model capabilities used by indexing and search.

use anyhow::Result;
use serde_json::Value;

/// A text-generating model, including its configured fallback policy.
pub trait Llm: Send + Sync {
    /// Stable cache identity of the configured primary model.
    fn profile(&self) -> Value;
    fn describe(&self, system: &str, prompt: &str) -> Result<String>;
}

/// A vector model producing ordered, normalized embeddings.
pub trait Vector: Send + Sync {
    fn profile(&self) -> Value;
    fn dimensions(&self) -> usize;
    /// Maximum inputs per call. Callers must persist each successful batch.
    fn batch_limit(&self) -> usize;
    fn embed(&self, inputs: &[String], query: bool) -> Result<Vec<Vec<f32>>>;
    /// Whether a legacy document embedding uses equivalent input processing.
    fn supports_legacy_input(&self, _input: &str) -> bool {
        false
    }
}

/// Ranks every supplied document, returning indices and descending scores.
pub trait Rerank: Send + Sync {
    fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<(usize, f64)>>;
    /// Hard request cap, if the provider imposes one.
    fn candidate_limit(&self) -> Option<usize> {
        None
    }
}
