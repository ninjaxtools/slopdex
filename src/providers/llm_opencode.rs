use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use std::sync::Arc;

use super::llm::{Adapter, Configured};
use super::{Context, Protocol, ZEN, auth_headers};
use crate::models::{Llm, Message};

pub(super) const DEFAULT_BASE: &str = ZEN;
pub(super) const DEFAULT_MODEL: &str = "muse-spark-1.3-contributor";

pub(super) struct OpenCode {
    configured: Configured,
}

impl OpenCode {
    pub(super) fn new(context: Arc<Context>) -> Result<Self> {
        Ok(Self {
            configured: Configured::new::<Self>(context)?,
        })
    }
}

impl Adapter for OpenCode {
    const PROVIDER: &'static str = "opencode";
    const DEFAULT_BASE: &'static str = DEFAULT_BASE;
    const DEFAULT_MODEL: &'static str = DEFAULT_MODEL;

    fn protocol(model: &str) -> Protocol {
        family_protocol(model)
    }

    fn headers(key: &str, protocol: Protocol, session: &str) -> Result<HeaderMap> {
        session_headers(key, protocol, session)
    }
}

impl Llm for OpenCode {
    fn profile(&self) -> Value {
        self.configured.profile::<Self>()
    }

    fn describe_conversation(
        &self,
        system: &str,
        messages: &[Message],
        session: &str,
    ) -> Result<String> {
        self.configured.describe::<Self>(system, messages, session)
    }
}

/// Model catalogue IDs are not interchangeable API protocols.
/// OpenCode Go applies its MiniMax exception before this shared family selection.
pub(super) fn family_protocol(model: &str) -> Protocol {
    if ["gpt-", "grok-", "muse-spark-"]
        .iter()
        .any(|prefix| model.starts_with(prefix))
    {
        return Protocol::Responses;
    }
    if model.starts_with("gemini-") {
        return Protocol::Gemini;
    }
    if model.starts_with("claude-") || model.starts_with("qwen") {
        return Protocol::Messages;
    }
    if [
        "big-pickle",
        "deepseek-",
        "glm-",
        "minimax-",
        "kimi-",
        "ling-",
        "longcat-",
        "mimo-",
        "nemotron-",
        "omen-",
    ]
    .iter()
    .any(|prefix| model.starts_with(prefix))
        || (model.starts_with("hy") && model.as_bytes().get(2).is_some_and(u8::is_ascii_digit))
    {
        return Protocol::Chat;
    }
    Protocol::Responses
}

pub(super) fn session_headers(key: &str, protocol: Protocol, session: &str) -> Result<HeaderMap> {
    let mut headers = auth_headers(key, protocol)?;
    headers.insert(
        "x-opencode-session",
        HeaderValue::from_str(session).unwrap(),
    );
    Ok(headers)
}
