//! Vector provider selection and shared embedding configuration/response handling.

use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::models::Vector;

use super::{
    Context, api_key, auth_headers, endpoint, index, normalize, positive, string, unqualify,
};

pub(super) fn create(context: Arc<Context>) -> Result<Box<dyn Vector>> {
    match string(&context.config, &["provider", "embeddingProvider"])?.unwrap_or("openai") {
        "openai" => Ok(Box::new(super::vector_openai::OpenAiVector::new(context)?)),
        "jina" => Ok(Box::new(super::vector_jina::JinaVector::new(context)?)),
        "ollama" => Ok(Box::new(super::vector_ollama::OllamaVector::new(context)?)),
        _ => bail!("Unsupported embedding provider"),
    }
}

pub(super) fn model(config: &Value, provider: &str, default: &str) -> Result<String> {
    Ok(unqualify(
        string(config, &["model", "embeddingModel"])?.unwrap_or(default),
        provider,
    )?
    .to_owned())
}

pub(super) fn check_batch(count: usize, limit: usize) -> Result<()> {
    // Never hide multiple batches behind one fallible return value.
    ensure!(
        count <= limit,
        "Embedding input count {} exceeds batch limit {}; split and persist batches in the caller",
        count,
        limit
    );
    Ok(())
}

pub(super) struct Configuration {
    pub(super) model: String,
    pub(super) dimensions: usize,
    pub(super) batch_size: usize,
    pub(super) url: String,
    custom_endpoint: bool,
}

impl Configuration {
    pub(super) fn new(
        config: &Value,
        model: String,
        default_dimensions: usize,
        maximum: usize,
        default_base: &str,
    ) -> Result<Self> {
        let dimensions = positive(
            config,
            &["dimensions", "embeddingDimensions"],
            default_dimensions,
        )?;
        let batch_size = positive(config, &["embeddingBatchSize"], maximum)?.min(maximum);
        let base = string(config, &["embeddingBaseUrl"])?.unwrap_or(default_base);
        let url = endpoint(base, "embeddings")?;
        super::check_hosted_endpoint(config, "embedding", "embeddingBaseUrl", default_base)?;
        Ok(Self {
            model,
            dimensions,
            batch_size,
            url,
            custom_endpoint: config["embeddingBaseUrl"].is_string(),
        })
    }

    /// Default endpoints are omitted so existing profiles and caches stay valid;
    /// custom endpoints are recorded without their query string.
    pub(super) fn profile(&self, provider: &str) -> Value {
        let mut profile = json!({"provider": provider, "model": self.model,
            "dimensions": self.dimensions, "strategyVersion": "rust-v1"});
        if self.custom_endpoint {
            profile["endpoint"] = json!(super::profile_endpoint(&self.url));
        }
        profile
    }

    pub(super) fn check_batch(&self, count: usize) -> Result<()> {
        check_batch(count, self.batch_size)
    }

    /// Send one hosted embedding request; each provider builds its own body.
    pub(super) fn request_embeddings(
        &self,
        context: &Context,
        provider: &str,
        body: &Value,
        count: usize,
    ) -> Result<Vec<Vec<f32>>> {
        let key = api_key(&context.config, "embeddingApiKey", provider)?;
        let headers = auth_headers(&key)?;
        let response = context
            .http
            .request_with_notice(&self.url, Some(body), &headers, || {
                context.report_call("vectors", provider, &self.model);
            })?;
        self.parse(&response, count)
    }

    pub(super) fn parse(&self, response: &Value, count: usize) -> Result<Vec<Vec<f32>>> {
        let data = response["data"]
            .as_array()
            .ok_or_else(|| anyhow!("Malformed embedding response: missing data array"))?;
        ensure!(
            data.len() == count,
            "Embedding response count does not match input count"
        );
        let mut ordered = vec![None; count];
        for item in data {
            let index = index(&item["index"], count, "embedding")?;
            ensure!(
                ordered[index].is_none(),
                "Duplicate embedding response index"
            );
            ordered[index] = Some(normalize(&item["embedding"], self.dimensions)?);
        }
        ordered
            .into_iter()
            .map(|v| v.ok_or_else(|| anyhow!("Missing embedding response index")))
            .collect()
    }
}
