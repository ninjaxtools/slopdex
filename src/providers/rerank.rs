//! Reranker selection, shared configuration, and native ranking requests.
//! Construct lazily on nonempty use.

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::models::Rerank;

use super::{
    COHERE, Context, JINA, api_key, auth_headers, endpoint, parse_ranking, string, unqualify,
};

pub(super) fn create(context: Arc<Context>) -> Result<Box<dyn Rerank>> {
    match string(&context.config, &["rerankerProvider"])?
        .ok_or_else(|| anyhow!("rerankerProvider is required for reranking"))?
    {
        "cohere" => Ok(Box::new(NativeRerank::new(
            context,
            "cohere",
            "rerank-v4.0-pro",
            COHERE,
        )?)),
        "jina" => Ok(Box::new(NativeRerank::new(
            context,
            "jina",
            "jina-reranker-v3.5",
            JINA,
        )?)),
        "command" => Ok(Box::new(super::rerank_command::CommandRerank::new(
            context,
        )?)),
        "openai" => bail!(
            "OpenAI generative reranking has been removed; set rerankerProvider to command and configure rerankerCommand"
        ),
        _ => bail!("Unsupported reranker provider"),
    }
}

/// Cohere and Jina speak the same native rerank protocol; only defaults and
/// Jina's `return_documents` opt-out differ.
struct NativeRerank {
    context: Arc<Context>,
    config: Configuration,
    provider: &'static str,
}

impl NativeRerank {
    fn new(
        context: Arc<Context>,
        provider: &'static str,
        default_model: &str,
        default_base: &str,
    ) -> Result<Self> {
        let config = Configuration::new(&context.config, provider, default_model, default_base)?;
        Ok(Self {
            context,
            config,
            provider,
        })
    }
}

impl Rerank for NativeRerank {
    fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<(usize, f64)>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        let mut body = json!({"model": self.config.model, "query": query, "documents": documents,
            "top_n": documents.len()});
        if self.provider == "jina" {
            body["return_documents"] = json!(false);
        }
        self.config
            .request_ranking(&self.context, self.provider, &body, documents.len())
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
        // Endpoint policy (TLS, custom-host notice) applies when the reranker is
        // constructed; path validation stays in rerank, after candidate checks.
        super::check_hosted_endpoint(config, "reranking", "rerankerBaseUrl", default_base)?;
        Ok(Self {
            model: model.to_owned(),
            base: base.to_owned(),
        })
    }

    /// Send one native rerank request and parse its ranking.
    pub(super) fn request_ranking(
        &self,
        context: &Context,
        provider: &str,
        body: &Value,
        count: usize,
    ) -> Result<Vec<(usize, f64)>> {
        let key = api_key(&context.config, "rerankerApiKey", provider)?;
        let headers = auth_headers(&key)?;
        let url = endpoint(&self.base, "rerank")?;
        let response = context
            .http
            .request_with_notice(&url, Some(body), &headers, || {
                context.report_call("reranking", provider, &self.model);
            })?;
        parse_ranking(&response["results"], count, false)
    }
}
