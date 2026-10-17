//! Flat file line counts, ranked largest first without folder aggregates.

use anyhow::Result;
use std::{io::Write, path::PathBuf};

pub(crate) fn print_files(
    out: &mut impl Write,
    files: impl IntoIterator<Item = (PathBuf, usize)>,
    limit: Option<usize>,
) -> Result<()> {
    let mut files = super::unique_files(files);
    files.sort_unstable_by(|(left_path, left_lines), (right_path, right_lines)| {
        right_lines
            .cmp(left_lines)
            .then_with(|| left_path.cmp(right_path))
    });
    if let Some(limit) = limit {
        files.truncate(limit);
    }
    for (path, lines) in files {
        writeln!(out, "{}  {lines} lines", path.display())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(entries: &[(&str, usize)], limit: Option<usize>) -> String {
        let mut out = Vec::new();
        print_files(
            &mut out,
            entries
                .iter()
                .map(|(path, lines)| (PathBuf::from(path), *lines)),
            limit,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn files_are_ranked_by_count_then_path_without_folder_totals() {
        let entries = [
            ("a.rs", 3),
            ("z.rs", 30),
            ("src/b.rs", 20),
            ("src/a.rs", 20),
        ];
        assert_eq!(
            render(&entries, None),
            "z.rs  30 lines\nsrc/a.rs  20 lines\nsrc/b.rs  20 lines\na.rs  3 lines\n"
        );
        assert_eq!(
            render(&entries, Some(2)),
            "z.rs  30 lines\nsrc/a.rs  20 lines\n"
        );
        let reversed: Vec<_> = entries.into_iter().rev().collect();
        assert_eq!(render(&entries, None), render(&reversed, None));
    }

    #[test]
    fn overlapping_files_are_deduplicated_before_ranking_and_limiting() {
        let entries = [("a.rs", 3), ("a.rs", 10), ("b.rs", 5), ("a.rs", 7)];
        assert_eq!(render(&entries, Some(2)), "a.rs  10 lines\nb.rs  5 lines\n");
    }

    #[test]
    fn zero_line_files_and_empty_results_respect_limits() {
        let entries = [("empty.rs", 0), ("real.rs", 1), ("blank.rs", 0)];
        let all = "real.rs  1 lines\nblank.rs  0 lines\nempty.rs  0 lines\n";
        assert_eq!(render(&entries, None), all);
        assert_eq!(render(&entries, Some(usize::MAX)), all);
        assert_eq!(render(&entries, Some(0)), "");
        for limit in [None, Some(0), Some(5)] {
            assert_eq!(render(&[], limit), "");
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_render_without_filesystem_assumptions() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let path = PathBuf::from(OsString::from_vec(b"non-utf8-\xff.rs".to_vec()));
        let mut out = Vec::new();
        print_files(&mut out, [(path.clone(), 2)], None).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{}  2 lines\n", path.display())
        );
    }
}
