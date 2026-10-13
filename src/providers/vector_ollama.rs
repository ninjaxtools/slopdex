//! Local Ollama embeddings; construction is offline and never resolves credentials.

use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::sync::Arc;

use super::{Context, normalize, ollama, positive, string};
use crate::models::Vector;

pub(super) struct OllamaVector {
    context: Arc<Context>,
    model: String,
    dimensions: usize,
    native: usize,
    batch_size: usize,
    context_length: usize,
    keep_alive: String,
}

impl OllamaVector {
    pub(super) fn new(context: Arc<Context>) -> Result<Self> {
        let model = ollama::model(&context.config)?;
        let native = positive(
            &context.config,
            &["ollamaNativeDimensions"],
            ollama::default_dimensions(&model).unwrap_or_else(|| {
                context.config["dimensions"]
                    .as_u64()
                    .or_else(|| context.config["embeddingDimensions"].as_u64())
                    .unwrap_or(0) as usize
            }),
        )?;
        ensure!(
            native > 0,
            "Custom Ollama models require embedding setup or explicit dimensions; run `slopdex config embeddings`"
        );
        let dimensions = positive(
            &context.config,
            &["dimensions", "embeddingDimensions"],
            native,
        )?;
        ensure!(
            dimensions <= native,
            "Embedding dimensions exceed the Ollama model's native dimensions ({native})"
        );
        ensure!(
            dimensions == native || ollama::supports_reduction(&context.config),
            "This Ollama model does not support reduced embedding dimensions"
        );
        let batch_size = positive(&context.config, &["embeddingBatchSize"], 8)?.min(64);
        let context_length = positive(&context.config, &["ollamaContextLength"], 8192)?;
        let keep_alive = string(&context.config, &["ollamaKeepAlive"])?
            .unwrap_or("5m")
            .to_owned();
        ollama::url(&context.config, "embed")?;
        Ok(Self {
            context,
            model,
            dimensions,
            native,
            batch_size,
            context_length,
            keep_alive,
        })
    }

    fn inputs(&self, inputs: &[String], query: bool) -> Vec<String> {
        let family = ollama::family(&self.model);
        inputs
            .iter()
            .map(|text| match (family, query) {
                ("qwen3", true) => format!(
                    "Instruct: Given a query, retrieve relevant code or documentation\nQuery:{text}"
                ),
                ("embeddinggemma", true) => format!("task: search result | query: {text}"),
                ("embeddinggemma", false) => format!("title: none | text: {text}"),
                _ => text.clone(),
            })
            .collect()
    }
}

impl Vector for OllamaVector {
    fn profile(&self) -> Value {
        json!({"provider": "ollama", "model": self.model,
            "dimensions": self.dimensions, "nativeDimensions": self.native,
            "modelDigest": self.context.config["ollamaModelDigest"],
            "endpoint": ollama::url(&self.context.config, "embed").expect("validated endpoint"),
            "contextLength": self.context_length, "truncate": true,
            "inputStrategy": ollama::family(&self.model), "strategyVersion": "ollama-v1"})
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
    fn batch_limit(&self) -> usize {
        self.batch_size
    }

    fn embed(&self, inputs: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        super::vector::check_batch(inputs.len(), self.batch_size)?;
        ollama::verify(&self.context)?;
        let mut body = json!({"model": self.model, "input": self.inputs(inputs, query),
            "truncate": true, "keep_alive": self.keep_alive,
            "options": {"num_ctx": self.context_length}});
        // Request the native space for models without a known Matryoshka contract.
        if self.dimensions != self.native {
            body["dimensions"] = json!(self.dimensions);
        }
        let response = self.context.http.request_with_notice(
            &ollama::url(&self.context.config, "embed")?,
            Some(&body),
            &Default::default(),
            || self.context.report_call("vectors", "ollama", &self.model),
        )?;
        let vectors = ollama::embeddings(&response, inputs.len())?;
        vectors
            .iter()
            .map(|v| normalize(v, self.dimensions))
            .collect()
    }
}
