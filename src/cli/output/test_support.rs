use serde_json::{Value, json};

pub(super) fn function(id: &str) -> Value {
    json!({"id": id, "path": format!("src/{id}.rs"), "qualifiedName": id, "startLine": 1, "startColumn": 1})
}

pub(super) fn edge(a: &str, b: &str, similarity: f64) -> Value {
    json!({"source": function(a), "matches": [{"function": function(b), "similarity": similarity}]})
}
