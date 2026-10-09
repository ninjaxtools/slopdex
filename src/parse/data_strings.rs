//! Decode keys using their document's quoting rules, rather than JSON's rules.

use super::super::syntax::text;
use super::document::children;
use tree_sitter::Node;

pub(super) fn decode(language: &str, value: &str) -> String {
    let value = value.trim();
    if language == "json" {
        return serde_json::from_str(value).unwrap_or_else(|_| value.to_owned());
    }
    if let Some(inner) = value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return if language == "yaml" {
            fold_yaml(&inner.replace("''", "'"))
        } else {
            inner.to_owned()
        };
    }
    let Some(inner) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return if language == "yaml" {
            fold_yaml(value)
        } else {
            value.to_owned()
        };
    };
    let normalized = inner.replace("\r\n", "\n");
    let mut chars = normalized.chars().peekable();
    let mut result = String::new();
    let mut raw_whitespace = 0;
    while let Some(c) = chars.next() {
        if c == '\n' && language == "yaml" {
            // Only physical trailing whitespace is folded away. Whitespace
            // decoded from \x20, \t, etc. is scalar content, not indentation.
            result.truncate(result.len() - raw_whitespace);
            fold_break(&mut result, &mut chars);
            raw_whitespace = 0;
            continue;
        }
        if c != '\\' {
            result.push(c);
            raw_whitespace = if matches!(c, ' ' | '\t') {
                raw_whitespace + 1
            } else {
                0
            };
            continue;
        }
        raw_whitespace = 0;
        let Some(escape) = chars.next() else {
            result.push('\\');
            break;
        };
        let decoded = match escape {
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            '"' => Some('"'),
            '\\' => Some('\\'),
            'b' if language != "terraform" => Some('\u{8}'),
            'f' if language != "terraform" => Some('\u{c}'),
            '0' if language == "yaml" => Some('\0'),
            'a' if language == "yaml" => Some('\u{7}'),
            'v' if language == "yaml" => Some('\u{b}'),
            'e' if language == "yaml" => Some('\u{1b}'),
            ' ' | '/' if language == "yaml" => Some(escape),
            'N' if language == "yaml" => Some('\u{85}'),
            '_' if language == "yaml" => Some('\u{a0}'),
            'L' if language == "yaml" => Some('\u{2028}'),
            'P' if language == "yaml" => Some('\u{2029}'),
            'x' | 'u' | 'U' if escape != 'x' || language == "yaml" => {
                let count = match escape {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let digits: String = chars.by_ref().take(count).collect();
                if let Some(c) = u32::from_str_radix(&digits, 16)
                    .ok()
                    .and_then(char::from_u32)
                {
                    result.push(c);
                } else {
                    result.push('\\');
                    result.push(escape);
                    result.push_str(&digits);
                }
                continue;
            }
            '\n' if language == "yaml" => {
                while chars.peek().is_some_and(|c| matches!(c, ' ' | '\t')) {
                    chars.next();
                }
                continue;
            }
            _ => None,
        };
        if let Some(c) = decoded {
            result.push(c);
        } else {
            result.push('\\');
            result.push(escape);
        }
    }
    if language == "terraform" {
        result = result.replace("$${", "${").replace("%%{", "%{");
    }
    result
}

fn fold_yaml(value: &str) -> String {
    let normalized = value.replace("\r\n", "\n");
    let mut chars = normalized.chars().peekable();
    let mut result = String::new();
    while let Some(c) = chars.next() {
        if c == '\n' {
            result.truncate(result.trim_end_matches([' ', '\t']).len());
            fold_break(&mut result, &mut chars);
        } else {
            result.push(c);
        }
    }
    result
}

fn fold_break(result: &mut String, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let mut blank = 0;
    while let Some(c) = chars.peek() {
        match c {
            ' ' | '\t' => {
                chars.next();
            }
            '\n' => {
                chars.next();
                blank += 1;
            }
            _ => break,
        }
    }
    if blank == 0 {
        result.push(' ');
    } else {
        result.extend(std::iter::repeat_n('\n', blank));
    }
}

pub(super) fn yaml_key(source: &str, mut node: Node<'_>) -> String {
    while matches!(node.kind(), "flow_node" | "block_node" | "plain_scalar") {
        let Some(child) = children(node)
            .into_iter()
            .find(|child| !matches!(child.kind(), "tag" | "anchor" | "comment"))
        else {
            break;
        };
        node = child;
    }
    decode("yaml", text(source, node))
}

pub(super) fn toml_key(source: &str, node: Node<'_>) -> Vec<String> {
    if node.kind() == "dotted_key" {
        children(node)
            .into_iter()
            .filter(|child| child.kind() != "comment")
            .flat_map(|child| toml_key(source, child))
            .collect()
    } else {
        vec![decode("toml", text(source, node))]
    }
}

pub(super) fn hcl_key(source: &str, mut node: Node<'_>) -> String {
    while matches!(
        node.kind(),
        "expression" | "literal_value" | "variable_expr"
    ) && node.named_child_count() == 1
    {
        // Parentheses turn a bare HCL object key into an evaluated expression.
        if text(source, node).trim_start().starts_with('(') {
            break;
        }
        node = node.named_child(0).unwrap();
    }
    decode("terraform", text(source, node))
}
