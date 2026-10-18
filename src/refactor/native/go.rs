use super::*;

#[derive(Clone)]
pub(super) struct Import {
    id: usize,
    range: Range<usize>,
    statement: Range<usize>,
    path: String,
    alias: String,
}

struct Layout {
    packages: BTreeMap<String, String>,
    modules: Vec<(String, String)>,
}

fn directory(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(directory, _)| directory)
}

fn build_suffix(path: &str) -> Vec<&str> {
    let stem = Path::new(path)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .trim_end_matches("_test");
    stem.rsplit('_')
        .take_while(|part| {
            matches!(
                *part,
                "aix"
                    | "android"
                    | "darwin"
                    | "dragonfly"
                    | "freebsd"
                    | "illumos"
                    | "ios"
                    | "js"
                    | "linux"
                    | "netbsd"
                    | "openbsd"
                    | "plan9"
                    | "solaris"
                    | "wasip1"
                    | "windows"
                    | "386"
                    | "amd64"
                    | "arm"
                    | "arm64"
                    | "loong64"
                    | "mips"
                    | "mipsle"
                    | "mips64"
                    | "mips64le"
                    | "ppc64"
                    | "ppc64le"
                    | "riscv64"
                    | "s390x"
                    | "wasm"
            )
        })
        .collect()
}

fn package(module: &Module) -> Result<(String, Range<usize>)> {
    let node = children(module.tree.root_node())
        .into_iter()
        .find(|node| node.kind() == "package_clause")
        .context("Go source requires a package clause")?;
    let name = children(node)
        .into_iter()
        .find(|node| matches!(node.kind(), "package_identifier" | "identifier"))
        .context("Cannot read Go package name")?;
    Ok((text(&module.source, name).into(), node.byte_range()))
}

impl Layout {
    fn target(&self, path: &str) -> Result<Option<String>> {
        let mut found = BTreeSet::new();
        for (directory, name) in &self.modules {
            let suffix = if path == name {
                Some("")
            } else {
                path.strip_prefix(&format!("{name}/"))
            };
            if let Some(suffix) = suffix {
                let directory = [directory.as_str(), suffix]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join("/");
                if self.packages.contains_key(&directory) {
                    found.insert(directory);
                }
            }
        }
        if self.modules.is_empty() {
            for directory in self
                .packages
                .keys()
                .filter(|directory| !directory.is_empty())
            {
                if path == directory || path.ends_with(&format!("/{directory}")) {
                    found.insert(directory.clone());
                }
            }
        }
        ensure!(found.len() <= 1, "Ambiguous Go package import: {path}");
        Ok(found.into_iter().next())
    }

    fn import_path(&self, directory: &str) -> Result<String> {
        let (root, module) = self
            .modules
            .iter()
            .filter(|(root, _)| {
                root.is_empty() || directory == root || directory.starts_with(&format!("{root}/"))
            })
            .max_by_key(|(root, _)| root.len())
            .context("Moving across Go packages requires an indexed workspace with go.mod")?;
        let suffix = directory
            .strip_prefix(root)
            .unwrap()
            .trim_start_matches('/');
        Ok(if suffix.is_empty() {
            module.clone()
        } else {
            format!("{module}/{suffix}")
        })
    }
}

impl Workspace {
    fn go_layout(&mut self) -> Result<Layout> {
        let mut packages = BTreeMap::new();
        let mut policies = BTreeSet::new();
        for (path, module) in &self.files {
            let (name, _) = package(module)?;
            let directory = directory(path).to_owned();
            if let Some(previous) = packages.insert(directory.clone(), name.clone()) {
                ensure!(
                    previous == name,
                    "Mixed Go packages/build variants in {directory}; refactor one package at a time"
                );
            }
            let mut parent = Some(Path::new(&directory));
            while let Some(path) = parent {
                policies.insert(path.join("go.mod").to_string_lossy().replace('\\', "/"));
                parent = path.parent();
            }
        }
        let module = regex::Regex::new(r#"(?m)^\s*module\s+(?:"([^"]+)"|([^\s]+))"#)?;
        let mut modules = Vec::new();
        for path in policies {
            if let Some(source) = self.policy(&path)? {
                let value = module
                    .captures(&source)
                    .context("Cannot read module path from go.mod")?;
                modules.push((
                    directory(&path).into(),
                    value
                        .get(1)
                        .or_else(|| value.get(2))
                        .unwrap()
                        .as_str()
                        .into(),
                ));
            }
        }
        Ok(Layout { packages, modules })
    }

    fn go_imports(&self, path: &str, layout: &Layout) -> Result<Vec<Import>> {
        let module = &self.files[path];
        if let Some(imports) = module.go_imports.get() {
            return Ok(imports.clone());
        }
        let mut imports = Vec::new();
        for node in walk(module.tree.root_node())
            .into_iter()
            .filter(|node| node.kind() == "import_spec")
        {
            let value = node
                .child_by_field_name("path")
                .context("Cannot read Go import")?;
            let source = text(&module.source, value);
            ensure!(
                !source.contains('\\'),
                "Escaped Go import paths are not supported"
            );
            let source = source.trim_matches(['"', '`']).to_owned();
            let target = layout.target(&source)?;
            let alias = node
                .child_by_field_name("name")
                .map(|node| text(&module.source, node).to_owned())
                .unwrap_or_else(|| {
                    target
                        .as_ref()
                        .and_then(|directory| layout.packages.get(directory))
                        .cloned()
                        .unwrap_or_else(|| source.rsplit('/').next().unwrap_or(&source).into())
                });
            let statement = std::iter::successors(Some(node), |node| node.parent())
                .find(|node| node.kind() == "import_declaration")
                .unwrap();
            let id = module
                .structure
                .nodes
                .iter()
                .find(|symbol| symbol.kind == "import" && symbol.end_byte == node.end_byte())
                .context("Missing indexed Go import")?
                .id;
            imports.push(Import {
                id,
                range: node.byte_range(),
                statement: statement.byte_range(),
                path: source,
                alias,
            });
        }
        let _ = module.go_imports.set(imports.clone());
        Ok(imports)
    }

    fn go_global(&self, directory: &str, name: &str) -> Result<Option<Key>> {
        let found: Vec<_> = self
            .files
            .iter()
            .filter(|(path, _)| go::directory(path) == directory)
            .flat_map(|(path, module)| {
                module
                    .structure
                    .nodes
                    .iter()
                    .filter(move |node| {
                        node.parent_id.is_none()
                            && !matches!(node.kind.as_str(), "import" | "method")
                            && node.name == name
                    })
                    .map(move |node| (path.clone(), node.id))
            })
            .collect();
        ensure!(
            found.len() <= 1,
            "Ambiguous Go package declaration {name}; build-tag variants are not inferred"
        );
        Ok(found.into_iter().next())
    }

    fn go_origin(&self, path: &str, site: &EditSite, layout: &Layout) -> Result<Option<Key>> {
        let module = &self.files[path];
        if let Some(id) = site.binding_id {
            if module.structure.nodes[id].kind != "import" {
                return Ok(site.receiver.is_none().then(|| (path.into(), id)));
            }
        } else if site.bound {
            return Ok(None);
        }
        let mut found = BTreeSet::new();
        for import in self.go_imports(path, layout)? {
            if (site.receiver.as_deref() == Some(&import.alias)
                || site.receiver.is_none() && import.alias == ".")
                && let Some(target) = layout.target(&import.path)?
                && let Some(key) = self.go_global(&target, &site.name)?
            {
                found.insert(key);
            }
        }
        let package_name = site.receiver.is_none()
            && self
                .go_imports(path, layout)?
                .iter()
                .any(|import| import.alias == site.name);
        if site.receiver.is_none()
            && !site.bound
            && !package_name
            && let Some(key) = self.go_global(directory(path), &site.name)?
        {
            found.insert(key);
        }
        ensure!(
            found.len() <= 1,
            "Ambiguous Go reference {} in {path}",
            site.name
        );
        Ok(found.into_iter().next())
    }

    pub(super) fn go_rename(&mut self, path: &str, selected: &str, new: &str) -> Result<()> {
        let layout = self.go_layout()?;
        let symbol = self.files[path].symbol(selected)?;
        ensure!(
            symbol.name != "init"
                && new != "init"
                && !(symbol.name == "main" && layout.packages[directory(path)] == "main"),
            "Go lifecycle functions cannot be renamed as ordinary declarations"
        );
        ensure!(
            symbol.parent_id.is_none() && symbol.kind != "method",
            "Go member renames require receiver-type resolution; select a package declaration"
        );
        let key = (path.to_owned(), symbol.id);
        ensure!(
            self.go_global(directory(path), &symbol.name)?.as_ref() == Some(&key),
            "Ambiguous Go declaration"
        );
        for (other, module) in &self.files {
            if directory(other) == directory(path) {
                module.name_conflict(new)?;
            }
        }
        let mut edits = Edits::new();
        for (other, module) in &self.files {
            for site in &module.sites {
                if self.go_origin(other, site, &layout)?.as_ref() == Some(&key) {
                    ensure!(
                        directory(other) == directory(path) || exported(new),
                        "Go references from another package require an exported (uppercase) destination name"
                    );
                    if directory(other) != directory(path) && site.receiver.is_none() {
                        module.name_conflict(new)?;
                    }
                    edits
                        .entry(other.clone())
                        .or_default()
                        .push(module.token(site, new));
                }
            }
        }
        self.apply(edits)
    }

    fn go_import(
        &self,
        path: &str,
        target: &str,
        layout: &Layout,
        additions: &mut BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<String> {
        ensure!(
            layout
                .packages
                .get(target)
                .is_none_or(|name| name != "main"),
            "Go program packages cannot be imported as libraries"
        );
        let import_path = layout.import_path(target)?;
        if let Some(import) = self
            .go_imports(path, layout)?
            .into_iter()
            .find(|import| import.path == import_path)
        {
            ensure!(
                !matches!(import.alias.as_str(), "_" | "."),
                "Cannot reuse a blank/dot Go import for a moved declaration"
            );
            return Ok(import.alias);
        }
        let name = layout
            .packages
            .get(target)
            .context("Missing destination Go package")?
            .clone();
        if !additions
            .get(path)
            .is_some_and(|imports| imports.contains_key(&import_path))
        {
            self.files[path].name_conflict(&name)?;
        }
        let imports = additions.entry(path.into()).or_default();
        ensure!(
            !imports
                .iter()
                .any(|(other, alias)| other != &import_path && alias == &name),
            "Conflicting Go import aliases: {name}"
        );
        imports.insert(import_path, name.clone());
        Ok(name)
    }

    pub(super) fn go_move_symbol(&mut self, from: &str, selected: &str, to: &str) -> Result<()> {
        ensure!(
            from.ends_with("_test.go") == to.ends_with("_test.go")
                && build_suffix(from).is_empty()
                && build_suffix(to).is_empty(),
            "Moving Go functions cannot change test/platform build constraints"
        );
        let mut layout = self.go_layout()?;
        let source_package = layout.packages[directory(from)].clone();
        if !self.files.contains_key(to) {
            let physical = self.root.join(directory(to));
            if !layout.packages.contains_key(directory(to)) && physical.is_dir() {
                for entry in std::fs::read_dir(physical)? {
                    let entry = entry?;
                    ensure!(
                        !entry.file_name().to_string_lossy().ends_with(".go"),
                        "Destination Go package contains unindexed source; include the complete package before moving declarations"
                    );
                }
            }
            let name = layout
                .packages
                .get(directory(to))
                .cloned()
                .unwrap_or_else(|| {
                    if directory(from) == directory(to) {
                        source_package.clone()
                    } else {
                        directory(to).rsplit('/').next().unwrap_or("main").into()
                    }
                });
            valid_name("go", &name)?;
            self.set_source(to, format!("package {name}\n"))?;
            layout.packages.insert(directory(to).into(), name);
        }
        ensure!(
            !self.files[from].source.contains("//go:build")
                && !self.files[from].source.contains("// +build"),
            "Moving Go declarations with build constraints is not supported"
        );
        let symbol = self.files[from].symbol(selected)?;
        ensure!(
            symbol.kind == "function" && symbol.parent_id.is_none(),
            "Only free Go functions can move between files"
        );
        let key = (from.to_owned(), symbol.id);
        let name = symbol.name.clone();
        let range = self.files[from].declaration_range(symbol.id)?;
        let cross_package = directory(from) != directory(to);
        ensure!(
            name != "init" && !(cross_package && name == "main" && source_package == "main"),
            "Go init/main lifecycle functions cannot change package initialization or entrypoint semantics"
        );
        ensure!(
            self.go_global(directory(from), &name)?.as_ref() == Some(&key),
            "Ambiguous Go function"
        );
        if cross_package {
            ensure!(
                self.go_global(directory(to), &name)?.is_none(),
                "Destination Go package already declares {name}"
            );
        }
        let source = &self.files[from];
        let mut body_edits = Vec::new();
        let mut edits = Edits::new();
        let mut additions = BTreeMap::<String, BTreeMap<String, String>>::new();
        let mut copied = BTreeMap::<String, String>::new();
        let imports = self.go_imports(from, &layout)?;
        ensure!(
            imports
                .iter()
                .all(|import| import.alias != "." && import.path != "C"),
            "Moving Go functions with dot imports or cgo dependencies requires compiler/build-context resolution"
        );
        ensure!(
            !cross_package || !imports.iter().any(|import| import.alias == "_"),
            "Moving across Go packages with side-effect imports requires initialization analysis"
        );
        let mut prune = BTreeSet::new();
        for import in &imports {
            if source.sites.iter().any(|site| {
                range.start <= site.range.start
                    && site.range.end <= range.end
                    && (site.binding_id == Some(import.id)
                        || !site.bound && site.receiver.as_deref() == Some(&import.alias))
            }) {
                ensure!(
                    !matches!(import.alias.as_str(), "." | "_"),
                    "Moving functions using dot/blank imports is not supported"
                );
                prune.insert((from.to_owned(), import.path.clone()));
                if layout.target(&import.path)?.as_deref() == Some(directory(to)) {
                    for site in source.sites.iter().filter(|site| {
                        range.start <= site.range.start
                            && site.range.end <= range.end
                            && site.receiver.as_deref() == Some(&import.alias)
                    }) {
                        let node = source
                            .tree
                            .root_node()
                            .descendant_for_byte_range(site.range.start, site.range.end)
                            .unwrap()
                            .parent()
                            .context("Missing Go selector")?;
                        body_edits.push(Edit {
                            range: node.start_byte() - range.start..node.end_byte() - range.start,
                            text: site.name.clone(),
                        });
                    }
                } else if !self
                    .go_imports(to, &layout)?
                    .iter()
                    .any(|other| other.path == import.path && other.alias == import.alias)
                {
                    self.files[to].name_conflict(&import.alias)?;
                    copied.insert(import.path.clone(), import.alias.clone());
                }
            }
        }
        let mut dependencies = BTreeSet::new();
        for site in &source.sites {
            let inside = range.start <= site.range.start && site.range.end <= range.end;
            let origin = self.go_origin(from, site, &layout)?;
            if !inside || site.declaration {
                continue;
            }
            if origin.is_none() && !site.bound {
                let name = site.receiver.as_deref().unwrap_or(&site.name);
                ensure!(
                    imports.iter().any(|import| import.alias == name)
                        || self.go_global(directory(from), name)?.is_some()
                        || site.receiver.is_none() && builtin(name),
                    "Unresolved Go dependency {name}; external imports with nonmatching package names need an explicit alias before moving this function"
                );
            }
            if !cross_package {
                continue;
            }
            if let Some(dependency) = origin {
                if dependency == key
                    || directory(&dependency.0) != directory(from)
                    || !dependencies.insert(site.range.clone().start)
                {
                    continue;
                }
                let declaration = &self.files[&dependency.0].structure.nodes[dependency.1];
                if range.start <= declaration.start_byte
                    && declaration.end_byte <= range.end
                    && dependency.0 == from
                {
                    continue;
                }
                ensure!(
                    exported(&declaration.name),
                    "Cannot move function across Go packages: private dependency {}",
                    declaration.name
                );
                let alias = self.go_import(to, directory(from), &layout, &mut additions)?;
                ensure!(
                    !source
                        .sites
                        .iter()
                        .any(|site| range.start <= site.range.start
                            && site.range.end <= range.end
                            && site.declaration
                            && site.name == alias),
                    "Generated Go package alias would capture {alias}"
                );
                body_edits.push(Edit {
                    range: site.range.start - range.start..site.range.end - range.start,
                    text: format!("{alias}.{}", site.name),
                });
            }
        }
        if cross_package {
            for (path, module) in &self.files {
                for site in &module.sites {
                    if self.go_origin(path, site, &layout)?.as_ref() != Some(&key)
                        || site.declaration
                        || path == from
                            && range.start <= site.range.start
                            && site.range.end <= range.end
                    {
                        continue;
                    }
                    ensure!(
                        exported(&name),
                        "Moving a called function across Go packages requires an exported (uppercase) name"
                    );
                    if directory(path) == directory(to) {
                        let range = selector_range(module, site)?;
                        edits.entry(path.clone()).or_default().push(Edit {
                            range,
                            text: name.clone(),
                        });
                    } else {
                        let alias = self.go_import(path, directory(to), &layout, &mut additions)?;
                        edits.entry(path.clone()).or_default().push(Edit {
                            range: selector_range(module, site)?,
                            text: format!("{alias}.{name}"),
                        });
                    }
                    for import in self.go_imports(path, &layout)? {
                        if layout.target(&import.path)?.as_deref() == Some(directory(from)) {
                            ensure!(
                                import.alias != ".",
                                "Cross-package moves through dot imports are not supported"
                            );
                            prune.insert((path.clone(), import.path));
                        }
                    }
                }
            }
        }
        let body = edited(&source.source[range.clone()], body_edits)?;
        edits.entry(from.into()).or_default().push(Edit {
            range,
            text: String::new(),
        });
        let end = self.files[to].source.len();
        edits.entry(to.into()).or_default().push(Edit {
            range: end..end,
            text: format!("\n{body}\n"),
        });
        for (path, alias) in copied {
            additions.entry(to.into()).or_default().insert(path, alias);
        }
        for (path, imports) in additions {
            let offset = package(&self.files[&path])?.1.end;
            self.add_imports(
                &mut edits,
                &path,
                offset,
                imports.into_iter().map(|(path, alias)| {
                    format!("import {alias} {}\n", serde_json::to_string(&path).unwrap())
                }),
            );
        }
        self.apply(edits)?;
        self.go_prune_imports(&layout, &prune)?;
        self.go_check_cycles(&layout)
    }

    fn go_prune_imports(
        &mut self,
        layout: &Layout,
        candidates: &BTreeSet<(String, String)>,
    ) -> Result<()> {
        let mut edits = Edits::new();
        for (path, module) in &self.files {
            let imports = self.go_imports(path, layout)?;
            let unused: Vec<_> = imports
                .iter()
                .filter(|import| {
                    candidates.contains(&(path.clone(), import.path.clone()))
                        && !matches!(import.alias.as_str(), "_" | ".")
                        && !module.sites.iter().any(|site| {
                            !site.declaration
                                && (site.binding_id == Some(import.id)
                                    || !site.bound
                                        && (site.receiver.as_deref() == Some(&import.alias)
                                            || site.receiver.is_none()
                                                && site.name == import.alias))
                        })
                })
                .collect();
            for import in unused.iter() {
                let whole = imports
                    .iter()
                    .filter(|other| other.statement == import.statement)
                    .all(|other| unused.iter().any(|unused| unused.range == other.range));
                edits.entry(path.clone()).or_default().push(Edit {
                    range: if whole {
                        import.statement.clone()
                    } else {
                        import.range.clone()
                    },
                    text: String::new(),
                });
            }
        }
        self.apply(edits)
    }

    fn go_check_cycles(&self, layout: &Layout) -> Result<()> {
        let mut edges = BTreeMap::<String, BTreeSet<String>>::new();
        for path in self.files.keys() {
            for import in self.go_imports(path, layout)? {
                if let Some(target) = layout.target(&import.path)? {
                    edges
                        .entry(directory(path).into())
                        .or_default()
                        .insert(target);
                }
            }
        }
        fn visit(
            path: &str,
            edges: &BTreeMap<String, BTreeSet<String>>,
            active: &mut BTreeSet<String>,
            done: &mut BTreeSet<String>,
        ) -> Result<()> {
            if done.contains(path) {
                return Ok(());
            }
            ensure!(
                active.len() < 256 && active.insert(path.into()),
                "Refactoring would create a Go import cycle (or an excessively deep package graph)"
            );
            if let Some(next) = edges.get(path) {
                for next in next {
                    visit(next, edges, active, done)?;
                }
            }
            active.remove(path);
            done.insert(path.into());
            Ok(())
        }
        let mut done = BTreeSet::new();
        for path in edges.keys() {
            visit(path, &edges, &mut BTreeSet::new(), &mut done)?;
        }
        Ok(())
    }

    pub(super) fn go_move_file(&mut self, from: &str, to: &str) -> Result<()> {
        ensure!(
            from.ends_with("_test.go") == to.ends_with("_test.go"),
            "A Go file move cannot change its test-file role"
        );
        ensure!(
            build_suffix(from) == build_suffix(to),
            "A Go file move cannot change its platform build constraints"
        );
        if directory(from) == directory(to) {
            let module = self.files.remove(from).unwrap();
            return self.set_source(to, module.source);
        }
        let layout = self.go_layout()?;
        let other_sources = self
            .files
            .keys()
            .any(|path| path != from && directory(path) == directory(from));
        if !other_sources {
            for path in self.files.keys() {
                ensure!(
                    !self
                        .go_imports(path, &layout)?
                        .iter()
                        .any(|import| import.alias == "_"
                            && layout.target(&import.path).ok().flatten().as_deref()
                                == Some(directory(from))),
                    "Side-effect-only imports prevent moving the last Go file out of its package"
                );
            }
        }
        let declarations: Vec<_> = self.files[from]
            .structure
            .nodes
            .iter()
            .filter(|node| node.parent_id.is_none() && node.kind != "import")
            .collect();
        ensure!(
            declarations.len() == 1 && declarations[0].kind == "function",
            "Moving a Go file across packages currently requires one free function; move declarations individually otherwise"
        );
        let name = declarations[0].name.clone();
        let package_start = package(&self.files[from])?.1.start;
        let prefix = self.files[from].source[..package_start].to_owned();
        self.go_move_symbol(from, &name, to)?;
        if !prefix.is_empty() {
            self.set_source(to, format!("{prefix}{}", self.files[to].source))?;
        }
        self.files.remove(from);
        Ok(())
    }
}

fn exported(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

fn builtin(name: &str) -> bool {
    matches!(
        name,
        "any"
            | "bool"
            | "byte"
            | "comparable"
            | "complex64"
            | "complex128"
            | "error"
            | "float32"
            | "float64"
            | "int"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "rune"
            | "string"
            | "uint"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "uintptr"
            | "true"
            | "false"
            | "nil"
            | "append"
            | "cap"
            | "clear"
            | "close"
            | "complex"
            | "copy"
            | "delete"
            | "imag"
            | "len"
            | "make"
            | "max"
            | "min"
            | "new"
            | "panic"
            | "print"
            | "println"
            | "real"
            | "recover"
    )
}

fn selector_range(module: &Module, site: &EditSite) -> Result<Range<usize>> {
    if site.receiver.is_none() {
        return Ok(site.range.clone());
    }
    let node = module
        .tree
        .root_node()
        .descendant_for_byte_range(site.range.start, site.range.end)
        .context("Missing Go reference")?;
    let parent = node
        .parent()
        .filter(|node| node.kind() == "selector_expression")
        .context("Cannot isolate Go package selector")?;
    Ok(parent.byte_range())
}
