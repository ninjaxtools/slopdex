//! Shared description configuration and bounded, sticky model failover.

use anyhow::{Result, anyhow, bail};
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::protocol::conversation_request;
use super::{
    Context, DESCRIPTION_ATTEMPTS, Protocol, api_key, auth_headers, description_text,
    llm_openai::OpenAi, llm_opencode::OpenCode, llm_opencode_go::OpenCodeGo, session_id, string,
    unqualify, validate_url,
};
use crate::models::{Llm, Message};

pub(super) fn create(context: Arc<Context>) -> Result<Box<dyn Llm>> {
    let configured_provider = string(&context.config, &["descriptionProvider"])?;
    let configured_model = string(&context.config, &["descriptionModel"])?;
    let inferred_provider = configured_model
        .and_then(|model| model.split_once('/'))
        .map(|(provider, _)| provider);
    match configured_provider
        .or(inferred_provider)
        .unwrap_or("openai")
    {
        "openai" => Ok(Box::new(OpenAi::new(context)?)),
        "opencode" => Ok(Box::new(OpenCode::new(context)?)),
        "opencode-go" => Ok(Box::new(OpenCodeGo::new(context)?)),
        _ => bail!("Unsupported description provider"),
    }
}

/// Provider policy; the shared retry loop does not select providers or protocols.
pub(super) trait Adapter {
    const PROVIDER: &'static str;
    const DEFAULT_BASE: &'static str;
    const DEFAULT_MODEL: &'static str;

    fn protocol(model: &str) -> Protocol;

    fn headers(key: &str, protocol: Protocol, _session: &str) -> Result<HeaderMap> {
        auth_headers(key, protocol)
    }
}

pub(super) struct Configured {
    context: Arc<Context>,
    models: Vec<String>,
    base: String,
    active: AtomicUsize,
}

impl Configured {
    pub(super) fn new<A: Adapter>(context: Arc<Context>) -> Result<Self> {
        let config = &context.config;
        let primary = unqualify(
            string(config, &["descriptionModel"])?.unwrap_or(A::DEFAULT_MODEL),
            A::PROVIDER,
        )?;
        let mut models = vec![primary.to_owned()];
        if let Some(fallback) = string(config, &["descriptionFallbackModel", "fallbackModel"])? {
            let fallback = unqualify(fallback, A::PROVIDER)?;
            if fallback != primary {
                models.push(fallback.to_owned());
            }
        }
        let base = string(config, &["descriptionBaseUrl"])?.unwrap_or(A::DEFAULT_BASE);
        validate_url(base)?;
        // Protocol builders normalize the path; trimming the whole URL can alter query values.
        let base = base.to_owned();
        Ok(Self {
            context,
            models,
            base,
            active: AtomicUsize::new(0),
        })
    }

    /// The profile identifies the configured primary, even while fallback is active.
    pub(super) fn profile<A: Adapter>(&self) -> Value {
        let mut profile = json!({"provider": A::PROVIDER, "model": self.models[0],
            "strategyVersion": "file-conversation-v3"});
        if self.context.config["descriptionBaseUrl"].is_string() {
            profile["endpoint"] = json!(self.base);
        }
        if self.models.len() > 1 {
            profile["fallbackModel"] = json!(self.models[1]);
        }
        profile
    }

    /// Successful fallback remains active across calls; a failure switches back.
    /// Six total attempts bound failover/empty-output retries without nesting HTTP retries.
    pub(super) fn describe<A: Adapter>(
        &self,
        system: &str,
        messages: &[Message],
        session: &str,
    ) -> Result<String> {
        let key = api_key(&self.context.config, "descriptionApiKey", A::PROVIDER)?;
        let failover = self.models.len() > 1;
        let mut permanent_failures = vec![false; self.models.len()];
        let mut active = self.active.load(Ordering::Relaxed);
        let session = if session.is_empty() {
            session_id()
        } else {
            session.to_owned()
        };
        for attempt in 0..DESCRIPTION_ATTEMPTS {
            let model = &self.models[active];
            let protocol = A::protocol(model);
            let (url, body) = conversation_request(&self.base, model, protocol, system, messages)?;
            let headers = A::headers(&key, protocol, &session)?;
            self.context.report_call("descriptions", A::PROVIDER, model);
            let outcome = self
                .context
                .http
                .once(&url, Some(&body), &headers)
                .and_then(|value| description_text(&value, protocol));
            match outcome {
                Ok(text) => {
                    self.active.store(active, Ordering::Relaxed);
                    return Ok(text);
                }
                Err(failure) => {
                    permanent_failures[active] = !failure.retryable;
                    if !failure.can_failover || permanent_failures.iter().all(|failed| *failed) {
                        return Err(anyhow!("Description request failed: {}", failure.message));
                    }
                    if failover {
                        if !permanent_failures[1 - active] {
                            active = 1 - active;
                        }
                        self.active.store(active, Ordering::Relaxed);
                    }
                    let limit = if failover || failure.empty_output {
                        DESCRIPTION_ATTEMPTS
                    } else {
                        self.context.http.retries + 1
                    };
                    if attempt + 1 >= limit || (!failover && !failure.retryable) {
                        return Err(anyhow!("Description request failed: {}", failure.message));
                    }
                    self.context.http.wait(attempt, failure.retry_after);
                }
            }
        }
        unreachable!("description attempts are bounded")
    }
}
