//! Line-count trees. JSON is a recursive root node containing `path`, `name`,
//! `lines`, `type` (`directory` or `file`), `selected`, and `children`. The root
//! is structural (`selected: false`); other unselected nodes are ancestor context.
//! Counts always describe the full input, even when children have been pruned.
//! Empty input has an empty root path, the name `(no files)`, and zero lines.

use anyhow::Result;
use serde::{Serialize, Serializer};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct Tree {
    root: Node,
}

#[derive(Debug, Serialize)]
struct Node {
    #[serde(serialize_with = "serialize_path")]
    path: PathBuf,
    name: String,
    lines: usize,
    #[serde(rename = "type")]
    kind: Kind,
    selected: bool,
    children: Vec<Node>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Directory,
    File,
}

#[derive(Default)]
struct Branch {
    lines: usize,
    directory: bool,
    children: BTreeMap<OsString, Branch>,
}

impl Tree {
    pub(crate) fn new(
        files: impl IntoIterator<Item = (PathBuf, usize)>,
        limit: Option<usize>,
    ) -> Self {
        // Overlapping positional paths can select the same file through more
        // than one workspace group. It is still one file, not extra lines.
        let mut unique = BTreeMap::<PathBuf, usize>::new();
        for (path, lines) in files {
            unique
                .entry(path)
                .and_modify(|count| *count = (*count).max(lines))
                .or_insert(lines);
        }
        let files: Vec<_> = unique.into_iter().collect();
        let path = common_parent(&files);
        let name = if files.is_empty() {
            "(no files)".to_owned()
        } else if path.as_os_str().is_empty() {
            "(files)".to_owned()
        } else {
            path.file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned()
        };
        let mut branch = Branch {
            directory: true,
            ..Branch::default()
        };
        for (file, lines) in files {
            // The common parent is a component prefix, not a string prefix.
            let relative = file.strip_prefix(&path).unwrap_or(&file);
            branch.insert(relative, lines);
        }
        let mut root = branch.into_node(path, name);
        root.selected = false;
        if let Some(limit) = limit {
            let selected = {
                let mut candidates = Vec::new();
                for child in &root.children {
                    child.candidates(&mut candidates);
                }
                candidates.sort_unstable_by(|left, right| {
                    right
                        .lines
                        .cmp(&left.lines)
                        .then_with(|| left.path.cmp(&right.path))
                });
                candidates
                    .into_iter()
                    .take(limit)
                    .map(|node| node.path.clone())
                    .collect()
            };
            root.retain(&selected);
        }
        Self { root }
    }

    pub(crate) fn print(&self, out: &mut impl Write) -> Result<()> {
        let label = if self.root.path.as_os_str().is_empty() {
            self.root.name.clone()
        } else {
            self.root.path.display().to_string()
        };
        writeln!(out, "{label}  {} lines", self.root.lines)?;
        self.root.print_children(out, "")
    }
}

fn common_parent(files: &[(PathBuf, usize)]) -> PathBuf {
    let Some((first, _)) = files.first() else {
        return PathBuf::new();
    };
    let mut common = first.parent().unwrap_or(Path::new("")).to_path_buf();
    for (file, _) in files.iter().skip(1) {
        let parent = file.parent().unwrap_or(Path::new(""));
        while !parent.starts_with(&common) {
            if !common.pop() {
                // There is no common absolute root across Windows volumes.
                common.clear();
                break;
            }
        }
    }
    common
}

impl Branch {
    fn insert(&mut self, path: &Path, lines: usize) {
        let mut branch = self;
        for component in path.components() {
            branch.lines = branch.lines.saturating_add(lines);
            branch.directory = true;
            branch = branch
                .children
                .entry(component.as_os_str().to_owned())
                .or_default();
        }
        branch.lines = branch.lines.saturating_add(lines);
    }

    fn into_node(self, path: PathBuf, name: String) -> Node {
        let children = self
            .children
            .into_iter()
            .map(|(name, branch)| {
                let child_path = path.join(&name);
                branch.into_node(child_path, name.to_string_lossy().into_owned())
            })
            .collect();
        Node {
            path,
            name,
            lines: self.lines,
            kind: if self.directory {
                Kind::Directory
            } else {
                Kind::File
            },
            selected: true,
            children,
        }
    }
}

impl Node {
    fn candidates<'a>(&'a self, candidates: &mut Vec<&'a Node>) {
        candidates.push(self);
        for child in &self.children {
            child.candidates(candidates);
        }
    }

    fn retain(&mut self, selected: &BTreeSet<PathBuf>) -> bool {
        self.selected = selected.contains(&self.path);
        self.children.retain_mut(|child| child.retain(selected));
        self.selected || !self.children.is_empty()
    }

    fn print_children(&self, out: &mut impl Write, prefix: &str) -> Result<()> {
        for (index, child) in self.children.iter().enumerate() {
            let last = index + 1 == self.children.len();
            let connector = if last { "└── " } else { "├── " };
            let suffix = if child.kind == Kind::Directory {
                "/"
            } else {
                ""
            };
            let context = if child.selected { "" } else { " (context)" };
            writeln!(
                out,
                "{prefix}{connector}{}{suffix}  {} lines{context}",
                child.name, child.lines,
            )?;
            if !child.children.is_empty() {
                let next = format!("{prefix}{}", if last { "    " } else { "│   " });
                child.print_children(out, &next)?;
            }
        }
        Ok(())
    }
}

fn serialize_path<S: Serializer>(
    path: &Path,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    // Match text output, including paths containing non-UTF-8 OS strings.
    serializer.collect_str(&path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PathBuf {
        std::env::current_dir()
            .unwrap()
            .join("line-tree-test-fixture")
    }

    fn files(entries: &[(&str, usize)]) -> Vec<(PathBuf, usize)> {
        let base = base();
        entries
            .iter()
            .map(|(path, lines)| (base.join(path), *lines))
            .collect()
    }

    fn child<'a>(node: &'a Node, name: &str) -> &'a Node {
        node.children.iter().find(|node| node.name == name).unwrap()
    }

    fn render(tree: &Tree) -> String {
        let mut out = Vec::new();
        tree.print(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn nested_aggregation_and_alphabetical_tree() {
        let tree = Tree::new(
            files(&[
                ("tests/check.rs", 7),
                ("src/nested/child.rs", 20),
                ("README.md", 5),
                ("src/lib.rs", 10),
            ]),
            None,
        );
        assert_eq!(tree.root.path, base());
        assert_eq!(tree.root.lines, 42);
        assert_eq!(child(&tree.root, "src").lines, 30);
        assert_eq!(child(child(&tree.root, "src"), "nested").lines, 20);
        assert!(!tree.root.selected);
        assert_eq!(
            render(&tree),
            format!(
                concat!(
                    "{}  42 lines\n",
                    "├── README.md  5 lines\n",
                    "├── src/  30 lines\n",
                    "│   ├── lib.rs  10 lines\n",
                    "│   └── nested/  20 lines\n",
                    "│       └── child.rs  20 lines\n",
                    "└── tests/  7 lines\n",
                    "    └── check.rs  7 lines\n",
                ),
                base().display(),
            ),
        );
    }

    #[test]
    fn common_root_is_the_deepest_shared_parent() {
        let tree = Tree::new(
            files(&[("nested/left/a.rs", 2), ("nested/right/b.rs", 3)]),
            None,
        );
        assert_eq!(tree.root.path, base().join("nested"));
        assert_eq!(tree.root.lines, 5);
        assert_eq!(tree.root.children.len(), 2);
        assert_eq!(tree.root.children[0].name, "left");
        assert_eq!(tree.root.children[1].name, "right");
    }

    #[test]
    fn a_single_file_uses_its_parent_as_the_root() {
        let tree = Tree::new(files(&[("nested/only.rs", 9)]), Some(1));
        assert_eq!(tree.root.path, base().join("nested"));
        assert_eq!(tree.root.kind, Kind::Directory);
        assert_eq!(tree.root.lines, 9);
        let file = &tree.root.children[0];
        assert_eq!(file.name, "only.rs");
        assert_eq!(file.kind, Kind::File);
        assert_eq!(file.lines, 9);
        assert!(file.selected);
    }

    #[test]
    fn overlapping_inputs_do_not_duplicate_file_or_folder_counts() {
        let tree = Tree::new(files(&[("nested/file.rs", 3), ("nested/file.rs", 3)]), None);
        assert_eq!(tree.root.lines, 3);
        assert_eq!(tree.root.children.len(), 1);
        assert_eq!(tree.root.children[0].lines, 3);
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_components_support_drive_verbatim_and_mixed_separators() {
        for (first, second, parent) in [
            (
                r"C:\workspace\src\a.rs",
                r"C:\workspace\docs\b.md",
                r"C:\workspace",
            ),
            (
                r"\\?\C:\workspace\src\a.rs",
                r"\\?\C:\workspace\docs\b.md",
                r"\\?\C:\workspace",
            ),
            (
                r"C:\workspace/src\a.rs",
                r"C:/workspace\docs/b.md",
                r"C:\workspace",
            ),
        ] {
            let tree = Tree::new(
                [(PathBuf::from(first), 2), (PathBuf::from(second), 3)],
                None,
            );
            assert_eq!(tree.root.path, PathBuf::from(parent));
            assert_eq!(tree.root.lines, 5);
            assert_eq!(child(child(&tree.root, "src"), "a.rs").lines, 2);
            assert_eq!(child(child(&tree.root, "docs"), "b.md").lines, 3);
        }
    }

    #[test]
    fn empty_input_is_a_readable_zero_tree() {
        for limit in [None, Some(0), Some(5)] {
            let tree = Tree::new([], limit);
            assert_eq!(render(&tree), "(no files)  0 lines\n");
            assert_eq!(tree.root.lines, 0);
            assert!(tree.root.children.is_empty());
            let json = serde_json::to_value(&tree).unwrap();
            assert_eq!(json["lines"], 0);
            assert_eq!(json["type"], "directory");
            assert_eq!(json["children"], serde_json::json!([]));
        }
    }

    #[test]
    fn limit_ranks_files_and_folders_together_without_expanding_folders() {
        let input = files(&[
            ("large.rs", 90),
            ("src/heavy.rs", 80),
            ("src/small.rs", 30),
            ("tests/test.rs", 100),
        ]);
        let tree = Tree::new(input.clone(), Some(2));
        assert_eq!(tree.root.lines, 300);
        assert_eq!(tree.root.children.len(), 2);
        assert_eq!(tree.root.children[0].name, "src");
        assert_eq!(tree.root.children[0].lines, 110);
        assert_eq!(tree.root.children[1].name, "tests");
        assert_eq!(tree.root.children[1].lines, 100);
        assert!(tree.root.children.iter().all(|node| node.selected));
        assert!(
            tree.root
                .children
                .iter()
                .all(|node| node.children.is_empty())
        );

        // A selected file remains under its directory, not flattened beside it.
        let tree = Tree::new(input, Some(4));
        assert_eq!(tree.root.lines, 300);
        assert_eq!(child(&tree.root, "large.rs").lines, 90);
        assert!(child(&tree.root, "src").children.is_empty());
        let tests = child(&tree.root, "tests");
        assert_eq!(tests.children.len(), 1);
        assert_eq!(tests.children[0].name, "test.rs");
        assert!(tests.children[0].selected);
    }

    #[test]
    fn ancestor_context_preserves_full_counts_and_omits_unselected_siblings() {
        let mut tree = Tree::new(
            files(&[
                ("root.rs", 4),
                ("src/large.rs", 30),
                ("src/nested/chosen.rs", 10),
                ("src/nested/sibling.rs", 7),
            ]),
            None,
        );
        // Exercise context retention independently of ranking: nonnegative
        // counts normally rank every ancestor ahead of its descendants.
        tree.root
            .retain(&BTreeSet::from([base().join("src/nested/chosen.rs")]));
        assert_eq!(tree.root.lines, 51);
        assert_eq!(tree.root.children.len(), 1);
        let src = &tree.root.children[0];
        assert_eq!(src.lines, 47);
        assert!(!src.selected);
        assert_eq!(src.children.len(), 1);
        let nested = &src.children[0];
        assert_eq!(nested.lines, 17);
        assert!(!nested.selected);
        assert_eq!(nested.children.len(), 1);
        assert_eq!(nested.children[0].name, "chosen.rs");
        assert!(nested.children[0].selected);
        assert!(render(&tree).contains("src/  47 lines (context)"));
        let json = serde_json::to_value(&tree).unwrap();
        assert_eq!(json["children"][0]["selected"], false);
        assert_eq!(json["children"][0]["lines"], 47);
        assert_eq!(
            json["children"][0]["children"][0]["children"][0]["selected"],
            true,
        );
    }

    #[test]
    fn ranking_ties_use_paths_and_output_stays_alphabetical() {
        let entries = [("z.rs", 10), ("c/child.rs", 10), ("b.rs", 10), ("a.rs", 10)];
        let tree = Tree::new(files(&entries), Some(3));
        assert_eq!(tree.root.children.len(), 3);
        assert_eq!(tree.root.children[0].name, "a.rs");
        assert_eq!(tree.root.children[1].name, "b.rs");
        assert_eq!(tree.root.children[2].name, "c");
        assert!(tree.root.children[2].children.is_empty());
        let reversed: Vec<_> = files(&entries).into_iter().rev().collect();
        assert_eq!(render(&tree), render(&Tree::new(reversed, Some(3))));

        let tree = Tree::new(files(&[("a.rs", 1), ("z.rs", 100)]), Some(2));
        assert_eq!(tree.root.children[0].name, "a.rs");
        assert_eq!(tree.root.children[1].name, "z.rs");
    }

    #[test]
    fn zero_line_files_survive_default_and_large_limits() {
        let input = files(&[("empty.rs", 0), ("nested/zero.rs", 0), ("real.rs", 3)]);
        let all = Tree::new(input.clone(), None);
        assert_eq!(all.root.lines, 3);
        assert_eq!(child(&all.root, "empty.rs").kind, Kind::File);
        assert_eq!(child(&all.root, "empty.rs").lines, 0);
        assert_eq!(child(child(&all.root, "nested"), "zero.rs").lines, 0);
        assert_eq!(
            render(&all),
            render(&Tree::new(input.clone(), Some(usize::MAX))),
        );
        let limited = Tree::new(input, Some(2));
        assert_eq!(limited.root.children.len(), 2);
        assert_eq!(limited.root.children[0].name, "empty.rs");
        assert_eq!(limited.root.children[1].name, "real.rs");
    }

    #[test]
    fn a_zero_limit_keeps_only_the_full_root_total() {
        let tree = Tree::new(files(&[("a.rs", 1), ("src/b.rs", 2)]), Some(0));
        assert_eq!(tree.root.path, base());
        assert_eq!(tree.root.lines, 3);
        assert!(tree.root.children.is_empty());
        assert_eq!(render(&tree), format!("{}  3 lines\n", base().display()));
    }

    #[test]
    fn json_is_recursive_and_distinguishes_structural_and_selected_nodes() {
        let tree = Tree::new(files(&[("root.rs", 2), ("src/lib.rs", 8)]), Some(1));
        let json = serde_json::to_value(tree).unwrap();
        assert!(json["path"].is_string());
        assert!(json["name"].is_string());
        assert_eq!(json["lines"], 10);
        assert_eq!(json["type"], "directory");
        assert_eq!(json["selected"], false);
        let children = json["children"].as_array().unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0]["name"], "src");
        assert_eq!(children[0]["lines"], 8);
        assert_eq!(children[0]["type"], "directory");
        assert_eq!(children[0]["selected"], true);
        assert_eq!(children[0]["children"], serde_json::json!([]));

        let json = serde_json::to_value(Tree::new(files(&[("file.rs", 1)]), None)).unwrap();
        assert_eq!(json["children"][0]["type"], "file");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_render_and_serialize_without_filesystem_assumptions() {
        use std::os::unix::ffi::OsStringExt;

        let name = OsString::from_vec(b"non-utf8-\xff.rs".to_vec());
        let tree = Tree::new([(base().join(&name), 2)], None);
        let json = serde_json::to_value(&tree).unwrap();
        assert!(json["children"][0]["path"].is_string());
        assert_eq!(json["children"][0]["name"], name.to_string_lossy().as_ref());
        assert!(render(&tree).contains(name.to_string_lossy().as_ref()));
    }
}
