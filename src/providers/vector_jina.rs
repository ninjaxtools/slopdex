use anyhow::Result;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::models::Vector;

use super::vector::{Configuration, model};
use super::{Context, JINA, Protocol, api_key, auth_headers};

pub(super) struct JinaVector {
    context: Arc<Context>,
    config: Configuration,
}

impl JinaVector {
    pub(super) fn new(context: Arc<Context>) -> Result<Self> {
        let model = model(&context.config, "jina", "jina-embeddings-v4")?;
        let config = Configuration::new(&context.config, model, 1024, 64, JINA)?;
        Ok(Self { context, config })
    }
}

impl Vector for JinaVector {
    fn profile(&self) -> Value {
        self.config.profile("jina")
    }

    fn dimensions(&self) -> usize {
        self.config.dimensions
    }

    fn batch_limit(&self) -> usize {
        self.config.batch_size
    }

    fn embed(&self, inputs: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        self.config.check_batch(inputs.len())?;
        let key = api_key(&self.context.config, "embeddingApiKey", "jina")?;
        let headers = auth_headers(&key, Protocol::Responses)?;
        let body = json!({"model": self.config.model, "input": inputs,
            "dimensions": self.config.dimensions, "embedding_type": "float", "truncate": true,
            "task": if query { "code.query" } else { "code.passage" }});
        let response = self.context.http.request_with_notice(
            &self.config.url,
            Some(&body),
            &headers,
            || {
                self.context
                    .report_call("vectors", "jina", &self.config.model)
            },
        )?;
        self.config.parse(&response, inputs.len())
    }
}
