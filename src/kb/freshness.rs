use super::*;

pub(super) fn reference_source<'a>(source: &'a str, reference: &Value) -> Option<&'a str> {
    match (reference.get("startLine"), reference.get("endLine")) {
        (None, None) => Some(source),
        (Some(start), Some(end)) => {
            let (start, end) = (start.as_u64()?, end.as_u64()?);
            if start == 0 || end < start {
                return None;
            }
            let mut offset = 0;
            let mut first = None;
            for (index, line) in source.split_inclusive('\n').enumerate() {
                if index as u64 + 1 == start {
                    first = Some(offset);
                }
                offset += line.len();
                if index as u64 + 1 == end {
                    return Some(&source[first?..offset]);
                }
            }
            None
        }
        _ => None,
    }
}

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
        .timeout(std::time::Duration::from_millis(50))
        .diff_lines(old, new);
    let equal = diff
        .ops()
        .iter()
        .filter(|op| op.tag() == similar::DiffTag::Equal)
        .map(|op| op.old_range().len())
        .sum();
    (equal, total)
}

fn current_reference(path: &str, old: &str, new: &str, reference: &Value) -> Option<Value> {
    let mut current = reference.clone();
    let (Some(start), Some(end)) = (
        reference["startLine"].as_u64(),
        reference["endLine"].as_u64(),
    ) else {
        return Some(current);
    };
    if let Some(symbol) = reference["symbol"].as_str() {
        let parsed = crate::parse::parse(path, new).ok()?;
        let node = parsed
            .structure
            .nodes
            .iter()
            .filter(|n| n.qualified_name == symbol)
            .min_by_key(|n| n.start_line.abs_diff(start as usize))?;
        current["startLine"] = json!(node.start_line);
        current["endLine"] = json!(node.end_line);
        return Some(current);
    }
    // Pure moves are not edits. Relocate a uniquely occurring, complete
    // line-aligned range before using the general edit-coordinate mapping.
    let selected = reference_source(old, reference)?;
    let old_lines: Vec<_> = selected.split_inclusive('\n').collect();
    let new_lines: Vec<_> = new.split_inclusive('\n').collect();
    if !old_lines.is_empty() && old_lines.len() <= new_lines.len() {
        let mut matches = new_lines
            .windows(old_lines.len())
            .enumerate()
            .filter(|(_, lines)| *lines == old_lines.as_slice());
        if let Some((index, _)) = matches.next()
            && matches.next().is_none()
        {
            current["startLine"] = json!(index + 1);
            current["endLine"] = json!(index + old_lines.len());
            return Some(current);
        }
    }
    let start = start as usize - 1;
    let end = end as usize;
    let diff = similar::TextDiff::configure()
        .timeout(std::time::Duration::from_millis(50))
        .diff_lines(old, new);
    let mut first = None;
    let mut last = 0;
    for op in diff.ops() {
        let a = op.old_range();
        let b = op.new_range();
        if op.tag() == similar::DiffTag::Equal {
            let lo = a.start.max(start);
            let hi = a.end.min(end);
            if lo < hi {
                first.get_or_insert(b.start + lo - a.start);
                last = b.start + hi - a.start;
            }
        } else if (a.start < end && a.end > start)
            || (a.is_empty() && a.start > start && a.start < end)
        {
            first.get_or_insert(b.start);
            last = b.end;
        }
    }
    let first = first?;
    if last <= first {
        return None;
    }
    current["startLine"] = json!(first + 1);
    current["endLine"] = json!(last);
    Some(current)
}

pub(super) fn check(root: &Path, article: &Article) -> Result<(Option<String>, f64, Vec<Value>)> {
    let commit = git::baseline(root, &article.path)?;
    let trusted = match &commit {
        Some(commit) => {
            git::unchanged(root, commit, &article.path)?
                && git::source(root, commit, &article.path)?
                    .is_some_and(|old| same_text(&old, &article.markdown))
        }
        None => false,
    };
    let mut kept = 0;
    let mut total = 0;
    let mut references = Vec::new();
    for reference in &article.references {
        let path = reference["path"].as_str().unwrap_or("");
        let old = match &commit {
            Some(commit) => git::source(root, commit, path)?,
            None => None,
        };
        let old_range = old
            .as_deref()
            .and_then(|old| reference_source(old, reference));
        let resolved = (|| -> Option<(Value, usize, usize)> {
            if !trusted {
                return None;
            }
            safe_path(root, Path::new(path)).ok()?;
            // Read disk even when Git's stat cache says unchanged: skip-worktree
            // and assume-unchanged flags must not conceal edits or deletions.
            let new = fs::read_to_string(root.join(path)).ok()?;
            if same_text(old.as_deref()?, &new)
                && git::unchanged(root, commit.as_deref()?, path).ok()?
            {
                let lines = old_range?.split_inclusive('\n').count().max(1);
                return Some((reference.clone(), lines, lines));
            }
            let current = current_reference(path, old.as_deref()?, &new, reference)?;
            let (kept, total) = retention(old_range?, reference_source(&new, &current)?);
            Some((current, kept, total))
        })();
        let (mut current, unchanged, lines) = resolved.unwrap_or_else(|| {
            (
                reference.clone(),
                0,
                old_range.map_or(1, |s| s.split_inclusive('\n').count().max(1)),
            )
        });
        if unchanged != lines {
            current["outdated"] = json!(true);
        }
        kept += unchanged;
        total += lines;
        references.push(current);
    }
    Ok((commit, kept as f64 / total.max(1) as f64, references))
}

/// Git checkout conversion may produce CRLF while a newly written, Git-clean
/// file still has LF. This is only used alongside Git's own unchanged check;
/// source hashes and changed-reference line comparisons retain exact bytes.
pub(super) fn same_text(old: &str, new: &str) -> bool {
    old == new || old.replace("\r\n", "\n") == new.replace("\r\n", "\n")
}
