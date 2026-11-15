//! Freshness is scoped to the verified references, never to the search context.
use super::*;
use crate::storage::FaqMaterial;

pub(in crate::engine) const DEFAULT_FRESHNESS: f64 = 0.8;

pub(super) fn freshness_threshold(options: &Value) -> Result<f64> {
    match options.get("freshness") {
        None => Ok(DEFAULT_FRESHNESS),
        Some(value) => value
            .as_f64()
            .filter(|n| n.is_finite() && (0.0..=1.0).contains(n))
            .context("freshness must be a finite number between 0 and 1"),
    }
}

pub(in crate::engine) struct CurrentFile {
    source: String,
    structure: Option<parse::FileStructure>,
}

pub(in crate::engine) struct Freshness {
    pub fraction: f64,
    pub references: Vec<Value>,
}

/// Line-weighted retention: unchanged / max(old, new). Replacements count once;
/// insertions and deletions both reduce freshness, without normalizing source bytes.
fn retention(old: &str, new: &str) -> (usize, usize) {
    let total = old
        .split_inclusive('\n')
        .count()
        .max(new.split_inclusive('\n').count())
        .max(1);
    if old == new {
        return (total, total);
    }
    let diff = similar::TextDiff::configure()
        // Adversarial/repetitive sources must not make FAQ lookups unbounded.
        .timeout(std::time::Duration::from_millis(50))
        .diff_lines(old, new);
    let unchanged = diff
        .ops()
        .iter()
        .filter(|op| op.tag() == similar::DiffTag::Equal)
        .map(|op| op.old_range().len())
        .sum();
    (unchanged, total)
}

pub(super) fn reference_source<'a>(source: &'a str, reference: &Value) -> Option<&'a str> {
    match (
        reference["startLine"].as_u64(),
        reference["endLine"].as_u64(),
    ) {
        (None, None) => Some(source),
        (Some(start), Some(end)) if start > 0 && end >= start => {
            let mut offset = 0;
            let mut first = None;
            for (index, line) in source.split_inclusive('\n').enumerate() {
                let number = index as u64 + 1;
                if number == start {
                    first = Some(offset);
                }
                offset += line.len();
                if number == end {
                    return Some(&source[first?..offset]);
                }
            }
            None
        }
        _ => None,
    }
}

impl CurrentFile {
    fn reference(&self, material: &FaqMaterial) -> Option<Value> {
        let mut reference = material.reference.clone();
        if let Some(symbol) = reference["symbol"].as_str() {
            // Resolve against current disk bytes, even on a warm/no-reindex engine.
            // Moving an unchanged symbol is not an edit to its referenced material.
            let expected = reference["startLine"].as_u64().unwrap_or(1) as usize;
            let node = self
                .structure
                .as_ref()?
                .nodes
                .iter()
                .filter(|node| node.qualified_name == symbol)
                .min_by_key(|node| node.start_line.abs_diff(expected))?;
            reference["startLine"] = json!(node.start_line);
            reference["endLine"] = json!(node.end_line);
        }
        if reference["startLine"].is_u64()
            && reference_source(&self.source, &reference) != Some(material.source.as_str())
        {
            // Only relocate an exact, unique line-aligned match. For named
            // symbols keep it inside that symbol: newly attached leading comments
            // need not extend the previously cited range. Anonymous ranges may
            // move anywhere in the file; ambiguous matches retain the old range.
            let old: Vec<_> = material.source.split_inclusive('\n').collect();
            let new: Vec<_> = self.source.split_inclusive('\n').collect();
            if !old.is_empty() && old.len() <= new.len() {
                let bounds = reference["symbol"].as_str().map(|_| {
                    (
                        reference["startLine"].as_u64().unwrap() as usize,
                        reference["endLine"].as_u64().unwrap() as usize,
                    )
                });
                let mut matches = new.windows(old.len()).enumerate().filter(|(index, lines)| {
                    *lines == old.as_slice()
                        && bounds.is_none_or(|(start, end)| {
                            index + 1 >= start && index + old.len() <= end
                        })
                });
                if let Some((index, _)) = matches.next()
                    && matches.next().is_none()
                {
                    reference["startLine"] = json!(index + 1);
                    reference["endLine"] = json!(index + old.len());
                }
            }
        }
        Some(reference)
    }
}

impl Engine {
    pub(in crate::engine) fn faq_freshness(
        &self,
        entry: &FaqEntry,
        checked: &mut HashMap<String, Option<CurrentFile>>,
    ) -> Freshness {
        self.material_freshness(&entry.materials, &entry.result, &entry.files, checked)
    }

    pub(super) fn material_freshness(
        &self,
        materials: &[FaqMaterial],
        result: &Value,
        files: &BTreeMap<String, String>,
        checked: &mut HashMap<String, Option<CurrentFile>>,
    ) -> Freshness {
        if materials.is_empty() {
            // Older entries have only whole-file hashes. Without a baseline we
            // cannot estimate partial freshness, so retain exact validation.
            let valid = !files.is_empty() && self.faq_files_current(files, &mut HashMap::new());
            let mut references = result["references"].as_array().cloned().unwrap_or_default();
            if !valid {
                for reference in &mut references {
                    reference["outdated"] = json!(true);
                }
            }
            return Freshness {
                fraction: if valid { 1.0 } else { 0.0 },
                references,
            };
        }
        let mut unchanged = 0;
        let mut total = 0;
        let mut references = Vec::new();
        for material in materials {
            let path = material.reference["path"].as_str().unwrap_or("");
            let current = checked.entry(path.to_owned()).or_insert_with(|| {
                let absolute = self.root.join(path);
                if !fs::symlink_metadata(&absolute).ok()?.is_file()
                    || !absolute.canonicalize().ok()?.starts_with(&self.root)
                {
                    return None;
                }
                let source = read_source(&self.root, path, self.config.max_file_size()).ok()?;
                let structure = parse::parse(path, &source)
                    .ok()
                    .map(|parsed| parsed.structure);
                Some(CurrentFile { source, structure })
            });
            let resolved = current.as_ref().and_then(|file| {
                let reference = file.reference(material)?;
                let source = reference_source(&file.source, &reference)?;
                let counts = if hash(source) == material.hash {
                    let lines = material.source.split_inclusive('\n').count().max(1);
                    (lines, lines)
                } else {
                    retention(&material.source, source)
                };
                Some((reference, counts))
            });
            let (mut reference, (kept, lines)) = resolved.unwrap_or_else(|| {
                (
                    material.reference.clone(),
                    (0, material.source.split_inclusive('\n').count().max(1)),
                )
            });
            if kept != lines {
                reference["outdated"] = json!(true);
            }
            unchanged += kept;
            total += lines;
            references.push(reference);
        }
        Freshness {
            fraction: unchanged as f64 / total.max(1) as f64,
            references,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_counts_added_removed_and_replaced_lines() {
        assert_eq!(retention("a\nb\nc\nd\ne\n", "a\nb\nc\nd\nx\n"), (4, 5));
        assert_eq!(retention("a\nb\nc\nd\ne\n", "a\nb\nc\nd\n"), (4, 5));
        assert_eq!(retention("a\nb\nc\nd\n", "a\nb\nx\nc\nd\n"), (4, 5));
        assert_eq!(retention("a\n", "a\r\n"), (0, 1));
        assert_eq!(retention("", ""), (1, 1));
        assert_eq!(retention("a\n", ""), (0, 1));
    }

    #[test]
    fn ranges_preserve_exact_bytes_and_reject_invalid_bounds() {
        let source = "a\r\nb\r\nc";
        assert_eq!(
            reference_source(source, &json!({"startLine":2,"endLine":3})),
            Some("b\r\nc")
        );
        assert_eq!(reference_source(source, &json!({})), Some(source));
        for (start, end) in [(0, 1), (2, 1), (1, 4)] {
            assert_eq!(
                reference_source(source, &json!({"startLine":start,"endLine":end})),
                None
            );
        }
    }

    #[test]
    fn freshness_is_an_inclusive_fraction() {
        assert_eq!(freshness_threshold(&json!({})).unwrap(), 0.8);
        for value in [json!(0), json!(0.8), json!(1)] {
            assert!(freshness_threshold(&json!({"freshness":value})).is_ok());
        }
        for value in [json!(-0.1), json!(1.1), json!(null), json!("0.8")] {
            assert!(freshness_threshold(&json!({"freshness":value})).is_err());
        }
    }
}
