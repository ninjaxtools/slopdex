//! Conservative, snapshot-based call resolution and bounded graph traversal.
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    parse::{FileStructure, ParsedFile, StructureNode},
    storage::{Database, STRUCTURE_PARSER_VERSION},
};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Key {
    pub path: String,
    pub id: usize,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Depth {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callee: Option<usize>,
}

#[derive(Default)]
pub struct Expansion {
    pub depths: BTreeMap<Key, Depth>,
    pub comments: BTreeMap<Key, Vec<String>>,
}

#[derive(Serialize, Deserialize)]
pub struct CallGraph {
    pub files: BTreeMap<String, FileStructure>,
    #[serde(with = "edge_map")]
    edges: BTreeMap<Key, BTreeSet<Key>>,
    #[serde(with = "edge_map")]
    reverse: BTreeMap<Key, BTreeSet<Key>>,
}

mod edge_map {
    use super::*;

    pub(super) fn serialize<S: serde::Serializer>(
        edges: &BTreeMap<Key, BTreeSet<Key>>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        edges.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<BTreeMap<Key, BTreeSet<Key>>, D::Error> {
        let entries = Vec::<(Key, BTreeSet<Key>)>::deserialize(deserializer)?;
        let count = entries.len();
        let edges: BTreeMap<_, _> = entries.into_iter().collect();
        if edges.len() != count {
            return Err(serde::de::Error::custom("Duplicate call graph key"));
        }
        Ok(edges)
    }
}

fn callable(node: &StructureNode) -> bool {
    matches!(
        node.kind.as_str(),
        "function" | "method" | "constructor" | "generator"
    )
}

impl CallGraph {
    pub fn load(db: &Database) -> Result<Self> {
        let mut indexed = BTreeSet::new();
        let mut stmt = db.conn.prepare(
            "SELECT path,symbol_id FROM search_units WHERE kind='function' AND symbol_id IS NOT NULL",
        )?;
        for row in stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })? {
            let (path, id) = row?;
            indexed.insert(Key {
                path,
                id: usize::try_from(id)?,
            });
        }
        let mut files = BTreeMap::new();
        for path in db.paths()? {
            // A stale graph would silently point at declarations from a different
            // parser generation, particularly under --no-reindex.
            let (hash, version): (String, Option<String>) = db.conn.query_row(
                "SELECT hash,parser_version FROM files WHERE path=?",
                [&path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            ensure!(
                db.structure_current(&path, &hash, STRUCTURE_PARSER_VERSION)?
                    && version.as_deref() == Some(STRUCTURE_PARSER_VERSION),
                "Call graph requires current structure for {path}; run without --no-reindex"
            );
            files.insert(path.clone(), db.structure(&path)?);
        }
        Ok(Self::from_parts(files, indexed))
    }

    pub(crate) fn from_parsed<'a>(
        files: impl IntoIterator<Item = (&'a str, &'a ParsedFile)>,
    ) -> Result<Self> {
        let mut structures = BTreeMap::new();
        let mut indexed = BTreeSet::new();
        for (path, parsed) in files {
            for callable in &parsed.callables {
                if let Some(node) =
                    crate::map::matching_node(&parsed.structure, &serde_json::to_value(callable)?)
                {
                    indexed.insert(Key {
                        path: path.to_owned(),
                        id: node.id,
                    });
                }
            }
            structures.insert(path.to_owned(), parsed.structure.clone());
        }
        Ok(Self::from_parts(structures, indexed))
    }

    fn from_parts(files: BTreeMap<String, FileStructure>, indexed: BTreeSet<Key>) -> Self {
        let mut graph = Self {
            files,
            edges: BTreeMap::new(),
            reverse: BTreeMap::new(),
        };
        for (path, structure) in &graph.files {
            for node in structure.nodes.iter().filter(|n| callable(n)) {
                let from = Key {
                    path: path.clone(),
                    id: node.id,
                };
                if !indexed.contains(&from) {
                    continue;
                }
                for site in &node.calls {
                    if let Some(to) = resolve(
                        &graph.files,
                        &indexed,
                        path,
                        node,
                        &site.name,
                        site.receiver.as_deref(),
                    ) {
                        graph
                            .edges
                            .entry(from.clone())
                            .or_default()
                            .insert(to.clone());
                        graph.reverse.entry(to).or_default().insert(from.clone());
                    }
                }
            }
        }
        graph
    }

    pub fn node(&self, key: &Key) -> Option<&StructureNode> {
        self.files
            .get(&key.path)?
            .nodes
            .iter()
            .find(|node| node.id == key.id)
    }

    pub fn expand(
        &self,
        seeds: impl IntoIterator<Item = Key>,
        callers: usize,
        callees: usize,
    ) -> Expansion {
        let mut result = Expansion::default();
        let seeds: BTreeSet<_> = seeds
            .into_iter()
            .filter(|k| self.node(k).is_some_and(callable))
            .collect();
        for (outgoing, limit) in [(false, callers), (true, callees)] {
            if limit == 0 {
                continue;
            }
            let mut queue: VecDeque<_> = seeds.iter().cloned().map(|k| (k, 0)).collect();
            let mut visited = HashSet::new();
            while let Some((key, level)) = queue.pop_front() {
                if !visited.insert(key.clone()) {
                    continue;
                }
                let depth = result.depths.entry(key.clone()).or_default();
                if outgoing {
                    depth.callee = Some(level);
                } else {
                    depth.caller = Some(level);
                }
                if level == limit {
                    continue;
                }
                let neighbors = if outgoing {
                    self.edges.get(&key)
                } else {
                    self.reverse.get(&key)
                };
                for neighbor in neighbors.into_iter().flatten() {
                    queue.push_back((neighbor.clone(), level + 1));
                }
            }
        }
        for (caller, callees) in &self.edges {
            if !result.depths.contains_key(caller) {
                continue;
            }
            for callee in callees {
                if result.depths.contains_key(callee)
                    && let Some(node) = self.node(callee)
                {
                    result.comments.entry(caller.clone()).or_default().push(
                        crate::map::symbol_location(
                            &callee.path,
                            node.start_line,
                            node.end_line,
                            &node.qualified_name,
                        ),
                    );
                }
            }
        }
        result
    }

    /// Nodes reached by an actual edge within the requested code depths. A
    /// node can be a seed and still be another seed's caller or callee.
    pub fn code_keys(
        &self,
        expansion: &Expansion,
        callers: usize,
        callees: usize,
    ) -> BTreeSet<Key> {
        let mut result = BTreeSet::new();
        for (key, depth) in &expansion.depths {
            if depth.caller.is_some_and(|n| n < callers) {
                result.extend(self.reverse.get(key).into_iter().flatten().cloned());
            }
            if depth.callee.is_some_and(|n| n < callees) {
                result.extend(self.edges.get(key).into_iter().flatten().cloned());
            }
        }
        result
    }
}

fn resolve(
    files: &BTreeMap<String, FileStructure>,
    indexed: &BTreeSet<Key>,
    path: &str,
    owner: &StructureNode,
    name: &str,
    receiver: Option<&str>,
) -> Option<Key> {
    let structure = files.get(path)?;
    let ancestors: Vec<_> = std::iter::successors(owner.parent_id, |id| {
        structure
            .nodes
            .iter()
            .find(|n| n.id == *id)
            .and_then(|n| n.parent_id)
    })
    .filter_map(|id| structure.nodes.iter().find(|n| n.id == id))
    .collect();
    let scope = ancestors.iter().find(|n| {
        matches!(
            n.kind.as_str(),
            "class" | "struct" | "impl" | "interface" | "module"
        )
    });
    let mut candidates = BTreeSet::new();
    for node in structure
        .nodes
        .iter()
        .filter(|n| callable(n) && n.name == name)
    {
        let valid = match receiver {
            None => {
                node.parent_id.is_none()
                    || ancestors
                        .iter()
                        .any(|parent| Some(parent.id) == node.parent_id)
            }
            Some("self" | "this" | "Self" | "cls") => scope.is_some_and(|s| {
                node.parent_id == Some(s.id)
                    || node.qualified_name.starts_with(&format!("{}.", s.name))
            }),
            Some(receiver) => structure.nodes.iter().any(|container| {
                matches!(
                    container.kind.as_str(),
                    "class" | "struct" | "module" | "interface"
                ) && container.name == receiver
                    && node.parent_id == Some(container.id)
            }),
        };
        if valid {
            let key = Key {
                path: path.to_owned(),
                id: node.id,
            };
            if indexed.contains(&key) {
                candidates.insert(key);
            }
        }
    }
    if receiver.is_none() && candidates.is_empty() {
        for class in structure
            .nodes
            .iter()
            .filter(|n| n.kind == "class" && n.name == name)
        {
            for constructor in structure.nodes.iter().filter(|n| {
                n.parent_id == Some(class.id) && n.kind == "constructor" && n.name == "__init__"
            }) {
                let key = Key {
                    path: path.to_owned(),
                    id: constructor.id,
                };
                if indexed.contains(&key) {
                    candidates.insert(key);
                }
            }
        }
    }
    // Explicit imports are the only basis for a cross-file edge. Do not guess
    // using a globally unique bare name or an unknown runtime receiver.
    for import in structure.nodes.iter().filter(|n| n.kind == "import") {
        for binding in &import.imports {
            let local = binding.alias.as_deref().or(binding.name.as_deref());
            let referenced = receiver.unwrap_or(name);
            if local != Some(referenced) || (binding.wildcard && receiver.is_none()) {
                continue;
            }
            let target_name = if receiver.is_some() {
                name
            } else {
                binding.name.as_deref().unwrap_or(name)
            };
            for (other_path, other) in files {
                if other_path == path
                    || !import_path_matches(
                        path,
                        other_path,
                        binding.source.as_deref(),
                        &binding.path,
                        receiver.is_none(),
                    )
                {
                    continue;
                }
                for node in other.nodes.iter().filter(|n| {
                    callable(n)
                        && n.name == target_name
                        && (n.parent_id.is_none()
                            || receiver.is_some_and(|r| {
                                n.qualified_name
                                    == format!(
                                        "{}.{target_name}",
                                        binding.name.as_deref().unwrap_or(r)
                                    )
                                    && (n.kind == "constructor" || n.signature.contains("static"))
                            }))
                }) {
                    let key = Key {
                        path: other_path.clone(),
                        id: node.id,
                    };
                    if indexed.contains(&key) {
                        candidates.insert(key);
                    }
                }
            }
        }
    }
    (candidates.len() == 1)
        .then(|| candidates.into_iter().next())
        .flatten()
}

fn import_path_matches(
    from: &str,
    target: &str,
    module: Option<&str>,
    binding: &str,
    imported_callable: bool,
) -> bool {
    let module = module.unwrap_or(binding);
    if module.starts_with('.')
        && (from.ends_with(".js")
            || from.ends_with(".mjs")
            || from.ends_with(".cjs")
            || from.ends_with(".jsx")
            || from.ends_with(".ts")
            || from.ends_with(".mts")
            || from.ends_with(".cts")
            || from.ends_with(".tsx"))
    {
        let base = std::path::Path::new(from)
            .parent()
            .unwrap_or(std::path::Path::new(""));
        let joined = base.join(module);
        let mut parts = Vec::new();
        for part in joined.components() {
            match part {
                std::path::Component::ParentDir => {
                    parts.pop();
                }
                std::path::Component::Normal(s) => parts.push(s.to_string_lossy().to_string()),
                _ => {}
            }
        }
        let prefix = parts.join("/");
        return [
            ".js",
            ".mjs",
            ".cjs",
            ".jsx",
            ".ts",
            ".mts",
            ".cts",
            ".tsx",
            "/index.js",
            "/index.ts",
        ]
        .iter()
        .any(|ext| target == format!("{prefix}{ext}"))
            || target == prefix;
    }
    if target.ends_with(".py") {
        let path = module.trim_start_matches('.').replace('.', "/");
        return target == format!("{path}.py")
            || target.ends_with(&format!("/{path}.py"))
            || target == format!("{path}/__init__.py");
    }
    if target.ends_with(".rs") {
        let module = module
            .strip_prefix("crate::")
            .or_else(|| module.strip_prefix("self::"))
            .unwrap_or(module);
        let module = if imported_callable {
            module
                .rsplit_once("::")
                .map(|(prefix, _)| prefix)
                .unwrap_or(module)
        } else {
            module
        };
        let segments = module.replace("::", "/");
        return target.ends_with(&format!("{segments}.rs"))
            || target.ends_with(&format!("{segments}/mod.rs"));
    }
    if target.ends_with(".java") {
        let module = if imported_callable {
            module
                .rsplit_once('.')
                .map(|(prefix, _)| prefix)
                .unwrap_or(module)
        } else {
            module
        };
        return target.ends_with(&format!("{}.java", module.replace('.', "/")));
    }
    false
}
