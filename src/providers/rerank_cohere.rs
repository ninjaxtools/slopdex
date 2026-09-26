use anyhow::Result;
use serde_json::json;
use std::sync::Arc;

use crate::models::Rerank;

use super::rerank::Configuration;
use super::{Context, Protocol, api_key, auth_headers, endpoint, parse_ranking};

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
        let key = api_key(&self.context.config, "rerankerApiKey", "cohere")?;
        let headers = auth_headers(&key, Protocol::Responses)?;
        let body = json!({"model": self.config.model, "query": query, "documents": documents,
            "top_n": documents.len()});
        let url = endpoint(&self.config.base, "rerank")?;
        let response =
            self.context
                .http
                .request_with_notice(&url, Some(&body), &headers, || {
                    self.context
                        .report_call("reranking", "cohere", &self.config.model);
                })?;
        parse_ranking(&response["results"], documents.len(), false)
    }
}
