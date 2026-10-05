use super::{array, number, text};
use anyhow::Result;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Write,
};

fn function_location(function: &Value) -> String {
    let name = function["qualifiedName"]
        .as_str()
        .unwrap_or_else(|| text(function, "name"));
    format!(
        "{}:{}:{} :: {name}",
        text(function, "path"),
        function["startLine"].as_u64().unwrap_or(1),
        function["startColumn"].as_u64().unwrap_or(1)
    )
}

#[derive(Debug)]
struct Cluster {
    members: Vec<ClusterMember>,
    min: f64,
    max: f64,
}

#[derive(Clone, Debug)]
struct ClusterMember {
    function: Value,
    role: &'static str,
}

impl ClusterMember {
    fn location(&self) -> String {
        let function = &self.function;
        let start = function["startLine"].as_u64().unwrap_or(1);
        let end = function["endLine"].as_u64().unwrap_or(start);
        let range = if end > start {
            format!("{start}-{end}")
        } else {
            start.to_string()
        };
        let name = function["qualifiedName"]
            .as_str()
            .unwrap_or_else(|| text(function, "name"));
        format!("{}:{range}:{name}", text(function, "path"))
    }

    fn label(&self) -> String {
        format!(
            "{}{}",
            if self.role == "index" {
                ""
            } else if self.role == "source" {
                "[source] "
            } else {
                "[target] "
            },
            function_location(&self.function)
        )
    }

    fn source_key(&self) -> (u8, &str, u64, u64, &str) {
        (
            u8::from(self.role == "target"),
            text(&self.function, "path"),
            self.function["startLine"].as_u64().unwrap_or(1),
            self.function["startColumn"].as_u64().unwrap_or(1),
            text(&self.function, "qualifiedName"),
        )
    }
}

fn node_key(function: &Value, role: &str) -> String {
    let id = function
        .get("id")
        .filter(|id| !id.is_null())
        .map(Value::to_string)
        .unwrap_or_else(|| function_location(function));
    format!("{role}:{id}")
}

fn clusters(rows: &[Value], same_index: bool) -> Vec<Cluster> {
    let mut nodes = BTreeMap::<String, ClusterMember>::new();
    let mut neighbors = HashMap::<String, Vec<(String, f64)>>::new();
    for row in rows {
        let source = &row["source"];
        let left = node_key(source, if same_index { "index" } else { "source" });
        for item in array(&row["matches"]) {
            let function = &item["function"];
            let right = node_key(function, if same_index { "index" } else { "target" });
            if left == right {
                continue;
            }
            let score = number(item, "similarity");
            nodes.entry(left.clone()).or_insert_with(|| ClusterMember {
                function: source.clone(),
                role: if same_index { "index" } else { "source" },
            });
            nodes.entry(right.clone()).or_insert_with(|| ClusterMember {
                function: function.clone(),
                role: if same_index { "index" } else { "target" },
            });
            neighbors
                .entry(left.clone())
                .or_default()
                .push((right.clone(), score));
            neighbors
                .entry(right)
                .or_default()
                .push((left.clone(), score));
        }
    }
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for start in nodes.keys() {
        if !seen.insert(start.clone()) {
            continue;
        }
        let mut pending = vec![start.clone()];
        let mut cluster = Cluster {
            members: Vec::new(),
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        };
        while let Some(key) = pending.pop() {
            cluster.members.push(nodes[&key].clone());
            for (neighbor, score) in &neighbors[&key] {
                cluster.min = cluster.min.min(*score);
                cluster.max = cluster.max.max(*score);
                if seen.insert(neighbor.clone()) {
                    pending.push(neighbor.clone());
                }
            }
        }
        cluster
            .members
            .sort_by(|a, b| a.source_key().cmp(&b.source_key()));
        result.push(cluster);
    }
    result.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then_with(|| a.members[0].label().cmp(&b.members[0].label()))
    });
    result
}

pub(super) fn print_clusters(
    out: &mut impl Write,
    rows: &[Value],
    same_index: bool,
    limit: Option<usize>,
) -> Result<()> {
    let clusters = clusters(rows, same_index);
    if clusters.is_empty() {
        writeln!(out, "No clusters.")?;
    }
    for (index, cluster) in clusters
        .iter()
        .take(limit.unwrap_or(usize::MAX))
        .enumerate()
    {
        if index > 0 {
            writeln!(out)?;
        }
        let range = if cluster.min == cluster.max {
            format!("{:.2}", cluster.min)
        } else {
            format!("{:.2}-{:.2}", cluster.min, cluster.max)
        };
        writeln!(
            out,
            "*** Cluster {} · {} symbols · similarity {range}",
            index + 1,
            cluster.members.len(),
        )?;
        for member in &cluster.members {
            if same_index {
                writeln!(out, "{}", member.location())?;
            } else {
                writeln!(out, "{} [{}]", member.location(), member.role)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "clusters_tests.rs"]
mod tests;
