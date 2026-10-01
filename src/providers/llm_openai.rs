use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

use super::llm::{Adapter, Configured};
use super::{Context, OPENAI, Protocol};
use crate::models::{Llm, Message};

pub(super) const DEFAULT_BASE: &str = OPENAI;
pub(super) const DEFAULT_MODEL: &str = "gpt-5.6-luna";

pub(super) struct OpenAi {
    configured: Configured,
}

impl OpenAi {
    pub(super) fn new(context: Arc<Context>) -> Result<Self> {
        Ok(Self {
            configured: Configured::new::<Self>(context)?,
        })
    }
}

impl Adapter for OpenAi {
    const PROVIDER: &'static str = "openai";
    const DEFAULT_BASE: &'static str = DEFAULT_BASE;
    const DEFAULT_MODEL: &'static str = DEFAULT_MODEL;

    fn protocol(_model: &str) -> Protocol {
        Protocol::Responses
    }
}

impl Llm for OpenAi {
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
