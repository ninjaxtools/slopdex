use anyhow::Result;
use serde_json::json;
use std::sync::Arc;

use crate::models::Rerank;

use super::rerank::Configuration;
use super::{Context, JINA, Protocol, api_key, auth_headers, endpoint, parse_ranking};

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
        let key = api_key(&self.context.config, "rerankerApiKey", "jina")?;
        let headers = auth_headers(&key, Protocol::Responses)?;
        let body = json!({"model": self.config.model, "query": query, "documents": documents,
            "top_n": documents.len(), "return_documents": false});
        let url = endpoint(&self.config.base, "rerank")?;
        let response =
            self.context
                .http
                .request_with_notice(&url, Some(&body), &headers, || {
                    self.context
                        .report_call("reranking", "jina", &self.config.model);
                })?;
        parse_ranking(&response["results"], documents.len(), false)
    }
}
