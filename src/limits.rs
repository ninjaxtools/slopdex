//! Shared output-limit parsing and CLI/config/API precedence.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::str::FromStr;

pub(crate) const DEFAULT_LIMIT: usize = 4096;

pub(crate) struct LimitedResults<T> {
    pub(crate) rows: Vec<T>,
    pub(crate) omitted: bool,
    pub(crate) limit: Option<usize>,
    /// Results subject to the limit, excluding ancestor/call-graph context.
    pub(crate) count: usize,
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

    fn from_value(value: &Value) -> Result<Self> {
        if value.as_str() == Some("none") {
            return Ok(Self::None);
        }
        if let Some(count) = value.as_u64().and_then(|count| usize::try_from(count).ok())
            && count > 0
        {
            return Ok(Self::Count(count));
        }
        bail!("limit must be a positive integer or \"none\"")
    }
}

impl FromStr for ResultLimit {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value == "none" {
            return Ok(Self::None);
        }
        value
            .parse::<usize>()
            .ok()
            .filter(|count| *count > 0)
            .map(Self::Count)
            .ok_or_else(|| "limit must be a positive integer or 'none'".into())
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
        assert_eq!("7".parse::<ResultLimit>().unwrap(), ResultLimit::Count(7));
    }
}
