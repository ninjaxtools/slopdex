use super::llm::Adapter;
use super::{OPENAI, Protocol};

pub(super) const DEFAULT_BASE: &str = OPENAI;
pub(super) const DEFAULT_MODEL: &str = "gpt-5.6-luna";

pub(super) struct OpenAi;

impl Adapter for OpenAi {
    const PROVIDER: &'static str = "openai";
    const DEFAULT_BASE: &'static str = DEFAULT_BASE;
    const DEFAULT_MODEL: &'static str = DEFAULT_MODEL;

    fn protocol(_model: &str) -> Protocol {
        Protocol::Responses
    }
}
