use anyhow::Result;
use serde_json::json;
use std::sync::Arc;

use crate::models::Rerank;

use super::rerank::Configuration;
use super::{Context, JINA};

pub(super) struct JinaRerank {
    context: Arc<Context>,
    config: Configuration,
}

impl JinaRerank {
    pub(super) fn new(context: Arc<Context>) -> Result<Self> {
        let config = Configuration::new(&context.config, "jina", "jina-reranker-v3.5", JINA)?;
        Ok(Self { context, config })
    }
}

impl Rerank for JinaRerank {
    fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<(usize, f64)>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        let body = json!({"model": self.config.model, "query": query, "documents": documents,
            "top_n": documents.len(), "return_documents": false});
        self.config
            .request_ranking(&self.context, "jina", &body, documents.len())
    }
}
