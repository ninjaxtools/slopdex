//! Offset-preserving document utilities shared by the data and Markdown parsers.

use std::ops::Range;
use tree_sitter::{Node, Point};

pub(in crate::parse) fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

pub(in crate::parse) struct Positions {
    lines: Vec<usize>,
}

impl Positions {
    pub(in crate::parse) fn new(source: &str) -> Self {
        let mut lines = vec![0];
        lines.extend(source.match_indices('\n').map(|(byte, _)| byte + 1));
        Self { lines }
    }

    pub(in crate::parse) fn point(&self, byte: usize) -> Point {
        let row = self.lines.partition_point(|start| *start <= byte) - 1;
        Point::new(row, byte - self.lines[row])
    }

    pub(in crate::parse) fn line_start(&self, row: usize) -> usize {
        self.lines[row]
    }
}

/// Filter search text only. Structural parsing always uses the original source
/// or a byte-for-byte-length repair; deleting a comment never changes line IDs.
pub(in crate::parse) fn without_spans(source: &str, spans: &mut [Range<usize>]) -> String {
    spans.sort_by_key(|range| range.start);
    let mut result = String::with_capacity(source.len());
    let mut end = 0;
    for range in spans {
        let start = range.start.max(end);
        if range.end <= start {
            continue;
        }
        result.push_str(&source[end..start]);
        result.extend(
            source[start..range.end]
                .chars()
                .filter(|c| matches!(c, '\r' | '\n')),
        );
        end = range.end;
    }
    result.push_str(&source[end..]);
    result
}

pub(in crate::parse) fn clean_range(
    source: &str,
    range: Range<usize>,
    spans: &[Range<usize>],
) -> String {
    let mut relative: Vec<_> = spans
        .iter()
        .filter_map(|span| {
            let start = span.start.max(range.start);
            let end = span.end.min(range.end);
            if start < end {
                Some(start - range.start..end - range.start)
            } else {
                None
            }
        })
        .collect();
    without_spans(&source[range], &mut relative)
}
