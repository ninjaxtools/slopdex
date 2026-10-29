use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::models::Vector;

use super::vector::{Configuration, model};
use super::{Context, JINA, string};

pub(super) struct JinaVector {
    context: Arc<Context>,
    config: Configuration,
    tasks: Option<(String, String)>,
}

/// Task family for the `task` request field: `code`/`retrieval`/`nl2code`/...
/// expand to `<family>.query`/`<family>.passage`, `text-matching` is symmetric,
/// and `none` omits the field (v2 models reject it). Defaults follow the model
/// generation; other models must configure `jinaTask` explicitly.
pub(super) fn tasks(config: &Value, model: &str) -> Result<Option<(String, String)>> {
    let family = match string(config, &["jinaTask"])? {
        Some(task) => {
            ensure!(
                task.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "jinaTask must be a lowercase task family such as code, retrieval, text-matching, or none"
            );
            task.to_owned()
        }
        None if model.starts_with("jina-embeddings-v4") => "code".to_owned(),
        None if model.starts_with("jina-embeddings-v3") => "retrieval".to_owned(),
        None if model.starts_with("jina-embeddings-v2") => "none".to_owned(),
        None => bail!(
            "Unknown Jina model family for {model}; set jinaTask (for example code, retrieval, or none)"
        ),
    };
    Ok(match family.as_str() {
        "none" => None,
        "text-matching" | "separation" | "classification" => Some((family.clone(), family)),
        _ => Some((format!("{family}.query"), format!("{family}.passage"))),
    })
}

impl JinaVector {
    pub(super) fn new(context: Arc<Context>) -> Result<Self> {
        let model = model(&context.config, "jina", "jina-embeddings-v4")?;
        let tasks = tasks(&context.config, &model)?;
        let config = Configuration::new(&context.config, model, 1024, 64, JINA)?;
        Ok(Self {
            context,
            config,
            tasks,
        })
    }
}

impl Vector for JinaVector {
    fn profile(&self) -> Value {
        let mut profile = self.config.profile("jina");
        // Only non-default task selection changes identity; default profiles stay stable.
        if self.context.config["jinaTask"].is_string() {
            profile["task"] = self.context.config["jinaTask"].clone();
        }
        profile
    }

    fn dimensions(&self) -> usize {
        self.config.dimensions
    }

    fn batch_limit(&self) -> usize {
        self.config.batch_size
    }

    fn embed(&self, inputs: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        self.config.check_batch(inputs.len())?;
        let mut body = json!({"model": self.config.model, "input": inputs,
            "dimensions": self.config.dimensions, "embedding_type": "float", "truncate": true});
        if let Some((query_task, passage_task)) = &self.tasks {
            body["task"] = json!(if query { query_task } else { passage_task });
        }
        self.config
            .request_embeddings(&self.context, "jina", &body, inputs.len())
    }
}
