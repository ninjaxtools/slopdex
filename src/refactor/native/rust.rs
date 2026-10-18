use super::*;
use anyhow::Context as _;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Address {
    root: String,
    parts: Vec<String>,
}

#[derive(Clone)]
struct Context {
    root: String,
    module: Vec<String>,
}

struct Layout {
    contexts: BTreeMap<String, Context>,
    names: BTreeMap<String, String>,
    registrations: BTreeMap<String, Vec<Registration>>,
}

type Registration = (String, Range<usize>, Range<usize>);

#[derive(Clone)]
struct Item {
    path: Vec<String>,
    alias: Option<String>,
    token: Range<usize>,
    wildcard: bool,
}
#[derive(Clone)]
pub(super) struct Use {
    id: usize,
    range: Range<usize>,
    scope: Range<usize>,
    module_scope: bool,
    prefix: String,
    items: Vec<Item>,
}
#[derive(Clone)]
struct Reference {
    key: Key,
    range: Range<usize>,
    path: Option<Range<usize>>,
    address: Address,
    follows_name: bool,
}

fn expression_macro(name: &str) -> bool {
    matches!(
        name,
        "assert"
            | "assert_eq"
            | "assert_ne"
            | "debug_assert"
            | "debug_assert_eq"
            | "debug_assert_ne"
            | "format"
            | "format_args"
            | "print"
            | "println"
            | "eprint"
            | "eprintln"
            | "panic"
            | "write"
            | "writeln"
            | "dbg"
            | "vec"
            | "bail"
            | "ensure"
            | "anyhow"
    )
}

fn macro_parts<'a>(node: Node<'a>) -> Option<(Node<'a>, Node<'a>)> {
    Some((
        node.child_by_field_name("macro")
            .or_else(|| node.named_child(0))?,
        children(node)
            .into_iter()
            .find(|node| node.kind() == "token_tree")?,
    ))
}

/// Standard expression macros can be parsed without executing/expanding them.
/// Replacements have identical byte lengths, so indexed identities and edits
/// remain anchored to original source bytes. Other macros stay opaque.
pub(super) fn editable_tree(path: &str, source: &str, original: &Tree) -> Result<(Tree, String)> {
    let mut bytes = source.as_bytes().to_vec();
    for node in walk(original.root_node())
        .into_iter()
        .filter(|node| node.kind() == "macro_invocation")
    {
        let Some((callee, args)) = macro_parts(node) else {
            continue;
        };
        let name = text(source, callee).rsplit("::").next().unwrap();
        if !expression_macro(name) {
            continue;
        }
        let mut cursor = node.walk();
        let bang = node
            .children(&mut cursor)
            .find(|node| node.kind() == "!")
            .context("Cannot read Rust macro delimiter")?;
        bytes[bang.start_byte()] = b' ';
        if name == "vec" {
            bytes[callee.byte_range()].fill(b' ');
        } else {
            bytes[args.start_byte()] = b'(';
            bytes[args.end_byte() - 1] = b')';
        }
    }
    let lexical = String::from_utf8(bytes)?;
    // Unsupported argument syntax is rejected, never parsed with recovery.
    let parsed = tree(path, &lexical)?;
    ensure!(
        !walk(parsed.root_node())
            .iter()
            .any(|node| node.kind() == "use_declaration"
                && walk(original.root_node())
                    .iter()
                    .any(|original| original.kind() == "macro_invocation"
                        && original.start_byte() <= node.start_byte()
                        && node.end_byte() <= original.end_byte())),
        "Rust expression macros with block-local imports require compiler name resolution"
    );
    Ok((parsed, lexical))
}

fn segments(node: Node<'_>, source: &str) -> Vec<(String, Range<usize>)> {
    walk(node)
        .into_iter()
        .filter(|node| {
            matches!(
                node.kind(),
                "identifier" | "type_identifier" | "crate" | "self" | "super"
            ) && node.named_child_count() == 0
                && !std::iter::successors(node.parent(), |node| node.parent())
                    .take_while(|parent| parent.start_byte() >= node.start_byte())
                    .any(|parent| matches!(parent.kind(), "type_arguments" | "type_parameters"))
        })
        .map(|node| (text(source, node).into(), node.byte_range()))
        .collect()
}

fn path_parts(node: Node<'_>, source: &str) -> Vec<String> {
    // Type arguments are not path components.
    walk(node)
        .into_iter()
        .filter(|part| {
            matches!(
                part.kind(),
                "identifier" | "type_identifier" | "crate" | "self" | "super"
            ) && part.named_child_count() == 0
                && !std::iter::successors(part.parent(), |parent| parent.parent())
                    .take_while(|parent| parent.id() != node.id())
                    .any(|parent| matches!(parent.kind(), "type_arguments" | "type_parameters"))
        })
        .map(|part| text(source, part).into())
        .collect()
}

fn use_items(
    node: Node<'_>,
    source: &str,
    prefix: &[String],
    result: &mut Vec<Item>,
) -> Result<()> {
    let join = |parts: Vec<String>| {
        let mut value = prefix.to_vec();
        if parts.first().is_some_and(|part| part == "self") && !prefix.is_empty() {
            value.extend(parts.into_iter().skip(1));
        } else {
            value.extend(parts);
        }
        value
    };
    match node.kind() {
        "scoped_use_list" => {
            let path = node
                .child_by_field_name("path")
                .map(|node| path_parts(node, source))
                .unwrap_or_default();
            let prefix = join(path);
            for child in children(
                node.child_by_field_name("list")
                    .context("Missing Rust use list")?,
            ) {
                use_items(child, source, &prefix, result)?;
            }
        }
        "use_list" => {
            for child in children(node) {
                use_items(child, source, prefix, result)?;
            }
        }
        "use_as_clause" => {
            let path = node
                .child_by_field_name("path")
                .context("Missing Rust use path")?;
            let token = segments(path, source)
                .last()
                .context("Missing Rust imported name")?
                .1
                .clone();
            result.push(Item {
                path: join(path_parts(path, source)),
                alias: node
                    .child_by_field_name("alias")
                    .map(|node| text(source, node).into()),
                token,
                wildcard: false,
            });
        }
        "use_wildcard" => result.push(Item {
            path: join(path_parts(node, source)),
            alias: None,
            token: node.byte_range(),
            wildcard: true,
        }),
        "identifier" | "scoped_identifier" | "self" | "super" | "crate" => {
            let token = segments(node, source)
                .last()
                .context("Missing Rust imported name")?
                .1
                .clone();
            result.push(Item {
                path: join(path_parts(node, source)),
                alias: None,
                token,
                wildcard: false,
            });
        }
        _ => bail!("Unsupported Rust use syntax: {}", node.kind()),
    }
    Ok(())
}

fn uses(module: &Module) -> Result<Vec<Use>> {
    if let Some(imports) = module.rust_uses.get() {
        return Ok(imports.clone());
    }
    let mut result = Vec::new();
    for node in walk(module.tree.root_node())
        .into_iter()
        .filter(|node| node.kind() == "use_declaration")
    {
        let argument = node
            .child_by_field_name("argument")
            .context("Missing Rust use argument")?;
        let mut items = Vec::new();
        use_items(argument, &module.source, &[], &mut items)?;
        let id = module
            .structure
            .nodes
            .iter()
            .find(|symbol| symbol.kind == "import" && symbol.end_byte == node.end_byte())
            .context("Missing indexed Rust use declaration")?
            .id;
        let scope = std::iter::successors(node.parent(), |node| node.parent())
            .find(|node| matches!(node.kind(), "block" | "declaration_list" | "source_file"))
            .context("Missing Rust use scope")?;
        let module_scope = scope.kind() != "block";
        ensure!(
            module_scope || !items.iter().any(|item| item.wildcard),
            "Block-local Rust glob imports require compiler name resolution"
        );
        result.push(Use {
            id,
            range: node.byte_range(),
            scope: scope.byte_range(),
            module_scope,
            prefix: module.source[node.start_byte()..argument.start_byte()].into(),
            items,
        });
    }
    let _ = module.rust_uses.set(result.clone());
    Ok(result)
}

fn context(layout: &Layout, path: &str, module: &Module, offset: usize) -> Context {
    let mut result = layout.contexts[path].clone();
    let nested = module
        .structure
        .nodes
        .iter()
        .filter(|node| {
            node.kind == "module"
                && node.start_byte <= offset
                && offset < node.end_byte
                && module
                    .declaration(node.id)
                    .is_ok_and(|node| node.child_by_field_name("body").is_some())
        })
        .min_by_key(|node| node.end_byte - node.start_byte);
    if let Some(node) = nested {
        result
            .module
            .extend(node.qualified_name.split('.').map(str::to_owned));
    }
    result
}

fn inferred(path: &str) -> Context {
    let (root, relative) = if let Some(offset) = path.rfind("/src/") {
        (path[..offset + 4].to_owned(), &path[offset + 5..])
    } else if let Some(relative) = path.strip_prefix("src/") {
        ("src".into(), relative)
    } else {
        (String::new(), path)
    };
    let mut module: Vec<_> = relative
        .trim_end_matches(".rs")
        .split('/')
        .map(str::to_owned)
        .collect();
    if module
        .last()
        .is_some_and(|name| matches!(name.as_str(), "mod" | "lib" | "main"))
    {
        module.pop();
    }
    Context { root, module }
}

impl Workspace {
    fn rust_attributes(&self, symbol: &StructureNode, moving: bool) -> Result<()> {
        for attribute in &symbol.attributes {
            let value = attribute
                .trim_start_matches('#')
                .trim_start_matches('!')
                .trim_start_matches('[')
                .trim();
            let name = value
                .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
                .next()
                .unwrap_or("");
            ensure!(
                matches!(
                    name,
                    "derive"
                        | "cfg"
                        | "doc"
                        | "inline"
                        | "test"
                        | "cold"
                        | "allow"
                        | "warn"
                        | "deny"
                        | "forbid"
                        | "deprecated"
                        | "must_use"
                        | "track_caller"
                        | "repr"
                        | "no_mangle"
                        | "export_name"
                        | "link_name"
                        | "link_section"
                        | "unsafe"
                        | "non_exhaustive"
                ),
                "Procedural Rust attribute {name} requires macro expansion before refactoring"
            );
            if name == "derive" {
                let derives = value
                    .split_once('(')
                    .and_then(|(_, rest)| rest.rsplit_once(')').map(|(derives, _)| derives))
                    .context("Cannot read Rust derive arguments")?;
                ensure!(
                    derives
                        .split(',')
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .all(|name| matches!(
                            name,
                            "Debug"
                                | "Clone"
                                | "Copy"
                                | "PartialEq"
                                | "Eq"
                                | "PartialOrd"
                                | "Ord"
                                | "Hash"
                                | "Default"
                        )),
                    "Custom Rust derives can generate additional symbols and require macro expansion"
                );
            }
            ensure!(
                !moving || !attribute.contains('!'),
                "Moving Rust attributes with nested macros requires expansion/location analysis"
            );
        }
        Ok(())
    }

    fn rust_macro_known(&self, module: &Module, callee: Node<'_>) -> bool {
        let parts = path_parts(callee, &module.source);
        let Some(name) = parts.last() else {
            return false;
        };
        if !expression_macro(name) {
            return false;
        }
        let standard = |name: &str| matches!(name, "std" | "core" | "alloc" | "anyhow");
        if parts.len() > 1 {
            return standard(&parts[0])
                && !module
                    .structure
                    .nodes
                    .iter()
                    .any(|node| node.kind == "module" && node.name == parts[0]);
        }
        !self.files.values().any(|module| {
            module
                .structure
                .nodes
                .iter()
                .any(|node| node.kind == "macro" && node.name == *name)
        }) && !module
            .structure
            .nodes
            .iter()
            .flat_map(|node| &node.imports)
            .any(|binding| {
                binding.alias.as_deref().or(binding.name.as_deref()) == Some(name)
                    && !binding.path.split("::").next().is_some_and(standard)
            })
    }

    fn rust_safe_range(&self, path: &str, range: &Range<usize>) -> Result<()> {
        let module = &self.files[path];
        for node in walk(module.tree.root_node()).into_iter().filter(|node| {
            node.kind() == "macro_invocation"
                && node.start_byte() <= range.start
                && range.end <= node.end_byte()
        }) {
            ensure!(
                macro_parts(node).is_some_and(|(callee, _)| self.rust_macro_known(module, callee)),
                "Cannot edit an opaque/shadowed Rust macro invocation in {path}"
            );
        }
        Ok(())
    }

    fn rust_layout(&mut self) -> Result<Layout> {
        ensure!(
            !self
                .files
                .values()
                .any(|module| walk(module.tree.root_node())
                    .into_iter()
                    .filter(|node| node.kind() == "macro_invocation")
                    .filter_map(macro_parts)
                    .any(
                        |(callee, _)| text(&module.source, callee).rsplit("::").next()
                            == Some("include")
                    )),
            "Rust include! source expansion requires compiler module resolution"
        );
        let mut layout = Layout {
            contexts: self
                .files
                .keys()
                .map(|path| (path.clone(), inferred(path)))
                .collect(),
            names: BTreeMap::new(),
            registrations: BTreeMap::new(),
        };
        if self.files.len() == 1
            && let Some((path, value)) = layout.contexts.iter_mut().next()
            && !path.starts_with("src/")
        {
            value.module.clear();
        }
        for (path, module) in &self.files {
            for node in walk(module.tree.root_node()).into_iter().filter(|node| {
                node.kind() == "mod_item" && node.child_by_field_name("body").is_none()
            }) {
                let name = node
                    .child_by_field_name("name")
                    .context("Missing Rust module name")?;
                let symbol = module
                    .structure
                    .nodes
                    .iter()
                    .find(|symbol| symbol.kind == "module" && symbol.end_byte == node.end_byte())
                    .context("Missing indexed Rust module")?;
                ensure!(
                    !symbol
                        .attributes
                        .iter()
                        .any(|attribute| attribute.contains("path")),
                    "#[path] Rust module registration cannot be inferred safely: {path}"
                );
                let scope = context(&layout, path, module, node.start_byte());
                let mut parts = scope.module.clone();
                parts.push(text(&module.source, name).into());
                let base = [scope.root.as_str(), &parts.join("/")]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join("/");
                let found: Vec<_> = [format!("{base}.rs"), format!("{base}/mod.rs")]
                    .into_iter()
                    .filter(|target| self.files.contains_key(target))
                    .collect();
                ensure!(found.len() <= 1, "Ambiguous Rust module file: {base}");
                if let Some(target) = found.first() {
                    layout
                        .registrations
                        .entry(target.clone())
                        .or_default()
                        .push((path.clone(), node.byte_range(), name.byte_range()));
                }
            }
        }
        let roots: BTreeSet<_> = layout
            .contexts
            .values()
            .map(|context| context.root.clone())
            .collect();
        for root in roots {
            let manifest = Path::new(&root)
                .parent()
                .unwrap_or(Path::new(""))
                .join("Cargo.toml")
                .to_string_lossy()
                .replace('\\', "/");
            if let Some(source) = self.policy(&manifest)? {
                let mut package = false;
                for line in source.lines() {
                    let line = line.trim();
                    if line.starts_with('[') {
                        package = line == "[package]";
                    }
                    if package
                        && let Some((name, value)) = line.split_once('=')
                        && name.trim() == "name"
                    {
                        let name = value.trim().trim_matches('"').replace('-', "_");
                        layout.names.insert(name, root.clone());
                    }
                }
            }
        }
        Ok(layout)
    }

    fn rust_conditional(&self, layout: &Layout, path: &str) -> bool {
        let mut pending = vec![path.to_owned()];
        let mut seen = BTreeSet::new();
        while let Some(path) = pending.pop() {
            if !seen.insert(path.clone()) {
                continue;
            }
            if self.files.get(&path).is_some_and(|module| {
                children(module.tree.root_node()).iter().any(|node| {
                    node.kind() == "inner_attribute_item"
                        && text(&module.source, *node).contains("cfg")
                })
            }) {
                return true;
            }
            if let Some(parents) = layout.registrations.get(&path) {
                for (parent, range, _) in parents {
                    if self.files[parent].structure.nodes.iter().any(|node| {
                        node.kind == "module"
                            && node.end_byte == range.end
                            && node
                                .attributes
                                .iter()
                                .any(|attribute| attribute.contains("cfg"))
                    }) {
                        return true;
                    }
                    pending.push(parent.clone());
                }
            } else if let Some(scope) = layout.contexts.get(&path)
                && !scope.module.is_empty()
            {
                let parent = &scope.module[..scope.module.len() - 1];
                pending.extend(
                    layout
                        .contexts
                        .iter()
                        .filter(|(_, other)| other.root == scope.root && other.module == parent)
                        .map(|(path, _)| path.clone()),
                );
            }
        }
        false
    }

    fn rust_address(
        &self,
        layout: &Layout,
        path: &str,
        offset: usize,
        parts: &[String],
        imports: bool,
    ) -> Result<Option<Address>> {
        if parts.is_empty() {
            return Ok(None);
        }
        let module = &self.files[path];
        let scope = context(layout, path, module, offset);
        let mut root = scope.root.clone();
        let mut base = Vec::new();
        let mut rest = parts;
        match parts[0].as_str() {
            "crate" => rest = &parts[1..],
            "self" => {
                base = scope.module;
                rest = &parts[1..];
            }
            "super" => {
                base = scope.module;
                while rest.first().is_some_and(|part| part == "super") {
                    ensure!(base.pop().is_some(), "Rust super path escapes its crate");
                    rest = &rest[1..];
                }
            }
            first if layout.names.contains_key(first) => {
                root = layout.names[first].clone();
                rest = &parts[1..];
            }
            first => {
                if !imports {
                    let lexical = module.sites.iter().find(|site| {
                        site.range.start == offset && site.name == first && site.receiver.is_none()
                    });
                    let mut visible = uses(module)?;
                    visible.retain(|import| {
                        import.scope.start <= offset
                            && offset <= import.scope.end
                            && lexical.and_then(|site| site.binding_id).is_none_or(|id| {
                                module.structure.nodes[id].kind != "import" || import.id == id
                            })
                    });
                    visible.sort_by_key(|import| import.scope.end - import.scope.start);
                    for import in visible {
                        let import_scope = context(layout, path, module, import.range.start);
                        if import_scope.root != scope.root || import_scope.module != scope.module {
                            continue;
                        }
                        for item in import.items.iter().filter(|item| {
                            !item.wildcard
                                && item
                                    .alias
                                    .as_deref()
                                    .or_else(|| item.path.last().map(String::as_str))
                                    == Some(first)
                        }) {
                            if let Some(mut address) = self.rust_address(
                                layout,
                                path,
                                import.range.start,
                                &item.path,
                                true,
                            )? {
                                address.parts.extend_from_slice(&parts[1..]);
                                return Ok(Some(address));
                            }
                        }
                    }
                    let local = lexical
                        .and_then(|site| site.binding_id)
                        .map(|id| &module.structure.nodes[id])
                        .filter(|node| node.kind != "import")
                        .or_else(|| {
                            module.structure.nodes.iter().find(|node| {
                                node.name == first
                                    && node.parent_id.is_none()
                                    && matches!(
                                        node.kind.as_str(),
                                        "module" | "struct" | "enum" | "type" | "trait"
                                    )
                            })
                        });
                    if let Some(local) = local {
                        let mut module_parts = layout.contexts[path].module.clone();
                        module_parts.extend(local.qualified_name.split('.').map(str::to_owned));
                        module_parts.extend_from_slice(&parts[1..]);
                        return Ok(Some(Address {
                            root,
                            parts: module_parts,
                        }));
                    }
                }
                let prefix = vec![first.to_owned()];
                let known = layout
                    .contexts
                    .values()
                    .any(|context| context.root == root && context.module.starts_with(&prefix))
                    || self.files.iter().any(|(path, module)| {
                        layout.contexts[path].root == root
                            && layout.contexts[path].module.is_empty()
                            && module
                                .structure
                                .nodes
                                .iter()
                                .any(|node| node.parent_id.is_none() && node.name == first)
                    });
                if !known {
                    return Ok(None);
                }
            }
        }
        base.extend_from_slice(rest);
        Ok(Some(Address { root, parts: base }))
    }

    fn rust_resolve(
        &self,
        layout: &Layout,
        address: &Address,
        visiting: &mut BTreeSet<Address>,
    ) -> Result<Option<(Key, bool)>> {
        ensure!(
            visiting.len() < 128,
            "Rust re-export graph is too deep to resolve safely"
        );
        if !visiting.insert(address.clone()) {
            return Ok(None);
        }
        let mut direct = BTreeSet::new();
        for (path, module) in &self.files {
            let scope = &layout.contexts[path];
            if scope.root != address.root {
                continue;
            }
            for node in module.structure.nodes.iter().filter(|node| {
                !matches!(
                    node.kind.as_str(),
                    "import" | "impl" | "field" | "method" | "variant"
                )
            }) {
                let mut parts = scope.module.clone();
                parts.extend(node.qualified_name.split('.').map(str::to_owned));
                if parts == address.parts {
                    direct.insert((path.clone(), node.id));
                }
            }
        }
        ensure!(
            direct.len() <= 1,
            "Ambiguous Rust declaration path {}",
            address.parts.join("::")
        );
        if let Some(key) = direct.into_iter().next() {
            visiting.remove(address);
            return Ok(Some((key, true)));
        }
        let Some((name, parent)) = address.parts.split_last() else {
            visiting.remove(address);
            return Ok(None);
        };
        let mut found = BTreeMap::new();
        for (path, module) in &self.files {
            for import in uses(module)?
                .into_iter()
                .filter(|import| import.module_scope)
            {
                let scope = context(layout, path, module, import.range.start);
                if scope.root != address.root || scope.module != parent {
                    continue;
                }
                for item in &import.items {
                    if !item.wildcard
                        && item.alias.as_ref().or_else(|| item.path.last()) != Some(name)
                    {
                        continue;
                    }
                    if let Some(mut target) =
                        self.rust_address(layout, path, import.range.start, &item.path, true)?
                    {
                        if item.wildcard {
                            target.parts.push(name.clone());
                        }
                        if let Some((key, follows)) =
                            self.rust_resolve(layout, &target, visiting)?
                        {
                            found.entry(key).or_insert(follows && item.alias.is_none());
                        }
                    }
                }
            }
        }
        visiting.remove(address);
        ensure!(
            found.len() <= 1,
            "Ambiguous Rust re-export {}",
            address.parts.join("::")
        );
        Ok(found.into_iter().next())
    }

    fn rust_refs(&self, layout: &Layout, path: &str) -> Result<Vec<Reference>> {
        let module = &self.files[path];
        let mut result = Vec::new();
        let macro_ranges: Vec<_> = walk(module.tree.root_node())
            .into_iter()
            .filter(|node| node.kind() == "macro_invocation")
            .filter_map(macro_parts)
            .map(|(callee, _)| callee.byte_range())
            .collect();
        for site in &module.sites {
            if site.receiver.is_some()
                || macro_ranges
                    .iter()
                    .any(|range| range.start <= site.range.start && site.range.end <= range.end)
            {
                continue;
            }
            // Formatting labels (`format!("{name}", name = value)`) are not
            // references to a same-spelled local declaration.
            let node = module
                .edit_tree
                .root_node()
                .descendant_for_byte_range(site.range.start, site.range.end);
            if node.is_some_and(|node| {
                node.parent().is_some_and(|parent| {
                    parent.kind() == "assignment_expression"
                        && parent.child_by_field_name("left") == Some(node)
                }) && std::iter::successors(node.parent(), |node| node.parent()).any(|parent| {
                    parent.kind() == "arguments"
                        && macro_ranges.iter().any(|range| {
                            range.end <= parent.start_byte() && parent.start_byte() - range.end <= 4
                        })
                })
            }) {
                continue;
            }
            if let Some(id) = site.binding_id
                && module.structure.nodes[id].kind != "import"
            {
                let symbol = &module.structure.nodes[id];
                let mut parts = layout.contexts[path].module.clone();
                parts.extend(symbol.qualified_name.split('.').map(str::to_owned));
                result.push(Reference {
                    key: (path.into(), id),
                    range: site.range.clone(),
                    path: None,
                    address: Address {
                        root: layout.contexts[path].root.clone(),
                        parts,
                    },
                    follows_name: true,
                });
                continue;
            }
            if site.bound && site.binding_id.is_none() {
                continue;
            }
            if let Some(id) = site.binding_id
                && let Some(import) = uses(module)?.into_iter().find(|import| import.id == id)
            {
                for item in import.items.iter().filter(|item| {
                    !item.wildcard
                        && item
                            .alias
                            .as_deref()
                            .or_else(|| item.path.last().map(String::as_str))
                            == Some(&site.name)
                }) {
                    if let Some(address) =
                        self.rust_address(layout, path, import.range.start, &item.path, true)?
                        && let Some((key, follows)) =
                            self.rust_resolve(layout, &address, &mut BTreeSet::new())?
                    {
                        result.push(Reference {
                            key,
                            range: site.range.clone(),
                            path: None,
                            address,
                            follows_name: follows && item.alias.is_none(),
                        });
                    }
                }
                continue;
            }
            let scope = context(layout, path, module, site.range.start);
            let mut parts = scope.module;
            parts.push(site.name.clone());
            let address = Address {
                root: scope.root,
                parts,
            };
            if let Some((key, follows)) =
                self.rust_resolve(layout, &address, &mut BTreeSet::new())?
            {
                result.push(Reference {
                    key,
                    range: site.range.clone(),
                    path: None,
                    address,
                    follows_name: follows,
                });
            }
        }
        for node in walk(module.edit_tree.root_node())
            .into_iter()
            .filter(|node| matches!(node.kind(), "scoped_identifier" | "scoped_type_identifier"))
        {
            if std::iter::successors(node.parent(), |node| node.parent()).any(|parent| {
                matches!(
                    parent.kind(),
                    "use_declaration" | "attribute_item" | "macro_definition" | "token_tree"
                )
            }) || macro_ranges
                .iter()
                .any(|range| range.start <= node.start_byte() && node.end_byte() <= range.end)
            {
                continue;
            }
            let parts = path_parts(node, &module.source);
            let tokens = segments(node, &module.source);
            let Some((_, token)) = tokens.last() else {
                continue;
            };
            if let Some((_, first)) = tokens.first()
                && module
                    .sites
                    .iter()
                    .any(|site| site.range == *first && site.bound && site.binding_id.is_none())
            {
                continue;
            }
            if let Some(address) =
                self.rust_address(layout, path, node.start_byte(), &parts, false)?
                && let Some((key, follows)) =
                    self.rust_resolve(layout, &address, &mut BTreeSet::new())?
            {
                result.push(Reference {
                    key,
                    range: token.clone(),
                    path: Some(node.byte_range()),
                    address,
                    follows_name: follows,
                });
            }
        }
        Ok(result)
    }

    fn rust_check_macros(&self, name: &str, moving: Option<(&str, Range<usize>)>) -> Result<()> {
        let word = regex::Regex::new(&format!(r"\b{}\b", regex::escape(name)))?;
        let capture =
            regex::Regex::new(&format!(r"(?:^|[^{{])\{{{}(?:\}}|:)", regex::escape(name)))?;
        for (path, module) in &self.files {
            for node in walk(module.tree.root_node())
                .into_iter()
                .filter(|node| matches!(node.kind(), "macro_invocation" | "macro_definition"))
            {
                let known = macro_parts(node)
                    .is_some_and(|(callee, _)| self.rust_macro_known(module, callee));
                let inside = moving.as_ref().is_some_and(|(from, range)| {
                    *from == path
                        && range.start <= node.start_byte()
                        && node.end_byte() <= range.end
                });
                ensure!(
                    known || !word.is_match(text(&module.source, node)) && !inside,
                    "Opaque Rust macro references prevent refactoring {name} in {path}"
                );
                let explicit = module
                    .edit_tree
                    .root_node()
                    .descendant_for_byte_range(node.start_byte(), node.end_byte())
                    .is_some_and(|node| {
                        walk(node).into_iter().any(|node| {
                            node.kind() == "assignment_expression"
                                && node
                                    .child_by_field_name("left")
                                    .is_some_and(|left| text(&module.source, left) == name)
                        })
                    });
                if known && !explicit {
                    for literal in walk(node).into_iter().filter(|node| {
                        matches!(node.kind(), "string_literal" | "raw_string_literal")
                    }) {
                        ensure!(
                            !capture.is_match(text(&module.source, literal)),
                            "Implicit Rust format-string captures of {name} require explicit arguments before refactoring"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn rust_rename(&mut self, path: &str, selected: &str, new: &str) -> Result<()> {
        let layout = self.rust_layout()?;
        let symbol = self.files[path].symbol(selected)?;
        self.rust_attributes(symbol, false)?;
        ensure!(
            !matches!(
                symbol.kind.as_str(),
                "method" | "field" | "variant" | "macro" | "module"
            ),
            "Rust receiver/member/module renames require a type-aware operation; use file paths for modules"
        );
        let key = (path.to_owned(), symbol.id);
        let old = symbol.name.clone();
        self.rust_check_macros(&old, None)?;
        let mut edits = Edits::new();
        for (other, module) in &self.files {
            ensure!(
                !module.sites.iter().any(|site| site.name == old
                    && site.binding_id.is_none()
                    && !site.binding_ids.is_empty()),
                "Ambiguous Rust type/value binding for {old} in {other}"
            );
            let references: Vec<_> = self
                .rust_refs(&layout, other)?
                .into_iter()
                .filter(|reference| {
                    reference.key == key
                        && reference.follows_name
                        && module.source.get(reference.range.clone()) == Some(&old)
                })
                .collect();
            if !references.is_empty() {
                module.name_conflict(new)?;
            }
            for reference in references {
                self.rust_safe_range(other, &reference.range)?;
                let site = module.sites.iter().find(|site| {
                    site.range == reference.range && site.binding_id == Some(key.1) && other == path
                });
                edits
                    .entry(other.clone())
                    .or_default()
                    .push(site.map_or_else(
                        || Edit {
                            range: reference.range,
                            text: new.into(),
                        },
                        |site| module.token(site, new),
                    ));
            }
            for import in uses(module)? {
                for item in &import.items {
                    if item.wildcard {
                        continue;
                    }
                    if let Some(address) =
                        self.rust_address(&layout, other, import.range.start, &item.path, true)?
                        && let Some((target, follows)) =
                            self.rust_resolve(&layout, &address, &mut BTreeSet::new())?
                        && target == key
                        && follows
                    {
                        edits.entry(other.clone()).or_default().push(Edit {
                            range: item.token.clone(),
                            text: new.into(),
                        });
                    }
                }
            }
        }
        self.apply(edits)
    }

    fn rust_display(&self, layout: &Layout, from: &str, address: &Address) -> Result<String> {
        let prefix = if layout.contexts[from].root == address.root {
            "crate"
        } else {
            layout
                .names
                .iter()
                .find(|(_, root)| **root == address.root)
                .map(|(name, _)| name.as_str())
                .context("Cannot address destination Rust crate")?
        };
        Ok(std::iter::once(prefix)
            .chain(address.parts.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("::"))
    }

    fn rust_insertion(&self, layout: &Layout, path: &str, scope: &[String]) -> Result<usize> {
        let module = &self.files[path];
        let base = &layout.contexts[path].module;
        let suffix = scope
            .strip_prefix(base.as_slice())
            .context("Rust import scope is outside its module")?;
        let body = if suffix.is_empty() {
            module.tree.root_node()
        } else {
            let name = suffix.join(".");
            let symbol = module
                .structure
                .nodes
                .iter()
                .find(|node| node.kind == "module" && node.qualified_name == name)
                .context("Missing inline Rust module scope")?;
            module
                .declaration(symbol.id)?
                .child_by_field_name("body")
                .context("Rust module is not inline")?
        };
        let mut offset = if suffix.is_empty() {
            0
        } else {
            body.start_byte() + 1
        };
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            if child.is_extra() || child.kind() == "inner_attribute_item" {
                offset = child.end_byte();
            } else {
                break;
            }
        }
        Ok(offset)
    }

    fn rust_register(
        &self,
        layout: &Layout,
        from: &str,
        to: &str,
        public: bool,
        edits: &mut Edits,
    ) -> Result<()> {
        if layout.registrations.contains_key(to) {
            return Ok(());
        }
        let scope = layout
            .contexts
            .get(to)
            .cloned()
            .unwrap_or_else(|| inferred(to));
        if scope.module.is_empty() && self.files.contains_key(to) {
            return Ok(());
        }
        let (name, parent) = scope
            .module
            .split_last()
            .context("Cannot move a declaration into an unregistered Rust crate entrypoint")?;
        valid_name("rust", name)?;
        fn roots(layout: &Layout, from: &str) -> BTreeSet<String> {
            let mut pending = vec![from.to_owned()];
            let mut seen = BTreeSet::new();
            let mut roots = BTreeSet::new();
            while let Some(path) = pending.pop() {
                if !seen.insert(path.clone()) {
                    continue;
                }
                if let Some(parents) = layout.registrations.get(&path) {
                    pending.extend(parents.iter().map(|(path, _, _)| path.clone()));
                } else {
                    roots.insert(path);
                }
            }
            roots
        }
        let selected = roots(layout, from);
        let parents: Vec<_> = layout
            .contexts
            .iter()
            .filter(|(path, context)| {
                context.root == scope.root
                    && context.module == parent
                    && !roots(layout, path).is_disjoint(&selected)
            })
            .map(|(path, _)| path.clone())
            .collect();
        ensure!(
            !parents.is_empty(),
            "Create/register the destination Rust parent module before moving to {to}"
        );
        for path in parents {
            let module = &self.files[&path];
            if module
                .structure
                .nodes
                .iter()
                .any(|node| node.parent_id.is_none() && node.kind == "module" && node.name == *name)
            {
                continue;
            }
            self.add_imports(
                edits,
                &path,
                self.rust_insertion(layout, &path, parent)?,
                [format!(
                    "{} mod {name};\n",
                    if public { "pub" } else { "pub(crate)" }
                )],
            );
        }
        Ok(())
    }

    fn rust_rewire_uses(
        &self,
        layout: &Layout,
        old: &Address,
        new: &Address,
        file_move: bool,
        from: &str,
        edits: &mut Edits,
    ) -> Result<()> {
        for (path, module) in &self.files {
            for import in uses(module)? {
                let mut changed = false;
                let mut rendered = Vec::new();
                for item in &import.items {
                    let mut alias = item.alias.clone();
                    let address =
                        self.rust_address(layout, path, import.range.start, &item.path, true)?;
                    let rewritten = if let Some(mut address) = address {
                        let affected = address.root == old.root
                            && if file_move {
                                address.parts.starts_with(&old.parts)
                            } else {
                                address.parts == old.parts
                            };
                        if affected {
                            let suffix = address.parts[old.parts.len()..].to_vec();
                            address.root = new.root.clone();
                            address.parts = new.parts.iter().cloned().chain(suffix).collect();
                            changed = true;
                            if file_move
                                && alias.is_none()
                                && !item.wildcard
                                && item.path.last() != address.parts.last()
                            {
                                alias = item.path.last().cloned();
                            }
                        } else if file_move && path == from {
                            changed = true;
                        }
                        self.rust_display(layout, path, &address)?
                    } else {
                        ensure!(
                            !(file_move
                                && path == from
                                && item.path.first().is_some_and(|part| matches!(
                                    part.as_str(),
                                    "self" | "super" | "crate"
                                ))),
                            "Cannot rebase an unresolved relative Rust import"
                        );
                        item.path.join("::")
                    };
                    rendered.push(format!(
                        "{}{}{}{};\n",
                        import.prefix,
                        rewritten,
                        if item.wildcard { "::*" } else { "" },
                        alias.map(|name| format!(" as {name}")).unwrap_or_default()
                    ));
                }
                if changed {
                    ensure!(
                        import.items.len() == 1
                            || module.structure.nodes[import.id].attributes.is_empty(),
                        "Splitting an attributed Rust use group requires preserving each import's conditional/macro context"
                    );
                    ensure!(
                        !text(
                            &module.source,
                            module
                                .tree
                                .root_node()
                                .descendant_for_byte_range(import.range.start, import.range.end)
                                .unwrap()
                        )
                        .contains("/*")
                            && !module.source[import.range.clone()].contains("//"),
                        "Commented Rust use groups cannot yet be split safely"
                    );
                    edits.entry(path.clone()).or_default().push(Edit {
                        range: import.range,
                        text: rendered.concat(),
                    });
                }
            }
        }
        Ok(())
    }

    pub(super) fn rust_move_file(&mut self, from: &str, to: &str) -> Result<()> {
        let layout = self.rust_layout()?;
        let old_context = &layout.contexts[from];
        let new_context = inferred(to);
        ensure!(
            !self.rust_conditional(&layout, from),
            "Moving a conditionally registered Rust module requires cfg-context resolution"
        );
        for (path, parent) in &layout.contexts {
            if parent.root == new_context.root
                && new_context.module.len() > 1
                && parent.module == new_context.module[..new_context.module.len() - 1]
            {
                ensure!(
                    !self.rust_conditional(&layout, path),
                    "Destination Rust module is conditionally registered"
                );
            }
        }
        ensure!(
            !old_context.module.is_empty() && !new_context.module.is_empty(),
            "Moving Rust crate entrypoints requires updating Cargo targets and is not supported"
        );
        ensure!(
            old_context.root == new_context.root,
            "A Rust file cannot move across crate roots"
        );
        ensure!(!walk(self.files[from].tree.root_node()).iter().any(|node| node.kind() == "mod_item" && node.child_by_field_name("body").is_none()), "Moving a Rust file with external child modules requires moving the module tree, not one file");
        let old = Address {
            root: old_context.root.clone(),
            parts: old_context.module.clone(),
        };
        let new = Address {
            root: new_context.root.clone(),
            parts: new_context.module.clone(),
        };
        let relocated = Path::new(from).parent() != Path::new(to).parent();
        self.rust_check_macros(
            old.parts.last().unwrap(),
            relocated.then(|| (from, 0..self.files[from].source.len())),
        )?;
        if relocated {
            for symbol in &self.files[from].structure.nodes {
                self.rust_attributes(symbol, true)?;
            }
        }
        let mut edits = Edits::new();
        self.rust_rewire_uses(&layout, &old, &new, true, from, &mut edits)?;
        let public = layout.registrations.get(from).is_some_and(|registrations| {
            registrations.iter().any(|(path, range, _)| {
                self.files[path]
                    .tree
                    .root_node()
                    .descendant_for_byte_range(range.start, range.end)
                    .is_some_and(|node| {
                        children(node).iter().any(|node| {
                            node.kind() == "visibility_modifier"
                                && text(&self.files[path].source, *node) == "pub"
                        })
                    })
            })
        });
        self.rust_register(&layout, from, to, public, &mut edits)?;
        for (path, module) in &self.files {
            let mut covered = Vec::<Range<usize>>::new();
            for node in walk(module.edit_tree.root_node())
                .into_iter()
                .filter(|node| {
                    matches!(node.kind(), "scoped_identifier" | "scoped_type_identifier")
                })
            {
                if std::iter::successors(node.parent(), |node| node.parent()).any(|parent| {
                    matches!(
                        parent.kind(),
                        "use_declaration" | "attribute_item" | "macro_definition" | "token_tree"
                    )
                }) || covered
                    .iter()
                    .any(|range| range.start <= node.start_byte() && node.end_byte() <= range.end)
                {
                    continue;
                }
                if let Some(mut address) = self.rust_address(
                    &layout,
                    path,
                    node.start_byte(),
                    &path_parts(node, &module.source),
                    false,
                )? {
                    let affected =
                        address.root == old.root && address.parts.starts_with(&old.parts);
                    if affected || path == from {
                        self.rust_safe_range(path, &node.byte_range())?;
                        ensure!(
                            !walk(node)
                                .iter()
                                .any(|node| node.kind() == "type_arguments"),
                            "Moving generic-qualified Rust paths is not supported"
                        );
                        if affected {
                            address.parts = new
                                .parts
                                .iter()
                                .cloned()
                                .chain(address.parts[old.parts.len()..].iter().cloned())
                                .collect();
                        }
                        edits.entry(path.clone()).or_default().push(Edit {
                            range: node.byte_range(),
                            text: self.rust_display(&layout, path, &address)?,
                        });
                        covered.push(node.byte_range());
                    }
                }
            }
        }
        let registrations = layout.registrations.get(from).context(
            "Source Rust module is not registered; implicit/custom module layouts cannot be moved",
        )?;
        for (path, range, _) in registrations {
            edits.entry(path.clone()).or_default().push(Edit {
                range: range.clone(),
                text: String::new(),
            });
        }
        self.apply(edits)?;
        let source = self.files.remove(from).unwrap().source;
        self.set_source(to, source)
    }

    fn rust_visible(&self, layout: &Layout, key: &Key, destination: &Context) -> Result<bool> {
        let module = &self.files[&key.0];
        let node = &module.structure.nodes[key.1];
        let scope = context(
            layout,
            &key.0,
            module,
            node.declaration_start_byte.unwrap_or(node.start_byte),
        );
        if scope.root != destination.root {
            return Ok(node.signature.starts_with("pub "));
        }
        let visibility = module
            .declaration(node.id)
            .ok()
            .and_then(|node| {
                children(node)
                    .into_iter()
                    .find(|node| node.kind() == "visibility_modifier")
            })
            .map(|node| text(&module.source, node));
        Ok(match visibility {
            Some("pub" | "pub(crate)") => true,
            Some("pub(super)") => destination
                .module
                .starts_with(&scope.module[..scope.module.len().saturating_sub(1)]),
            Some("pub(self)") | None => destination.module.starts_with(&scope.module),
            _ => false,
        })
    }

    pub(super) fn rust_move_symbol(&mut self, from: &str, selected: &str, to: &str) -> Result<()> {
        if !self.files.contains_key(to) {
            self.set_source(to, String::new())?;
        }
        let layout = self.rust_layout()?;
        let symbol = self.files[from].symbol(selected)?;
        self.rust_attributes(symbol, true)?;
        ensure!(
            symbol.kind == "function" && symbol.parent_id.is_none(),
            "Only free, file-level Rust functions currently move between modules"
        );
        let key = (from.to_owned(), symbol.id);
        let range = self.files[from].declaration_range(symbol.id)?;
        self.rust_check_macros(&symbol.name, Some((from, range.clone())))?;
        ensure!(
            !self.rust_conditional(&layout, from) && !self.rust_conditional(&layout, to),
            "Moving Rust declarations across conditional module contexts requires cfg resolution"
        );
        let old_context = &layout.contexts[from];
        let destination = &layout.contexts[to];
        ensure!(
            old_context.root == destination.root,
            "Rust function moves cannot cross crate roots"
        );
        let old = Address {
            root: old_context.root.clone(),
            parts: old_context
                .module
                .iter()
                .cloned()
                .chain([symbol.name.clone()])
                .collect(),
        };
        let new = Address {
            root: destination.root.clone(),
            parts: destination
                .module
                .iter()
                .cloned()
                .chain([symbol.name.clone()])
                .collect(),
        };
        ensure!(
            self.rust_resolve(&layout, &new, &mut BTreeSet::new())?
                .is_none(),
            "Destination Rust module already resolves {}",
            symbol.name
        );
        self.files[to].name_conflict(&symbol.name)?;
        let source = &self.files[from];
        let mut body_edits = Vec::new();
        let mut edits = Edits::new();
        let mut additions = BTreeMap::<(String, Vec<String>), BTreeSet<String>>::new();
        self.rust_register(
            &layout,
            from,
            to,
            symbol.signature.starts_with("pub "),
            &mut edits,
        )?;
        self.rust_rewire_uses(&layout, &old, &new, false, from, &mut edits)?;
        for import in uses(source)? {
            for item in &import.items {
                let local = item
                    .alias
                    .as_ref()
                    .or_else(|| item.path.last())
                    .cloned()
                    .unwrap_or_default();
                let used = source.sites.iter().any(|site| {
                    range.start <= site.range.start
                        && site.range.end <= range.end
                        && site.binding_id == Some(import.id)
                        && site.receiver.as_deref().unwrap_or(&site.name) == local
                });
                if !used {
                    continue;
                }
                ensure!(
                    !item.wildcard,
                    "Moving a Rust function with an unresolved glob dependency is not supported"
                );
                let address =
                    self.rust_address(&layout, from, import.range.start, &item.path, true)?;
                if let Some(address) = &address
                    && let Some((dependency, _)) =
                        self.rust_resolve(&layout, address, &mut BTreeSet::new())?
                {
                    ensure!(
                        self.rust_visible(&layout, &dependency, destination)?,
                        "Rust move would lose access to private imported dependency {local}"
                    );
                }
                self.files[to].name_conflict(&local)?;
                let imported = address
                    .as_ref()
                    .map(|address| self.rust_display(&layout, to, address))
                    .transpose()?
                    .unwrap_or_else(|| item.path.join("::"));
                additions
                    .entry((to.into(), destination.module.clone()))
                    .or_default()
                    .insert(format!(
                        "use {imported}{};\n",
                        item.alias
                            .as_ref()
                            .map(|alias| format!(" as {alias}"))
                            .unwrap_or_default()
                    ));
            }
        }
        for (path, module) in &self.files {
            let mut covered = Vec::<Range<usize>>::new();
            for reference in self.rust_refs(&layout, path)? {
                let inside = path == from
                    && range.start <= reference.range.start
                    && reference.range.end <= range.end;
                if reference.key == key {
                    if let Some(path_range) = reference.path.clone()
                        && reference.address == old
                    {
                        if covered.iter().any(|range| {
                            range.start <= path_range.start && path_range.end <= range.end
                        }) {
                            continue;
                        }
                        let replacement = self.rust_display(&layout, path, &new)?;
                        if inside {
                            body_edits.push(Edit {
                                range: path_range.start - range.start..path_range.end - range.start,
                                text: replacement,
                            });
                        } else {
                            edits.entry(path.clone()).or_default().push(Edit {
                                range: path_range.clone(),
                                text: replacement,
                            });
                        }
                        covered.push(path_range);
                    } else if !inside
                        && reference.path.is_none()
                        && !module
                            .sites
                            .iter()
                            .any(|site| site.range == reference.range && site.declaration)
                    {
                        let imported = module
                            .sites
                            .iter()
                            .find(|site| site.range == reference.range)
                            .and_then(|site| site.binding_id)
                            .is_some_and(|id| module.structure.nodes[id].kind == "import");
                        if !imported {
                            let scope = context(&layout, path, module, reference.range.start);
                            additions
                                .entry((path.clone(), scope.module))
                                .or_default()
                                .insert(format!(
                                    "use {};\n",
                                    self.rust_display(&layout, path, &new)?
                                ));
                        }
                    }
                } else if inside {
                    let dependency = &self.files[&reference.key.0].structure.nodes[reference.key.1];
                    if reference.key.0 == from
                        && range.start <= dependency.start_byte
                        && dependency.end_byte <= range.end
                    {
                        continue;
                    }
                    ensure!(
                        self.rust_visible(&layout, &reference.key, destination)?,
                        "Rust move would capture private dependency {}",
                        dependency.name
                    );
                    if let Some(path_range) = reference.path {
                        if covered.iter().any(|range| {
                            range.start <= path_range.start && path_range.end <= range.end
                        }) {
                            continue;
                        }
                        body_edits.push(Edit {
                            range: path_range.start - range.start..path_range.end - range.start,
                            text: self.rust_display(&layout, to, &reference.address)?,
                        });
                        covered.push(path_range);
                    } else if module.sites.iter().any(|site| {
                        site.range == reference.range
                            && site
                                .binding_id
                                .is_some_and(|id| module.structure.nodes[id].kind != "import")
                    }) {
                        self.files[to].name_conflict(&dependency.name)?;
                        additions
                            .entry((to.into(), destination.module.clone()))
                            .or_default()
                            .insert(format!(
                                "use {};\n",
                                self.rust_display(&layout, to, &reference.address)?
                            ));
                    }
                }
            }
        }
        let declaration = source.declaration(symbol.id)?;
        if let Some(visibility) = children(declaration)
            .into_iter()
            .find(|node| node.kind() == "visibility_modifier")
        {
            if text(&source.source, visibility) != "pub"
                && text(&source.source, visibility) != "pub(crate)"
            {
                body_edits.push(Edit {
                    range: visibility.start_byte() - range.start
                        ..visibility.end_byte() - range.start,
                    text: "pub(crate)".into(),
                });
            }
        } else {
            body_edits.push(Edit {
                range: declaration.start_byte() - range.start
                    ..declaration.start_byte() - range.start,
                text: "pub(crate) ".into(),
            });
        }
        let moved = edited(&source.source[range.clone()], body_edits)?;
        edits.entry(from.into()).or_default().push(Edit {
            range,
            text: String::new(),
        });
        let end = self.files[to].source.len();
        edits.entry(to.into()).or_default().push(Edit {
            range: end..end,
            text: format!("\n{moved}\n"),
        });
        for ((path, scope), imports) in additions {
            self.add_imports(
                &mut edits,
                &path,
                self.rust_insertion(&layout, &path, &scope)?,
                imports,
            );
        }
        self.apply(edits)
    }
}
