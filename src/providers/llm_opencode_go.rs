use anyhow::Result;
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::sync::Arc;

use super::llm::{Adapter, Configured};
use super::llm_opencode::{family_protocol, session_headers};
use super::{Context, GO, Protocol};
use crate::models::Llm;

pub(super) const DEFAULT_BASE: &str = GO;
pub(super) const DEFAULT_MODEL: &str = "muse-spark-1.3-contributor";

pub(super) struct OpenCodeGo {
    configured: Configured,
}

impl OpenCodeGo {
    pub(super) fn new(context: Arc<Context>) -> Result<Self> {
        Ok(Self {
            configured: Configured::new::<Self>(context)?,
        })
    }
}

impl Adapter for OpenCodeGo {
    const PROVIDER: &'static str = "opencode-go";
    const DEFAULT_BASE: &'static str = DEFAULT_BASE;
    const DEFAULT_MODEL: &'static str = DEFAULT_MODEL;

    fn protocol(model: &str) -> Protocol {
        if model.starts_with("minimax-") {
            Protocol::Messages
        } else {
            family_protocol(model)
        }
    }

    fn headers(key: &str, protocol: Protocol, session: &str) -> Result<HeaderMap> {
        session_headers(key, protocol, session)
    }
}

impl Llm for OpenCodeGo {
    fn profile(&self) -> Value {
        self.configured.profile::<Self>()
    }

    fn describe(&self, system: &str, prompt: &str) -> Result<String> {
        self.configured.describe::<Self>(system, prompt)
    }
}
