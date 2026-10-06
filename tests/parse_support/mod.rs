use slopdex::parse::{ParsedFile, StructureNode, parse};

pub fn clean(path: &str, source: &str) -> ParsedFile {
    let parsed = parse(path, source).unwrap();
    assert!(parsed.errors.is_empty(), "{path}: {:?}", parsed.errors);
    for node in &parsed.structure.nodes {
        assert_eq!(
            node.start_byte,
            byte_at(source, node.start_line, node.start_column)
        );
        assert_eq!(
            node.end_byte,
            byte_at(source, node.end_line, node.end_column)
        );
        assert!(source.get(node.start_byte..node.end_byte).is_some());
    }
    for callable in &parsed.callables {
        let start = byte_at(source, callable.start_line, callable.start_column);
        let end = byte_at(source, callable.end_line, callable.end_column);
        assert_eq!(source.get(start..end), Some(callable.source.as_str()));
    }
    parsed
}

pub fn declarations(parsed: &ParsedFile) -> Vec<(&str, &str)> {
    parsed
        .structure
        .nodes
        .iter()
        .map(|node| (node.qualified_name.as_str(), node.kind.as_str()))
        .collect()
}

pub fn callables(parsed: &ParsedFile) -> Vec<(&str, &str)> {
    parsed
        .callables
        .iter()
        .map(|node| (node.qualified_name.as_str(), node.kind.as_str()))
        .collect()
}

pub fn named<'a>(parsed: &'a ParsedFile, name: &str) -> &'a StructureNode {
    parsed
        .structure
        .nodes
        .iter()
        .find(|node| node.qualified_name == name)
        .unwrap_or_else(|| panic!("missing {name}: {:?}", declarations(parsed)))
}

pub fn byte_at(source: &str, line: usize, column: usize) -> usize {
    source
        .split_inclusive('\n')
        .take(line - 1)
        .map(str::len)
        .sum::<usize>()
        + column
        - 1
}
