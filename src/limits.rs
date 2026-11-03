//! Shared output-limit parsing and CLI/config/API precedence.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::str::FromStr;

pub(crate) const DEFAULT_LIMIT: usize = 4096;

pub(crate) struct LimitedResults<T> {
    pub(crate) rows: Vec<T>,
    pub(crate) omitted: bool,
    pub(crate) limit: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResultLimit {
    Count(usize),
    None,
}

impl ResultLimit {
    pub(crate) fn value(self) -> Value {
        match self {
            Self::Count(count) => json!(count),
            Self::None => json!("none"),
        }
    }

    pub(crate) fn count(self) -> Option<usize> {
        match self {
            Self::Count(count) => Some(count),
            Self::None => None,
        }
    }

    pub(crate) fn from_value(value: &Value) -> Result<Self> {
        if matches!(value.as_str(), Some("none" | "off")) {
            return Ok(Self::None);
        }
        if let Some(count) = value.as_u64().and_then(|count| usize::try_from(count).ok())
            && count > 0
        {
            return Ok(Self::Count(count));
        }
        bail!("limit must be a positive integer, \"none\", or \"off\"")
    }
}

impl FromStr for ResultLimit {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if matches!(value, "none" | "off") {
            return Ok(Self::None);
        }
        value
            .parse::<usize>()
            .ok()
            .filter(|count| *count > 0)
            .map(Self::Count)
            .ok_or_else(|| "limit must be a positive integer, 'none', or 'off'".into())
    }
}

pub(crate) fn resolve(config: &Value, options: &Value) -> Result<ResultLimit> {
    match options.get("limit").or_else(|| config.get("defaultLimit")) {
        Some(value) => ResultLimit::from_value(value),
        None => Ok(ResultLimit::Count(DEFAULT_LIMIT)),
    }
}

pub(crate) fn validate_config(config: &Value) -> Result<()> {
    if let Some(value) = config.get("defaultLimit") {
        ResultLimit::from_value(value)
            .map_err(|_| anyhow::anyhow!("defaultLimit must be a positive integer or \"none\""))?;
    }
    validate_numeric_config(config)
}

/// Every documented numeric setting, bounded in one place shared by the CLI
/// and the library entry points, so a value accepted by `config set` cannot
/// fail later inside a provider and a repository config cannot defeat the
/// resource caps with huge values. An explicit null is an error for these
/// keys (existing contract); only absence means unset. The two HTTP retry
/// settings are the exception: null already means unset there.
pub(crate) fn validate_numeric_config(config: &Value) -> Result<()> {
    for (key, max) in [
        ("dimensions", 16_384u64),
        ("symbolDimensions", 16_384),
        ("ollamaNativeDimensions", 16_384),
        ("maxFileSize", 64 * 1024 * 1024),
        ("embeddingBatchSize", 10_000),
        ("parallelism", 256),
        ("embeddingParallelism", 256),
        ("agentTimeoutMs", 3_600_000),
        ("providerTimeoutMs", 3_600_000),
        ("ollamaPullTimeoutMs", 10_800_000),
        ("ollamaContextLength", 1_000_000),
        ("rerankerCandidates", 100),
    ] {
        if let Some(value) = config.get(key) {
            anyhow::ensure!(
                value
                    .as_u64()
                    .is_some_and(|number| number > 0 && number <= max),
                "{key} must be an integer between 1 and {max}"
            );
        }
    }
    if let Some(value) = config
        .get("providerMaxRetries")
        .filter(|value| !value.is_null())
    {
        anyhow::ensure!(
            value.as_u64().is_some_and(|number| number <= 5),
            "providerMaxRetries must be an integer between 0 and 5"
        );
    }
    if let Some(value) = config.get("retryDelayMs").filter(|value| !value.is_null()) {
        anyhow::ensure!(
            value.as_u64().is_some_and(|number| number <= 60_000),
            "retryDelayMs must be an integer between 0 and 60000"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_resolve_explicit_configured_and_builtin_defaults() {
        assert_eq!(resolve(&json!({}), &json!({})).unwrap().count(), Some(4096));
        for default in [json!(17), json!("none")] {
            let config = json!({"defaultLimit": default});
            assert_eq!(resolve(&config, &json!({})).unwrap().value(), default);
            for limit in [json!(3), json!("none")] {
                assert_eq!(
                    resolve(&config, &json!({"limit": limit})).unwrap().value(),
                    limit
                );
            }
        }
    }

    #[test]
    fn invalid_limits_are_rejected() {
        for value in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!("5"),
            json!("all"),
            Value::Null,
            json!(true),
        ] {
            assert!(resolve(&json!({}), &json!({"limit": value})).is_err());
            assert!(validate_config(&json!({"defaultLimit": value})).is_err());
        }
        for value in ["0", "-1", "1.5", "NaN", "all"] {
            assert!(value.parse::<ResultLimit>().is_err());
        }
        assert_eq!("none".parse::<ResultLimit>().unwrap(), ResultLimit::None);
        assert_eq!("off".parse::<ResultLimit>().unwrap(), ResultLimit::None);
        assert_eq!(
            resolve(&json!({}), &json!({"limit": "off"})).unwrap(),
            ResultLimit::None
        );
        assert_eq!("7".parse::<ResultLimit>().unwrap(), ResultLimit::Count(7));
    }

    #[test]
    fn numeric_config_values_are_bounded_for_library_callers_too() {
        assert!(validate_config(&json!({"maxFileSize": 1024, "parallelism": 4})).is_ok());
        for config in [
            json!({"maxFileSize": "1024"}),
            json!({"maxFileSize": 1.5}),
            json!({"maxFileSize": null}),
            json!({"maxFileSize": 999_999_999_999u64}),
            json!({"parallelism": 0}),
            json!({"providerMaxRetries": 99}),
            json!({"dimensions": 1_000_000}),
        ] {
            assert!(validate_config(&config).is_err(), "{config}");
        }
        // The HTTP retry settings keep their null-means-unset contract.
        assert!(
            validate_config(&json!({"providerMaxRetries": null, "retryDelayMs": null})).is_ok()
        );
    }
}
