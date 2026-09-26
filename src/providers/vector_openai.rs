use anyhow::Result;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::models::Vector;

use super::vector::{Configuration, model};
use super::{Context, OPENAI, Protocol, api_key, auth_headers, truncate_bytes};

pub(super) struct OpenAiVector {
    context: Arc<Context>,
    config: Configuration,
}

impl OpenAiVector {
    pub(super) fn new(context: Arc<Context>) -> Result<Self> {
        let model = model(&context.config, "openai", "text-embedding-3-large")?;
        let dimensions = match model.as_str() {
            "text-embedding-3-small" | "text-embedding-ada-002" => 1536,
            _ => 3072,
        };
        // The byte cap and batch cap also keep requests under the aggregate token budget.
        let config = Configuration::new(&context.config, model, dimensions, 32, OPENAI)?;
        Ok(Self { context, config })
    }
}

impl Vector for OpenAiVector {
    fn profile(&self) -> Value {
        self.config.profile("openai")
    }

    fn dimensions(&self) -> usize {
        self.config.dimensions
    }

    fn batch_limit(&self) -> usize {
        self.config.batch_size
    }

    fn embed(&self, inputs: &[String], _query: bool) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        self.config.check_batch(inputs.len())?;
        let key = api_key(&self.context.config, "embeddingApiKey", "openai")?;
        let headers = auth_headers(&key, Protocol::Responses)?;
        // A byte is a conservative token upper bound for OpenAI's byte-level BPE.
        let values: Vec<&str> = inputs.iter().map(|s| truncate_bytes(s, 8191)).collect();
        let mut body = json!({"model": self.config.model, "input": values,
            "dimensions": self.config.dimensions, "encoding_format": "float"});
        if self.config.model == "text-embedding-ada-002" {
            body.as_object_mut().unwrap().remove("dimensions");
        }
        let response = self.context.http.request_with_notice(
            &self.config.url,
            Some(&body),
            &headers,
            || {
                self.context
                    .report_call("vectors", "openai", &self.config.model)
            },
        )?;
        self.config.parse(&response, inputs.len())
    }

    fn supports_legacy_input(&self, input: &str) -> bool {
        // Legacy OpenAI truncated at 8192 tokens; this implementation uses bytes.
        input.len() <= 8191
    }
}
