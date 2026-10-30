//! Bearer authentication for embedding and native reranking APIs.

use anyhow::{Result, anyhow};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};

pub(super) fn auth_headers(key: &str) -> Result<HeaderMap> {
    let mut value = HeaderValue::from_str(&format!("Bearer {key}"))
        .map_err(|_| anyhow!("API key is not a valid HTTP header value"))?;
    value.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, value);
    Ok(headers)
}
