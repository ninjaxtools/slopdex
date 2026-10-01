//! Provider-independent model capabilities used by indexing and search.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn user(content: String) -> Self {
        Self {
            role: "user".into(),
            content,
        }
    }

    pub fn assistant(content: String) -> Self {
        Self {
            role: "assistant".into(),
            content,
        }
    }
}

/// A text-generating model, including its configured fallback policy.
pub trait Llm: Send + Sync {
    /// Stable cache identity of the configured primary model.
    fn profile(&self) -> Value;
    fn describe(&self, system: &str, prompt: &str) -> Result<String> {
        self.describe_conversation(system, &[Message::user(prompt.into())], "")
    }
    fn describe_conversation(
        &self,
        system: &str,
        messages: &[Message],
        session: &str,
    ) -> Result<String>;
}

/// A vector model producing ordered, normalized embeddings.
pub trait Vector: Send + Sync {
    fn profile(&self) -> Value;
    fn dimensions(&self) -> usize;
    /// Maximum inputs per call. Callers must persist each successful batch.
    fn batch_limit(&self) -> usize;
    fn embed(&self, inputs: &[String], query: bool) -> Result<Vec<Vec<f32>>>;
}

/// Ranks every supplied document, returning indices and descending scores.
pub trait Rerank: Send + Sync {
    fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<(usize, f64)>>;
    /// Hard request cap, if the provider imposes one.
    fn candidate_limit(&self) -> Option<usize> {
        None
    }
}
