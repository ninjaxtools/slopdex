//! Shared blocking HTTP transport and bounded retry handling.

use anyhow::{Result, anyhow};
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde_json::Value;
use std::io::Read;
use std::thread;
use std::time::Duration;

use super::positive;

const MAX_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct Failure {
    pub(super) message: String,
    pub(super) retryable: bool,
    pub(super) can_failover: bool,
    pub(super) empty_output: bool,
    pub(super) retry_after: Option<Duration>,
}

impl Failure {
    pub(super) fn new(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            message: message.into(),
            retryable,
            can_failover: true,
            empty_output: false,
            retry_after: None,
        }
    }
    pub(super) fn empty(message: &str) -> Self {
        Self {
            empty_output: true,
            ..Self::new(message, true)
        }
    }
}

pub(super) struct Http {
    client: Client,
    pub(super) retries: usize,
    delay: Duration,
}

impl Http {
    pub(super) fn new(config: &Value) -> Result<Self> {
        let timeout = positive(config, &["providerTimeoutMs"], 60_000)?.min(300_000);
        let retries = match config.get("providerMaxRetries") {
            Some(value) => value
                .as_u64()
                .filter(|n| *n <= 5)
                .ok_or_else(|| anyhow!("providerMaxRetries must be an integer from 0 to 5"))?
                as usize,
            None => 2,
        };
        let delay = match config.get("retryDelayMs") {
            Some(value) => value
                .as_u64()
                .ok_or_else(|| anyhow!("retryDelayMs must be a nonnegative integer"))?
                .min(5000),
            None => 250,
        };
        let client = Client::builder()
            .timeout(Duration::from_millis(timeout as u64))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("slopdex")
            .build()
            .map_err(|_| anyhow!("Could not initialize provider HTTP client"))?;
        Ok(Self {
            client,
            retries,
            delay: Duration::from_millis(delay),
        })
    }

    pub(super) fn wait(&self, attempt: usize, retry_after: Option<Duration>) {
        let delay = self
            .delay
            .saturating_mul(1 << attempt.min(5))
            .max(retry_after.unwrap_or_default())
            .min(Duration::from_secs(5));
        thread::sleep(delay);
    }

    pub(super) fn request(
        &self,
        url: &str,
        body: Option<&Value>,
        headers: &HeaderMap,
    ) -> Result<Value> {
        self.request_with_notice(url, body, headers, || {})
    }

    pub(super) fn request_with_notice(
        &self,
        url: &str,
        body: Option<&Value>,
        headers: &HeaderMap,
        mut before_attempt: impl FnMut(),
    ) -> Result<Value> {
        for attempt in 0..=self.retries {
            before_attempt();
            match self.once(url, body, headers) {
                Ok(value) => return Ok(value),
                Err(failure) => {
                    if !failure.retryable || attempt == self.retries {
                        return Err(anyhow!("Provider request failed: {}", failure.message));
                    }
                    self.wait(attempt, failure.retry_after);
                }
            }
        }
        unreachable!("HTTP retries are bounded")
    }

    pub(super) fn once(
        &self,
        url: &str,
        body: Option<&Value>,
        headers: &HeaderMap,
    ) -> std::result::Result<Value, Failure> {
        let request = match body {
            Some(body) => self.client.post(url).json(body),
            None => self.client.get(url),
        };
        let response = request
            .headers(headers.clone())
            .header("accept", "application/json")
            .send()
            .map_err(|e| {
                Failure::new(
                    if e.is_timeout() {
                        "Provider request timed out"
                    } else {
                        "Provider transport error"
                    },
                    e.is_timeout() || e.is_connect() || e.is_request(),
                )
            })?;
        let status = response.status();
        if !status.is_success() {
            // Never include remote bodies, URLs, or reqwest's error chain: custom
            // servers and proxies can echo Authorization or URL query credentials.
            let code = status.as_u16();
            let retryable = matches!(code, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529);
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(|n| Duration::from_secs(n.min(5)));
            return Err(Failure {
                retry_after,
                // A different model cannot repair invalid shared credentials or
                // a redirect rejected by our HTTP policy. A 403 can be model-scoped.
                can_failover: code != 401 && !status.is_redirection(),
                ..Failure::new(format!("HTTP {code}"), retryable)
            });
        }
        let mut bytes = Vec::new();
        response
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Failure::new("Could not read provider response", true))?;
        if bytes.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(Failure::new("Provider response exceeds size limit", false));
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| Failure::new("Provider returned malformed JSON", false))
    }
}
