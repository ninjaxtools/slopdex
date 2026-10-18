//! Relative ES-module resolution and lexical, token-level JS/TS edits.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::Range,
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use tree_sitter::{Node, Parser, Tree};

use crate::parse::{self, EditSite, FileStructure, StructureNode};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Import,
    Export,
    Star,
    Dynamic,
    Require,
}

#[derive(Clone)]
struct Binding {
    imported: String,
    local: String,
    name_range: Range<usize>,
    range: Range<usize>,
    alias: bool,
    type_only: bool,
}

#[derive(Clone)]
struct Link {
    kind: Kind,
    range: Range<usize>,
    source: Option<String>,
    source_range: Option<Range<usize>>,
    bindings: Vec<Binding>,
    id: Option<usize>,
    type_only: bool,
}

struct Module {
    source: String,
    structure: FileStructure,
    tree: Tree,
    sites: Vec<EditSite>,
    links: Vec<Link>,
    /// Local name and its directly exported name (which may be `default`).
    exports: Vec<(String, String)>,
}

#[derive(Clone)]
struct Edit {
    range: Range<usize>,
    text: String,
}
type Edits = BTreeMap<String, Vec<Edit>>;

pub(super) struct Workspace {
    files: BTreeMap<String, Module>,
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|node| !node.is_extra())
        .collect()
}

fn walk(node: Node<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        result.push(node);
        stack.extend(children(node).into_iter().rev());
    }
    result
}

fn text<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.byte_range()]
}

fn tree(path: &str, source: &str) -> Result<Tree> {
    let language = match parse::language_for_path(path) {
        Some("typescript") => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Some("tsx") => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Some("javascript" | "jsx") => tree_sitter_javascript::LANGUAGE.into(),
        _ => bail!("Unsupported refactoring language: {path}"),
    };
    let mut parser = Parser::new();
    parser.set_language(&language)?;
    let tree = parser
        .parse(source, None)
        .context("Tree-sitter returned no tree")?;
    ensure!(
        !tree.root_node().has_error(),
        "Cannot safely refactor invalid syntax in {path}"
    );
    Ok(tree)
}

fn has_token(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| child.kind() == kind)
}

impl Module {
    fn new(path: &str, source: String, structure: FileStructure) -> Result<Self> {
        let tree = tree(path, &source)?;
        let sites = parse::edit_sites(
            tree.root_node(),
            &source,
            parse::language_for_path(path).unwrap(),
            &structure,
        );
        let mut links = Vec::new();
        let mut exports = Vec::new();
        for node in walk(tree.root_node()) {
            match node.kind() {
                "import_statement" | "export_statement" => {
                    ensure!(
                        node.parent()
                            .is_some_and(|parent| parent.kind() == "program"),
                        "Nested/ambient module exports are not supported by refactor: {path}"
                    );
                    let module = node.child_by_field_name("source");
                    let clause = children(node).into_iter().find(|node| {
                        matches!(
                            node.kind(),
                            "import_clause" | "export_clause" | "namespace_export"
                        )
                    });
                    let mut bindings = Vec::new();
                    if let Some(clause) = clause {
                        for entry in walk(clause) {
                            match entry.kind() {
                                "import_specifier" | "export_specifier" => {
                                    let name = entry
                                        .child_by_field_name("name")
                                        .context("Unsupported module binding")?;
                                    let alias = entry.child_by_field_name("alias");
                                    ensure!(
                                        matches!(name.kind(), "identifier" | "type_identifier"),
                                        "String-named imports/exports are not supported: {path}"
                                    );
                                    bindings.push(Binding {
                                        imported: text(&source, name).into(),
                                        local: text(&source, alias.unwrap_or(name)).into(),
                                        name_range: name.byte_range(),
                                        range: entry.byte_range(),
                                        alias: alias.is_some(),
                                        type_only: has_token(entry, "type"),
                                    });
                                }
                                "namespace_import" | "namespace_export" => {
                                    let name = children(entry)
                                        .into_iter()
                                        .find(|node| node.kind() == "identifier")
                                        .context("Unsupported namespace binding")?;
                                    bindings.push(Binding {
                                        imported: "*".into(),
                                        local: text(&source, name).into(),
                                        name_range: name.byte_range(),
                                        range: entry.byte_range(),
                                        alias: true,
                                        type_only: false,
                                    });
                                }
                                "identifier"
                                    if entry
                                        .parent()
                                        .is_some_and(|node| node.kind() == "import_clause") =>
                                {
                                    bindings.push(Binding {
                                        imported: "default".into(),
                                        local: text(&source, entry).into(),
                                        name_range: entry.byte_range(),
                                        range: entry.byte_range(),
                                        alias: true,
                                        type_only: false,
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                    if module.is_some() || !bindings.is_empty() {
                        let (module, source_range) = module
                            .map(|node| module_string(path, &source, node))
                            .transpose()?
                            .map_or((None, None), |(value, range)| (Some(value), Some(range)));
                        let kind = if node.kind() == "import_statement" {
                            Kind::Import
                        } else if bindings.is_empty() {
                            Kind::Star
                        } else {
                            Kind::Export
                        };
                        links.push(Link {
                            kind,
                            range: node.byte_range(),
                            source: module,
                            source_range,
                            bindings,
                            id: structure
                                .nodes
                                .iter()
                                .find(|symbol| {
                                    symbol.kind == "import" && symbol.end_byte == node.end_byte()
                                })
                                .map(|symbol| symbol.id),
                            type_only: has_token(node, "type"),
                        });
                    }
                    if node.kind() == "export_statement"
                        && node.child_by_field_name("declaration").is_some()
                    {
                        for symbol in structure.nodes.iter().filter(|symbol| {
                            symbol.parent_id.is_none()
                                && symbol.kind != "import"
                                && node.start_byte()
                                    <= symbol.declaration_start_byte.unwrap_or(symbol.start_byte)
                                && symbol.end_byte <= node.end_byte()
                        }) {
                            exports.push((
                                symbol.name.clone(),
                                if has_token(node, "default") {
                                    "default".into()
                                } else {
                                    symbol.name.clone()
                                },
                            ));
                        }
                    } else if node.kind() == "export_statement"
                        && has_token(node, "default")
                        && let Some(value) = node
                            .child_by_field_name("value")
                            .filter(|node| node.kind() == "identifier")
                    {
                        links.push(Link {
                            kind: Kind::Export,
                            range: node.byte_range(),
                            source: None,
                            source_range: None,
                            bindings: vec![Binding {
                                imported: text(&source, value).into(),
                                local: "default".into(),
                                name_range: value.byte_range(),
                                range: value.byte_range(),
                                alias: true,
                                type_only: false,
                            }],
                            id: None,
                            type_only: false,
                        });
                    }
                }
                "call_expression" => {
                    let Some(function) = node.child_by_field_name("function") else {
                        continue;
                    };
                    let callee = text(&source, function);
                    if matches!(callee, "require" | "require.resolve")
                        && sites.iter().any(|site| {
                            site.name == "require"
                                && site.range
                                    == function
                                        .child_by_field_name("object")
                                        .unwrap_or(function)
                                        .byte_range()
                                && site.bound
                        })
                    {
                        continue;
                    }
                    ensure!(
                        callee != "eval",
                        "Direct eval is not supported by refactor: {path}"
                    );
                    let kind = match callee {
                        "import" => Kind::Dynamic,
                        "require" | "require.resolve" => Kind::Require,
                        _ => continue,
                    };
                    let Some(argument) = node
                        .child_by_field_name("arguments")
                        .and_then(|args| children(args).into_iter().next())
                        .filter(|node| node.kind() == "string")
                    else {
                        bail!("Nonliteral import/require is not supported by refactor: {path}");
                    };
                    let (module, range) = module_string(path, &source, argument)?;
                    links.push(Link {
                        kind,
                        range: node.byte_range(),
                        source: Some(module),
                        source_range: Some(range),
                        bindings: Vec::new(),
                        id: None,
                        type_only: false,
                    });
                }
                "import_alias" | "import_require_clause" | "with_statement" => bail!(
                    "Unsupported module/binding syntax in {path}: {}",
                    node.kind()
                ),
                _ => {}
            }
        }
        Ok(Self {
            source,
            structure,
            tree,
            sites,
            links,
            exports,
        })
    }

    fn symbol(&self, name: &str) -> Result<&StructureNode> {
        let candidates: Vec<_> = self
            .structure
            .nodes
            .iter()
            .filter(|node| {
                node.kind != "import"
                    && node.parent_id.is_none()
                    && (node.name == name || node.qualified_name == name)
            })
            .collect();
        ensure!(
            candidates.len() == 1,
            "Expected one top-level declaration for {name:?}; found {}",
            candidates.len()
        );
        let symbol = candidates[0];
        ensure!(
            symbol.names.len() <= 1,
            "Destructuring/multi-name declarations cannot be refactored"
        );
        ensure!(
            matches!(
                symbol.kind.as_str(),
                "function"
                    | "generator"
                    | "constant"
                    | "variable"
                    | "class"
                    | "interface"
                    | "type"
                    | "enum"
            ),
            "Unsupported declaration kind: {}",
            symbol.kind
        );
        Ok(symbol)
    }

    fn check_name(&self, old: &str, new: &str) -> Result<()> {
        ensure!(old != new || !old.is_empty(), "Empty symbol name");
        if old == new {
            return Ok(());
        }
        ensure!(
            !self
                .sites
                .iter()
                .any(|site| site.receiver.is_none() && site.name == new)
                && !self
                    .links
                    .iter()
                    .filter(|link| link.kind == Kind::Import)
                    .flat_map(|link| &link.bindings)
                    .any(|binding| binding.local == new),
            "Renaming {old} to {new} could capture or collide with an existing binding/reference"
        );
        Ok(())
    }

    fn rename_binding(&self, id: usize, old: &str, new: &str, edits: &mut Vec<Edit>) -> Result<()> {
        self.check_name(old, new)?;
        for site in self.sites.iter().filter(|site| {
            site.binding_id == Some(id) && site.receiver.is_none() && site.name == old
        }) {
            edits.push(Edit {
                range: site.range.clone(),
                text: if site.shorthand {
                    format!("{old}: {new}")
                } else {
                    new.into()
                },
            });
        }
        Ok(())
    }

    fn insertion(&self) -> usize {
        let mut end = 0;
        for node in children(self.tree.root_node()) {
            if node.kind() == "hash_bang_line"
                || node.kind() == "import_statement"
                || node.kind() == "expression_statement"
                    && node
                        .named_child(0)
                        .is_some_and(|node| node.kind() == "string")
            {
                end = node.end_byte();
            } else {
                break;
            }
        }
        end
    }

    fn declaration<'a>(&'a self, symbol: &StructureNode) -> Result<Node<'a>> {
        let declarations: Vec<_> = children(self.tree.root_node())
            .into_iter()
            .filter(|node| {
                node.start_byte() <= symbol.declaration_start_byte.unwrap_or(symbol.start_byte)
                    && symbol.end_byte <= node.end_byte()
            })
            .collect();
        ensure!(
            declarations.len() == 1,
            "Cannot isolate declaration {}",
            symbol.name
        );
        Ok(declarations[0])
    }
}

fn module_string(path: &str, source: &str, node: Node<'_>) -> Result<(String, Range<usize>)> {
    let value = text(source, node);
    ensure!(
        value.len() >= 2
            && value.starts_with(['\'', '"'])
            && !value[1..value.len() - 1].contains(['\\', '\n', '\r']),
        "Escaped module specifiers are not supported: {path}"
    );
    Ok((
        value[1..value.len() - 1].into(),
        node.start_byte() + 1..node.end_byte() - 1,
    ))
}

impl Workspace {
    pub(super) fn new(files: BTreeMap<String, (String, FileStructure)>) -> Result<Self> {
        Ok(Self {
            files: files
                .into_iter()
                .map(|(path, (source, structure))| {
                    Ok((path.clone(), Module::new(&path, source, structure)?))
                })
                .collect::<Result<_>>()?,
        })
    }

    pub(super) fn sources(&self) -> BTreeMap<String, String> {
        self.files
            .iter()
            .map(|(path, module)| (path.clone(), module.source.clone()))
            .collect()
    }

    pub(super) fn check_local_packages(&self, policies: &BTreeMap<String, String>) -> Result<()> {
        let mut names = BTreeSet::new();
        for (path, text) in policies
            .iter()
            .filter(|(path, _)| path.as_str() == "package.json" || path.ends_with("/package.json"))
        {
            let package: serde_json::Value = serde_json::from_str(text)
                .with_context(|| format!("Invalid package manifest: {path}"))?;
            if let Some(name) = package["name"].as_str() {
                names.insert(name.to_owned());
            }
        }
        for (path, module) in &self.files {
            for source in module
                .links
                .iter()
                .filter_map(|link| link.source.as_deref())
            {
                ensure!(
                    !source.starts_with(['/', '#'])
                        && !source.starts_with("@/")
                        && !source.starts_with("~/")
                        && !names.iter().any(|name| source == name
                            || source
                                .strip_prefix(name)
                                .is_some_and(|suffix| suffix.starts_with('/'))),
                    "Unsupported nonrelative workspace module in {path}: {source}"
                );
            }
        }
        Ok(())
    }

    fn apply(&mut self, edits: Edits) -> Result<()> {
        for (path, edits) in edits {
            let source = edited(&self.files[&path].source, edits)?;
            let parsed = parse::parse(&path, &source)?;
            ensure!(
                parsed.errors.is_empty(),
                "Refactoring would introduce parse errors in {path}"
            );
            self.files
                .insert(path.clone(), Module::new(&path, source, parsed.structure)?);
        }
        Ok(())
    }

    fn resolve(&self, from: &str, specifier: &str) -> Result<Option<String>> {
        if !specifier.starts_with('.') {
            return Ok(None);
        }
        let base = super::lexical(
            &Path::new(from)
                .parent()
                .unwrap_or(Path::new(""))
                .join(specifier),
        )
        .to_string_lossy()
        .replace('\\', "/");
        ensure!(
            !base.starts_with("../"),
            "Relative module leaves the indexed workspace: {from}: {specifier}"
        );
        let mut candidates = vec![base.clone()];
        let stem = match Path::new(&base).extension().and_then(|ext| ext.to_str()) {
            Some("js" | "jsx") => Some((&base[..base.rfind('.').unwrap()], vec!["ts", "tsx"])),
            Some("mjs") => Some((&base[..base.len() - 4], vec!["mts"])),
            Some("cjs") => Some((&base[..base.len() - 4], vec!["cts"])),
            None => Some((
                base.as_str(),
                vec!["ts", "tsx", "js", "jsx", "mts", "mjs", "cts", "cjs"],
            )),
            _ => None,
        };
        if let Some((stem, extensions)) = stem {
            candidates.extend(
                extensions
                    .into_iter()
                    .map(|extension| format!("{stem}.{extension}")),
            );
        }
        if Path::new(&base).extension().is_none() {
            candidates.extend(
                ["ts", "tsx", "js", "jsx", "mts", "mjs"]
                    .map(|extension| format!("{base}/index.{extension}")),
            );
        }
        let found: BTreeSet<_> = candidates
            .into_iter()
            .filter(|path| self.files.contains_key(path))
            .collect();
        ensure!(
            found.len() <= 1,
            "Ambiguous module resolution in {from}: {specifier}"
        );
        Ok(found.into_iter().next())
    }

    fn specifier(&self, from: &str, to: &str, style: Option<&str>) -> String {
        let relative = |link: &Link| {
            link.source
                .as_deref()
                .filter(|source| source.starts_with('.'))
                .map(str::to_owned)
        };
        let inferred = self
            .files
            .get(from)
            .and_then(|module| module.links.iter().find_map(relative))
            .or_else(|| {
                self.files
                    .values()
                    .flat_map(|module| &module.links)
                    .find_map(relative)
            });
        relative_module(from, to, style.or(inferred.as_deref()))
    }

    fn checked_specifier(&self, from: &str, to: &str, style: Option<&str>) -> Result<String> {
        let specifier = self.specifier(from, to, style);
        ensure!(
            self.resolve(from, &specifier)?.as_deref() == Some(to),
            "Generated module reference would not resolve unambiguously: {from}: {specifier}"
        );
        Ok(specifier)
    }

    /// Explicit exports win over stars. Never use a star edge as evidence that
    /// a same-spelled, unrelated declaration is the symbol being refactored.
    fn export_origin(
        &self,
        path: &str,
        name: &str,
        visiting: &mut BTreeSet<(String, String)>,
    ) -> Result<Option<(String, usize)>> {
        let key = (path.to_owned(), name.to_owned());
        if !visiting.insert(key.clone()) {
            return Ok(None);
        }
        let module = &self.files[path];
        let mut explicit = Vec::new();
        for (local, exported) in &module.exports {
            if exported == name {
                explicit.push(self.local_origin(path, local, visiting)?);
            }
        }
        for link in module.links.iter().filter(|link| link.kind == Kind::Export) {
            for binding in link.bindings.iter().filter(|binding| binding.local == name) {
                explicit.push(if let Some(source) = &link.source {
                    if let Some(source) = self.resolve(path, source)? {
                        self.export_origin(&source, &binding.imported, visiting)?
                    } else {
                        None
                    }
                } else {
                    self.local_origin(path, &binding.imported, visiting)?
                });
            }
        }
        let explicit_found = !explicit.is_empty();
        let mut candidates: BTreeSet<_> = explicit.into_iter().flatten().collect();
        if !explicit_found && name != "default" {
            for link in module.links.iter().filter(|link| link.kind == Kind::Star) {
                if let Some(source) = link
                    .source
                    .as_deref()
                    .map(|source| self.resolve(path, source))
                    .transpose()?
                    .flatten()
                    && let Some(origin) = self.export_origin(&source, name, visiting)?
                {
                    candidates.insert(origin);
                }
            }
        }
        visiting.remove(&key);
        ensure!(candidates.len() <= 1, "Ambiguous export {name} from {path}");
        Ok(candidates.into_iter().next())
    }

    fn local_origin(
        &self,
        path: &str,
        name: &str,
        visiting: &mut BTreeSet<(String, String)>,
    ) -> Result<Option<(String, usize)>> {
        let module = &self.files[path];
        if let Ok(symbol) = module.symbol(name) {
            return Ok(Some((path.into(), symbol.id)));
        }
        for link in module.links.iter().filter(|link| link.kind == Kind::Import) {
            for binding in link
                .bindings
                .iter()
                .filter(|binding| binding.local == name && binding.imported != "*")
            {
                if let Some(source) = link
                    .source
                    .as_deref()
                    .map(|source| self.resolve(path, source))
                    .transpose()?
                    .flatten()
                {
                    return self.export_origin(&source, &binding.imported, visiting);
                }
            }
        }
        Ok(None)
    }

    pub(super) fn move_file(&mut self, from: &str, to: &str) -> Result<()> {
        ensure!(
            from.ends_with(".cjs") == to.ends_with(".cjs"),
            "A file move cannot change CommonJS module mode"
        );
        ensure!(
            !self.files.contains_key(to),
            "Destination already exists: {to}"
        );
        ensure!(
            parse::language_for_path(from) == parse::language_for_path(to),
            "A file move cannot change its source language"
        );
        let mut edits = Edits::new();
        let mut expected = Vec::new();
        for (path, module) in &self.files {
            for link in &module.links {
                let Some(specifier) = &link.source else {
                    continue;
                };
                let resolved = self.resolve(path, specifier)?;
                if resolved.as_deref() == Some(from) || path == from && specifier.starts_with('.') {
                    let target = resolved.as_deref().with_context(|| {
                        format!("Cannot rebase unresolved relative module in {path}: {specifier}")
                    })?;
                    let destination = if target == from { to } else { target };
                    let importer = if path == from { to } else { path };
                    let specifier = self.specifier(importer, destination, Some(specifier));
                    expected.push((
                        importer.to_owned(),
                        specifier.clone(),
                        destination.to_owned(),
                    ));
                    edits.entry(path.clone()).or_default().push(Edit {
                        range: link.source_range.clone().unwrap(),
                        text: specifier,
                    });
                }
            }
        }
        self.apply(edits)?;
        let module = self.files.remove(from).unwrap();
        // Validate TSX/JSX and filename-dependent parsing at its new location.
        self.files
            .insert(to.into(), Module::new(to, module.source, module.structure)?);
        for (importer, specifier, destination) in expected {
            ensure!(
                self.resolve(&importer, &specifier)?.as_deref() == Some(&destination),
                "Moved module would not resolve unambiguously: {importer}: {specifier}"
            );
        }
        Ok(())
    }

    pub(super) fn rename(&mut self, path: &str, old: &str, new: &str) -> Result<()> {
        // Tree-sitter accepts reserved names in some binding positions. Enforce
        // strict module binding names as well as checking the generated syntax.
        ensure!(
            !matches!(
                new,
                "await"
                    | "break"
                    | "case"
                    | "catch"
                    | "class"
                    | "const"
                    | "continue"
                    | "debugger"
                    | "default"
                    | "delete"
                    | "do"
                    | "else"
                    | "enum"
                    | "export"
                    | "extends"
                    | "false"
                    | "finally"
                    | "for"
                    | "function"
                    | "if"
                    | "implements"
                    | "import"
                    | "in"
                    | "instanceof"
                    | "interface"
                    | "let"
                    | "new"
                    | "null"
                    | "package"
                    | "private"
                    | "protected"
                    | "public"
                    | "return"
                    | "static"
                    | "super"
                    | "switch"
                    | "this"
                    | "throw"
                    | "true"
                    | "try"
                    | "typeof"
                    | "var"
                    | "void"
                    | "while"
                    | "with"
                    | "yield"
                    | "eval"
                    | "arguments"
            ),
            "Reserved destination symbol name: {new}"
        );
        tree(path, &format!("export const {new} = 0;"))?;
        let module = &self.files[path];
        let symbol = module.symbol(old)?;
        let target = (path.to_owned(), symbol.id);
        ensure!(
            module.sites.iter().any(|site| site.declaration
                && site.binding_id == Some(symbol.id)
                && site.name == old),
            "Cannot locate declaration token for {old}"
        );
        let mut edits = Edits::new();
        module.rename_binding(symbol.id, old, new, edits.entry(path.into()).or_default())?;
        let mut pending = VecDeque::new();
        for (local, exported) in &module.exports {
            if local == old && exported == old {
                pending.push_back((path.to_owned(), old.to_owned(), new.to_owned()));
            }
        }
        for link in module
            .links
            .iter()
            .filter(|link| link.kind == Kind::Export && link.source.is_none())
        {
            for binding in &link.bindings {
                if binding.imported == old {
                    edits.entry(path.into()).or_default().push(Edit {
                        range: binding.name_range.clone(),
                        text: new.into(),
                    });
                    if !binding.alias {
                        pending.push_back((path.into(), old.into(), new.into()));
                    }
                }
            }
        }
        let mut visited = BTreeSet::new();
        while let Some((origin, old_export, new_export)) = pending.pop_front() {
            if !visited.insert((origin.clone(), old_export.clone(), new_export.clone())) {
                continue;
            }
            if self
                .export_origin(&origin, &old_export, &mut BTreeSet::new())?
                .as_ref()
                != Some(&target)
            {
                continue;
            }
            ensure!(
                self.export_origin(&origin, &new_export, &mut BTreeSet::new())?
                    .is_none()
                    && !self.files[&origin]
                        .exports
                        .iter()
                        .any(|(_, exported)| exported == &new_export)
                    && !self.files[&origin]
                        .links
                        .iter()
                        .filter(|link| link.kind == Kind::Export)
                        .flat_map(|link| &link.bindings)
                        .any(|binding| binding.local == new_export),
                "Export {new_export} already exists in {origin}"
            );
            for (consumer, module) in &self.files {
                for link in &module.links {
                    let Some(source) = &link.source else {
                        continue;
                    };
                    if self.resolve(consumer, source)?.as_deref() != Some(&origin) {
                        continue;
                    }
                    ensure!(
                        !matches!(link.kind, Kind::Dynamic | Kind::Require),
                        "Cannot rename through dynamic import/require in {consumer}"
                    );
                    if link.kind == Kind::Star {
                        pending.push_back((
                            consumer.clone(),
                            old_export.clone(),
                            new_export.clone(),
                        ));
                    }
                    for binding in &link.bindings {
                        if binding.imported == "*" {
                            ensure!(
                                link.kind == Kind::Import,
                                "Namespace re-exports cannot be renamed safely: {consumer}"
                            );
                            self.namespace_edits(
                                consumer,
                                module,
                                link,
                                binding,
                                (&old_export, &new_export),
                                &mut edits,
                            )?;
                        } else if binding.imported == old_export {
                            edits.entry(consumer.clone()).or_default().push(Edit {
                                range: binding.name_range.clone(),
                                text: new_export.clone(),
                            });
                            if !binding.alias {
                                if link.kind == Kind::Import {
                                    let id = link.id.context("Missing indexed import binding")?;
                                    module.rename_binding(
                                        id,
                                        &binding.local,
                                        &new_export,
                                        edits.entry(consumer.clone()).or_default(),
                                    )?;
                                    for export in module.links.iter().filter(|link| {
                                        link.kind == Kind::Export && link.source.is_none()
                                    }) {
                                        for entry in export
                                            .bindings
                                            .iter()
                                            .filter(|entry| entry.imported == binding.local)
                                        {
                                            edits.entry(consumer.clone()).or_default().push(Edit {
                                                range: entry.name_range.clone(),
                                                text: new_export.clone(),
                                            });
                                            if !entry.alias {
                                                pending.push_back((
                                                    consumer.clone(),
                                                    entry.local.clone(),
                                                    new_export.clone(),
                                                ));
                                            }
                                        }
                                    }
                                } else {
                                    pending.push_back((
                                        consumer.clone(),
                                        binding.local.clone(),
                                        new_export.clone(),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
        self.apply(edits)
    }

    fn namespace_edits(
        &self,
        path: &str,
        module: &Module,
        link: &Link,
        binding: &Binding,
        names: (&str, &str),
        edits: &mut Edits,
    ) -> Result<()> {
        let (old, new) = names;
        let id = link.id.context("Missing indexed namespace binding")?;
        for site in module
            .sites
            .iter()
            .filter(|site| site.binding_id == Some(id))
        {
            if site.receiver.as_deref() == Some(&binding.local) && site.name == old {
                edits.entry(path.into()).or_default().push(Edit {
                    range: site.range.clone(),
                    text: new.into(),
                });
            } else if site.receiver.is_none() && site.name == binding.local {
                let node = module
                    .tree
                    .root_node()
                    .descendant_for_byte_range(site.range.start, site.range.end)
                    .context("Missing namespace reference")?;
                ensure!(
                    node.parent().is_some_and(|parent| matches!(
                        parent.kind(),
                        "member_expression" | "nested_type_identifier"
                    ) && parent
                        .child_by_field_name("object")
                        .or_else(|| parent.child_by_field_name("module"))
                        == Some(node)),
                    "Namespace escapes or computed access in {path}; cannot safely update {}",
                    binding.local
                );
            }
        }
        Ok(())
    }

    pub(super) fn move_symbol(&mut self, from: &str, name: &str, to: &str) -> Result<()> {
        ensure!(
            !from.ends_with(".cjs") && !to.ends_with(".cjs"),
            "Moving functions requires ES modules, not .cjs files"
        );
        ensure!(
            parse::language_for_path(from) == parse::language_for_path(to),
            "A symbol move cannot change its source language"
        );
        if !self.files.contains_key(to) {
            self.files.insert(
                to.into(),
                Module::new(to, String::new(), FileStructure::default())?,
            );
        }
        let module = &self.files[from];
        let symbol = module.symbol(name)?;
        ensure!(
            matches!(symbol.kind.as_str(), "function" | "generator"),
            "Only top-level functions can currently move between files"
        );
        let declaration = module.declaration(symbol)?;
        ensure!(
            !has_token(declaration, "default"),
            "Moving default exports is not supported"
        );
        let inner = declaration
            .child_by_field_name("declaration")
            .unwrap_or(declaration);
        ensure!(
            matches!(
                inner.kind(),
                "function_declaration"
                    | "generator_function_declaration"
                    | "lexical_declaration"
                    | "variable_declaration"
            ),
            "Unsupported function declaration for move"
        );
        if matches!(inner.kind(), "lexical_declaration" | "variable_declaration") {
            ensure!(
                children(inner)
                    .into_iter()
                    .filter(|node| node.kind() == "variable_declarator")
                    .count()
                    == 1,
                "Cannot move one function out of a multi-binding declaration"
            );
        }
        let destination = &self.files[to];
        let own_imports: Vec<_> = destination
            .links
            .iter()
            .filter(|link| {
                link.kind == Kind::Import
                    && link
                        .bindings
                        .iter()
                        .any(|binding| binding.imported == name && binding.local == name)
            })
            .filter(|link| {
                link.source.as_deref().is_some_and(|source| {
                    self.resolve(to, source).ok().flatten().as_deref() == Some(from)
                })
            })
            .collect();
        if own_imports.is_empty() {
            destination.check_name("", name)?;
        } else {
            ensure!(
                own_imports.len() == 1
                    && !destination
                        .sites
                        .iter()
                        .any(|site| site.declaration && site.name == name),
                "Destination has a conflicting declaration for {name}"
            );
        }
        let start = symbol.start_byte.min(declaration.start_byte());
        let range = start..declaration.end_byte();
        let mut body = module.source[range.clone()].to_owned();
        if declaration.kind() != "export_statement" {
            body.insert_str(declaration.start_byte() - start, "export ");
        }
        let mut additions: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut seen_dependencies = BTreeSet::new();
        for site in module.sites.iter().filter(|site| {
            !site.declaration && range.start <= site.range.start && site.range.end <= range.end
        }) {
            let Some(id) = site.binding_id else {
                // A free name must not become captured by a destination local.
                // Unknown parameter/local bindings are conservative here too.
                destination.check_name("", site.receiver.as_deref().unwrap_or(&site.name))?;
                continue;
            };
            if id == symbol.id
                || !seen_dependencies.insert((
                    id,
                    site.receiver.clone().unwrap_or_else(|| site.name.clone()),
                ))
            {
                continue;
            }
            let dependency = &module.structure.nodes[id];
            if range.start <= dependency.start_byte && dependency.end_byte <= range.end {
                continue;
            }
            if dependency.kind == "import" {
                let link = module
                    .links
                    .iter()
                    .find(|link| link.id == Some(id) && link.kind == Kind::Import)
                    .context("Missing dependency import")?;
                let local = site.receiver.as_deref().unwrap_or(&site.name);
                let binding = link
                    .bindings
                    .iter()
                    .find(|binding| binding.local == local)
                    .context("Missing dependency binding")?;
                let source = link
                    .source
                    .as_deref()
                    .context("Missing dependency module")?;
                if let Some(resolved) = self.resolve(from, source)? {
                    let origin =
                        self.export_origin(&resolved, &binding.imported, &mut BTreeSet::new())?;
                    if origin.is_some()
                        && self.local_origin(to, local, &mut BTreeSet::new())? == origin
                    {
                        continue;
                    }
                    ensure!(
                        resolved != to,
                        "Cannot move a dependency alias/namespace into its own module: {local}"
                    );
                }
                let source = if source.starts_with('.') {
                    self.checked_specifier(
                        to,
                        &self.resolve(from, source)?.with_context(|| {
                            format!("Cannot move unresolved dependency: {source}")
                        })?,
                        Some(source),
                    )?
                } else {
                    source.into()
                };
                if !destination.has_import(&source, binding, link.type_only) {
                    destination.check_name("", local)?;
                    additions
                        .entry(to.into())
                        .or_default()
                        .push(import_statement(&source, binding, link.type_only));
                }
            } else {
                let key = (from.to_owned(), dependency.id);
                if self
                    .local_origin(to, &dependency.name, &mut BTreeSet::new())?
                    .as_ref()
                    == Some(&key)
                {
                    continue;
                }
                let exported_names: BTreeSet<_> = module
                    .exports
                    .iter()
                    .map(|(_, exported)| exported.as_str())
                    .chain(
                        module
                            .links
                            .iter()
                            .filter(|link| link.kind == Kind::Export && link.source.is_none())
                            .flat_map(|link| {
                                link.bindings.iter().map(|binding| binding.local.as_str())
                            }),
                    )
                    .collect();
                let mut exported = None;
                for candidate in exported_names {
                    if self
                        .export_origin(from, candidate, &mut BTreeSet::new())?
                        .as_ref()
                        == Some(&key)
                    {
                        exported = Some(candidate);
                        break;
                    }
                }
                let exported = exported.with_context(|| format!("Cannot move {name}: dependency {} is not an exported top-level declaration", dependency.name))?;
                ensure!(
                    dependency.parent_id.is_none(),
                    "Cannot move a captured nested dependency: {}",
                    dependency.name
                );
                destination.check_name("", &dependency.name)?;
                additions
                    .entry(to.into())
                    .or_default()
                    .push(named_statement(
                        "import",
                        &self.checked_specifier(to, from, None)?,
                        exported,
                        &dependency.name,
                        matches!(dependency.kind.as_str(), "interface" | "type"),
                    ));
            }
        }
        // Location-dependent constructs must not silently change meaning.
        ensure!(
            !walk(inner)
                .iter()
                .any(|node| matches!(node.kind(), "meta_property" | "this" | "super")),
            "Cannot move location-dependent this/super/import.meta code"
        );
        ensure!(
            !module
                .links
                .iter()
                .any(|link| range.start <= link.range.start && link.range.end <= range.end),
            "Moving functions containing import/require is not supported"
        );
        let mut edits = Edits::new();
        edits.entry(from.into()).or_default().push(Edit {
            range: range.clone(),
            text: String::new(),
        });
        let needs_local = module.sites.iter().any(|site| {
            site.binding_id == Some(symbol.id)
                && !site.declaration
                && !(range.start <= site.range.start && site.range.end <= range.end)
        }) || module.links.iter().any(|link| {
            link.kind == Kind::Export
                && link.source.is_none()
                && link.bindings.iter().any(|binding| binding.imported == name)
        });
        if needs_local {
            additions
                .entry(from.into())
                .or_default()
                .push(named_statement(
                    "import",
                    &self.checked_specifier(from, to, None)?,
                    name,
                    name,
                    false,
                ));
        }
        for (path, consumer) in &self.files {
            for link in &consumer.links {
                let Some(source) = &link.source else {
                    continue;
                };
                if self.resolve(path, source)?.as_deref() != Some(from) {
                    continue;
                }
                ensure!(
                    !matches!(link.kind, Kind::Dynamic | Kind::Require)
                        && !link.bindings.iter().any(|binding| binding.imported == "*"),
                    "Cannot move {name} through namespace/dynamic import/require in {path}"
                );
                if link.kind == Kind::Star
                    && self.export_origin(path, name, &mut BTreeSet::new())?
                        == Some((from.to_owned(), symbol.id))
                {
                    additions
                        .entry(path.clone())
                        .or_default()
                        .push(named_statement(
                            "export",
                            &self.checked_specifier(path, to, Some(source))?,
                            name,
                            name,
                            link.type_only,
                        ));
                }
                for binding in link
                    .bindings
                    .iter()
                    .filter(|binding| binding.imported == name)
                {
                    if path == to && link.kind == Kind::Import {
                        ensure!(
                            binding.local == name,
                            "Destination imports {name} under an alias; remove that import before moving"
                        );
                    } else {
                        additions
                            .entry(path.clone())
                            .or_default()
                            .push(named_statement(
                                if link.kind == Kind::Import {
                                    "import"
                                } else {
                                    "export"
                                },
                                &self.checked_specifier(path, to, Some(source))?,
                                &binding.imported,
                                &binding.local,
                                link.type_only || binding.type_only,
                            ));
                    }
                    edits
                        .entry(path.clone())
                        .or_default()
                        .push(remove_binding(consumer, link, binding)?);
                }
            }
        }
        for (path, statements) in additions {
            let module = &self.files[&path];
            let mut unique = BTreeSet::new();
            let statements: String = statements
                .into_iter()
                .filter(|statement| unique.insert(statement.clone()))
                .collect();
            let offset = module.insertion();
            // A source may have no imports and begin with the moved function:
            // insertion at the removed range's boundary is handled by edited().
            edits.entry(path).or_default().push(Edit {
                range: offset..offset,
                text: format!("\n{statements}\n"),
            });
        }
        edits.entry(to.into()).or_default().push(Edit {
            range: destination.source.len()..destination.source.len(),
            text: format!("\n{body}\n"),
        });
        self.apply(edits)
    }
}

impl Module {
    fn has_import(&self, source: &str, binding: &Binding, type_only: bool) -> bool {
        self.links.iter().any(|link| {
            link.kind == Kind::Import
                && link.source.as_deref() == Some(source)
                && (type_only || !link.type_only)
                && link.bindings.iter().any(|entry| {
                    entry.imported == binding.imported
                        && entry.local == binding.local
                        && (binding.type_only || !entry.type_only)
                })
        })
    }
}

fn remove_binding(module: &Module, link: &Link, binding: &Binding) -> Result<Edit> {
    if link.bindings.len() == 1 {
        return Ok(Edit {
            range: link.range.clone(),
            text: String::new(),
        });
    }
    let clause = walk(module.tree.root_node())
        .into_iter()
        .find(|node| {
            matches!(node.kind(), "named_imports" | "export_clause")
                && node.start_byte() <= binding.range.start
                && binding.range.end <= node.end_byte()
        })
        .context("Cannot split this import/export declaration")?;
    ensure!(
        !["/*", "//"]
            .iter()
            .any(|&pattern| text(&module.source, clause).contains(pattern)),
        "Cannot safely split a commented import/export"
    );
    let siblings = children(clause);
    let index = siblings
        .iter()
        .position(|node| node.byte_range() == binding.range)
        .context("Cannot locate import/export specifier")?;
    let range = if let Some(next) = siblings.get(index + 1) {
        binding.range.start..next.start_byte()
    } else if index > 0 {
        siblings[index - 1].end_byte()..binding.range.end
    } else {
        // A named specifier alongside a default import: remove the whole named
        // clause and its preceding comma, preserving the default binding.
        let prefix = &module.source[link.range.start..clause.start_byte()];
        let comma = prefix
            .rfind(',')
            .context("Cannot split default and named import")?;
        link.range.start + comma..clause.end_byte()
    };
    Ok(Edit {
        range,
        text: String::new(),
    })
}

fn named_statement(
    kind: &str,
    source: &str,
    imported: &str,
    local: &str,
    type_only: bool,
) -> String {
    let binding = if imported == local {
        imported.to_owned()
    } else {
        format!("{imported} as {local}")
    };
    format!(
        "{kind} {}{{ {binding} }} from {};\n",
        if type_only { "type " } else { "" },
        serde_json::to_string(source).unwrap()
    )
}

fn import_statement(source: &str, binding: &Binding, type_only: bool) -> String {
    let module = serde_json::to_string(source).unwrap();
    match binding.imported.as_str() {
        "default" => format!(
            "import {}{} from {module};\n",
            if type_only { "type " } else { "" },
            binding.local
        ),
        "*" => format!(
            "import {}* as {} from {module};\n",
            if type_only { "type " } else { "" },
            binding.local
        ),
        _ => named_statement(
            "import",
            source,
            &binding.imported,
            &binding.local,
            type_only || binding.type_only,
        ),
    }
}

fn relative_module(from: &str, to: &str, style: Option<&str>) -> String {
    let mut target = to.to_owned();
    match style.and_then(|style| {
        Path::new(style)
            .extension()
            .and_then(|extension| extension.to_str())
    }) {
        Some("js" | "jsx" | "mjs" | "cjs") => {
            let extension = match Path::new(to)
                .extension()
                .and_then(|extension| extension.to_str())
            {
                Some("mts" | "mjs") => "mjs",
                Some("cts" | "cjs") => "cjs",
                Some("js") => "js",
                Some("jsx") => "jsx",
                _ => Path::new(style.unwrap())
                    .extension()
                    .unwrap()
                    .to_str()
                    .unwrap(),
            };
            if let Some(offset) = target.rfind('.') {
                target.replace_range(offset + 1.., extension);
            }
        }
        Some(_) => {}
        None => {
            if let Some(offset) = target.rfind('.') {
                target.truncate(offset);
            }
            if style.is_some_and(|style| !style.ends_with("/index")) && target.ends_with("/index") {
                target.truncate(target.len() - 6);
            }
        }
    }
    let from: Vec<_> = from.split('/').collect();
    let from = &from[..from.len() - 1];
    let to: Vec<_> = target.split('/').collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut result = "../".repeat(from.len() - common);
    result.push_str(&to[common..].join("/"));
    if !result.starts_with('.') {
        result.insert_str(0, "./");
    }
    result
}

fn edited(source: &str, mut edits: Vec<Edit>) -> Result<String> {
    edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
    edits.dedup_by(|a, b| a.range == b.range && a.text == b.text);
    let mut result = String::new();
    let mut end = 0;
    for edit in edits {
        ensure!(
            edit.range.start >= end,
            "Conflicting refactoring edits; no files were changed"
        );
        result.push_str(
            source
                .get(end..edit.range.start)
                .context("Invalid edit boundary")?,
        );
        result.push_str(&edit.text);
        end = edit.range.end;
    }
    result.push_str(source.get(end..).context("Invalid edit boundary")?);
    Ok(result)
}
