//! Stable word normalization for symbol-name embeddings.

pub(crate) const NORMALIZATION_VERSION: &str = "symbols-v1";

/// Convert identifier spelling into lowercase, space-separated words.
pub(crate) fn normalize(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut previous: Option<char> = None;
    let mut separator = false;

    while let Some(current) = chars.next() {
        if !current.is_alphanumeric() {
            separator = !output.is_empty();
            previous = None;
            continue;
        }
        let boundary = previous.is_some_and(|previous| {
            (previous.is_lowercase() && current.is_uppercase())
                || (previous.is_uppercase()
                    && current.is_uppercase()
                    && chars.peek().is_some_and(|next| next.is_lowercase()))
                || (previous.is_alphabetic() && current.is_numeric())
                || (previous.is_numeric() && current.is_alphabetic())
        });
        if separator || boundary {
            output.push(' ');
        }
        // Some case mappings introduce combining marks (e.g. dotted capital I).
        // Keep the emitted alphabet identical to the accepted input alphabet so
        // normalization remains idempotent for stored names and query inputs.
        output.extend(current.to_lowercase().filter(|c| c.is_alphanumeric()));
        separator = false;
        previous = Some(current);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn normalize_equivalent_identifier_spellings_and_prose() {
        for input in [
            "getSomethingAndDoSomethingElse",
            "GetSomethingAndDoSomethingElse",
            "get_something_and_do_something_else",
            "get something and do something else",
        ] {
            assert_eq!(normalize(input), "get something and do something else");
        }
        for input in [
            "GLOBAL_CONSTANT_SUCH_AND_SUCH",
            "globalConstantSuchAndSuch",
            "global constant such and such",
        ] {
            assert_eq!(normalize(input), "global constant such and such");
        }
    }

    #[test]
    fn normalize_acronyms_and_digit_boundaries() {
        for (input, expected) in [
            ("HTTPServer", "http server"),
            ("parseHTTPResponse", "parse http response"),
            ("XMLHttpRequest", "xml http request"),
            ("HTTP", "http"),
            ("sha256Digest", "sha 256 digest"),
            ("HTTP2Server", "http 2 server"),
            ("version123", "version 123"),
            ("123abc", "123 abc"),
        ] {
            assert_eq!(normalize(input), expected, "{input}");
        }
    }

    #[test]
    fn normalize_separators_and_empty_names() {
        for (input, expected) in [
            ("  __foo::bar.baz/qux-zip\t\n", "foo bar baz qux zip"),
            ("foo🙂bar—baz", "foo bar baz"),
            ("", ""),
            (" \t_::.!🙂", ""),
        ] {
            assert_eq!(normalize(input), expected, "{input}");
            assert_eq!(normalize(expected), expected);
        }
    }

    #[test]
    fn normalize_unicode_case_and_words() {
        for (input, expected) in [
            ("ÉclairHTTPServer", "éclair http server"),
            ("ÜberStraße", "über straße"),
            ("ПроверитьHTTPОтвет", "проверить http ответ"),
            ("東京2猫", "東京 2 猫"),
            ("İtem", "item"),
        ] {
            assert_eq!(normalize(input), expected, "{input}");
            assert_eq!(normalize(expected), expected, "{input}");
        }
    }
}
