//! Vector provider selection and shared embedding configuration/response handling.

use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::models::Vector;

use super::{Context, endpoint, index, normalize, positive, string, unqualify};

pub(super) fn create(context: Arc<Context>) -> Result<Box<dyn Vector>> {
    match string(&context.config, &["provider", "embeddingProvider"])?.unwrap_or("openai") {
        "openai" => Ok(Box::new(super::vector_openai::OpenAiVector::new(context)?)),
        "jina" => Ok(Box::new(super::vector_jina::JinaVector::new(context)?)),
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

pub(super) struct Configuration {
    pub(super) model: String,
    pub(super) dimensions: usize,
    pub(super) batch_size: usize,
    pub(super) url: String,
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
        Ok(Self {
            model,
            dimensions,
            batch_size,
            url,
        })
    }

    pub(super) fn profile(&self, provider: &str) -> Value {
        json!({"provider": provider, "model": self.model,
            "dimensions": self.dimensions, "strategyVersion": "rust-v1"})
    }

    pub(super) fn check_batch(&self, count: usize) -> Result<()> {
        // Never hide multiple paid batches behind one fallible return value.
        ensure!(
            count <= self.batch_size,
            "Embedding input count {} exceeds batch limit {}; split and persist batches in the caller",
            count,
            self.batch_size
        );
        Ok(())
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
