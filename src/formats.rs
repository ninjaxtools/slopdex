//! Format groups shared by parser dispatch, query selection, and content indexes.

use std::collections::BTreeSet;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bump when group membership or default selection changes.
pub const VERSION: &str = "format-groups-v2";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FormatGroup {
    Code,
    Docs,
    Config,
    Markup,
}

impl FormatGroup {
    pub const ALL: [Self; 4] = [Self::Code, Self::Docs, Self::Config, Self::Markup];
    pub const DEFAULT: [Self; 2] = [Self::Code, Self::Docs];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Docs => "docs",
            Self::Config => "config",
            Self::Markup => "markup",
        }
    }

    pub fn for_language(language: &str) -> Option<Self> {
        match language {
            "typescript" | "tsx" | "javascript" | "jsx" | "python" | "rust" | "go" | "java"
            | "c" | "bash" => Some(Self::Code),
            "markdown" => Some(Self::Docs),
            "json" | "terraform" | "yaml" | "toml" => Some(Self::Config),
            "xml" | "html" | "css" => Some(Self::Markup),
            _ => None,
        }
    }

    pub fn for_path(path: &str) -> Option<Self> {
        crate::parse::language_for_path(path).and_then(Self::for_language)
    }

    /// Parse one group name; the selection-only `all` shorthand is not a group.
    pub fn from_name(name: &str) -> Result<Self> {
        match name.trim() {
            "code" => Ok(Self::Code),
            "docs" => Ok(Self::Docs),
            "config" => Ok(Self::Config),
            "markup" => Ok(Self::Markup),
            _ => bail!("Unknown format group {name:?}; expected code, docs, config, or markup"),
        }
    }
}

/// Select whole format groups, replacing defaults when `formats` is specified.
/// Strings and array entries may contain comma-separated names; `all` expands
/// to every supported group. The sorted set canonicalizes order and duplicates.
pub fn selected_groups(options: &Value) -> Result<BTreeSet<FormatGroup>> {
    let value = match options.get("formats") {
        None | Some(Value::Null) => return Ok(FormatGroup::DEFAULT.into_iter().collect()),
        Some(value) => value,
    };
    let selections: Vec<&str> = match value {
        Value::String(value) => vec![value],
        Value::Array(values) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("formats entries must be strings"))
            })
            .collect::<Result<_>>()?,
        _ => bail!("formats must be a string or an array of strings"),
    };
    let mut groups = BTreeSet::new();
    for selection in selections {
        for name in selection.split(',').map(str::trim) {
            ensure!(
                !name.is_empty(),
                "formats must not contain empty group names"
            );
            if name == "all" {
                groups.extend(FormatGroup::ALL);
            } else {
                groups.insert(FormatGroup::from_name(name)?);
            }
        }
    }
    ensure!(!groups.is_empty(), "formats must select at least one group");
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_supported_extension_has_a_group() {
        for (group, extensions) in [
            (
                FormatGroup::Code,
                "ts mts cts tsx js mjs cjs jsx py pyw rs go java c h sh bash zsh",
            ),
            (FormatGroup::Docs, "md markdown"),
            (FormatGroup::Config, "json tf tfvars hcl yaml yml toml"),
            (FormatGroup::Markup, "xml svg xsd xsl xslt html htm css"),
        ] {
            for extension in extensions.split_whitespace() {
                for path in [
                    format!("dir/file.{extension}"),
                    format!("C:\\dir\\file.{}", extension.to_uppercase()),
                ] {
                    assert_eq!(FormatGroup::for_path(&path), Some(group), "{path}");
                    let language = crate::parse::language_for_path(&path).unwrap();
                    assert_eq!(FormatGroup::for_language(language), Some(group));
                }
            }
        }
        for path in [
            "file.cpp",
            "file.txt",
            ".ts",
            "dir.ts/file",
            "file",
            "file.",
        ] {
            assert_eq!(FormatGroup::for_path(path), None, "{path}");
        }
        assert_eq!(FormatGroup::for_language("unknown"), None);
    }

    #[test]
    fn group_names_and_serialization_are_canonical() {
        for group in FormatGroup::ALL {
            assert_eq!(FormatGroup::from_name(group.as_str()).unwrap(), group);
            assert_eq!(serde_json::to_value(group).unwrap(), json!(group.as_str()));
            assert_eq!(
                serde_json::from_value::<FormatGroup>(json!(group.as_str())).unwrap(),
                group
            );
        }
        for name in ["all", "", "Code", "markdown", "json", "other", "code,docs"] {
            assert!(FormatGroup::from_name(name).is_err(), "{name}");
        }
    }

    #[test]
    fn absent_or_null_selection_excludes_config_and_markup_by_default() {
        let defaults = BTreeSet::from([FormatGroup::Code, FormatGroup::Docs]);
        for options in [json!({}), json!({"formats": null}), Value::Null] {
            assert_eq!(selected_groups(&options).unwrap(), defaults);
        }
        assert!(!defaults.contains(&FormatGroup::Config));
        assert!(!defaults.contains(&FormatGroup::Markup));
    }

    #[test]
    fn explicit_selection_replaces_defaults_and_canonicalizes_lists() {
        assert_eq!(
            selected_groups(&json!({"formats": "config"})).unwrap(),
            BTreeSet::from([FormatGroup::Config])
        );
        let expected = BTreeSet::from([FormatGroup::Code, FormatGroup::Config]);
        for value in [
            json!("config,code"),
            json!(" code , config ,code "),
            json!(["config", "code"]),
            json!(["config,code", "config"]),
        ] {
            assert_eq!(
                selected_groups(&json!({"formats": value})).unwrap(),
                expected
            );
        }
        let all: BTreeSet<_> = FormatGroup::ALL.into_iter().collect();
        for value in [
            json!("all"),
            json!("config,all"),
            json!(["all,docs", "code"]),
        ] {
            assert_eq!(selected_groups(&json!({"formats": value})).unwrap(), all);
        }
    }

    #[test]
    fn invalid_selections_are_rejected_even_alongside_all() {
        for value in [
            json!(""),
            json!(" "),
            json!([]),
            json!([""]),
            json!("code,"),
            json!(",docs"),
            json!("code,,docs"),
            json!("json"),
            json!("Code"),
            json!("all,unknown"),
            json!(["all", ""]),
            json!(["code", null]),
            json!(["all", 1]),
            json!(true),
            json!(1),
            json!({"code": true}),
        ] {
            assert!(
                selected_groups(&json!({"formats": value})).is_err(),
                "{value}"
            );
        }
    }
}
