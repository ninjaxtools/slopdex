use super::*;

#[derive(Clone)]
struct Import {
    path: String,
    tokens: Vec<(String, Range<usize>)>,
    static_: bool,
    wildcard: bool,
}

#[derive(Clone)]
struct Reference {
    key: Key,
    range: Range<usize>,
    receiver: Option<Range<usize>>,
    qualified_type: Option<Range<usize>>,
    declaration: bool,
}

fn words(node: Node<'_>, source: &str) -> Vec<(String, Range<usize>)> {
    walk(node)
        .into_iter()
        .filter(|node| {
            matches!(node.kind(), "identifier" | "type_identifier") && node.named_child_count() == 0
        })
        .map(|node| (text(source, node).into(), node.byte_range()))
        .collect()
}

fn package(module: &Module) -> String {
    children(module.tree.root_node())
        .into_iter()
        .find(|node| node.kind() == "package_declaration")
        .map(|node| {
            words(node, &module.source)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>()
                .join(".")
        })
        .unwrap_or_default()
}

fn imports(module: &Module) -> Vec<Import> {
    children(module.tree.root_node())
        .into_iter()
        .filter(|node| node.kind() == "import_declaration")
        .map(|node| {
            let tokens = words(node, &module.source);
            let source = text(&module.source, node);
            Import {
                path: tokens
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join("."),
                tokens,
                static_: source.split_whitespace().any(|word| word == "static"),
                wildcard: source.contains('*'),
            }
        })
        .collect()
}

fn type_node(node: &StructureNode) -> bool {
    matches!(
        node.kind.as_str(),
        "class" | "interface" | "enum" | "record"
    )
}
fn modifier(node: &StructureNode, word: &str) -> bool {
    node.signature
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|token| token == word)
}

impl Workspace {
    fn java_type(&self, name: &str) -> Result<Option<Key>> {
        let mut found = Vec::new();
        for (path, module) in &self.files {
            for node in module
                .structure
                .nodes
                .iter()
                .filter(|node| node.parent_id.is_none() && type_node(node))
            {
                let qualified = [package(module), node.name.clone()]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(".");
                if qualified == name {
                    found.push((path.clone(), node.id));
                }
            }
        }
        ensure!(found.len() <= 1, "Ambiguous Java type {name}");
        Ok(found.into_iter().next())
    }

    fn java_fqn(&self, key: &Key) -> String {
        let module = &self.files[&key.0];
        [package(module), module.structure.nodes[key.1].name.clone()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(".")
    }

    fn java_member(&self, class: &Key, name: &str) -> Result<Option<Key>> {
        let found: Vec<_> = self.files[&class.0]
            .structure
            .nodes
            .iter()
            .filter(|node| {
                node.parent_id == Some(class.1) && node.name == name && modifier(node, "static")
            })
            .map(|node| (class.0.clone(), node.id))
            .collect();
        ensure!(
            found.len() <= 1,
            "Overloaded Java member {name} requires type-aware overload resolution"
        );
        Ok(found.into_iter().next())
    }

    fn java_simple_type(&self, path: &str, name: &str) -> Result<Option<Key>> {
        let module = &self.files[path];
        let mut found = BTreeSet::new();
        for import in imports(module).into_iter().filter(|import| !import.static_) {
            let qualified = if import.wildcard {
                format!("{}.{name}", import.path)
            } else if import.path.rsplit('.').next() == Some(name) {
                import.path
            } else {
                continue;
            };
            if let Some(key) = self.java_type(&qualified)? {
                found.insert(key);
            }
        }
        let qualified = [package(module), name.into()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(".");
        if let Some(key) = self.java_type(&qualified)? {
            found.insert(key);
        }
        ensure!(
            found.len() <= 1,
            "Ambiguous Java type reference {name} in {path}"
        );
        Ok(found.into_iter().next())
    }

    fn java_site(&self, path: &str, site: &EditSite) -> Result<Option<Key>> {
        let module = &self.files[path];
        if let Some(id) = site.binding_id
            && module.structure.nodes[id].kind != "import"
        {
            return if site.receiver.is_none() {
                Ok(Some((path.into(), id)))
            } else if type_node(&module.structure.nodes[id]) {
                self.java_member(&(path.into(), id), &site.name)
            } else {
                Ok(None)
            };
        }
        if site.bound
            && site.binding_id.is_none()
            && !matches!(site.receiver.as_deref(), Some("this" | "Self"))
        {
            return Ok(None);
        }
        if let Some(receiver) = &site.receiver {
            if receiver == "this"
                && let Some(class) = module
                    .structure
                    .nodes
                    .iter()
                    .filter(|node| {
                        type_node(node)
                            && node.start_byte <= site.range.start
                            && site.range.end <= node.end_byte
                    })
                    .min_by_key(|node| node.end_byte - node.start_byte)
            {
                return self.java_member(&(path.into(), class.id), &site.name);
            }
            if let Some(class) = self.java_simple_type(path, receiver)? {
                return self.java_member(&class, &site.name);
            }
            return Ok(None);
        }
        let mut found = BTreeSet::new();
        for import in imports(module).into_iter().filter(|import| import.static_) {
            let (class, member) = if import.wildcard {
                (import.path.as_str(), site.name.as_str())
            } else {
                let Some((class, member)) = import.path.rsplit_once('.') else {
                    continue;
                };
                if member != site.name {
                    continue;
                }
                (class, member)
            };
            if let Some(class) = self.java_type(class)?
                && let Some(key) = self.java_member(&class, member)?
            {
                found.insert(key);
            }
        }
        if let Some(class) = self.java_simple_type(path, &site.name)? {
            found.insert(class);
        }
        ensure!(
            found.len() <= 1,
            "Ambiguous Java symbol {} in {path}",
            site.name
        );
        Ok(found.into_iter().next())
    }

    fn java_refs(&self, path: &str) -> Result<Vec<Reference>> {
        let module = &self.files[path];
        let mut result = Vec::new();
        for site in &module.sites {
            if let Some(key) = self.java_site(path, site)? {
                let node = module
                    .tree
                    .root_node()
                    .descendant_for_byte_range(site.range.start, site.range.end);
                let receiver = site.receiver.as_ref().and_then(|_| {
                    node.and_then(|node| node.parent())
                        .and_then(|node| node.child_by_field_name("object"))
                        .map(|node| node.byte_range())
                });
                result.push(Reference {
                    key,
                    range: site.range.clone(),
                    receiver,
                    qualified_type: None,
                    declaration: site.declaration,
                });
            }
        }
        for node in walk(module.tree.root_node()) {
            if std::iter::successors(node.parent(), |node| node.parent())
                .any(|node| matches!(node.kind(), "import_declaration" | "package_declaration"))
            {
                continue;
            }
            if matches!(node.kind(), "scoped_type_identifier" | "field_access") {
                let parts = words(node, &module.source);
                let qualified = parts
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(".");
                if let Some(key) = self.java_type(&qualified)?
                    && let Some((_, last)) = parts.last()
                {
                    let shadowed = parts.first().is_some_and(|(_, range)| {
                        module
                            .sites
                            .iter()
                            .any(|site| site.range == *range && site.bound)
                    });
                    ensure!(
                        !shadowed,
                        "Ambiguous shadowed Java package qualifier in {path}"
                    );
                    result.push(Reference {
                        key,
                        range: last.clone(),
                        receiver: None,
                        qualified_type: Some(node.byte_range()),
                        declaration: false,
                    });
                }
            }
            if node.kind() == "method_invocation"
                && let Some(object) = node.child_by_field_name("object")
            {
                let parts = words(object, &module.source);
                let qualified = parts
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(".");
                if parts.len() > 1
                    && let Some(class) = self.java_type(&qualified)?
                    && let Some(name) = node.child_by_field_name("name")
                    && let Some(key) = self.java_member(&class, text(&module.source, name))?
                {
                    result.push(Reference {
                        key,
                        range: name.byte_range(),
                        receiver: Some(object.byte_range()),
                        qualified_type: None,
                        declaration: false,
                    });
                }
            }
        }
        result.sort_by_key(|reference| {
            (
                reference.range.start,
                reference.range.end,
                reference.key.clone(),
            )
        });
        result.dedup_by(|a, b| {
            a.range == b.range
                && a.key == b.key
                && a.receiver == b.receiver
                && a.qualified_type == b.qualified_type
        });
        Ok(result)
    }

    pub(super) fn java_rename(
        &mut self,
        path: &str,
        selected: &str,
        new: &str,
        rename_file: bool,
    ) -> Result<()> {
        let symbol = self.files[path].symbol(selected)?;
        let key = (path.to_owned(), symbol.id);
        let is_type = type_node(symbol) && symbol.parent_id.is_none();
        ensure!(
            is_type || symbol.parent_id.is_some() && modifier(symbol, "static"),
            "Java refactors support top-level types and static members; instance members require receiver-type resolution"
        );
        let old = symbol.name.clone();
        let class = if is_type {
            key.clone()
        } else {
            (path.into(), symbol.parent_id.unwrap())
        };
        let class_name = self.java_fqn(&class);
        let public = modifier(symbol, "public");
        let mut edits = Edits::new();
        for (other, module) in &self.files {
            if is_type {
                ensure!(
                    !walk(module.tree.root_node()).iter().any(|node| matches!(
                        node.kind(),
                        "annotation" | "marker_annotation"
                    ) && words(
                        *node,
                        &module.source
                    )
                    .iter()
                    .any(|(name, _)| name == &old)),
                    "Java annotation references to {old} require annotation/type resolution"
                );
                ensure!(
                    !module
                        .sites
                        .iter()
                        .any(|site| site.name == old && site.bound && site.binding_id.is_none()),
                    "Ambiguous Java type/value shadowing for {old} in {other}"
                );
            }
            let references: Vec<_> = self
                .java_refs(other)?
                .into_iter()
                .filter(|reference| reference.key == key)
                .collect();
            if !references.is_empty() {
                module.name_conflict(new)?;
            }
            for reference in references {
                edits.entry(other.clone()).or_default().push(Edit {
                    range: reference.range,
                    text: new.into(),
                });
            }
            for import in imports(module) {
                if is_type
                    && (import.path == class_name
                        || import.path.starts_with(&format!("{class_name}.")))
                {
                    let offset = class_name.split('.').count() - 1;
                    edits.entry(other.clone()).or_default().push(Edit {
                        range: import.tokens[offset].1.clone(),
                        text: new.into(),
                    });
                } else if !is_type && import.static_ && import.path == format!("{class_name}.{old}")
                {
                    edits.entry(other.clone()).or_default().push(Edit {
                        range: import.tokens.last().unwrap().1.clone(),
                        text: new.into(),
                    });
                }
            }
            if is_type && other == path {
                for constructor in module
                    .structure
                    .nodes
                    .iter()
                    .filter(|node| node.parent_id == Some(key.1) && node.kind == "constructor")
                {
                    let node = module.declaration(constructor.id)?;
                    if let Some(name) = node.child_by_field_name("name") {
                        edits.entry(other.clone()).or_default().push(Edit {
                            range: name.byte_range(),
                            text: new.into(),
                        });
                    }
                }
            }
            if !is_type {
                for site in module
                    .sites
                    .iter()
                    .filter(|site| !site.declaration && site.name == old && site.receiver.is_some())
                {
                    ensure!(
                        self.java_site(other, site)?.is_some(),
                        "Unresolved Java receiver for {old} in {other}; instance dispatch cannot be guessed"
                    );
                }
            }
        }
        self.apply(edits)?;
        if is_type
            && public
            && rename_file
            && Path::new(path).file_stem().and_then(|stem| stem.to_str()) == Some(&old)
        {
            let destination = Path::new(path)
                .with_file_name(format!("{new}.java"))
                .to_string_lossy()
                .replace('\\', "/");
            ensure!(
                !self.root.join(&destination).try_exists()?,
                "Java type rename requires an available matching filename: {destination}"
            );
            let source = self.files.remove(path).unwrap().source;
            self.set_source(&destination, source)?;
        }
        Ok(())
    }

    fn java_insert_imports(
        &self,
        edits: &mut Edits,
        path: &str,
        requested: BTreeSet<String>,
    ) -> Result<()> {
        let module = &self.files[path];
        let current = imports(module);
        let offset = children(module.tree.root_node())
            .into_iter()
            .filter(|node| matches!(node.kind(), "package_declaration" | "import_declaration"))
            .map(|node| node.end_byte())
            .max()
            .unwrap_or(0);
        let mut additions = Vec::new();
        for import in requested {
            let static_ = import.starts_with("static ");
            let name = import.strip_prefix("static ").unwrap_or(&import);
            if current
                .iter()
                .any(|old| old.static_ == static_ && old.path == name && !old.wildcard)
            {
                continue;
            }
            let short = name.rsplit('.').next().unwrap();
            ensure!(
                !current.iter().any(|old| old.static_ == static_
                    && !old.wildcard
                    && old.path.rsplit('.').next() == Some(short)
                    && old.path != name),
                "Conflicting Java import for {short} in {path}"
            );
            additions.push(format!("import {import};\n"));
        }
        self.add_imports(edits, path, offset, additions);
        Ok(())
    }

    pub(super) fn java_move_file(&mut self, from: &str, to: &str) -> Result<()> {
        let stem = Path::new(to)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .context("Java destination requires a class filename")?;
        valid_name("java", stem)?;
        let public: Vec<_> = self.files[from]
            .structure
            .nodes
            .iter()
            .filter(|node| node.parent_id.is_none() && type_node(node) && modifier(node, "public"))
            .collect();
        ensure!(public.len() <= 1, "Java source has several public types");
        if let Some(public) = public.first()
            && public.name != stem
        {
            let name = public.name.clone();
            self.java_rename(from, &name, stem, false)?;
        }
        let old_package = package(&self.files[from]);
        let directory = Path::new(from).parent().unwrap_or(Path::new(""));
        let package_path = old_package.replace('.', "/");
        ensure!(
            package_path.is_empty() || directory.ends_with(Path::new(&package_path)),
            "Java package does not match its source directory"
        );
        let root = directory
            .ancestors()
            .nth(if package_path.is_empty() {
                0
            } else {
                old_package.split('.').count()
            })
            .context("Java package has no source root")?;
        let new_directory = Path::new(to).parent().unwrap_or(Path::new(""));
        let relative = new_directory
            .strip_prefix(root)
            .context("Java move must remain inside its source root")?;
        let new_package = relative
            .components()
            .map(|part| {
                part.as_os_str()
                    .to_str()
                    .context("Java package path must be UTF-8")
            })
            .collect::<Result<Vec<_>>>()?
            .join(".");
        for name in new_package.split('.').filter(|name| !name.is_empty()) {
            valid_name("java", name)?;
        }
        let moved: BTreeMap<_, _> = self.files[from]
            .structure
            .nodes
            .iter()
            .filter(|node| node.parent_id.is_none() && type_node(node))
            .map(|node| {
                (
                    (from.to_owned(), node.id),
                    (
                        self.java_fqn(&(from.to_owned(), node.id)),
                        if new_package.is_empty() {
                            node.name.clone()
                        } else {
                            format!("{new_package}.{}", node.name)
                        },
                        modifier(node, "public"),
                    ),
                )
            })
            .collect();
        if old_package != new_package {
            let mut edits = Edits::new();
            let mut needed = BTreeMap::<String, BTreeSet<String>>::new();
            for (path, module) in &self.files {
                for import in imports(module) {
                    for (old, new, _) in moved.values() {
                        if import.path == *old || import.path.starts_with(&format!("{old}.")) {
                            let count = old.split('.').count();
                            let range = import.tokens[0].1.start..import.tokens[count - 1].1.end;
                            edits.entry(path.clone()).or_default().push(Edit {
                                range,
                                text: new.clone(),
                            });
                        }
                    }
                }
                for reference in self.java_refs(path)? {
                    if let Some((old, new, public)) = moved.get(&reference.key) {
                        if reference.declaration {
                            continue;
                        }
                        ensure!(
                            path == from || *public || package(module) == new_package,
                            "Package-private Java types cannot move away from existing callers"
                        );
                        if let Some(range) = reference.qualified_type {
                            edits.entry(path.clone()).or_default().push(Edit {
                                range,
                                text: new.clone(),
                            });
                        } else if path != from
                            && package(module) != new_package
                            && !imports(module)
                                .iter()
                                .any(|import| !import.static_ && import.path == *old)
                        {
                            ensure!(
                                !new_package.is_empty(),
                                "Java named packages cannot import a moved default-package type"
                            );
                            needed.entry(path.clone()).or_default().insert(new.clone());
                        }
                    } else if path == from
                        && type_node(&self.files[&reference.key.0].structure.nodes[reference.key.1])
                        && reference.qualified_type.is_none()
                        && package(&self.files[&reference.key.0]) == old_package
                    {
                        let node = &self.files[&reference.key.0].structure.nodes[reference.key.1];
                        ensure!(
                            modifier(node, "public"),
                            "Java move would lose access to package-private type {}",
                            node.name
                        );
                        needed
                            .entry(path.clone())
                            .or_default()
                            .insert(self.java_fqn(&reference.key));
                    }
                }
            }
            for (path, requested) in needed {
                self.java_insert_imports(&mut edits, &path, requested)?;
            }
            if let Some(node) = children(self.files[from].tree.root_node())
                .into_iter()
                .find(|node| node.kind() == "package_declaration")
            {
                edits.entry(from.into()).or_default().push(Edit {
                    range: node.byte_range(),
                    text: if new_package.is_empty() {
                        String::new()
                    } else {
                        format!("package {new_package};")
                    },
                });
            } else if !new_package.is_empty() {
                edits.entry(from.into()).or_default().push(Edit {
                    range: 0..0,
                    text: format!("package {new_package};\n"),
                });
            }
            self.apply(edits)?;
        }
        let source = self.files.remove(from).unwrap().source;
        self.set_source(to, source)
    }

    pub(super) fn java_move_symbol(
        &mut self,
        from: &str,
        selected: &str,
        to: &str,
        destination: &str,
    ) -> Result<()> {
        let symbol = self.files[from].symbol(selected)?;
        ensure!(
            symbol.kind == "method" && modifier(symbol, "static"),
            "Move Java static methods with file::Class.method; move types using file paths"
        );
        ensure!(
            self.files.contains_key(to),
            "Moving a Java method requires an existing destination class file"
        );
        let key = (from.to_owned(), symbol.id);
        let class = (
            from.to_owned(),
            symbol.parent_id.context("Java method has no class")?,
        );
        let scope = destination
            .rsplit_once('.')
            .map(|(scope, _)| scope)
            .unwrap_or_else(|| {
                Path::new(to)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap()
            });
        let target = self.files[to].symbol(scope)?;
        ensure!(
            type_node(target) && target.parent_id.is_none() && target.kind != "interface",
            "Destination must be a top-level Java class"
        );
        ensure!(
            package(&self.files[from]) == package(&self.files[to]) || modifier(target, "public"),
            "Destination Java class is not accessible from the source package"
        );
        ensure!(
            !self.files[to]
                .structure
                .nodes
                .iter()
                .any(|node| node.parent_id == Some(target.id) && node.name == symbol.name),
            "Destination Java class already declares {}",
            symbol.name
        );
        let target_key = (to.to_owned(), target.id);
        let target_fqn = self.java_fqn(&target_key);
        let declaration = self.files[from].declaration(symbol.id)?;
        ensure!(
            !walk(declaration)
                .iter()
                .any(|node| matches!(node.kind(), "annotation" | "marker_annotation")),
            "Moving annotated Java methods requires annotation dependency resolution"
        );
        let range = self.files[from].declaration_range(symbol.id)?;
        ensure!(
            !walk(declaration)
                .iter()
                .any(|node| matches!(node.kind(), "this" | "super")),
            "Moving Java this/super-dependent code is not supported"
        );
        let mut body_edits = Vec::new();
        let mut edits = Edits::new();
        let mut requested = BTreeMap::<String, BTreeSet<String>>::new();
        for (path, module) in &self.files {
            for reference in self.java_refs(path)? {
                let inside = path == from
                    && range.start <= reference.range.start
                    && reference.range.end <= range.end;
                if reference.key == key && !reference.declaration {
                    if inside {
                        if let Some(receiver) = reference.receiver {
                            body_edits.push(Edit {
                                range: receiver.start - range.start..receiver.end - range.start,
                                text: target_fqn.clone(),
                            });
                        }
                    } else {
                        ensure!(
                            !package(&self.files[to]).is_empty() || package(module).is_empty(),
                            "Java named packages cannot reference a default-package destination class"
                        );
                        if let Some(receiver) = reference.receiver {
                            edits.entry(path.clone()).or_default().push(Edit {
                                range: receiver,
                                text: target_fqn.clone(),
                            });
                        } else {
                            edits.entry(path.clone()).or_default().push(Edit {
                                range: reference.range,
                                text: format!("{target_fqn}.{}", symbol.name),
                            });
                        }
                    }
                } else if inside && !reference.declaration {
                    let dependency = &self.files[&reference.key.0].structure.nodes[reference.key.1];
                    if reference.key.0 == from
                        && range.start <= dependency.start_byte
                        && dependency.end_byte <= range.end
                    {
                        continue;
                    }
                    if dependency.parent_id == Some(class.1) && reference.key.0 == from {
                        ensure!(
                            modifier(dependency, "static")
                                && !modifier(dependency, "private")
                                && (package(&self.files[from]) == package(&self.files[to])
                                    || modifier(dependency, "public")),
                            "Java move would capture inaccessible member {}",
                            dependency.name
                        );
                        ensure!(
                            package(&self.files[from]) == package(&self.files[to])
                                || modifier(&self.files[from].structure.nodes[class.1], "public"),
                            "Java move would lose access to the dependency's declaring class"
                        );
                        if reference.receiver.is_none() {
                            body_edits.push(Edit {
                                range: reference.range.start - range.start
                                    ..reference.range.end - range.start,
                                text: format!("{}.{}", self.java_fqn(&class), dependency.name),
                            });
                        }
                    } else if type_node(dependency)
                        && dependency.parent_id.is_none()
                        && reference.qualified_type.is_none()
                        && reference.key != target_key
                    {
                        requested
                            .entry(to.into())
                            .or_default()
                            .insert(self.java_fqn(&reference.key));
                    }
                }
            }
            for import in imports(module).into_iter().filter(|import| {
                import.static_
                    && !import.wildcard
                    && import.path == format!("{}.{}", self.java_fqn(&class), symbol.name)
            }) {
                let end = import.tokens.len() - 1;
                edits.entry(path.clone()).or_default().push(Edit {
                    range: import.tokens[0].1.start..import.tokens[end - 1].1.end,
                    text: target_fqn.clone(),
                });
            }
        }
        let source = &self.files[from];
        let source_imports = imports(source);
        for site in source.sites.iter().filter(|site| {
            !site.declaration && range.start <= site.range.start && site.range.end <= range.end
        }) {
            if let Some(id) = site.binding_id
                && source.structure.nodes[id].kind == "import"
            {
                let import = source_imports
                    .iter()
                    .find(|import| {
                        import.path.rsplit('.').next()
                            == Some(site.receiver.as_deref().unwrap_or(&site.name))
                    })
                    .context("Cannot transfer Java dependency import")?;
                if !import.static_ && import.path == target_fqn {
                    continue;
                }
                requested.entry(to.into()).or_default().insert(format!(
                    "{}{}",
                    if import.static_ { "static " } else { "" },
                    import.path
                ));
            } else if !site.bound && self.java_site(from, site)?.is_none() {
                let root = site.receiver.as_deref().unwrap_or(&site.name);
                ensure!(
                    java_lang(root)
                        || source_imports.iter().any(|import| !import.wildcard
                            && import.path.rsplit('.').next() == Some(root)),
                    "Unresolved/inherited Java dependency {root} prevents moving this method"
                );
                ensure!(
                    !self.files[to]
                        .sites
                        .iter()
                        .any(|site| site.declaration && site.name == root),
                    "Destination Java declaration would capture {root}"
                );
                if java_lang(root) {
                    ensure!(
                        !imports(&self.files[to]).iter().any(|import| !import.static_
                            && !import.wildcard
                            && import.path.rsplit('.').next() == Some(root)
                            && import.path != format!("java.lang.{root}")),
                        "Destination Java import would capture java.lang.{root}"
                    );
                }
                for import in source_imports.iter().filter(|import| {
                    !import.wildcard && import.path.rsplit('.').next() == Some(root)
                }) {
                    requested.entry(to.into()).or_default().insert(format!(
                        "{}{}",
                        if import.static_ { "static " } else { "" },
                        import.path
                    ));
                }
            }
        }
        if !modifier(symbol, "public") {
            let modifiers = children(declaration)
                .into_iter()
                .find(|node| node.kind() == "modifiers");
            if let Some(modifiers) = modifiers {
                let mut cursor = modifiers.walk();
                if let Some(access) = modifiers
                    .children(&mut cursor)
                    .find(|node| matches!(node.kind(), "private" | "protected"))
                {
                    body_edits.push(Edit {
                        range: access.start_byte() - range.start..access.end_byte() - range.start,
                        text: "public".into(),
                    });
                } else {
                    body_edits.push(Edit {
                        range: modifiers.start_byte() - range.start
                            ..modifiers.start_byte() - range.start,
                        text: "public ".into(),
                    });
                }
            } else {
                body_edits.push(Edit {
                    range: declaration.start_byte() - range.start
                        ..declaration.start_byte() - range.start,
                    text: "public ".into(),
                });
            }
        }
        let moved = edited(&source.source[range.clone()], body_edits)?;
        edits.entry(from.into()).or_default().push(Edit {
            range,
            text: String::new(),
        });
        let target_body = self.files[to]
            .declaration(target.id)?
            .child_by_field_name("body")
            .context("Destination Java class has no body")?;
        let offset = target_body.end_byte() - 1;
        edits.entry(to.into()).or_default().push(Edit {
            range: offset..offset,
            text: format!("\n{moved}\n"),
        });
        for (path, requested) in requested {
            for import in &requested {
                let qualified = import.strip_prefix("static ").unwrap_or(import);
                let name = qualified.rsplit('.').next().unwrap();
                ensure!(
                    !self.files[&path]
                        .sites
                        .iter()
                        .any(|site| site.declaration && site.name == name),
                    "Destination Java declaration would capture imported dependency {name}"
                );
            }
            self.java_insert_imports(&mut edits, &path, requested)?;
        }
        self.apply(edits)
    }
}

fn java_lang(name: &str) -> bool {
    matches!(
        name,
        "String"
            | "Object"
            | "Class"
            | "System"
            | "Math"
            | "StrictMath"
            | "Integer"
            | "Long"
            | "Double"
            | "Float"
            | "Boolean"
            | "Byte"
            | "Short"
            | "Character"
            | "Number"
            | "Exception"
            | "RuntimeException"
            | "IllegalArgumentException"
            | "IllegalStateException"
            | "Throwable"
            | "Error"
            | "Thread"
            | "StringBuilder"
            | "StringBuffer"
            | "Void"
            | "Comparable"
            | "Iterable"
            | "Enum"
            | "Record"
            | "Override"
            | "Deprecated"
            | "SuppressWarnings"
    )
}
