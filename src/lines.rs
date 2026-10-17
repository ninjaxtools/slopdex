//! Unique physical source-line counts, independent of presentation metadata.

mod files;
mod tree;
pub(crate) use files::print_files;
pub(crate) use tree::Tree;

use crate::{
    engine,
    filter::Selection,
    map::StructureSource,
    parse::{self, FileStructure, line_exclusions},
    ui,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    ops::Range,
    path::{Path, PathBuf},
};

pub(crate) const CACHE_KIND: &str = "line-stats";
// Bump when counting or comment/test recognition changes. Parser changes also
// invalidate these artifacts through STRUCTURE_PARSER_VERSION in the key.
const STATISTICS_VERSION: &str = "line-totals-v1";

/// Bit 0 includes blanks, bit 1 excludes comments, and bit 2 excludes tests.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Totals([usize; 8]);

impl Totals {
    pub(crate) fn compute(path: &str, source: &str) -> Result<Self> {
        let exclusions = line_exclusions::all_ranges(path, source)?;
        let mut totals = [0; 8];
        for exclude_tests in [false, true] {
            for exclude_comments in [false, true] {
                let mut excluded = Vec::new();
                if exclude_comments {
                    excluded.extend_from_slice(&exclusions.comments);
                }
                if exclude_tests {
                    excluded.extend_from_slice(&exclusions.tests);
                }
                for include_blank_lines in [false, true] {
                    let index = Self::index(include_blank_lines, exclude_comments, exclude_tests);
                    totals[index] = count_ranges(
                        source,
                        std::iter::once(0..source.len()).collect(),
                        excluded.clone(),
                        include_blank_lines,
                    );
                }
            }
        }
        Ok(Self(totals))
    }

    fn index(include_blank_lines: bool, exclude_comments: bool, exclude_tests: bool) -> usize {
        usize::from(include_blank_lines)
            | (usize::from(exclude_comments) << 1)
            | (usize::from(exclude_tests) << 2)
    }

    fn selected(&self, options: &Value) -> usize {
        self.0[Self::index(
            options["includeBlankLines"] == true,
            options["excludeComments"] == true,
            options["excludeTests"] == true,
        )]
    }
}

pub(crate) fn cache_key(language: &str, source_hash: &str, test_file: bool) -> String {
    crate::hash(
        serde_json::json!([
            STATISTICS_VERSION,
            crate::storage::STRUCTURE_PARSER_VERSION,
            language,
            source_hash,
            test_file
        ])
        .to_string(),
    )
}

pub(crate) fn unindexed(
    root: &Path,
    index: &Path,
    config: &Value,
    options: &Value,
) -> Result<Vec<(PathBuf, usize)>> {
    let selection = Selection::compile(options)?;
    let paths = selected_paths(root, options)?;
    let mut result = Vec::new();
    let max_size = config["maxFileSize"].as_u64().unwrap_or(1_048_576);
    for path in engine::source_paths(root, index, config)? {
        if !matches_path(&selection, &paths, &path)
            || options["excludeTests"] == true && line_exclusions::is_test_file(&path)
        {
            continue;
        }
        let source = match engine::read_source(root, &path, max_size) {
            Ok(source) => source,
            Err(error) => {
                if config["ignoreErrors"] != true {
                    ui::warning(format!("slopdex: {path}: {error:#}"));
                }
                continue;
            }
        };
        let parsed = if filtered(options)? {
            let parsed = parse::parse(&path, &source)?;
            if config["ignoreErrors"] != true {
                for error in &parsed.errors {
                    ui::warning(format!(
                        "slopdex: {path}:{}: {}",
                        error.start_line, error.message
                    ));
                }
            }
            Some(parsed)
        } else {
            None
        };
        if let Some(lines) = count_file(
            &path,
            &source,
            parsed.as_ref().map(|parsed| &parsed.structure),
            &selection,
            options,
        )? {
            result.push((root.join(path), lines));
        }
    }
    Ok(result)
}

pub(crate) fn indexed(
    source: &dyn StructureSource,
    root: &Path,
    options: &Value,
) -> Result<Vec<(PathBuf, usize)>> {
    let selection = source.selection(options)?;
    let paths = selected_paths(root, options)?;
    let filtered = filtered(options)?;
    let mut result = Vec::new();
    for path in source.paths()? {
        if !matches_path(&selection, &paths, &path)
            || options["excludeTests"] == true && line_exclusions::is_test_file(&path)
        {
            continue;
        }
        if !filtered && let Some(totals) = source.line_totals(&path)? {
            result.push((root.join(path), totals.selected(options)));
            continue;
        }
        let text = source.selection_source(&path)?.with_context(|| {
            format!("Source for {path} requires refresh; run lines without --no-reindex")
        })?;
        let structure = if filtered {
            source.structure(&path)?
        } else {
            None
        };
        if let Some(lines) = count_file(&path, text, structure.as_ref(), &selection, options)? {
            result.push((root.join(path), lines));
        }
    }
    Ok(result)
}

fn unique_files(files: impl IntoIterator<Item = (PathBuf, usize)>) -> Vec<(PathBuf, usize)> {
    // Overlapping positional paths can select the same file through more
    // than one workspace group. It is still one file, not extra lines.
    let mut unique = BTreeMap::<PathBuf, usize>::new();
    for (path, lines) in files {
        unique
            .entry(path)
            .and_modify(|count| *count = (*count).max(lines))
            .or_insert(lines);
    }
    unique.into_iter().collect()
}

fn selected_paths(root: &Path, options: &Value) -> Result<Vec<String>> {
    crate::filter::strings(options, "paths")?
        .into_iter()
        .map(|path| engine::normalize_path(root, path))
        .collect()
}

fn matches_path(selection: &Selection, paths: &[String], path: &str) -> bool {
    selection.path_matches(path)
        && (paths.is_empty() || paths.iter().any(|parent| engine::under(path, parent)))
}

fn filtered(options: &Value) -> Result<bool> {
    Ok(!crate::filter::strings(options, "regexp")?.is_empty()
        || !crate::filter::strings(options, "symbolQuery")?.is_empty())
}

fn count_file(
    path: &str,
    source: &str,
    structure: Option<&FileStructure>,
    selection: &Selection,
    options: &Value,
) -> Result<Option<usize>> {
    let ranges = if filtered(options)? {
        let structure = structure.context("Missing structure for filtered line count")?;
        let mut headings: Vec<_> = structure
            .nodes
            .iter()
            .filter(|node| node.kind == "heading")
            .collect();
        headings.sort_unstable_by_key(|node| node.start_byte);
        let heading_ends: HashMap<_, _> = headings
            .iter()
            .enumerate()
            .map(|(index, node)| {
                (
                    node.id,
                    headings
                        .get(index + 1)
                        .map_or(source.len(), |next| next.start_byte),
                )
            })
            .collect();
        let mut ranges = Vec::new();
        for node in &structure.nodes {
            if !selection.symbol_matches_in(path, node, Some(source)) {
                continue;
            }
            if node.kind == "heading" {
                // As in expanded map output, a heading owns only its immediate
                // body; unmatched child/sibling headings still delimit it.
                let end = heading_ends[&node.id];
                let start = node.start_byte;
                let end = source.get(start..end).map_or(end, |section| {
                    start + section.trim_end_matches(['\r', '\n']).len()
                });
                ranges.push(start..end);
            } else {
                ranges.push(node.start_byte..node.end_byte);
            }
        }
        if ranges.is_empty() {
            return Ok(None);
        }
        ranges
    } else {
        std::iter::once(0..source.len()).collect()
    };
    let excluded = line_exclusions::excluded_ranges(
        path,
        source,
        options["excludeComments"] == true,
        options["excludeTests"] == true,
    )?;
    Ok(Some(count_ranges(
        source,
        ranges,
        excluded,
        options["includeBlankLines"] == true,
    )))
}

fn merge(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_unstable_by_key(|range| (range.start, range.end));
    let mut result: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(last) = result.last_mut()
            && range.start <= last.end
        {
            last.end = last.end.max(range.end);
        } else {
            result.push(range);
        }
    }
    result
}

fn count_ranges(
    source: &str,
    ranges: Vec<Range<usize>>,
    excluded: Vec<Range<usize>>,
    include_blank_lines: bool,
) -> usize {
    let ranges = merge(ranges);
    let excluded = merge(excluded);
    if excluded.is_empty()
        && ranges.len() == 1
        && ranges[0].start == 0
        && ranges[0].end == source.len()
    {
        return source
            .lines()
            .filter(|line| include_blank_lines || !line.trim().is_empty())
            .count();
    }
    let mut range_index = 0;
    let mut exclude_index = 0;
    let mut offset = 0;
    let mut count = 0;
    for line in source.split_inclusive('\n') {
        let end = offset + line.len();
        if !include_blank_lines && line.trim().is_empty() {
            offset = end;
            continue;
        }
        while range_index < ranges.len() && ranges[range_index].end <= offset {
            range_index += 1;
        }
        if ranges
            .get(range_index)
            .is_some_and(|range| range.start < end)
        {
            if excluded.is_empty() {
                count += 1;
                offset = end;
                continue;
            }
            let mut removed = false;
            let mut remaining = false;
            let mut selected_index = range_index;
            for (index, ch) in line.char_indices() {
                let byte = offset + index;
                while selected_index < ranges.len() && ranges[selected_index].end <= byte {
                    selected_index += 1;
                }
                if !ranges
                    .get(selected_index)
                    .is_some_and(|range| range.contains(&byte))
                {
                    continue;
                }
                while exclude_index < excluded.len() && excluded[exclude_index].end <= byte {
                    exclude_index += 1;
                }
                if excluded
                    .get(exclude_index)
                    .is_some_and(|range| range.contains(&byte))
                {
                    removed = true;
                } else if !ch.is_whitespace() {
                    remaining = true;
                }
            }
            // Opting into blank lines never restores excluded block interiors.
            // A trailing comment never removes a code line.
            if remaining || include_blank_lines && !removed {
                count += 1;
            }
        }
        offset = end;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn count(path: &str, source: &str, options: Value) -> Result<Option<usize>> {
        let mut options = options;
        options["private"] = Value::Bool(true);
        let selection = Selection::compile(&options)?;
        let parsed = parse::parse(path, source)?;
        count_file(path, source, Some(&parsed.structure), &selection, &options)
    }

    #[test]
    fn cached_totals_match_range_counting_for_every_flag_combination() -> Result<()> {
        for (path, source) in [
            ("empty.rs", ""),
            ("blank.rs", "\n \t\r\n\u{a0}\u{2003}\n"),
            (
                "file.rs",
                "// header\r\n\r\nfn keep() { /* first\r\n\r\nlast */ work(); }\r\n// test docs\r\n#[test]\r\nfn check() {\r\n // comment\r\n\r\n}\r\n",
            ),
            ("mixed.rs", "fn keep() {} #[test] fn check() {} // trailing"),
            ("inner.rs", "#![cfg(test)]\n// header\nfn check() {}\n"),
            ("tests/check.rs", "// header\n\nfn helper() {}\n"),
            (
                "file.py",
                "\"\"\"Module docs.\"\"\"\n\ndef keep():\n    return 1  # trailing\n\ndef test_check():\n    \"\"\"Test docs.\"\"\"\n    # comment\n\n    assert keep()\n",
            ),
            (
                "file.ts",
                "const value = 1; /* note */\n\ndescribe('suite', () => {\n // comment\n\n test('check', () => {});\n});\n",
            ),
            (
                "file_test.go",
                "package example\n\n// header\nfunc helper() {}\n",
            ),
            (
                "guide.md",
                "# Guide\n\n<!-- comment\n\nend -->\nProse. <!-- trailing -->\n",
            ),
            ("settings.yaml", "# header\n\nvalue: 1 # trailing\n"),
        ] {
            let totals = Totals::compute(path, source)?;
            for include_blank_lines in [false, true] {
                for exclude_comments in [false, true] {
                    for exclude_tests in [false, true] {
                        let options = json!({
                            "includeBlankLines": include_blank_lines,
                            "excludeComments": exclude_comments,
                            "excludeTests": exclude_tests,
                        });
                        assert_eq!(
                            Some(totals.selected(&options)),
                            count(path, source, options.clone())?,
                            "{path}, {options}"
                        );
                    }
                }
            }
            let encoded = serde_json::to_string(&totals)?;
            assert_eq!(serde_json::from_str::<Totals>(&encoded)?, totals);
        }
        Ok(())
    }

    #[test]
    fn cache_identity_preserves_source_bytes_language_and_test_path_classification() {
        let source = crate::hash("fn code() {}\n");
        let key = cache_key("rust", &source, false);
        assert_eq!(key, cache_key("rust", &source, false));
        assert_ne!(
            key,
            cache_key("rust", &crate::hash("fn code() {}\r\n"), false)
        );
        assert_ne!(key, cache_key("typescript", &source, false));
        assert_ne!(key, cache_key("rust", &source, true));
        for invalid in [
            "[]",
            "[0,0,0,0,0,0,0]",
            "[0,0,0,0,0,0,0,0,0]",
            "[-1,0,0,0,0,0,0,0]",
        ] {
            assert!(serde_json::from_str::<Totals>(invalid).is_err());
        }
    }

    #[test]
    fn physical_lines_skip_blanks_by_default_with_an_explicit_opt_in() {
        for (source, nonblank, all) in [
            ("", 0, 0),
            ("\n", 0, 1),
            (" \t", 0, 1),
            (" \t\r\n\u{a0}\u{2003}\n", 0, 2),
            ("a", 1, 1),
            ("a\n", 1, 1),
            ("a\r\n\r\nb", 2, 3),
        ] {
            for (include, expected) in [(false, nonblank), (true, all)] {
                assert_eq!(
                    count_ranges(
                        source,
                        std::iter::once(0..source.len()).collect(),
                        vec![],
                        include
                    ),
                    expected,
                    "{source:?}, include={include}"
                );
            }
        }
    }

    #[test]
    fn whole_files_include_non_symbol_content_and_private_code() -> Result<()> {
        assert_eq!(
            count("file.rs", "// prose\n\nfn private() {}\n", json!({}))?,
            Some(2)
        );
        assert_eq!(
            count("file.md", "No headings here.\n\nMore prose.\n", json!({}))?,
            Some(2)
        );
        assert_eq!(
            count(
                "file.rs",
                "// prose\n\nfn private() {}\n",
                json!({"includeBlankLines": true})
            )?,
            Some(3)
        );
        Ok(())
    }

    #[test]
    fn symbols_count_full_source_once_without_unmatched_ancestor_bodies() -> Result<()> {
        let source = "struct Item;\nimpl Item {\n    fn wanted() {\n        let x = 1;\n    }\n    fn other() {}\n}\n";
        assert_eq!(
            count("file.rs", source, json!({"regexp": "wanted"}))?,
            Some(3)
        );
        assert_eq!(
            count("file.rs", source, json!({"regexp": "missing"}))?,
            None
        );
        assert_eq!(
            count("file.rs", source, json!({"regexp": ["Item", "wanted"]}))?,
            Some(7)
        );
        Ok(())
    }

    #[test]
    fn heading_sections_stop_before_unmatched_children() -> Result<()> {
        let source = "# Guide\n\nParent body.\n\n## Child\n\nChild body.\n";
        for (include, expected) in [(false, 2), (true, 3)] {
            for pattern in ["^Guide$", "Child$"] {
                assert_eq!(
                    count(
                        "file.md",
                        source,
                        json!({"regexp": pattern, "includeBlankLines": include})
                    )?,
                    Some(expected)
                );
            }
        }
        Ok(())
    }

    #[test]
    fn exclusions_preserve_mixed_code_and_remove_multiline_blank_interiors() {
        let source = "code /* comment\n\ncomment */ code\n// comment\n\n";
        let start = source.find("/*").unwrap();
        let end = source.find("*/").unwrap() + 2;
        let line = source.find("//").unwrap();
        for (include, expected) in [(false, 2), (true, 3)] {
            assert_eq!(
                count_ranges(
                    source,
                    std::iter::once(0..source.len()).collect(),
                    vec![start..end, line..source.len() - 2],
                    include
                ),
                expected
            );
        }
    }

    #[test]
    fn blank_line_opt_in_applies_to_filtered_code_and_respects_exclusions() -> Result<()> {
        let source = "fn wanted() {\n \t\n // comment\n let x = 1; // trailing\n}\n\n#[test]\nfn check() {\n\n}\n";
        for (include, selected, uncommented, production) in [(false, 4, 3, 3), (true, 5, 4, 5)] {
            assert_eq!(
                count(
                    "file.rs",
                    source,
                    json!({"regexp": "wanted", "includeBlankLines": include})
                )?,
                Some(selected)
            );
            assert_eq!(
                count(
                    "file.rs",
                    source,
                    json!({"regexp": "wanted", "includeBlankLines": include, "excludeComments": true})
                )?,
                Some(uncommented)
            );
            assert_eq!(
                count(
                    "file.rs",
                    source,
                    json!({"includeBlankLines": include, "excludeComments": true, "excludeTests": true})
                )?,
                Some(production)
            );
        }
        Ok(())
    }

    #[test]
    fn selection_does_not_apply_the_map_declaration_limit() -> Result<()> {
        let source = (0..4100)
            .map(|index| format!("fn selected_{index}() {{}}\n"))
            .collect::<String>();
        assert_eq!(
            count(
                "file.rs",
                &source,
                json!({"regexp": "selected", "limit": 1})
            )?,
            Some(4100)
        );
        Ok(())
    }

    #[test]
    fn unrelated_code_on_the_same_line_cannot_preserve_an_excluded_match() -> Result<()> {
        let source = "fn keep() {} #[test] fn check() {}\n";
        assert_eq!(
            count(
                "file.rs",
                source,
                json!({"regexp": "check", "excludeTests": true})
            )?,
            Some(0)
        );
        assert_eq!(
            count("file.rs", source, json!({"excludeTests": true}))?,
            Some(1)
        );
        Ok(())
    }

    #[test]
    fn semantic_matches_union_with_regex_without_counting_context() -> Result<()> {
        let source =
            "impl Item {\n fn north() {\n  work();\n }\n fn west() {}\n fn ordinary() {}\n}\n";
        let parsed = parse::parse("file.rs", source)?;
        let options = json!({"regexp": "west", "symbolQuery": "north", "private": true});
        let selection = Selection::compile(&options)?
            .with_symbol_names(["north".to_owned()].into_iter().collect());
        assert_eq!(
            count_file(
                "file.rs",
                source,
                Some(&parsed.structure),
                &selection,
                &options
            )?,
            Some(4)
        );
        Ok(())
    }
}
