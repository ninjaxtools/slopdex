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
    lines: u64,
}

impl Cluster {
    fn rank(&self) -> f64 {
        self.max * self.lines as f64
    }
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
        let name = function["qualifiedName"]
            .as_str()
            .unwrap_or_else(|| text(function, "name"));
        crate::map::symbol_location(text(function, "path"), start as usize, end as usize, name)
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

fn covered_lines(members: &[ClusterMember]) -> u64 {
    let mut files = BTreeMap::<(&str, &str), Vec<(u64, u64)>>::new();
    for member in members {
        let function = &member.function;
        let start = function["startLine"].as_u64().unwrap_or(1).max(1);
        let mut end = function["endLine"].as_u64().unwrap_or(start).max(start);
        // Tree-sitter ranges end exclusively; column 1 covers none of that line.
        if end > start && function["endColumn"].as_u64() == Some(1) {
            end -= 1;
        }
        files
            .entry((member.role, text(function, "path")))
            .or_default()
            .push((start, end));
    }
    let mut lines = 0_u64;
    for ranges in files.values_mut() {
        ranges.sort_unstable();
        let mut previous_end = 0;
        for &(start, end) in ranges.iter() {
            if end > previous_end {
                lines = lines.saturating_add(end - start.max(previous_end + 1) + 1);
                previous_end = end;
            }
        }
    }
    lines
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
            lines: 0,
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
        cluster.lines = covered_lines(&cluster.members);
        result.push(cluster);
    }
    result.sort_by(|a, b| {
        b.rank()
            .total_cmp(&a.rank())
            .then_with(|| b.max.total_cmp(&a.max))
            .then_with(|| b.lines.cmp(&a.lines))
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
            "*** Cluster {} · {} symbols · {} lines · similarity {range}",
            index + 1,
            cluster.members.len(),
            cluster.lines,
        )?;
        let locations: Vec<_> = cluster
            .members
            .iter()
            .map(|member| {
                if same_index {
                    member.location()
                } else {
                    format!("{} [{}]", member.location(), member.role)
                }
            })
            .collect();
        for (path, symbols) in
            crate::map::group_symbol_locations(locations.iter().map(String::as_str))
        {
            if let [symbol] = symbols.as_slice() {
                writeln!(out, "{path}:{symbol}")?;
                continue;
            }
            writeln!(out, "{path}:")?;
            for symbol in symbols {
                writeln!(out, "  {symbol}")?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "clusters_tests.rs"]
mod tests;
