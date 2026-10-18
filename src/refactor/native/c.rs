use super::super::syntax::relative_path;
use super::*;

#[derive(Clone)]
struct Include {
    range: Range<usize>,
    target: Option<String>,
    quoted: bool,
}

fn static_symbol(module: &Module, node: &StructureNode) -> bool {
    module.declaration(node.id).is_ok_and(|declaration| {
        children(declaration).iter().any(|node| {
            node.kind() == "storage_class_specifier" && text(&module.source, *node) == "static"
        })
    })
}

fn preprocessor_site(module: &Module, site: &EditSite) -> bool {
    module
        .tree
        .root_node()
        .descendant_for_byte_range(site.range.start, site.range.end)
        .is_some_and(|node| {
            std::iter::successors(Some(node), |node| node.parent()).any(|parent| {
                parent.kind().starts_with("preproc_")
                    && ["condition", "name"]
                        .iter()
                        .filter_map(|field| parent.child_by_field_name(field))
                        .any(|condition| {
                            condition.start_byte() <= site.range.start
                                && site.range.end <= condition.end_byte()
                        })
            })
        })
}

impl Workspace {
    fn c_includes(&self, path: &str) -> Result<Vec<Include>> {
        let module = &self.files[path];
        let mut includes = Vec::new();
        for node in walk(module.tree.root_node())
            .into_iter()
            .filter(|node| node.kind() == "preproc_include")
        {
            let value = node
                .child_by_field_name("path")
                .context("Missing C include path")?;
            let source = text(&module.source, value);
            ensure!(
                source.starts_with(['"', '<'])
                    && source.ends_with(['"', '>'])
                    && !source.contains('\\'),
                "Macro/escaped C include paths are not supported: {path}"
            );
            let literal = &source[1..source.len() - 1];
            let joined = super::super::lexical(
                &Path::new(path)
                    .parent()
                    .unwrap_or(Path::new(""))
                    .join(literal),
            )
            .to_string_lossy()
            .replace('\\', "/");
            let mut candidates = BTreeSet::new();
            if source.starts_with('"') && self.files.contains_key(&joined) {
                candidates.insert(joined);
            } else {
                for target in self.files.keys() {
                    if target == literal || target.ends_with(&format!("/{literal}")) {
                        candidates.insert(target.clone());
                    }
                }
            }
            ensure!(
                candidates.len() <= 1,
                "Ambiguous C include in {path}: {literal}; include search paths cannot be inferred"
            );
            includes.push(Include {
                range: value.byte_range(),
                target: candidates.into_iter().next(),
                quoted: source.starts_with('"'),
            });
        }
        Ok(includes)
    }

    fn c_headers(&self, path: &str) -> Result<BTreeSet<String>> {
        let mut found = BTreeSet::new();
        let mut pending = vec![path.to_owned()];
        while let Some(path) = pending.pop() {
            if !found.insert(path.clone()) {
                continue;
            }
            pending.extend(
                self.c_includes(&path)?
                    .into_iter()
                    .filter_map(|include| include.target),
            );
        }
        Ok(found)
    }

    fn c_move_context(&self, path: &str) -> Result<()> {
        for header in self.c_headers(path)? {
            let module = &self.files[&header];
            ensure!(
                self.c_includes(&header)?
                    .iter()
                    .all(|include| include.target.is_some()),
                "Moving C functions requires indexed includes; preprocessing external/system headers cannot be inferred: {header}"
            );
            let guard = children(module.tree.root_node())
                .into_iter()
                .find(|node| {
                    node.kind() == "preproc_ifdef"
                        && text(&module.source, *node)
                            .trim_start()
                            .starts_with("#ifndef")
                })
                .and_then(|node| node.child_by_field_name("name"))
                .map(|node| text(&module.source, node));
            for node in walk(module.tree.root_node()) {
                let harmless = match node.kind() {
                    "preproc_def" | "preproc_ifdef" => node
                        .child_by_field_name("name")
                        .is_some_and(|name| Some(text(&module.source, name)) == guard),
                    "preproc_call" => text(&module.source, node).trim() == "#pragma once",
                    "preproc_function_def" | "preproc_if" | "preproc_else" | "preproc_elif" => {
                        false
                    }
                    _ => true,
                };
                ensure!(
                    harmless,
                    "Moving C functions with file-local preprocessor state requires build-context analysis: {header}"
                );
            }
        }
        Ok(())
    }

    fn c_symbol(&self, path: &str, selected: &str) -> Result<&StructureNode> {
        let module = &self.files[path];
        let candidates: Vec<_> = module
            .structure
            .nodes
            .iter()
            .filter(|node| {
                node.parent_id.is_none() && node.kind != "import" && node.name == selected
            })
            .collect();
        if candidates.len() == 1 {
            return Ok(candidates[0]);
        }
        let definitions: Vec<_> = candidates
            .iter()
            .filter(|node| {
                module
                    .declaration(node.id)
                    .is_ok_and(|node| node.kind() == "function_definition")
            })
            .collect();
        ensure!(
            definitions.len() == 1 && candidates.iter().all(|node| node.kind == "function"),
            "Ambiguous C declaration: {selected}"
        );
        Ok(definitions[0])
    }

    fn c_family(&self, path: &str, symbol: &StructureNode) -> Result<BTreeSet<Key>> {
        if static_symbol(&self.files[path], symbol) {
            return Ok(BTreeSet::from([(path.into(), symbol.id)]));
        }
        // Tags and typedefs have translation-unit scope, not external linkage.
        // Their users are reached through includes, never same-name guessing.
        if !matches!(symbol.kind.as_str(), "function" | "variable" | "constant") {
            return Ok(BTreeSet::from([(path.into(), symbol.id)]));
        }
        let mut group = BTreeSet::new();
        let mut definitions = 0;
        for (path, module) in &self.files {
            for node in module.structure.nodes.iter().filter(|node| {
                node.parent_id.is_none()
                    && node.name == symbol.name
                    && node.kind == symbol.kind
                    && !static_symbol(module, node)
            }) {
                definitions += usize::from(
                    node.kind == "function"
                        && module
                            .declaration(node.id)
                            .is_ok_and(|node| node.kind() == "function_definition"),
                );
                definitions += usize::from(
                    matches!(node.kind.as_str(), "variable" | "constant")
                        && !node
                            .signature
                            .split_whitespace()
                            .any(|word| word == "extern"),
                );
                group.insert((path.clone(), node.id));
            }
        }
        ensure!(
            definitions <= 1,
            "Multiple external C definitions of {}; select an index scoped to one build target",
            symbol.name
        );
        Ok(group)
    }

    fn c_origin(&self, path: &str, site: &EditSite) -> Result<BTreeSet<Key>> {
        // Preprocessor conditions are a separate macro namespace. A C
        // declaration with the same spelling is not evidence of a reference.
        let module = &self.files[path];
        if preprocessor_site(module, site) {
            return Ok(BTreeSet::new());
        }
        if site.receiver.is_some() {
            return Ok(BTreeSet::new());
        }
        if let Some(id) = site.binding_id {
            return Ok(BTreeSet::from([(path.into(), id)]));
        }
        if site.bound {
            return Ok(site
                .binding_ids
                .iter()
                .map(|id| (path.into(), *id))
                .collect());
        }
        let mut found = BTreeSet::new();
        for header in self.c_headers(path)? {
            for node in self.files[&header].structure.nodes.iter().filter(|node| {
                node.parent_id.is_none() && node.kind != "import" && node.name == site.name
            }) {
                found.insert((header.clone(), node.id));
            }
        }
        Ok(found)
    }

    pub(super) fn c_rename(&mut self, path: &str, selected: &str, new: &str) -> Result<()> {
        let symbol = self.c_symbol(path, selected)?;
        ensure!(
            !matches!(symbol.kind.as_str(), "macro" | "field" | "variant"),
            "C macros and members require preprocessor/type-aware resolution"
        );
        let group = self.c_family(path, symbol)?;
        let old = symbol.name.clone();
        let word = regex::Regex::new(&format!(r"\b{}\b", regex::escape(&old)))?;
        let mut edits = Edits::new();
        for (other, module) in &self.files {
            for node in walk(module.tree.root_node()).into_iter().filter(|node| {
                matches!(
                    node.kind(),
                    "preproc_def" | "preproc_function_def" | "preproc_call"
                ) || node.kind().contains("asm")
            }) {
                ensure!(
                    !word.is_match(text(&module.source, node)),
                    "C macro references to {old} cannot be refactored safely in {other}"
                );
            }
            let mut selected_sites = Vec::new();
            for site in module.sites.iter().filter(|site| site.name == old) {
                if preprocessor_site(module, site) {
                    continue;
                }
                let origin = self.c_origin(other, site)?;
                if !origin.is_disjoint(&group) {
                    ensure!(
                        origin.is_subset(&group),
                        "Ambiguous C linkage for {old} in {other}"
                    );
                    selected_sites.push(site);
                } else if origin.is_empty()
                    && !site.bound
                    && site.receiver.is_none()
                    && !static_symbol(&self.files[path], symbol)
                {
                    bail!(
                        "Unresolved C reference to {old} in {other}; explicit indexed declarations/includes are required"
                    );
                }
            }
            if !selected_sites.is_empty() {
                module.name_conflict(new)?;
                for site in selected_sites {
                    edits
                        .entry(other.clone())
                        .or_default()
                        .push(module.token(site, new));
                }
            }
        }
        self.apply(edits)
    }

    pub(super) fn c_move_file(&mut self, from: &str, to: &str) -> Result<()> {
        ensure!(
            Path::new(from).extension() == Path::new(to).extension(),
            "A C file move cannot turn a header into a translation unit or vice versa"
        );
        let mut edits = Edits::new();
        for path in self.files.keys() {
            for include in self.c_includes(path)? {
                if include.target.as_deref() == Some(from)
                    || path == from
                        && include.quoted
                        && Path::new(from).parent() != Path::new(to).parent()
                {
                    let target = include
                        .target
                        .as_deref()
                        .context("Cannot rebase an unresolved quoted C include")?;
                    let target = if target == from { to } else { target };
                    let importer = if path == from { to } else { path };
                    edits.entry(path.clone()).or_default().push(Edit {
                        range: include.range,
                        text: serde_json::to_string(&relative_path(importer, target))?,
                    });
                }
            }
        }
        self.apply(edits)?;
        let source = self.files.remove(from).unwrap().source;
        self.set_source(to, source)
    }

    pub(super) fn c_move_symbol(&mut self, from: &str, selected: &str, to: &str) -> Result<()> {
        if !self.files.contains_key(to) {
            self.set_source(to, String::new())?;
        }
        self.c_move_context(from)?;
        self.c_move_context(to)?;
        let symbol = self.c_symbol(from, selected)?;
        ensure!(
            symbol.kind == "function",
            "Only C function definitions can move between files"
        );
        let declaration = self.files[from].declaration(symbol.id)?;
        ensure!(
            declaration.kind() == "function_definition"
                && declaration
                    .parent()
                    .is_some_and(|node| node.kind() == "translation_unit"),
            "Conditional/prototype-only C declarations cannot be moved"
        );
        let body = declaration
            .child_by_field_name("body")
            .context("C function has no body")?;
        ensure!(
            !walk(declaration)
                .iter()
                .any(|node| node.kind().contains("asm")),
            "Moving C assembly-dependent functions requires assembler/linkage analysis"
        );
        let signature = self.files[from].source[declaration.start_byte()..body.start_byte()]
            .trim()
            .to_owned();
        ensure!(
            !signature.split_whitespace().any(|word| word == "inline"),
            "Moving C inline definitions requires linkage analysis"
        );
        let range = self.files[from].declaration_range(symbol.id)?;
        let group = self.c_family(from, symbol)?;
        let private = static_symbol(&self.files[from], symbol);
        let name = symbol.name.clone();
        let source = &self.files[from];
        let visible = self.c_headers(to)?;
        for path in &visible {
            for existing in self.files[path].structure.nodes.iter().filter(|node| {
                node.parent_id.is_none()
                    && node.name == name
                    && !matches!(node.kind.as_str(), "struct" | "union" | "enum")
            }) {
                ensure!(
                    existing.kind == "function"
                        && static_symbol(&self.files[path], existing) == private
                        && self.files[path].declaration(existing.id)?.kind()
                            != "function_definition",
                    "Destination C translation unit already defines or conflicts with {name}"
                );
            }
        }
        let mut includes = BTreeSet::new();
        for site in source.sites.iter().filter(|site| {
            !site.declaration && range.start <= site.range.start && site.range.end <= range.end
        }) {
            if site.receiver.is_some() || site.bound && site.binding_id.is_none() {
                continue;
            }
            let origin = self.c_origin(from, site)?;
            if !origin.is_disjoint(&group) {
                continue;
            }
            if let Some(id) = site.binding_id
                && range.start <= source.structure.nodes[id].start_byte
                && source.structure.nodes[id].end_byte <= range.end
            {
                continue;
            }
            ensure!(
                !origin.is_empty(),
                "Unresolved C dependency {} prevents moving {name}",
                site.name
            );
            for (path, id) in &origin {
                ensure!(
                    !static_symbol(&self.files[path], &self.files[path].structure.nodes[*id])
                        && self.files[path].structure.nodes[*id].kind != "macro",
                    "Moving {name} would capture private/preprocessor dependency {}",
                    site.name
                );
            }
            let first = origin.first().unwrap();
            let dependency = &self.files[&first.0].structure.nodes[first.1];
            let allowed = if matches!(
                dependency.kind.as_str(),
                "function" | "variable" | "constant"
            ) {
                self.c_family(&first.0, dependency)?
            } else {
                origin.clone()
            };
            for path in &visible {
                for existing in self.files[path]
                    .structure
                    .nodes
                    .iter()
                    .filter(|node| node.parent_id.is_none() && node.name == site.name)
                {
                    ensure!(
                        allowed.contains(&(path.clone(), existing.id)),
                        "Destination C declaration would capture dependency {}",
                        site.name
                    );
                }
            }
            let header = allowed
                .iter()
                .find(|(path, id)| {
                    path.ends_with(".h")
                        && !static_symbol(&self.files[path], &self.files[path].structure.nodes[*id])
                })
                .map(|(path, _)| path.clone())
                .with_context(|| {
                    format!(
                        "Moving {name} would capture private/unprototyped C dependency {}",
                        site.name
                    )
                })?;
            includes.insert(header);
        }
        let mut remaining = false;
        for (path, module) in &self.files {
            for site in module.sites.iter().filter(|site| {
                !(site.declaration
                    || path == from
                        && range.start <= site.range.start
                        && site.range.end <= range.end)
            }) {
                remaining |= !self.c_origin(path, site)?.is_disjoint(&group);
            }
        }
        ensure!(
            !private || !remaining,
            "A static C function still used in its original translation unit cannot move without changing linkage"
        );
        let moved = source.source[range.clone()].to_owned();
        let mut edits = Edits::new();
        edits.entry(from.into()).or_default().push(Edit {
            range,
            text: if remaining {
                format!("{signature};")
            } else {
                String::new()
            },
        });
        let end = self.files[to].source.len();
        edits.entry(to.into()).or_default().push(Edit {
            range: end..end,
            text: format!("\n{moved}\n"),
        });
        let existing = self.c_headers(to)?;
        self.add_imports(
            &mut edits,
            to,
            0,
            includes
                .into_iter()
                .filter(|header| !existing.contains(header))
                .map(|header| {
                    format!(
                        "#include {}\n",
                        serde_json::to_string(&relative_path(to, &header)).unwrap()
                    )
                }),
        );
        self.apply(edits)
    }
}
