//! Reranker selection, shared configuration, and native ranking requests.
//! Construct lazily on nonempty use.

use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::sync::Arc;

use crate::models::Rerank;

use super::{Context, Protocol, api_key, auth_headers, endpoint, parse_ranking, string, unqualify};

pub(super) fn create(context: Arc<Context>) -> Result<Box<dyn Rerank>> {
    match string(&context.config, &["rerankerProvider"])?
        .ok_or_else(|| anyhow!("rerankerProvider is required for reranking"))?
    {
        "cohere" => Ok(Box::new(super::rerank_cohere::CohereRerank::new(context)?)),
        "jina" => Ok(Box::new(super::rerank_jina::JinaRerank::new(context)?)),
        "openai" => Ok(Box::new(super::rerank_openai::OpenAiRerank::new(context)?)),
        _ => bail!("Unsupported reranker provider"),
    }
}

pub(super) struct Configuration {
    pub(super) model: String,
    pub(super) base: String,
}

impl Configuration {
    pub(super) fn new(
        config: &Value,
        provider: &str,
        default_model: &str,
        default_base: &str,
    ) -> Result<Self> {
        let model = unqualify(
            string(config, &["rerankerModel"])?.unwrap_or(default_model),
            provider,
        )?;
        let base = string(config, &["rerankerBaseUrl"])?.unwrap_or(default_base);
        // URL validation stays in rerank, after candidate and credential validation.
        Ok(Self {
            model: model.to_owned(),
            base: base.to_owned(),
        })
    }

    /// Cohere/Jina share the native rerank protocol; each builds its own request body.
    pub(super) fn request_ranking(
        &self,
        context: &Context,
        provider: &str,
        body: &Value,
        count: usize,
    ) -> Result<Vec<(usize, f64)>> {
        let key = api_key(&context.config, "rerankerApiKey", provider)?;
        let headers = auth_headers(&key, Protocol::Responses)?;
        let url = endpoint(&self.base, "rerank")?;
        let response = context
            .http
            .request_with_notice(&url, Some(body), &headers, || {
                context.report_call("reranking", provider, &self.model);
            })?;
        parse_ranking(&response["results"], count, false)
    }
}
