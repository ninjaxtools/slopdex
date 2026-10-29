//! Conservative, snapshot-based call resolution and bounded graph traversal.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    parse::{FileStructure, ImportBinding, ParsedFile, ReferenceSite, StructureNode},
    storage::{Database, STRUCTURE_PARSER_VERSION},
};

/// Outgoing symbol comments are independent of bounded callable traversal.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Refs {
    #[default]
    None,
    All,
    CrossFile,
}

impl std::str::FromStr for Refs {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "all" => Ok(Self::All),
            "cross-file" => Ok(Self::CrossFile),
            _ => anyhow::bail!("Invalid refs mode {value:?}; expected none, all, or cross-file"),
        }
    }
}

impl Refs {
    pub fn resolve(value: Option<&str>, callers: usize, callees: usize) -> Result<Self> {
        value.map(str::parse).unwrap_or_else(|| {
            Ok(if callers > 0 || callees > 0 {
                Self::CrossFile
            } else {
                Self::None
            })
        })
    }

    pub fn enabled(self) -> bool {
        self != Self::None
    }
}

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
}

#[derive(Serialize, Deserialize)]
pub struct CallGraph {
    #[serde(with = "arc_files")]
    pub files: BTreeMap<String, std::sync::Arc<FileStructure>>,
    #[serde(with = "edge_map")]
    edges: BTreeMap<Key, BTreeSet<Key>>,
    #[serde(with = "edge_map")]
    reverse: BTreeMap<Key, BTreeSet<Key>>,
    #[serde(default, with = "edge_map")]
    references: BTreeMap<Key, BTreeSet<Key>>,
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

/// Structures are shared with the engine's cache; serialization is the same
/// map shape as owned structures, so cached graphs stay readable.
mod arc_files {
    use super::*;

    pub(super) fn serialize<S: serde::Serializer>(
        files: &BTreeMap<String, std::sync::Arc<FileStructure>>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        files
            .iter()
            .map(|(path, structure)| (path, &**structure))
            .collect::<BTreeMap<_, _>>()
            .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<BTreeMap<String, std::sync::Arc<FileStructure>>, D::Error> {
        Ok(
            BTreeMap::<String, FileStructure>::deserialize(deserializer)?
                .into_iter()
                .map(|(path, structure)| (path, std::sync::Arc::new(structure)))
                .collect(),
        )
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
        // A stale graph would silently point at declarations from a different
        // parser generation, particularly under --no-reindex.
        let files = db.all_structures(STRUCTURE_PARSER_VERSION)?;
        for path in db.paths()? {
            ensure!(
                files.contains_key(&path),
                "Call graph requires current structure for {path}; run without --no-reindex"
            );
        }
        let files = files
            .into_iter()
            .map(|(path, structure)| (path, std::sync::Arc::new(structure)))
            .collect();
        Ok(Self::from_structures(files, Self::indexed_keys(db)?))
    }

    /// Search-indexed callable keys: only these can be call-graph edge ends.
    pub fn indexed_keys(db: &Database) -> Result<BTreeSet<Key>> {
        let mut indexed = BTreeSet::new();
        let mut stmt = db.conn.prepare_cached(
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
        Ok(indexed)
    }

    pub(crate) fn from_structures(
        files: BTreeMap<String, std::sync::Arc<FileStructure>>,
        indexed: BTreeSet<Key>,
    ) -> Self {
        Self::from_parts(files, indexed)
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
            structures.insert(
                path.to_owned(),
                std::sync::Arc::new(parsed.structure.clone()),
            );
        }
        Ok(Self::from_parts(structures, indexed))
    }

    fn from_parts(
        files: BTreeMap<String, std::sync::Arc<FileStructure>>,
        indexed: BTreeSet<Key>,
    ) -> Self {
        let mut graph = Self {
            files,
            edges: BTreeMap::new(),
            reverse: BTreeMap::new(),
            references: BTreeMap::new(),
        };
        let Self {
            files,
            edges,
            reverse,
            references,
        } = &mut graph;
        let files: &BTreeMap<String, std::sync::Arc<FileStructure>> = files;
        let mut resolver = Resolver::new(files, &indexed);
        let progress = crate::ui::counted("Resolving calls and references", files.len());
        for (path, structure) in files {
            for node in &structure.nodes {
                for site in &node.references {
                    if let Some(to) = resolver.resolve_reference(path, node, site) {
                        references
                            .entry(Key {
                                path: path.clone(),
                                id: node.id,
                            })
                            .or_default()
                            .insert(to);
                    }
                }
            }
            for node in structure.nodes.iter().filter(|n| callable(n)) {
                let from = Key {
                    path: path.clone(),
                    id: node.id,
                };
                if !indexed.contains(&from) {
                    continue;
                }
                for site in &node.calls {
                    if let Some(to) =
                        resolver.resolve(path, node, &site.name, site.receiver.as_deref())
                    {
                        // Shell command lookup has a separate namespace from
                        // variables. Its static callable sites already supply
                        // the conservative function references we can support.
                        if node.language == "bash" {
                            references
                                .entry(from.clone())
                                .or_default()
                                .insert(to.clone());
                        }
                        edges.entry(from.clone()).or_default().insert(to.clone());
                        reverse.entry(to).or_default().insert(from.clone());
                    }
                }
            }
            progress.inc(1);
        }
        progress.finish();
        graph
    }

    pub fn node(&self, key: &Key) -> Option<&StructureNode> {
        node_by_id(self.files.get(&key.path)?, key.id)
    }

    /// Resolved outgoing symbol locations, regardless of traversal membership.
    pub fn references(&self, key: &Key, refs: Refs) -> Vec<String> {
        self.reference_locations(key, refs, false)
    }

    /// Comment labels distinguish actual invocations from function values without
    /// changing the symbol identities returned in JSON.
    pub(crate) fn reference_labels(&self, key: &Key, refs: Refs) -> Vec<String> {
        self.reference_locations(key, refs, true)
    }

    pub(crate) fn reference_paths(&self, key: &Key, refs: Refs) -> Vec<String> {
        self.references
            .get(key)
            .into_iter()
            .flatten()
            .filter(|target| refs.enabled() && (refs != Refs::CrossFile || target.path != key.path))
            .map(|target| target.path.clone())
            .collect()
    }

    fn reference_locations(&self, key: &Key, refs: Refs, label_invocations: bool) -> Vec<String> {
        if !refs.enabled() {
            return Vec::new();
        }
        self.references
            .get(key)
            .into_iter()
            .flatten()
            .filter(|target| refs != Refs::CrossFile || target.path != key.path)
            .filter_map(|target| {
                self.node(target).map(|node| {
                    let name = if label_invocations
                        && self
                            .edges
                            .get(key)
                            .is_some_and(|calls| calls.contains(target))
                    {
                        format!("{}()", node.qualified_name)
                    } else {
                        node.qualified_name.clone()
                    };
                    crate::map::symbol_location(&target.path, node.start_line, node.end_line, &name)
                })
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
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
    module_binding_parts(language, binding.wildcard, binding.name.is_none())
}

fn module_binding_parts(language: &str, wildcard: bool, name_none: bool) -> bool {
    !matches!(language, "javascript" | "jsx" | "typescript" | "tsx") || wildcard || name_none
}

/// Per-file candidate indexes built once per graph construction. Resolution
/// previously rescanned a file's declarations for every call/reference site
/// and rescanned every file for every import binding a site touched, which
/// is quadratic in repository size.
struct FileIndex<'a> {
    imports: Vec<&'a StructureNode>,
    /// Nodes by declared name, every alias, and the synthetic `default` key
    /// for default-exported declarations. Consumers re-apply their exact
    /// name predicate, so this is a safe superset for each of them.
    by_name: HashMap<&'a str, Vec<&'a StructureNode>>,
    by_qualified: HashMap<&'a str, Vec<&'a StructureNode>>,
    children: HashMap<usize, Vec<&'a StructureNode>>,
}

impl<'a> FileIndex<'a> {
    fn build(structure: &'a FileStructure) -> Self {
        let mut index = Self {
            imports: Vec::new(),
            by_name: HashMap::new(),
            by_qualified: HashMap::new(),
            children: HashMap::new(),
        };
        for node in &structure.nodes {
            if node.kind == "import" {
                index.imports.push(node);
            }
            index
                .by_name
                .entry(node.name.as_str())
                .or_default()
                .push(node);
            for name in &node.names {
                index.by_name.entry(name.as_str()).or_default().push(node);
            }
            if node.attributes.iter().any(|attr| attr == "default") {
                index.by_name.entry("default").or_default().push(node);
            }
            index
                .by_qualified
                .entry(node.qualified_name.as_str())
                .or_default()
                .push(node);
            if let Some(parent) = node.parent_id {
                index.children.entry(parent).or_default().push(node);
            }
        }
        index
    }

    fn named(&self, name: &str) -> &[&'a StructureNode] {
        self.by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    fn bindings(&self) -> Vec<&'a ImportBinding> {
        self.imports.iter().flat_map(|node| &node.imports).collect()
    }
}

/// Everything target-file selection for one import depends on. Two bindings
/// with equal keys select identical files, so the all-files scan runs once
/// per distinct key instead of once per call/reference site.
#[derive(Clone, Hash, PartialEq, Eq)]
struct TargetKey {
    from: String,
    language: String,
    source: Option<String>,
    binding_path: String,
    wildcard: bool,
    binding_name_none: bool,
    kind: TargetKind,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum TargetKind {
    Reference { receiver: bool },
    Call { imported_callable: bool },
    Header,
}

impl TargetKey {
    fn new(from: &str, language: &str, binding: &ImportBinding, kind: TargetKind) -> Self {
        Self {
            from: from.to_owned(),
            language: language.to_owned(),
            source: binding.source.clone(),
            binding_path: binding.path.clone(),
            wildcard: binding.wildcard,
            binding_name_none: binding.name.is_none(),
            kind,
        }
    }
}

enum Pattern {
    Import(ImportPattern),
    Rust(RustPattern),
    Header(HeaderPattern),
}

impl Pattern {
    fn matches(&self, target: &str) -> bool {
        match self {
            Self::Import(pattern) => pattern.matches(target),
            Self::Rust(pattern) => pattern.matches(target),
            Self::Header(pattern) => pattern.matches(target),
        }
    }

    /// Indexed paths that could match; callers re-verify with `matches`, so
    /// this only has to be a superset of the true matches.
    fn candidates<'a>(
        &self,
        paths: &PathIndex<'a>,
        files: &'a BTreeMap<String, std::sync::Arc<FileStructure>>,
    ) -> Vec<&'a str> {
        match self {
            Self::Import(pattern) => pattern.candidates(paths, files),
            Self::Rust(pattern) => pattern.candidates(paths, files),
            Self::Header(pattern) => pattern.candidates(paths, files),
        }
    }
}

/// Exact, suffix, and Go-directory lookups over indexed paths, so an
/// import's target files are found without scanning every path in the
/// repository (the remaining superlinear term in graph construction).
struct PathIndex<'a> {
    /// Paths sorted by reversed chars; a suffix query is a prefix range.
    reversed: Vec<(String, &'a str)>,
    go_directories: HashMap<&'a str, Vec<&'a str>>,
}

impl<'a> PathIndex<'a> {
    fn build(files: &'a BTreeMap<String, std::sync::Arc<FileStructure>>) -> Self {
        let mut reversed: Vec<(String, &'a str)> = files
            .keys()
            .map(|path| (path.chars().rev().collect(), path.as_str()))
            .collect();
        reversed.sort();
        let mut go_directories: HashMap<&'a str, Vec<&'a str>> = HashMap::new();
        for path in files.keys().filter(|path| path.ends_with(".go")) {
            go_directories
                .entry(go_directory(path))
                .or_default()
                .push(path.as_str());
        }
        Self {
            reversed,
            go_directories,
        }
    }

    fn suffix<'b>(&'b self, suffix: &str) -> impl Iterator<Item = &'a str> + 'b {
        let reversed: String = suffix.chars().rev().collect();
        let start = self
            .reversed
            .partition_point(|(candidate, _)| candidate.as_str() < reversed.as_str());
        let end = self.reversed.partition_point(|(candidate, _)| {
            candidate.as_str() < reversed.as_str() || candidate.starts_with(&reversed)
        });
        self.reversed[start..end].iter().map(|(_, path)| *path)
    }
}

/// The directory name containing a Go file, as import matching defines it.
fn go_directory(target: &str) -> &str {
    target
        .rsplit_once('/')
        .map(|(directory, _)| directory.rsplit('/').next().unwrap_or(directory))
        .unwrap_or("")
}

/// A value imported from a module is not itself that module. Verify the full
/// module path before accepting top-level members through a receiver.
fn namespace_pattern(key: &TargetKey) -> Option<Pattern> {
    match key.language.as_str() {
        "rust" => Some(Pattern::Rust(RustPattern::new(
            &key.from,
            &format!("{}::__member", key.binding_path),
            true,
        ))),
        "python" => Some(Pattern::Import(ImportPattern::new(
            &key.from,
            None,
            &key.binding_path,
            false,
        ))),
        "java" => None,
        _ => module_binding_parts(&key.language, key.wildcard, key.binding_name_none).then(|| {
            Pattern::Import(ImportPattern::new(
                &key.from,
                key.source.as_deref(),
                &key.binding_path,
                false,
            ))
        }),
    }
}

struct Resolver<'a> {
    files: &'a BTreeMap<String, std::sync::Arc<FileStructure>>,
    indexed: &'a BTreeSet<Key>,
    indexes: HashMap<&'a str, FileIndex<'a>>,
    paths: PathIndex<'a>,
    reference_target_cache: HashMap<TargetKey, Vec<(&'a str, bool)>>,
    file_target_cache: HashMap<TargetKey, Vec<&'a str>>,
}

impl<'a> Resolver<'a> {
    fn new(
        files: &'a BTreeMap<String, std::sync::Arc<FileStructure>>,
        indexed: &'a BTreeSet<Key>,
    ) -> Self {
        Self {
            files,
            indexed,
            indexes: files
                .iter()
                .map(|(path, structure)| (path.as_str(), FileIndex::build(structure)))
                .collect(),
            paths: PathIndex::build(files),
            reference_target_cache: HashMap::new(),
            file_target_cache: HashMap::new(),
        }
    }

    /// Files an import can reference, each with whether it is the import's
    /// namespace (top-level members are reachable through a receiver only
    /// for namespace files). The cached list is cloned: it is almost always
    /// empty or a single file, and callers need `&mut self` again to resolve
    /// further sites while iterating it.
    fn reference_targets(&mut self, key: TargetKey) -> Vec<(&'a str, bool)> {
        let TargetKind::Reference { receiver } = key.kind else {
            unreachable!("reference target kind")
        };
        self.reference_target_cache
            .entry(key)
            .or_insert_with_key(|key| {
                let namespace = receiver.then(|| namespace_pattern(key)).flatten();
                let mut patterns = vec![Pattern::Import(ImportPattern::new(
                    &key.from,
                    key.source.as_deref(),
                    &key.binding_path,
                    !receiver && key.language != "java",
                ))];
                if key.language == "java" && !receiver {
                    patterns.push(Pattern::Import(ImportPattern::new(
                        &key.from,
                        None,
                        &key.binding_path,
                        true,
                    )));
                }
                let mut candidates = BTreeSet::new();
                if let Some(namespace) = &namespace {
                    candidates.extend(namespace.candidates(&self.paths, self.files));
                }
                for pattern in &patterns {
                    candidates.extend(pattern.candidates(&self.paths, self.files));
                }
                candidates
                    .into_iter()
                    .filter_map(|path| {
                        let namespace = receiver
                            && namespace
                                .as_ref()
                                .is_some_and(|pattern| pattern.matches(path));
                        let matches_path = patterns.iter().any(|pattern| pattern.matches(path));
                        (namespace || matches_path).then_some((path, namespace))
                    })
                    .collect()
            })
            .clone()
    }

    /// Files an import can call into, or C headers an include can name; both
    /// exclude the importing file itself.
    fn file_targets(&mut self, key: TargetKey) -> Vec<&'a str> {
        self.file_target_cache
            .entry(key)
            .or_insert_with_key(|key| {
                let pattern = match key.kind {
                    TargetKind::Call { imported_callable } => Pattern::Import(ImportPattern::new(
                        &key.from,
                        key.source.as_deref(),
                        &key.binding_path,
                        imported_callable,
                    )),
                    TargetKind::Header => {
                        Pattern::Header(HeaderPattern::new(&key.from, &key.binding_path))
                    }
                    TargetKind::Reference { .. } => unreachable!("file target kind"),
                };
                pattern
                    .candidates(&self.paths, self.files)
                    .into_iter()
                    .filter(|path| *path != key.from.as_str() && pattern.matches(path))
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            })
            .clone()
    }

    /// General references use the parser's lexical binding, not the callable
    /// resolver's candidate search. In particular, an unknown local must never
    /// fall back to a same-named import or a globally unique declaration.
    fn resolve_reference(
        &mut self,
        path: &str,
        owner: &StructureNode,
        site: &ReferenceSite,
    ) -> Option<Key> {
        let files = self.files;
        let structure = files.get(path)?;
        let binding = site.binding_id.and_then(|id| node_by_id(structure, id));
        let mut candidates = BTreeSet::new();
        if let Some(binding) = binding {
            if binding.kind == "import" {
                for import in &binding.imports {
                    let root = site.receiver.as_deref().unwrap_or(&site.name);
                    if local_name(&owner.language, import) != Some(root)
                        || import.wildcard && site.receiver.is_none()
                    {
                        continue;
                    }
                    let local = self.indexes.get(path).map(|index| {
                        Self::local_import_targets(
                            structure,
                            index,
                            binding,
                            import,
                            site,
                            &owner.language,
                        )
                    });
                    for target in local.into_iter().flatten() {
                        candidates.insert(Key {
                            path: path.to_owned(),
                            id: target,
                        });
                    }
                    let target_name = if site.receiver.is_some() {
                        site.name.as_str()
                    } else {
                        import.name.as_deref().unwrap_or(&site.name)
                    };
                    let key = TargetKey::new(
                        path,
                        &owner.language,
                        import,
                        TargetKind::Reference {
                            receiver: site.receiver.is_some(),
                        },
                    );
                    for (other_path, namespace) in self.reference_targets(key) {
                        let other = &files[other_path];
                        let Some(index) = self.indexes.get(other_path) else {
                            continue;
                        };
                        for target in index.named(target_name) {
                            let named = target.names.iter().any(|name| name == target_name)
                                || target.name == target_name
                                || target_name == "default"
                                    && target.attributes.iter().any(|attr| attr == "default");
                            if !named || matches!(target.kind.as_str(), "import" | "impl") {
                                continue;
                            }
                            let valid = if let Some(receiver) = site.receiver.as_deref() {
                                if target.parent_id.is_none() {
                                    namespace && (owner.language != "go" || target.kind != "method")
                                } else {
                                    let container = import.name.as_deref().unwrap_or(receiver);
                                    target
                                        .parent_id
                                        .and_then(|id| node_by_id(other, id))
                                        .is_some_and(|parent| {
                                            container_named(parent, container)
                                                && reference_static_member(target, parent)
                                        })
                                }
                            } else {
                                target.parent_id.is_none()
                                    || owner.language == "java"
                                        && target
                                            .parent_id
                                            .and_then(|id| node_by_id(other, id))
                                            .is_some_and(|parent| {
                                                import.path.rsplit('.').nth(1) == Some(&parent.name)
                                                    && reference_static_member(target, parent)
                                            })
                            };
                            if valid {
                                candidates.insert(Key {
                                    path: other_path.to_owned(),
                                    id: target.id,
                                });
                            }
                        }
                    }
                }
            } else if site.receiver.is_none() {
                candidates.insert(Key {
                    path: path.to_owned(),
                    id: binding.id,
                });
            } else if let Some(index) = self.indexes.get(path) {
                for target in index.named(&site.name) {
                    if target.name != site.name && !target.names.contains(&site.name) {
                        continue;
                    }
                    let parent = target.parent_id.and_then(|id| node_by_id(structure, id));
                    let valid = parent.is_some_and(|parent| {
                        (parent.id == binding.id
                            || parent.kind == "impl"
                                && binding.parent_id == parent.parent_id
                                && impl_type(&parent.name) == binding.name)
                            && reference_static_member(target, parent)
                    });
                    if valid {
                        candidates.insert(Key {
                            path: path.to_owned(),
                            id: target.id,
                        });
                    }
                }
            }
        } else if owner.language == "c" && site.receiver.is_none() {
            // Includes provide explicit cross-file evidence, unlike a globally
            // unique name. Several headers defining the name remain ambiguous.
            let bindings = self
                .indexes
                .get(path)
                .map(FileIndex::bindings)
                .unwrap_or_default();
            for binding in bindings {
                let key = TargetKey::new(path, &owner.language, binding, TargetKind::Header);
                for other_path in self.file_targets(key) {
                    let other = &files[other_path];
                    let Some(index) = self.indexes.get(other_path) else {
                        continue;
                    };
                    for target in index.named(&site.name) {
                        if (target.name == site.name || target.names.contains(&site.name))
                            && (target.parent_id.is_none()
                                || target.kind == "variant"
                                    && target
                                        .parent_id
                                        .and_then(|id| node_by_id(other, id))
                                        .is_some_and(|parent| {
                                            parent.kind == "enum" && parent.parent_id.is_none()
                                        }))
                            && !matches!(target.kind.as_str(), "import" | "impl")
                        {
                            candidates.insert(Key {
                                path: other_path.to_owned(),
                                id: target.id,
                            });
                        }
                    }
                }
            }
        } else if matches!(
            site.receiver.as_deref(),
            Some("self" | "this" | "Self" | "cls")
        ) {
            let scope = std::iter::successors(owner.parent_id, |id| {
                node_by_id(structure, *id).and_then(|node| node.parent_id)
            })
            .filter_map(|id| node_by_id(structure, id))
            .find(|node| {
                matches!(
                    node.kind.as_str(),
                    "class" | "struct" | "impl" | "interface"
                )
            });
            if let Some(scope) = scope
                && let Some(children) = self
                    .indexes
                    .get(path)
                    .and_then(|index| index.children.get(&scope.id))
            {
                for target in children {
                    if target.parent_id == Some(scope.id)
                        && (target.name == site.name || target.names.contains(&site.name))
                        && (!matches!(site.receiver.as_deref(), Some("Self" | "cls"))
                            || reference_static_member(target, scope))
                    {
                        candidates.insert(Key {
                            path: path.to_owned(),
                            id: target.id,
                        });
                    }
                }
            }
        }
        (candidates.len() == 1)
            .then(|| candidates.into_iter().next())
            .flatten()
    }

    /// Inline modules/namespaces are still same-file declarations. Resolve an
    /// explicit item path, never a bare-name search across unrelated containers.
    fn local_import_targets(
        structure: &FileStructure,
        index: &FileIndex<'_>,
        declaration: &StructureNode,
        import: &ImportBinding,
        site: &ReferenceSite,
        language: &str,
    ) -> Vec<usize> {
        let mut path: Vec<String> = match language {
            "rust" => {
                let mut module = std::iter::successors(declaration.parent_id, |id| {
                    node_by_id(structure, *id).and_then(|node| node.parent_id)
                })
                .filter_map(|id| node_by_id(structure, id))
                .find(|node| node.kind == "module")
                .map(|node| {
                    node.qualified_name
                        .split('.')
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
                let mut segments = import.path.split("::").peekable();
                match segments.peek().copied() {
                    Some("self") => {
                        segments.next();
                    }
                    Some("super") => {
                        while segments.peek() == Some(&"super") {
                            segments.next();
                            if module.pop().is_none() {
                                return Vec::new();
                            }
                        }
                    }
                    _ => {
                        module.clear();
                        if segments.peek() == Some(&"crate") {
                            segments.next();
                        }
                    }
                }
                module.extend(segments.map(str::to_owned));
                module
            }
            "typescript" | "tsx" if import.source.is_none() && import.name.is_none() => {
                import.path.split('.').map(str::to_owned).collect()
            }
            _ => return Vec::new(),
        };
        if site.receiver.is_some() {
            path.push(site.name.clone());
        }
        let qualified = path.join(".");
        index
            .by_qualified
            .get(qualified.as_str())
            .into_iter()
            .flatten()
            .filter(|target| {
                !matches!(target.kind.as_str(), "import" | "impl")
                    && (site.receiver.is_none()
                        || target
                            .parent_id
                            .and_then(|id| node_by_id(structure, id))
                            .is_some_and(|parent| reference_static_member(target, parent)))
            })
            .map(|target| target.id)
            .collect()
    }

    fn resolve(
        &mut self,
        path: &str,
        owner: &StructureNode,
        name: &str,
        receiver: Option<&str>,
    ) -> Option<Key> {
        let files = self.files;
        let structure = files.get(path)?;
        let index = self.indexes.get(path)?;
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
        for node in index.named(name) {
            if !callable(node) || node.name != name {
                continue;
            }
            let valid = match receiver {
                None => match node.parent_id {
                    None => true,
                    Some(parent_id) => ancestors.iter().any(|parent| {
                        parent.id == parent_id && bare_name_reaches(language, parent)
                    }),
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
                if self.indexed.contains(&key) {
                    candidates.insert(key);
                }
            }
        }
        if receiver.is_none() && candidates.is_empty() {
            for class in index.named(name) {
                if class.kind != "class" || class.name != name {
                    continue;
                }
                for constructor in index.children.get(&class.id).into_iter().flatten() {
                    if constructor.kind == "constructor" && constructor.name == "__init__" {
                        let key = Key {
                            path: path.to_owned(),
                            id: constructor.id,
                        };
                        if self.indexed.contains(&key) {
                            candidates.insert(key);
                        }
                    }
                }
            }
        }
        // Explicit imports are the only basis for a cross-file edge. Do not guess
        // using a globally unique bare name or an unknown runtime receiver.
        for binding in index.bindings() {
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
            let key = TargetKey::new(
                path,
                language,
                binding,
                TargetKind::Call {
                    imported_callable: receiver.is_none(),
                },
            );
            for other_path in self.file_targets(key) {
                let other = &files[other_path];
                let Some(other_index) = self.indexes.get(other_path) else {
                    continue;
                };
                for node in other_index.named(target_name) {
                    if !callable(node) || node.name != target_name {
                        continue;
                    }
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
                        path: other_path.to_owned(),
                        id: node.id,
                    };
                    if self.indexed.contains(&key) {
                        candidates.insert(key);
                    }
                }
            }
        }
        (candidates.len() == 1)
            .then(|| candidates.into_iter().next())
            .flatten()
    }
}

/// A C include path resolved once, so scanning candidate files is plain
/// string comparison instead of rebuilding the joined path per file.
struct HeaderPattern {
    joined: String,
    include: String,
    suffix: String,
}

impl HeaderPattern {
    fn new(from: &str, include: &str) -> Self {
        let base = std::path::Path::new(from)
            .parent()
            .unwrap_or(std::path::Path::new(""));
        let mut parts = Vec::new();
        for part in base.join(include).components() {
            match part {
                std::path::Component::ParentDir => {
                    parts.pop();
                }
                std::path::Component::Normal(part) => {
                    parts.push(part.to_string_lossy().into_owned())
                }
                _ => {}
            }
        }
        Self {
            joined: parts.join("/"),
            include: include.to_owned(),
            suffix: format!("/{include}"),
        }
    }

    fn matches(&self, target: &str) -> bool {
        target == self.joined || target == self.include || target.ends_with(&self.suffix)
    }

    fn candidates<'a>(
        &self,
        paths: &PathIndex<'a>,
        files: &'a BTreeMap<String, std::sync::Arc<FileStructure>>,
    ) -> Vec<&'a str> {
        let mut candidates: Vec<&'a str> = [&self.joined, &self.include]
            .into_iter()
            .filter_map(|exact| {
                files
                    .get_key_value(exact.as_str())
                    .map(|(path, _)| path.as_str())
            })
            .collect();
        candidates.extend(paths.suffix(&self.suffix));
        candidates
    }
}

fn reference_static_member(node: &StructureNode, parent: &StructureNode) -> bool {
    parent.kind == "module"
        || matches!(parent.kind.as_str(), "variable" | "constant")
        || node.kind == "variant"
        || callable(node)
            && (node.kind == "constructor"
                || if node.language == "rust" {
                    node.kind == "method" && !rust_self_receiver(&node.signature)
                } else {
                    node.signature
                        .split_whitespace()
                        .any(|word| word == "static")
                        || node.attributes.iter().any(|attribute| {
                            attribute.contains("staticmethod") || attribute.contains("classmethod")
                        })
                })
        || !callable(node)
            && (node.language == "python"
                || node.language == "rust" && matches!(node.kind.as_str(), "constant" | "type")
                || node
                    .signature
                    .split_whitespace()
                    .any(|word| word == "static")
                || matches!(node.kind.as_str(), "class" | "struct" | "enum" | "type"))
}

/// One import binding's target-file predicate with every target-independent
/// string (joined paths, suffixes, candidate files) computed once. Resolution
/// scans all indexed files per binding; with per-target formatting and
/// splitting that scan dominated call-graph construction on large repos.
struct ImportPattern {
    /// Exact candidate files for relative JS/TS imports; when present, no
    /// other branch applies.
    js_exact: Option<Vec<String>>,
    python_exact: String,
    python_suffix: String,
    python_init: String,
    rust: RustPattern,
    go_package: String,
    java_suffix: String,
}

impl ImportPattern {
    fn new(from: &str, module: Option<&str>, binding: &str, imported_callable: bool) -> Self {
        let module = module.unwrap_or(binding);
        let js_exact = if module.starts_with('.')
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
            let mut candidates: Vec<String> = [
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
            .map(|ext| format!("{prefix}{ext}"))
            .collect();
            candidates.push(prefix);
            Some(candidates)
        } else {
            None
        };
        let python_path = module.trim_start_matches('.').replace('.', "/");
        // Files of package `p` live in a directory named `p`; the import
        // path's last segment is the package directory for conventional
        // Go layouts.
        let go_package = module
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(module)
            .to_owned();
        let java_module = if imported_callable {
            module
                .rsplit_once('.')
                .map(|(prefix, _)| prefix)
                .unwrap_or(module)
        } else {
            module
        };
        Self {
            js_exact,
            python_exact: format!("{python_path}.py"),
            python_suffix: format!("/{python_path}.py"),
            python_init: format!("{python_path}/__init__.py"),
            rust: RustPattern::new(from, module, imported_callable),
            go_package,
            java_suffix: format!("{}.java", java_module.replace('.', "/")),
        }
    }

    fn matches(&self, target: &str) -> bool {
        if let Some(candidates) = &self.js_exact {
            return candidates.iter().any(|candidate| candidate == target);
        }
        if target.ends_with(".py") {
            target == self.python_exact
                || target.ends_with(&self.python_suffix)
                || target == self.python_init
        } else if target.ends_with(".rs") {
            self.rust.matches(target)
        } else if target.ends_with(".go") {
            !self.go_package.is_empty() && go_directory(target) == self.go_package
        } else if target.ends_with(".java") {
            target.ends_with(&self.java_suffix)
        } else {
            false
        }
    }

    fn candidates<'a>(
        &self,
        paths: &PathIndex<'a>,
        files: &'a BTreeMap<String, std::sync::Arc<FileStructure>>,
    ) -> Vec<&'a str> {
        if let Some(exact) = &self.js_exact {
            return exact
                .iter()
                .filter_map(|candidate| {
                    files
                        .get_key_value(candidate.as_str())
                        .map(|(path, _)| path.as_str())
                })
                .collect();
        }
        let mut candidates = Vec::new();
        for exact in [&self.python_exact, &self.python_init] {
            candidates.extend(
                files
                    .get_key_value(exact.as_str())
                    .map(|(path, _)| path.as_str()),
            );
        }
        candidates.extend(paths.suffix(&self.python_suffix));
        candidates.extend(self.rust.candidates(paths, files));
        if !self.go_package.is_empty() {
            candidates.extend(
                paths
                    .go_directories
                    .get(self.go_package.as_str())
                    .into_iter()
                    .flatten()
                    .copied(),
            );
        }
        candidates.extend(paths.suffix(&self.java_suffix));
        candidates
    }
}

/// Rust paths name items: a callable import points at its parent module's
/// file, a receiver import names either a module file or a type in its parent
/// module. `super::` paths resolve relative to the importing file's module.
/// A Rust item path prepared once: exact files for `super::` paths, path
/// suffixes for crate-relative paths.
struct RustPattern {
    exact: Vec<String>,
    suffixes: Vec<String>,
    crate_root: bool,
    never: bool,
}

impl RustPattern {
    fn new(from: &str, module: &str, imported_callable: bool) -> Self {
        let segments: Vec<&str> = module.split("::").collect();
        let candidates: Vec<&[&str]> = if imported_callable {
            vec![&segments[..segments.len().saturating_sub(1)]]
        } else {
            vec![&segments[..], &segments[..segments.len().saturating_sub(1)]]
        };
        if segments.first() == Some(&"super") {
            let supers = segments.iter().take_while(|s| **s == "super").count();
            let Some(base) = rust_super_directory(from, supers) else {
                return Self {
                    exact: Vec::new(),
                    suffixes: Vec::new(),
                    crate_root: false,
                    never: true,
                };
            };
            return Self {
                exact: candidates
                    .into_iter()
                    .flat_map(|segments| {
                        rust_module_files(&base, &segments[supers.min(segments.len())..])
                    })
                    .collect(),
                suffixes: Vec::new(),
                crate_root: false,
                never: false,
            };
        }
        let mut suffixes = Vec::new();
        let mut crate_root = false;
        for segments in candidates {
            let segments: Vec<&str> = segments
                .iter()
                .copied()
                .filter(|s| !matches!(*s, "crate" | "self"))
                .collect();
            if segments.is_empty() {
                crate_root = true;
            } else {
                let path = segments.join("/");
                suffixes.push(format!("{path}.rs"));
                suffixes.push(format!("{path}/mod.rs"));
            }
        }
        Self {
            exact: Vec::new(),
            suffixes,
            crate_root,
            never: false,
        }
    }

    fn matches(&self, target: &str) -> bool {
        !self.never
            && (self.exact.iter().any(|candidate| candidate == target)
                || self.suffixes.iter().any(|suffix| target.ends_with(suffix))
                || self.crate_root && (target.ends_with("lib.rs") || target.ends_with("main.rs")))
    }

    fn candidates<'a>(
        &self,
        paths: &PathIndex<'a>,
        files: &'a BTreeMap<String, std::sync::Arc<FileStructure>>,
    ) -> Vec<&'a str> {
        if self.never {
            return Vec::new();
        }
        let mut candidates: Vec<&'a str> = self
            .exact
            .iter()
            .filter_map(|candidate| {
                files
                    .get_key_value(candidate.as_str())
                    .map(|(path, _)| path.as_str())
            })
            .collect();
        for suffix in &self.suffixes {
            candidates.extend(paths.suffix(suffix));
        }
        if self.crate_root {
            candidates.extend(paths.suffix("lib.rs"));
            candidates.extend(paths.suffix("main.rs"));
        }
        candidates
    }
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

    fn key(graph: &CallGraph, path: &str, name: &str) -> Key {
        Key {
            path: path.to_owned(),
            id: graph.files[path]
                .nodes
                .iter()
                .find(|node| node.qualified_name == name)
                .unwrap_or_else(|| panic!("missing {path}:{name}"))
                .id,
        }
    }

    fn reference_names(graph: &CallGraph, path: &str, name: &str, mode: Refs) -> Vec<String> {
        graph.references(&key(graph, path, name), mode)
    }

    #[test]
    fn refs_modes_parse_serialize_and_default_from_effective_depths() {
        assert_eq!(Refs::default(), Refs::None);
        for (value, mode) in [
            ("none", Refs::None),
            ("all", Refs::All),
            ("cross-file", Refs::CrossFile),
        ] {
            assert_eq!(value.parse::<Refs>().unwrap(), mode);
            assert_eq!(Refs::resolve(Some(value), 4, 9).unwrap(), mode);
            assert_eq!(
                serde_json::to_string(&mode).unwrap(),
                format!("\"{value}\"")
            );
            assert_eq!(
                serde_json::from_str::<Refs>(&format!("\"{value}\"")).unwrap(),
                mode
            );
            assert_eq!(mode.enabled(), mode != Refs::None);
        }
        for value in ["", "crossFile", "ALL", "other"] {
            assert!(
                Refs::resolve(Some(value), 0, 0)
                    .unwrap_err()
                    .to_string()
                    .contains("none, all, or cross-file")
            );
        }
        assert_eq!(Refs::resolve(None, 0, 0).unwrap(), Refs::None);
        assert_eq!(Refs::resolve(None, 1, 0).unwrap(), Refs::CrossFile);
        assert_eq!(Refs::resolve(None, 0, 1).unwrap(), Refs::CrossFile);
    }

    #[test]
    fn general_references_are_mode_filtered_and_do_not_expand_the_callable_graph() {
        let graph = build(&[
            (
                "src/dep.rs",
                "pub const LIMIT: usize = 4;\npub struct Config;\npub fn helper() {}\n",
            ),
            (
                "src/main.rs",
                "use crate::dep::{LIMIT, Config, helper};\nconst LOCAL: usize = 1;\nconst COPY: usize = LIMIT;\ntype Alias = Config;\nfn leaf() {}\nfn run(config: Config) { let callback = helper; let count = LIMIT + LIMIT + LOCAL; leaf(); }\n",
            ),
        ]);
        let run = key(&graph, "src/main.rs", "run");
        let all = graph.references(&run, Refs::All);
        assert_eq!(all.len(), 5, "{all:?}");
        for target in ["Config", "LIMIT", "helper", "LOCAL", "leaf"] {
            assert!(
                all.iter()
                    .any(|location| location.ends_with(&format!(":{target}"))),
                "{target}: {all:?}"
            );
        }
        assert!(graph.references(&run, Refs::None).is_empty());
        let cross = graph.references(&run, Refs::CrossFile);
        assert_eq!(cross.len(), 3, "{cross:?}");
        assert!(
            cross
                .iter()
                .all(|location| location.starts_with("src/dep.rs:"))
        );
        assert_eq!(
            reference_names(&graph, "src/main.rs", "COPY", Refs::All).len(),
            1
        );
        assert_eq!(
            reference_names(&graph, "src/main.rs", "Alias", Refs::All).len(),
            1
        );
        assert_eq!(edges(&graph), ["src/main.rs:run -> src/main.rs:leaf"]);
        let expansion = graph.expand([run.clone()], 0, 1);
        assert!(
            expansion
                .depths
                .contains_key(&key(&graph, "src/main.rs", "leaf"))
        );
        assert_eq!(
            graph.code_keys(&expansion, 0, 1),
            BTreeSet::from([key(&graph, "src/main.rs", "leaf")])
        );
        assert_eq!(graph.references(&run, Refs::CrossFile), cross);
        assert!(
            graph
                .expand([key(&graph, "src/main.rs", "COPY")], 2, 2)
                .depths
                .is_empty()
        );
        let restored: CallGraph =
            serde_json::from_value(serde_json::to_value(&graph).unwrap()).unwrap();
        assert_eq!(restored.references(&run, Refs::All), all);
    }

    #[test]
    fn rust_invocation_references_distinguish_calls_from_function_values() {
        let graph = build(&[
            (
                "src/dep.rs",
                "pub const LIMIT: usize = 4;\npub struct Config;\npub fn helper() {}\npub fn callback() {}\npub fn mixed() {}\nimpl Config { pub fn new() -> Self { Config } pub fn factory() -> Self { Config } }\n",
            ),
            (
                "src/main.rs",
                "use crate::dep::{LIMIT, Config, helper as invoke, callback, mixed};\nfn run(config: Config) { let f = callback; let g = mixed; let constructor = Config::factory; invoke(); mixed(); mixed(); Config::new(); let count = LIMIT; }\nfn value_only() { let f = invoke; }\nstruct Runner;\nimpl Runner { fn step(&self) {} fn run(&self) { self.step(); self.run(); } }\n",
            ),
        ]);
        let run = key(&graph, "src/main.rs", "run");
        let refs = graph.reference_labels(&run, Refs::CrossFile);
        assert_eq!(
            refs,
            [
                "src/dep.rs:1:LIMIT",
                "src/dep.rs:2:Config",
                "src/dep.rs:3:helper()",
                "src/dep.rs:4:callback",
                "src/dep.rs:5:mixed()",
                "src/dep.rs:6:Config.factory",
                "src/dep.rs:6:Config.new()",
            ]
        );
        assert_eq!(graph.reference_labels(&run, Refs::All), refs);
        assert!(graph.reference_labels(&run, Refs::None).is_empty());
        assert!(
            graph
                .references(&run, Refs::All)
                .iter()
                .all(|location| !location.ends_with("()"))
        );
        assert_eq!(
            graph.reference_labels(&key(&graph, "src/main.rs", "value_only"), Refs::CrossFile),
            ["src/dep.rs:3:helper"]
        );
        assert_eq!(
            graph.reference_labels(&key(&graph, "src/main.rs", "Runner.run"), Refs::All),
            ["src/main.rs:5:Runner.run()", "src/main.rs:5:Runner.step()"]
        );
        assert!(reference_names(&graph, "src/main.rs", "Runner.run", Refs::CrossFile).is_empty());
        let restored: CallGraph =
            serde_json::from_value(serde_json::to_value(&graph).unwrap()).unwrap();
        assert_eq!(restored.reference_labels(&run, Refs::CrossFile), refs);
    }

    #[test]
    fn invocation_reference_suffix_is_shared_across_languages() {
        for (path, source, owner) in [
            (
                "a.ts",
                "function helper() {}\nfunction callback() {}\nfunction run() { helper(); return callback; }\n",
                "run",
            ),
            (
                "a.py",
                "def helper():\n    pass\ndef callback():\n    pass\ndef run():\n    helper()\n    return callback\n",
                "run",
            ),
            (
                "a.go",
                "package a\nfunc helper() {}\nfunc callback() {}\nfunc run() { helper(); f := callback; _ = f }\n",
                "run",
            ),
            (
                "A.java",
                "class A { static void helper() {} void run() { A.helper(); } }\n",
                "A.run",
            ),
            (
                "a.c",
                "void helper(void) {}\nvoid callback(void) {}\nvoid run(void) { helper(); void (*f)(void) = callback; }\n",
                "run",
            ),
        ] {
            let graph = build(&[(path, source)]);
            let refs = graph.reference_labels(&key(&graph, path, owner), Refs::All);
            assert!(
                refs.iter().any(|location| location.ends_with("helper()")),
                "{path}: {refs:?}"
            );
            if owner == "run" {
                assert!(
                    refs.iter().any(|location| location.ends_with(":callback")),
                    "{path}: {refs:?}"
                );
                assert_eq!(refs.len(), 2, "{path}: {refs:?}");
            }
        }
    }

    #[test]
    fn typescript_imported_types_constants_variables_and_function_values() {
        let graph = build(&[
            (
                "lib/dep.ts",
                "export type Options = { value: number };\nexport const LIMIT = 4;\nexport let current = 0;\nexport function helper() {}\n",
            ),
            (
                "lib/main.ts",
                "import { Options as Settings, LIMIT, current, helper } from './dep';\ntype Alias = Settings;\nconst NEXT = LIMIT;\nconst callback = helper;\nfunction run(options: Settings) { return [LIMIT, current, helper, helper]; }\n",
            ),
        ]);
        assert!(edges(&graph).is_empty());
        let refs = reference_names(&graph, "lib/main.ts", "run", Refs::CrossFile);
        assert_eq!(refs.len(), 4, "{refs:?}");
        for (owner, target) in [
            ("Alias", "Options"),
            ("NEXT", "LIMIT"),
            ("callback", "helper"),
        ] {
            let refs = reference_names(&graph, "lib/main.ts", owner, Refs::All);
            assert_eq!(refs.len(), 1, "{owner}: {refs:?}");
            assert!(refs[0].ends_with(&format!(":{target}")), "{refs:?}");
        }
    }

    #[test]
    fn reference_lookup_obeys_parameters_locals_blocks_and_nearest_lexical_scope() {
        let graph = build(&[
            (
                "dep.ts",
                "export const VALUE = 1;\nexport class Type {}\nexport function helper() {}\n",
            ),
            (
                "main.ts",
                "import { VALUE, Type, helper } from './dep';\nfunction shadow(VALUE: number, helper: unknown) { const Type = 0; return [VALUE, helper, Type]; }\nfunction outer(helper: unknown) { function inner() { return helper; } return inner; }\nfunction blocks() { { const helper = 0; helper; } return helper; }\nfunction generic<Type>(x: Type): Type { return x; }\nfunction nearest() { function helper() {} function nested() { return helper; } return helper; }\n",
            ),
        ]);
        for owner in ["shadow", "outer.inner", "generic"] {
            assert!(
                reference_names(&graph, "main.ts", owner, Refs::All).is_empty(),
                "{owner}"
            );
        }
        let block_refs = reference_names(&graph, "main.ts", "blocks", Refs::All);
        assert_eq!(block_refs.len(), 1, "{block_refs:?}");
        assert!(block_refs[0].starts_with("dep.ts:"));
        for owner in ["nearest", "nearest.nested"] {
            let refs = reference_names(&graph, "main.ts", owner, Refs::All);
            assert_eq!(refs.len(), 1, "{owner}: {refs:?}");
            assert!(refs[0].ends_with(":nearest.helper"), "{refs:?}");
        }
    }

    #[test]
    fn ambiguous_unimported_and_dynamic_references_are_not_guessed() {
        let graph = build(&[
            (
                "dep.ts",
                "export function validate() {}\nexport const config = {};\nexport class Config { static validate() {} run() {} }\n",
            ),
            (
                "main.ts",
                "import { config, Config } from './dep';\nfunction run(value: unknown) { config.validate(); value.validate(); Config.run(); validate; }\nfunction shadow(Config: unknown) { Config.validate(); }\nfunction overload(x: string): void;\nfunction overload(x: number): void;\nfunction overload(x: unknown) {}\nfunction ambiguous() { return overload; }\n",
            ),
        ]);
        let refs = reference_names(&graph, "main.ts", "run", Refs::All);
        assert!(
            refs.iter()
                .all(|location| !location.ends_with(":validate")
                    && !location.ends_with(":Config.run")),
            "{refs:?}"
        );
        assert!(reference_names(&graph, "main.ts", "shadow", Refs::All).is_empty());
        assert!(reference_names(&graph, "main.ts", "ambiguous", Refs::All).is_empty());
    }

    #[test]
    fn namespace_imports_and_static_noncallable_members_are_references() {
        let graph = build(&[
            (
                "dep.ts",
                "export const LIMIT = 1;\nexport function helper() {}\nexport class Config { static VALUE = 1; field = 0; static create() {} }\n",
            ),
            (
                "main.ts",
                "import * as dep from './dep';\nimport { Config } from './dep';\nfunction run() { const callback = dep.helper; return [dep.LIMIT, Config.VALUE, Config.create, Config.field]; }\n",
            ),
            (
                "src/dep.rs",
                "pub const LIMIT: usize = 1;\npub struct Config;\nimpl Config { pub const VALUE: usize = 2; pub fn new() -> Self { Config } }\n",
            ),
            (
                "src/main.rs",
                "use crate::dep;\nuse crate::dep::Config;\nfn run() { let f = Config::new; let n = dep::LIMIT + Config::VALUE; }\n",
            ),
        ]);
        let ts = reference_names(&graph, "main.ts", "run", Refs::CrossFile);
        assert_eq!(ts.len(), 5, "{ts:?}");
        assert!(
            ts.iter()
                .all(|location| !location.ends_with(":Config.field"))
        );
        let rust = reference_names(&graph, "src/main.rs", "run", Refs::CrossFile);
        assert_eq!(rust.len(), 4, "{rust:?}");
        assert!(edges(&graph).is_empty());
    }

    #[test]
    fn noncallable_references_resolve_in_python_go_java_and_c() {
        let graph = build(&[
            (
                "dep.py",
                "LIMIT = 1\nclass Config:\n    pass\ndef helper():\n    pass\n",
            ),
            (
                "main.py",
                "from dep import LIMIT, Config, helper\ndef run(value: Config):\n    return LIMIT, helper, Config\ndef shadow(LIMIT):\n    return LIMIT\n",
            ),
            (
                "config/config.go",
                "package config\nconst LIMIT = 1\ntype Config struct{}\nfunc Helper() {}\n",
            ),
            (
                "cmd/main.go",
                "package main\nimport cfg \"example.com/app/config\"\nfunc run(value cfg.Config) { f := cfg.Helper; _ = f; _ = cfg.LIMIT }\n",
            ),
            (
                "pkg/Config.java",
                "package pkg; public class Config { public static int VALUE = 1; }",
            ),
            (
                "pkg/Constants.java",
                "package pkg; public class Constants { public static final int LIMIT = 1; }",
            ),
            (
                "Main.java",
                "import pkg.Config; import static pkg.Constants.LIMIT; class Main { void run(Config value) { int n = Config.VALUE + LIMIT; } }",
            ),
            (
                "include/dep.h",
                "#define LIMIT 1\ntypedef int Config;\nextern int current;\nvoid helper(void);\n",
            ),
            (
                "src/main.c",
                "#include \"../include/dep.h\"\nvoid run(Config value) { int n = LIMIT + current; void (*f)(void) = helper; }\n",
            ),
        ]);
        for (path, owner, count) in [
            ("main.py", "run", 3),
            ("cmd/main.go", "run", 3),
            ("Main.java", "Main.run", 3),
            ("src/main.c", "run", 4),
        ] {
            let refs = reference_names(&graph, path, owner, Refs::CrossFile);
            assert_eq!(refs.len(), count, "{path}: {refs:?}");
        }
        assert!(reference_names(&graph, "main.py", "shadow", Refs::All).is_empty());
        assert!(edges(&graph).is_empty());
    }

    #[test]
    fn scoped_imports_wildcards_and_ambiguous_headers_do_not_escape_their_scope() {
        let graph = build(&[
            ("dep.py", "VALUE = 1\n"),
            (
                "main.py",
                "def imported():\n    from dep import VALUE\n    return VALUE\ndef absent():\n    return VALUE\n",
            ),
            (
                "wild.py",
                "from dep import *\ndef run():\n    return VALUE\n",
            ),
            ("one.h", "extern int VALUE;\n"),
            ("two.h", "extern int VALUE;\n"),
            (
                "main.c",
                "#include \"one.h\"\n#include \"two.h\"\nint run(void) { return VALUE; }\n",
            ),
        ]);
        assert_eq!(
            reference_names(&graph, "main.py", "imported", Refs::CrossFile).len(),
            1
        );
        assert!(reference_names(&graph, "main.py", "absent", Refs::All).is_empty());
        assert!(reference_names(&graph, "wild.py", "run", Refs::All).is_empty());
        assert!(reference_names(&graph, "main.c", "run", Refs::All).is_empty());
    }

    #[test]
    fn explicit_same_file_aliases_resolve_to_inline_module_items() {
        let graph = build(&[
            (
                "main.rs",
                "mod inner { pub const VALUE: usize = 1; pub struct Config; impl Config { pub const LIMIT: usize = 2; pub fn new() -> Self { Config } pub fn run(&self) {} } }\nuse crate::inner::{VALUE as LIMIT, Config as Settings};\nfn run() { let f = Settings::new; let n = LIMIT + Settings::LIMIT; Settings::run(); }\n",
            ),
            (
                "main.ts",
                "namespace Inner { export const VALUE = 1; export class Config {} }\nimport Settings = Inner.Config;\nconst selected: Settings = null;\n",
            ),
        ]);
        let rust = reference_names(&graph, "main.rs", "run", Refs::All);
        assert_eq!(rust.len(), 4, "{rust:?}");
        assert!(
            rust.iter()
                .all(|location| !location.ends_with(":inner.Config.run"))
        );
        assert!(reference_names(&graph, "main.rs", "run", Refs::CrossFile).is_empty());
        let ts = reference_names(&graph, "main.ts", "selected", Refs::All);
        assert_eq!(ts.len(), 1, "{ts:?}");
        assert!(ts[0].ends_with(":Inner.Config"));
    }

    #[test]
    fn rust_static_lifetimes_do_not_make_instance_members_static_references() {
        let graph = build(&[
            (
                "src/dep.rs",
                "pub struct Config { pub name: &'static str }\nimpl Config { pub fn run(&self) -> &'static str { self.name } }\n",
            ),
            (
                "src/main.rs",
                "use crate::dep::Config;\nfn run() { let f = Config::run; let name = Config::name; }\n",
            ),
        ]);
        let refs = reference_names(&graph, "src/main.rs", "run", Refs::All);
        assert_eq!(refs.len(), 1, "{refs:?}");
        assert!(refs[0].ends_with(":Config"));
        assert!(edges(&graph).is_empty());
    }

    #[test]
    fn shell_static_function_commands_remain_outgoing_references() {
        let graph = build(&[(
            "main.sh",
            "helper() { :; }\nrun() { helper; helper; \"$DYNAMIC\"; }\n",
        )]);
        let refs = reference_names(&graph, "main.sh", "run", Refs::All);
        assert_eq!(refs.len(), 1, "{refs:?}");
        assert!(refs[0].ends_with(":helper"));
        assert_eq!(
            graph.reference_labels(&key(&graph, "main.sh", "run"), Refs::All),
            ["main.sh:1:helper()"]
        );
        assert!(reference_names(&graph, "main.sh", "run", Refs::CrossFile).is_empty());
        assert_eq!(edges(&graph), ["main.sh:run -> main.sh:helper"]);
    }

    #[test]
    fn typescript_namespaces_bind_static_references_without_expanding_constants() {
        let graph = build(&[(
            "main.ts",
            "namespace Values { export const LIMIT = 1; export function helper() {} }\nexport function run() { Values.helper(); return Values.LIMIT; }\n",
        )]);
        let run = key(&graph, "main.ts", "run");
        let refs = graph.references(&run, Refs::All);
        for target in ["Values.helper", "Values.LIMIT"] {
            assert!(
                refs.iter()
                    .any(|location| location.ends_with(&format!(":{target}"))),
                "{refs:?}"
            );
        }
        assert!(graph.references(&run, Refs::CrossFile).is_empty());
        assert_eq!(edges(&graph), ["main.ts:run -> main.ts:Values.helper"]);
        let expansion = graph.expand([run.clone()], 0, 1);
        assert_eq!(
            expansion.depths.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([run, key(&graph, "main.ts", "Values.helper")])
        );
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

    #[test]
    fn import_and_header_path_matching_characterization() {
        // Characterization table for the path predicates the resolver scans
        // with: (from, target, module, binding, imported_callable, expected).
        // Guards the prepared-pattern fast path against semantic drift.
        type ImportCase<'a> = (&'a str, &'a str, Option<&'a str>, &'a str, bool, bool);
        let cases: &[ImportCase<'_>] = &[
            // JS/TS relative imports resolve to exact candidate files.
            ("src/a.ts", "src/b.ts", Some("./b"), "", false, true),
            ("src/a.ts", "src/b/index.ts", Some("./b"), "", false, true),
            ("src/a.ts", "src/b.js", Some("./b"), "", false, true),
            ("src/a.ts", "src/c.ts", Some("./b"), "", false, false),
            ("src/sub/a.ts", "src/b.ts", Some("../b"), "", false, true),
            ("src/a.rs", "src/b.ts", Some("./b"), "", false, false),
            ("src/a.ts", "src/b.ts", Some("b"), "", false, false),
            // Python dotted modules match by path suffix or package init.
            ("a.py", "pkg/mod.py", Some("pkg.mod"), "", false, true),
            ("a.py", "src/pkg/mod.py", Some("pkg.mod"), "", false, true),
            ("a.py", "pkg/__init__.py", Some("pkg"), "", false, true),
            ("a.py", "pkg/other.py", Some("pkg.mod"), "", false, false),
            ("a.py", "pkg/mod.py", Some(".pkg.mod"), "", false, true),
            ("a.py", "pkg/mod.py", None, "pkg.mod", false, true),
            // Rust item paths match module files; callables point at parents.
            (
                "src/main.rs",
                "src/dep.rs",
                Some("crate::dep::helper"),
                "",
                true,
                true,
            ),
            (
                "src/main.rs",
                "src/dep.rs",
                Some("crate::dep::helper"),
                "",
                false,
                true,
            ),
            (
                "src/main.rs",
                "src/dep/mod.rs",
                Some("crate::dep"),
                "",
                false,
                true,
            ),
            (
                "src/main.rs",
                "src/other.rs",
                Some("crate::dep::helper"),
                "",
                true,
                false,
            ),
            ("src/main.rs", "src/lib.rs", Some("crate"), "", true, true),
            ("src/main.rs", "src/dep.rs", Some("crate"), "", true, false),
            (
                "src/pkg/m.rs",
                "src/pkg/sib.rs",
                Some("super::sib::func"),
                "",
                true,
                true,
            ),
            (
                "src/pkg/m.rs",
                "src/sib.rs",
                Some("super::sib::func"),
                "",
                true,
                false,
            ),
            (
                "src/pkg/mod.rs",
                "src/top.rs",
                Some("super::top::f"),
                "",
                true,
                true,
            ),
            // Go matches by package directory name.
            (
                "cmd/main.go",
                "pkg/foo/bar.go",
                Some("example.com/foo"),
                "",
                false,
                true,
            ),
            (
                "cmd/main.go",
                "pkg/baz/bar.go",
                Some("example.com/foo"),
                "",
                false,
                false,
            ),
            ("cmd/main.go", "bar.go", Some("foo"), "", false, false),
            // Java matches the qualified class file; callables strip a segment.
            (
                "A.java",
                "src/com/x/Widget.java",
                Some("com.x.Widget"),
                "",
                false,
                true,
            ),
            (
                "A.java",
                "src/com/x/Widget.java",
                Some("com.x.Widget.render"),
                "",
                true,
                true,
            ),
            (
                "A.java",
                "src/com/x/Widget.java",
                Some("com.x.Widget.render"),
                "",
                false,
                false,
            ),
            // Other target kinds never match.
            ("src/a.ts", "src/b.md", Some("./b"), "", false, false),
        ];
        for &(from, target, module, binding, callable, expected) in cases {
            assert_eq!(
                ImportPattern::new(from, module, binding, callable).matches(target),
                expected,
                "ImportPattern({from:?}, {module:?}, {binding:?}, {callable}).matches({target:?})"
            );
        }
        let headers: &[(&str, &str, &str, bool)] = &[
            ("src/a.c", "src/b.h", "b.h", true),
            ("src/a.c", "include/b.h", "b.h", true),
            ("src/a.c", "src/c.h", "b.h", false),
            ("src/sub/a.c", "src/b.h", "../b.h", true),
        ];
        for &(from, target, include, expected) in headers {
            assert_eq!(
                HeaderPattern::new(from, include).matches(target),
                expected,
                "HeaderPattern({from:?}, {include:?}).matches({target:?})"
            );
        }
    }

    #[test]
    fn pattern_candidates_cover_every_matching_path() {
        // Candidate generation via the path index must never drop a file
        // that the pattern itself would match in an exhaustive scan.
        let files: BTreeMap<String, std::sync::Arc<FileStructure>> = [
            "src/a.ts",
            "src/b.ts",
            "src/b/index.ts",
            "pkg/mod.py",
            "src/pkg/mod.py",
            "pkg/__init__.py",
            "src/dep.rs",
            "src/dep/mod.rs",
            "src/lib.rs",
            "pkg/foo/bar.go",
            "src/com/x/Widget.java",
            "src/b.h",
            "include/b.h",
        ]
        .into_iter()
        .map(|path| {
            (
                path.to_owned(),
                std::sync::Arc::new(FileStructure::default()),
            )
        })
        .collect();
        let index = PathIndex::build(&files);
        assert_eq!(
            index.suffix("dep.rs").collect::<BTreeSet<_>>(),
            BTreeSet::from(["src/dep.rs"])
        );
        assert_eq!(
            index.suffix("/pkg/mod.py").collect::<BTreeSet<_>>(),
            BTreeSet::from(["src/pkg/mod.py"])
        );
        assert_eq!(
            index.suffix(".rs").collect::<BTreeSet<_>>(),
            BTreeSet::from(["src/dep.rs", "src/dep/mod.rs", "src/lib.rs"])
        );
        let patterns = vec![
            Pattern::Import(ImportPattern::new("src/a.ts", Some("./b"), "", false)),
            Pattern::Import(ImportPattern::new("a.py", Some("pkg.mod"), "", false)),
            Pattern::Import(ImportPattern::new("a.py", Some("pkg"), "", false)),
            Pattern::Import(ImportPattern::new(
                "src/main.rs",
                Some("crate::dep::helper"),
                "",
                true,
            )),
            Pattern::Import(ImportPattern::new("src/main.rs", Some("crate"), "", true)),
            Pattern::Import(ImportPattern::new(
                "cmd/main.go",
                Some("example.com/foo"),
                "",
                false,
            )),
            Pattern::Import(ImportPattern::new(
                "A.java",
                Some("com.x.Widget"),
                "",
                false,
            )),
            Pattern::Rust(RustPattern::new("src/pkg/m.rs", "super::sib::func", true)),
            Pattern::Header(HeaderPattern::new("src/a.c", "b.h")),
        ];
        for pattern in &patterns {
            let scanned: BTreeSet<&str> = files
                .keys()
                .map(String::as_str)
                .filter(|path| pattern.matches(path))
                .collect();
            let via_index: BTreeSet<&str> = pattern
                .candidates(&index, &files)
                .into_iter()
                .filter(|path| pattern.matches(path))
                .collect();
            assert_eq!(via_index, scanned);
            assert!(!scanned.is_empty() || matches!(pattern, Pattern::Rust(_)));
        }
    }
}
