//! Conservative, snapshot-based call resolution and bounded graph traversal.
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    parse::{FileStructure, ImportBinding, ParsedFile, StructureNode},
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

/// Structure ids are file-local vector indexes; verify rather than scan.
fn node_by_id(structure: &FileStructure, id: usize) -> Option<&StructureNode> {
    structure.nodes.get(id).filter(|node| node.id == id)
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
        node_by_id(self.files.get(&key.path)?, key.id)
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

/// In Python and JavaScript/TypeScript a bare name is a lexical lookup: it
/// reaches enclosing functions and namespaces, never the members of an
/// enclosing class, object, or interface. Other languages keep their nested
/// member rules (Java inner classes, Go/C nested scopes, Rust nested items).
fn bare_name_reaches(language: &str, ancestor: &StructureNode) -> bool {
    !matches!(
        language,
        "python" | "javascript" | "jsx" | "typescript" | "tsx"
    ) || callable(ancestor)
        || ancestor.kind == "module"
}

/// `Foo` for `impl Foo`, `impl<T> Foo<T>`, and `impl Trait for Foo<T>`.
fn impl_type(name: &str) -> &str {
    let name = name
        .strip_prefix('<')
        .and_then(|inner| inner.split(" as ").next())
        .unwrap_or(name);
    name.split('<').next().unwrap_or(name).trim()
}

/// Whether `container` is the static receiver written as `receiver`.
fn container_named(container: &StructureNode, receiver: &str) -> bool {
    match container.kind.as_str() {
        "class" | "struct" | "module" | "interface" | "enum" => container.name == receiver,
        "impl" => impl_type(&container.name) == receiver,
        _ => false,
    }
}

/// A member reachable through its type's name rather than an instance.
fn static_member(node: &StructureNode) -> bool {
    node.kind == "constructor"
        || node.signature.contains("static")
        || node.attributes.iter().any(|attribute| {
            attribute.contains("staticmethod") || attribute.contains("classmethod")
        })
        || node.language == "rust" && node.kind == "method" && !rust_self_receiver(&node.signature)
}

/// Whether a Rust signature's parameter list starts with a `self` receiver.
fn rust_self_receiver(signature: &str) -> bool {
    let mut depth = 0usize;
    let mut parameters = None;
    for (offset, c) in signature.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            '(' if depth == 0 => {
                parameters = Some(&signature[offset + 1..]);
                break;
            }
            _ => {}
        }
    }
    let Some(parameters) = parameters else {
        return false;
    };
    let mut rest = parameters.trim_start();
    rest = rest.strip_prefix('&').unwrap_or(rest).trim_start();
    if rest.starts_with('\'') {
        rest = rest.split_once(char::is_whitespace).map_or("", |(_, r)| r);
    }
    rest = rest.trim_start();
    rest = rest.strip_prefix("mut ").unwrap_or(rest).trim_start();
    rest.strip_prefix("self")
        .is_some_and(|after| after.is_empty() || after.starts_with([',', ')', ':', ' ']))
}

/// The local name an import binds; Go packages bind their last path segment.
fn local_name<'a>(language: &str, binding: &'a ImportBinding) -> Option<&'a str> {
    binding
        .alias
        .as_deref()
        .or(binding.name.as_deref())
        .or_else(|| {
            (language == "go").then(|| binding.path.rsplit('/').next().unwrap_or(&binding.path))
        })
}

/// Whether a binding names a module/namespace whose members are reached with
/// a receiver. JavaScript named and default imports are values, so only
/// namespace (`* as`) and `require` bindings qualify; paths in the other
/// languages always name modules, packages, or types.
fn module_binding(language: &str, binding: &ImportBinding) -> bool {
    !matches!(language, "javascript" | "jsx" | "typescript" | "tsx")
        || binding.wildcard
        || binding.name.is_none()
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
    let language = owner.language.as_str();
    let ancestors: Vec<_> = std::iter::successors(owner.parent_id, |id| {
        node_by_id(structure, *id).and_then(|n| n.parent_id)
    })
    .filter_map(|id| node_by_id(structure, id))
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
            None => match node.parent_id {
                None => true,
                Some(parent_id) => ancestors
                    .iter()
                    .any(|parent| parent.id == parent_id && bare_name_reaches(language, parent)),
            },
            Some("self" | "this" | "Self" | "cls") => scope.is_some_and(|s| {
                node.parent_id == Some(s.id)
                    || node.qualified_name.starts_with(&format!("{}.", s.name))
            }),
            Some(receiver) => node
                .parent_id
                .and_then(|id| node_by_id(structure, id))
                .is_some_and(|container| container_named(container, receiver)),
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
            let referenced = receiver.unwrap_or(name);
            if local_name(language, binding) != Some(referenced)
                || (binding.wildcard && receiver.is_none())
            {
                continue;
            }
            let target_name = if receiver.is_some() {
                name
            } else {
                binding.name.as_deref().unwrap_or(name)
            };
            // `config.validate()` reaches a module-level function only through
            // a namespace binding; a value binding would need the value's type.
            let namespace = receiver.is_some() && module_binding(language, binding);
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
                for node in other
                    .nodes
                    .iter()
                    .filter(|n| callable(n) && n.name == target_name)
                {
                    let valid = match (receiver, node.parent_id) {
                        (None, parent) => parent.is_none(),
                        // Go methods are top-level declarations too, but only
                        // functions are reachable through a package name.
                        (Some(_), None) => {
                            namespace && (language != "go" || node.kind == "function")
                        }
                        (Some(receiver), Some(parent)) => {
                            let container = binding.name.as_deref().unwrap_or(receiver);
                            node_by_id(other, parent).is_some_and(|c| container_named(c, container))
                                && static_member(node)
                        }
                    };
                    if !valid {
                        continue;
                    }
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
        return rust_import_matches(from, target, module, imported_callable);
    }
    if target.ends_with(".go") {
        // Files of package `p` live in a directory named `p`; the import path's
        // last segment is the package directory for conventional layouts.
        let package = module
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(module);
        let directory = target
            .rsplit_once('/')
            .map(|(directory, _)| directory.rsplit('/').next().unwrap_or(directory))
            .unwrap_or("");
        return !package.is_empty() && directory == package;
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

/// Rust paths name items: a callable import points at its parent module's
/// file, a receiver import names either a module file or a type in its parent
/// module. `super::` paths resolve relative to the importing file's module.
fn rust_import_matches(from: &str, target: &str, module: &str, imported_callable: bool) -> bool {
    let segments: Vec<&str> = module.split("::").collect();
    let candidates: Vec<&[&str]> = if imported_callable {
        vec![&segments[..segments.len().saturating_sub(1)]]
    } else {
        vec![&segments[..], &segments[..segments.len().saturating_sub(1)]]
    };
    if segments.first() == Some(&"super") {
        let supers = segments.iter().take_while(|s| **s == "super").count();
        let Some(base) = rust_super_directory(from, supers) else {
            return false;
        };
        return candidates.into_iter().any(|segments| {
            let segments = &segments[supers.min(segments.len())..];
            rust_module_files(&base, segments)
                .iter()
                .any(|candidate| candidate == target)
        });
    }
    candidates.into_iter().any(|segments| {
        let segments: Vec<&str> = segments
            .iter()
            .copied()
            .filter(|s| !matches!(*s, "crate" | "self"))
            .collect();
        if segments.is_empty() {
            return target.ends_with("lib.rs") || target.ends_with("main.rs");
        }
        let path = segments.join("/");
        target.ends_with(&format!("{path}.rs")) || target.ends_with(&format!("{path}/mod.rs"))
    })
}

/// The directory whose files belong to the module `supers` levels above the
/// importing file's module. `b.rs` is module `b` whose parent's files share
/// its directory; `mod.rs`, `lib.rs`, and `main.rs` are their directory's own
/// module, so their parent lives one level up.
fn rust_super_directory(from: &str, supers: usize) -> Option<Vec<String>> {
    let (directory, file) = from.rsplit_once('/').unwrap_or(("", from));
    let mut directory: Vec<String> = directory
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    let stem = file.strip_suffix(".rs").unwrap_or(file);
    let climbs = supers - usize::from(!matches!(stem, "mod" | "lib" | "main"));
    for _ in 0..climbs {
        directory.pop()?;
    }
    Some(directory)
}

/// Files that can hold the module at `segments` under `base`; an empty path
/// names `base`'s own module file.
fn rust_module_files(base: &[String], segments: &[&str]) -> Vec<String> {
    let join = |parts: &[&str]| parts.join("/");
    let base_parts: Vec<&str> = base.iter().map(String::as_str).collect();
    if segments.is_empty() {
        let mut files = vec![
            join(&[&base_parts[..], &["mod.rs"]].concat()),
            join(&[&base_parts[..], &["lib.rs"]].concat()),
            join(&[&base_parts[..], &["main.rs"]].concat()),
        ];
        if let Some((name, parent)) = base_parts.split_last() {
            let file = format!("{name}.rs");
            files.push(join(&[parent, &[file.as_str()]].concat()));
        }
        return files;
    }
    let path = [&base_parts[..], segments].concat();
    vec![
        format!("{}.rs", join(&path)),
        join(&[&path[..], &["mod.rs"]].concat()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn build(files: &[(&str, &str)]) -> CallGraph {
        let parsed: Vec<_> = files
            .iter()
            .map(|(path, source)| (*path, parse(path, source).unwrap()))
            .collect();
        for (path, parsed) in &parsed {
            assert!(parsed.errors.is_empty(), "{path}: {:?}", parsed.errors);
        }
        CallGraph::from_parsed(parsed.iter().map(|(path, parsed)| (*path, parsed))).unwrap()
    }

    /// Edges as `path:qualified -> path:qualified`, sorted.
    fn edges(graph: &CallGraph) -> Vec<String> {
        let name = |key: &Key| format!("{}:{}", key.path, graph.node(key).unwrap().qualified_name);
        let mut result: Vec<_> = graph
            .edges
            .iter()
            .flat_map(|(from, to)| {
                to.iter()
                    .map(move |to| format!("{} -> {}", name(from), name(to)))
            })
            .collect();
        result.sort();
        result
    }

    #[test]
    fn node_lookup_uses_ids_as_indexes() {
        let graph = build(&[("a.rs", "fn first() {}\nfn second() { first(); }")]);
        let key = Key {
            path: "a.rs".into(),
            id: 1,
        };
        assert_eq!(graph.node(&key).unwrap().name, "second");
        assert!(
            graph
                .node(&Key {
                    path: "a.rs".into(),
                    id: 7
                })
                .is_none()
        );
        assert!(
            graph
                .node(&Key {
                    path: "missing.rs".into(),
                    id: 0
                })
                .is_none()
        );
    }

    #[test]
    fn rust_associated_functions_resolve_through_impl_blocks() {
        let graph = build(&[(
            "a.rs",
            "pub struct Foo;\nimpl Foo {\n  pub fn new() -> Self { Foo }\n  pub fn run(&self) { self.step(); }\n  fn step(&self) {}\n}\nimpl<T> Wrapper<T> {\n  pub fn create() -> Self { todo!() }\n}\npub fn build() { let foo = Foo::new(); foo.run(); Wrapper::create(); }\n",
        )]);
        assert_eq!(
            edges(&graph),
            [
                "a.rs:Foo.run -> a.rs:Foo.step",
                "a.rs:build -> a.rs:Foo.new",
                "a.rs:build -> a.rs:Wrapper<T>.create",
            ]
        );
    }

    #[test]
    fn rust_static_methods_resolve_across_files_without_self_receivers() {
        let graph = build(&[
            (
                "src/foo.rs",
                "pub struct Foo;\nimpl Foo {\n  pub fn new() -> Self { Foo }\n  pub fn with(count: usize) -> Self { Foo }\n  pub fn run(&self) {}\n  pub fn reset(&mut self) {}\n  pub fn shared(self: Box<Self>) {}\n}\n",
            ),
            (
                "src/main.rs",
                "use crate::foo::Foo;\nfn main() { let foo = Foo::new(); Foo::with(2); Foo::run(); Foo::reset(); Foo::shared(); foo.run(); }\n",
            ),
        ]);
        assert_eq!(
            edges(&graph),
            [
                "src/main.rs:main -> src/foo.rs:Foo.new",
                "src/main.rs:main -> src/foo.rs:Foo.with",
            ]
        );
        assert!(rust_self_receiver("fn run(&self)"));
        assert!(rust_self_receiver("fn run(&'a mut self, x: i32)"));
        assert!(rust_self_receiver("fn run(mut self)"));
        assert!(rust_self_receiver("fn run(self: Box<Self>)"));
        assert!(!rust_self_receiver("fn run(selfish: i32)"));
        assert!(!rust_self_receiver("fn run<F: Fn(i32)>(f: F)"));
    }

    #[test]
    fn bare_names_skip_enclosing_class_members_in_python_and_javascript() {
        let graph = build(&[
            (
                "a.py",
                "def helper():\n    pass\n\nclass Service:\n    def helper(self):\n        pass\n\n    def run(self):\n        helper()\n        self.helper()\n",
            ),
            (
                "a.ts",
                "function helper() {}\nclass Service {\n  helper() {}\n  run() { helper(); this.helper(); }\n}\nconst api = { helper() {}, run() { helper(); } };\n",
            ),
        ]);
        assert_eq!(
            edges(&graph),
            [
                "a.py:Service.run -> a.py:Service.helper",
                "a.py:Service.run -> a.py:helper",
                "a.ts:Service.run -> a.ts:Service.helper",
                "a.ts:Service.run -> a.ts:helper",
                "a.ts:api.run -> a.ts:helper",
            ]
        );
        // Nested functions and namespaces are lexical scopes.
        let graph = build(&[(
            "b.py",
            "def outer():\n    def first():\n        pass\n    def second():\n        first()\n",
        )]);
        assert_eq!(edges(&graph), ["b.py:outer.second -> b.py:outer.first"]);
    }

    #[test]
    fn java_bare_names_still_reach_enclosing_class_methods() {
        let graph = build(&[(
            "Demo.java",
            "class Demo { void helper() {} void run() { helper(); } }",
        )]);
        assert_eq!(
            edges(&graph),
            ["Demo.java:Demo.run -> Demo.java:Demo.helper"]
        );
    }

    #[test]
    fn value_receivers_do_not_resolve_as_imported_modules() {
        let graph = build(&[
            (
                "src/config.rs",
                "pub struct Config;\npub fn load() -> Config { Config }\npub fn validate() {}\nimpl Config { pub fn validate(&self) {} }\n",
            ),
            (
                "src/main.rs",
                "use crate::config;\nfn main() { let config = config::load(); config.validate(); }\n",
            ),
            (
                "lib/config.js",
                "export function validate() {}\nexport default { validate };\n",
            ),
            (
                "lib/named.js",
                "import { config } from './config';\nexport function run() { config.validate(); }\n",
            ),
            (
                "lib/namespace.js",
                "import * as config from './config';\nexport function run() { config.validate(); }\n",
            ),
            (
                "lib/required.ts",
                "import config = require('./config');\nexport function run() { config.validate(); }\n",
            ),
        ]);
        assert_eq!(
            edges(&graph),
            [
                "lib/namespace.js:run -> lib/config.js:validate",
                "lib/required.ts:run -> lib/config.js:validate",
                "src/main.rs:main -> src/config.rs:load",
            ]
        );
    }

    #[test]
    fn rust_super_imports_resolve_relative_to_the_importing_module() {
        let graph = build(&[
            ("src/util.rs", "pub fn shared() {}\n"),
            ("src/a/mod.rs", "pub fn parent_helper() {}\n"),
            ("src/a/b.rs", "pub fn deep() {}\n"),
            (
                "src/a/c.rs",
                "use super::b;\nuse super::parent_helper;\nuse super::super::util::shared;\nuse super::super::util;\npub fn run() { b::deep(); parent_helper(); shared(); util::shared(); }\n",
            ),
            (
                "src/a/d/mod.rs",
                "use super::b::deep;\npub fn run() { deep(); }\n",
            ),
            ("src/other/util.rs", "pub fn shared() {}\n"),
        ]);
        assert_eq!(
            edges(&graph),
            [
                "src/a/c.rs:run -> src/a/b.rs:deep",
                "src/a/c.rs:run -> src/a/mod.rs:parent_helper",
                "src/a/c.rs:run -> src/util.rs:shared",
                "src/a/d/mod.rs:run -> src/a/b.rs:deep",
            ]
        );
        assert_eq!(
            rust_super_directory("src/a/b.rs", 1),
            Some(vec!["src".to_owned(), "a".to_owned()])
        );
        assert_eq!(
            rust_super_directory("src/a/mod.rs", 1),
            Some(vec!["src".to_owned()])
        );
        assert_eq!(rust_super_directory("lib.rs", 1), None);
    }

    #[test]
    fn go_imports_resolve_package_functions_by_path_segment() {
        let graph = build(&[
            (
                "config/config.go",
                "package config\n\nfunc Load() {}\n\ntype Store struct{}\n\nfunc (s Store) Load() {}\n\nfunc Validate() {}\n",
            ),
            (
                "cmd/main.go",
                "package main\n\nimport (\n\t\"fmt\"\n\t\"example.com/app/config\"\n\tcfg \"example.com/app/config\"\n)\n\nfunc main() { config.Load(); cfg.Validate(); fmt.Println() }\n",
            ),
            (
                "other/config/x.go",
                "package config\n\nfunc Validate() {}\n",
            ),
        ]);
        // `Load` ignores the same-named method; `Validate` is ambiguous between
        // two `config` directories and is dropped.
        assert_eq!(edges(&graph), ["cmd/main.go:main -> config/config.go:Load"]);
    }
}
