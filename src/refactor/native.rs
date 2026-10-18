//! Language adapters for the statically typed native-language indexes.
//!
//! Discovery and lexical identities come from the same parser/index as map.
//! Each adapter supplies its language's package, linkage, and import rules.

mod c;
mod go;
mod java;
mod rust;

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use tree_sitter::{Node, Tree};

use super::syntax::{Edit, Edits, children, edited, text, tree, walk};
use crate::parse::{self, EditSite, FileStructure, StructureNode};

type Key = (String, usize);

struct Module {
    source: String,
    structure: FileStructure,
    tree: Tree,
    edit_tree: Tree,
    sites: Vec<EditSite>,
    rust_uses: std::cell::OnceCell<Vec<rust::Use>>,
    go_imports: std::cell::OnceCell<Vec<go::Import>>,
}

impl Module {
    fn new(path: &str, source: String, structure: FileStructure) -> Result<Self> {
        let tree = tree(path, &source)?;
        let (edit_tree, lexical_source) = if parse::language_for_path(path) == Some("rust") {
            rust::editable_tree(path, &source, &tree)?
        } else {
            (tree.clone(), source.clone())
        };
        let sites = parse::edit_sites(
            edit_tree.root_node(),
            &lexical_source,
            parse::language_for_path(path).unwrap(),
            &structure,
        );
        Ok(Self {
            source,
            structure,
            tree,
            edit_tree,
            sites,
            rust_uses: std::cell::OnceCell::new(),
            go_imports: std::cell::OnceCell::new(),
        })
    }

    fn symbol(&self, name: &str) -> Result<&StructureNode> {
        let candidates: Vec<_> = self
            .structure
            .nodes
            .iter()
            .filter(|node| {
                node.kind != "import"
                    && node.kind != "impl"
                    && (node.qualified_name == name
                        || !name.contains('.') && node.parent_id.is_none() && node.name == name)
            })
            .collect();
        ensure!(
            candidates.len() == 1,
            "Expected one indexed declaration for {name:?}; found {} (use a qualified name for members)",
            candidates.len()
        );
        Ok(candidates[0])
    }

    fn declaration(&self, id: usize) -> Result<Node<'_>> {
        let symbol = &self.structure.nodes[id];
        let start = symbol.declaration_start_byte.unwrap_or(symbol.start_byte);
        let root = self.tree.root_node();
        if let Some(node) = root.named_descendant_for_byte_range(start, symbol.end_byte)
            && node.id() != root.id()
            && node.start_byte() == start
            && node.end_byte() == symbol.end_byte
        {
            return Ok(node);
        }
        walk(self.tree.root_node())
            .into_iter()
            .skip(1)
            .find(|node| node.start_byte() == start && node.end_byte() == symbol.end_byte)
            .or_else(|| {
                walk(self.tree.root_node()).into_iter().find(|node| {
                    node.end_byte() == symbol.end_byte
                        && node
                            .child_by_field_name("name")
                            .is_some_and(|name| text(&self.source, name) == symbol.name)
                })
            })
            .with_context(|| format!("Cannot isolate declaration {}", symbol.qualified_name))
    }

    fn name_conflict(&self, name: &str) -> Result<()> {
        ensure!(
            !self
                .sites
                .iter()
                .any(|site| site.receiver.is_none() && site.name == name)
                && !self
                    .structure
                    .nodes
                    .iter()
                    .filter(|node| node.kind == "import")
                    .flat_map(|node| &node.imports)
                    .any(
                        |binding| binding.alias.as_deref().or(binding.name.as_deref())
                            == Some(name)
                    ),
            "Destination name {name:?} would collide with or capture an existing binding/reference"
        );
        Ok(())
    }

    fn token(&self, site: &EditSite, new: &str) -> Edit {
        let node = self
            .tree
            .root_node()
            .descendant_for_byte_range(site.range.start, site.range.end);
        let shorthand = site.shorthand
            || node.is_some_and(|node| {
                node.parent()
                    .is_some_and(|parent| parent.kind() == "shorthand_field_initializer")
            });
        Edit {
            range: site.range.clone(),
            text: if shorthand {
                format!("{}: {new}", site.name)
            } else {
                new.into()
            },
        }
    }

    fn declaration_range(&self, id: usize) -> Result<Range<usize>> {
        let node = self.declaration(id)?;
        Ok(self.structure.nodes[id].start_byte.min(node.start_byte())..node.end_byte())
    }
}

pub(super) struct Workspace {
    root: PathBuf,
    language: &'static str,
    files: BTreeMap<String, Module>,
    policies: BTreeMap<String, String>,
}

impl Workspace {
    pub(super) fn new(
        root: &Path,
        files: BTreeMap<String, (String, FileStructure)>,
    ) -> Result<Self> {
        let language = files
            .keys()
            .next()
            .and_then(|path| parse::language_for_path(path))
            .context("No indexed source files")?;
        Ok(Self {
            root: root.to_owned(),
            language,
            files: files
                .into_iter()
                .map(|(path, (source, structure))| {
                    Ok((path.clone(), Module::new(&path, source, structure)?))
                })
                .collect::<Result<_>>()?,
            policies: BTreeMap::new(),
        })
    }

    pub(super) fn consulted(&self) -> BTreeMap<String, String> {
        self.policies.clone()
    }
    pub(super) fn sources(&self) -> BTreeMap<String, String> {
        self.files
            .iter()
            .map(|(path, module)| (path.clone(), module.source.clone()))
            .collect()
    }

    pub(super) fn refactor(
        &mut self,
        from: &str,
        symbol: Option<&str>,
        to: &str,
        new_name: Option<&str>,
    ) -> Result<()> {
        ensure!(
            parse::language_for_path(from) == parse::language_for_path(to),
            "Refactoring cannot cross source languages"
        );
        if let Some(symbol) = symbol {
            let new = new_name.unwrap();
            let old = symbol.rsplit('.').next().unwrap();
            let new = new.rsplit('.').next().unwrap();
            valid_name(self.language, new)?;
            ensure!(
                from != to || old != new,
                "Source and destination are identical"
            );
            if old != new {
                self.rename(from, symbol, new)?;
            }
            if from != to {
                let selected = symbol
                    .rsplit_once('.')
                    .map_or_else(|| new.to_owned(), |(scope, _)| format!("{scope}.{new}"));
                match self.language {
                    "rust" => self.rust_move_symbol(from, &selected, to)?,
                    "go" => self.go_move_symbol(from, &selected, to)?,
                    "java" => self.java_move_symbol(from, &selected, to, new_name.unwrap())?,
                    "c" => self.c_move_symbol(from, &selected, to)?,
                    _ => unreachable!(),
                }
            }
        } else {
            ensure!(from != to, "Source and destination are identical");
            ensure!(
                !self.root.join(to).try_exists()?,
                "Destination already exists: {to}"
            );
            match self.language {
                "rust" => self.rust_move_file(from, to)?,
                "go" => self.go_move_file(from, to)?,
                "java" => self.java_move_file(from, to)?,
                "c" => self.c_move_file(from, to)?,
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    fn rename(&mut self, path: &str, symbol: &str, new: &str) -> Result<()> {
        match self.language {
            "rust" => self.rust_rename(path, symbol, new),
            "go" => self.go_rename(path, symbol, new),
            "java" => self.java_rename(path, symbol, new, true),
            "c" => self.c_rename(path, symbol, new),
            _ => unreachable!(),
        }
    }

    fn apply(&mut self, edits: Edits) -> Result<()> {
        for (path, edits) in edits {
            let source = edited(&self.files[&path].source, edits)?;
            self.set_source(&path, source)?;
        }
        Ok(())
    }

    fn set_source(&mut self, path: &str, source: String) -> Result<()> {
        let parsed = parse::parse(path, &source)?;
        ensure!(
            parsed.errors.is_empty(),
            "Refactoring would introduce parse errors in {path}"
        );
        self.files
            .insert(path.into(), Module::new(path, source, parsed.structure)?);
        Ok(())
    }

    fn policy(&mut self, path: &str) -> Result<Option<String>> {
        if let Some(value) = self.policies.get(path) {
            return Ok(Some(value.clone()));
        }
        super::checked_path(&self.root, Path::new(path))?;
        let file = self.root.join(path);
        if !file.try_exists()? {
            return Ok(None);
        }
        let value = std::fs::read_to_string(&file)?;
        self.policies.insert(path.into(), value.clone());
        Ok(Some(value))
    }

    fn add_imports(
        &self,
        edits: &mut Edits,
        path: &str,
        offset: usize,
        imports: impl IntoIterator<Item = String>,
    ) {
        let imports: BTreeSet<_> = imports.into_iter().collect();
        if !imports.is_empty() {
            edits.entry(path.into()).or_default().push(Edit {
                range: offset..offset,
                text: format!("\n{}\n", imports.into_iter().collect::<String>()),
            });
        }
    }
}

fn valid_name(language: &str, name: &str) -> Result<()> {
    ensure!(
        super::identifier(name) && name != "_" && (language == "java" || !name.contains('$')),
        "Invalid {language} identifier: {name}"
    );
    let reserved = match language {
        "rust" => {
            "as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while abstract become box do final gen macro override priv typeof unsized virtual yield try"
        }
        "go" => {
            "break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var"
        }
        "java" => {
            "abstract assert boolean break byte case catch char class const continue default do double else enum extends final finally float for goto if implements import instanceof int interface long native new package private protected public return short static strictfp super switch synchronized this throw throws transient try void volatile while true false null _"
        }
        "c" => {
            "auto break case char const continue default do double else enum extern float for goto if inline int long register restrict return short signed sizeof static struct switch typedef union unsigned void volatile while _Alignas _Alignof _Atomic _Bool _Complex _Generic _Imaginary _Noreturn _Static_assert _Thread_local"
        }
        _ => bail!("Unsupported typed language: {language}"),
    };
    ensure!(
        !reserved.split_whitespace().any(|word| word == name),
        "Reserved {language} identifier: {name}"
    );
    Ok(())
}
