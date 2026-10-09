use anyhow::Result;
use serde_json::json;
use std::sync::Arc;

use crate::models::Rerank;

use super::Context;
use super::rerank::Configuration;

pub(super) struct CohereRerank {
    context: Arc<Context>,
    config: Configuration,
}

impl CohereRerank {
    pub(super) fn new(context: Arc<Context>) -> Result<Self> {
        let config = Configuration::new(
            &context.config,
            "cohere",
            "rerank-v4.0-pro",
            "https://api.cohere.com/v2",
        )?;
        Ok(Self { context, config })
    }
}

impl Rerank for CohereRerank {
    fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<(usize, f64)>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        let body = json!({"model": self.config.model, "query": query, "documents": documents,
            "top_n": documents.len()});
        self.config
            .request_ranking(&self.context, "cohere", &body, documents.len())
    }
}
