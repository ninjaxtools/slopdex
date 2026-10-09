mod parse_support;

use parse_support::{callables, clean, named};
use slopdex::parse::{ParsedFile, parse};

#[test]
fn descriptions_cover_every_supported_language() {
    for (path, comment, declaration, name) in [
        (
            "source.ts",
            "// Description.",
            "export function run() {}",
            "run",
        ),
        (
            "source.tsx",
            "/** Description. */",
            "const run = () => <div />;",
            "run",
        ),
        ("source.js", "// Description.", "function run() {}", "run"),
        (
            "source.jsx",
            "/** Description. */",
            "const run = () => <div />;",
            "run",
        ),
        ("source.py", "# Description.", "def run(): pass", "run"),
        ("source.rs", "/// Description.", "pub fn run() {}", "run"),
        ("source.go", "// Description.", "func run() {}", "run"),
        ("source.java", "/** Description. */", "class Run {}", "Run"),
        (
            "source.c",
            "/* Description. */",
            "int run(void) { return 0; }",
            "run",
        ),
        ("source.sh", "# Description.", "run() { :; }", "run"),
        ("source.md", "<!-- Description. -->", "# Run", "Run"),
        ("source.json", "// Description.", "{\"run\": 1}", "run"),
        ("source.tf", "# Description.", "run = 1", "run"),
        ("source.yaml", "# Description.", "run: 1", "run"),
        ("source.toml", "# Description.", "run = 1", "run"),
        ("source.xml", "<!-- Description. -->", "<run/>", "run"),
        ("source.html", "<!-- Description. -->", "<run></run>", "run"),
        (
            "source.css",
            "/* Description. */",
            ".run { color: red; }",
            ".run",
        ),
    ] {
        let source = format!("\n \t\n{comment}\n{declaration}\n");
        let parsed = clean(path, &source);
        assert_eq!(
            parsed.description.as_deref(),
            Some("Description."),
            "{path}"
        );
        // JSON's comment precedes the root object, not its member. Only a
        // comment next to a member describes that member (tested separately).
        if path != "source.json" {
            assert_eq!(
                named(&parsed, name).description.as_deref(),
                Some("Description."),
                "{path}"
            );
        }
        for callable in &parsed.callables {
            assert_eq!(
                callable.description.as_deref(),
                Some("Description."),
                "{path}"
            );
        }
    }
}

#[test]
fn declaration_kinds_and_wrappers_share_the_association_rules() {
    let parsed = clean(
        "source.ts",
        r#"/** Public API. */
export interface API {
  // Current state.
  state: string;
  // Fetch state.
  fetch(): string;
}
// Maximum size.
export const MAX = 10, MIN = 0;
// Mutable state.
let state = 0;
// An alias.
type Alias = API;
// A callable binding.
export const run = () => state;
"#,
    );
    for (name, description) in [
        ("API", "Public API."),
        ("API.state", "Current state."),
        ("API.fetch", "Fetch state."),
        ("MAX", "Maximum size."),
        ("MIN", "Maximum size."),
        ("state", "Mutable state."),
        ("Alias", "An alias."),
        ("run", "A callable binding."),
    ] {
        assert_eq!(
            named(&parsed, name).description.as_deref(),
            Some(description),
            "{name}"
        );
    }
    assert_eq!(
        parsed.callables[0].description.as_deref(),
        Some("A callable binding.")
    );

    let parsed = clean(
        "source.rs",
        "/// Service.\n#[derive(Debug)]\npub struct Service {\n    /// Value.\n    pub value: i32,\n}\n/// Run.\n#[inline]\npub fn run() {}\n",
    );
    assert_eq!(
        named(&parsed, "Service").description.as_deref(),
        Some("Service.")
    );
    assert_eq!(
        named(&parsed, "Service.value").description.as_deref(),
        Some("Value.")
    );
    assert_eq!(parsed.callables[0].description.as_deref(), Some("Run."));
}

#[test]
fn native_types_constants_fields_and_grouped_declarations_get_descriptions() {
    for (path, source, expected) in [
        (
            "source.py",
            "# A type.\nclass Kind:\n    # A field.\n    value = 1\n# A constant.\nMAX = 2\n",
            vec![
                ("Kind", "A type."),
                ("Kind.value", "A field."),
                ("MAX", "A constant."),
            ],
        ),
        (
            "source.rs",
            "//! File header.\n\n/// A type.\npub type Kind = i32;\n/// A constant.\npub const MAX: i32 = 2;\n/// A variable.\npub static STATE: i32 = 0;\n",
            vec![
                ("Kind", "A type."),
                ("MAX", "A constant."),
                ("STATE", "A variable."),
            ],
        ),
        (
            "source.go",
            "// A type.\ntype Kind int\n// A constant.\nconst MAX = 2\n// A variable.\nvar state = 0\nvar (\n  // A grouped variable.\n  grouped = 1\n  bare = 2\n)\n",
            vec![
                ("Kind", "A type."),
                ("MAX", "A constant."),
                ("state", "A variable."),
                ("grouped", "A grouped variable."),
            ],
        ),
        (
            "source.java",
            "/** A type. */\nclass Kind {\n  /** A constant. */\n  static final int MAX = 2;\n  /** A field. */\n  int value = 0;\n  /** A method. */\n  @Deprecated\n  void run() {}\n}\n",
            vec![
                ("Kind", "A type."),
                ("Kind.MAX", "A constant."),
                ("Kind.value", "A field."),
                ("Kind.run", "A method."),
            ],
        ),
        (
            "source.c",
            "/* A type. */\ntypedef struct Kind {\n  /* A field. */\n  int value;\n} Alias;\n/* Variables. */\nint left = 0, right = 1;\n",
            vec![
                ("Kind", "A type."),
                ("Kind.value", "A field."),
                ("Alias", "A type."),
                ("left", "Variables."),
                ("right", "Variables."),
            ],
        ),
    ] {
        let parsed = clean(path, source);
        for (name, description) in expected {
            assert_eq!(
                named(&parsed, name).description.as_deref(),
                Some(description),
                "{path}: {name}"
            );
        }
        if path == "source.go" {
            assert_eq!(named(&parsed, "bare").description, None);
        }
        if path == "source.rs" {
            assert_eq!(parsed.description.as_deref(), Some("File header."));
        }
        for callable in &parsed.callables {
            assert_eq!(
                callable.description,
                named(&parsed, &callable.qualified_name).description
            );
        }
    }
}

#[test]
fn overloaded_and_nested_functions_keep_their_own_descriptions() {
    let parsed = clean(
        "source.java",
        "class Service {\n// No arguments.\nvoid run() {}\n// One argument.\nvoid run(int value) {}\n}\n",
    );
    assert_eq!(parsed.callables.len(), 2);
    assert_eq!(
        parsed.callables[0].description.as_deref(),
        Some("No arguments.")
    );
    assert_eq!(
        parsed.callables[1].description.as_deref(),
        Some("One argument.")
    );

    let parsed = clean(
        "source.py",
        "# Outer.\ndef outer():\n    \"\"\"Outer docstring.\"\"\"\n    # Inner.\n    def inner():\n        \"\"\"Inner docstring.\"\"\"\n        pass\n",
    );
    assert_eq!(
        named(&parsed, "outer").description.as_deref(),
        Some("Outer.\n\nOuter docstring.")
    );
    assert_eq!(
        named(&parsed, "outer.inner").description.as_deref(),
        Some("Inner.\n\nInner docstring.")
    );
}

#[test]
fn blank_lines_delimit_groups_and_never_attach_trailing_comments() {
    let parsed = clean(
        "source.ts",
        "\r\n\r\n// File header.\r\n\t// Second line.\r\n \t\r\nfunction bare() {}\r\n// Old note.\r\n\r\n// New note.\r\n/* More detail. */\r\n\tfunction described() {}\r\nconst value = 1; // Trailing note.\r\nfunction following() {}\r\n",
    );
    assert_eq!(
        parsed.description.as_deref(),
        Some("File header.\nSecond line.")
    );
    assert_eq!(named(&parsed, "bare").description, None);
    assert_eq!(
        named(&parsed, "described").description.as_deref(),
        Some("New note.\nMore detail.")
    );
    assert_eq!(named(&parsed, "following").description, None);

    let parsed = clean(
        "source.ts",
        "/**\n * First paragraph.\n *\n * Second paragraph with café 🚀.\n */\nfunction run() {}\n",
    );
    assert_eq!(
        parsed.description.as_deref(),
        Some("First paragraph.\n\nSecond paragraph with café 🚀.")
    );
    assert_eq!(parsed.description, named(&parsed, "run").description);

    let parsed = clean(
        "source.c",
        "/* First. */ \t /* Second. */\nint run(void) { return 0; }\n",
    );
    assert_eq!(parsed.description.as_deref(), Some("First.\nSecond."));
    assert_eq!(
        parsed.callables[0].description.as_deref(),
        Some("First.\nSecond.")
    );
}

#[test]
fn python_docstrings_are_prose_and_combine_with_adjacent_comments() {
    let parsed = clean(
        "source.py",
        r#"# Run a task.
@decorate
async def run():
    # A body comment does not replace the docstring.
    u"""Perform the work.

        Details:
            Keep relative indentation.
    """
    pass

def concatenated():
    ("One " r"description.")
    pass

def raw():
    r'''Use \n literally.'''

def not_documented():
    value = "not a docstring"
    "also not a docstring"

def formatted():
    f"not a docstring"

def binary():
    b"not a docstring"
"#,
    );
    for (name, description) in [
        (
            "run",
            Some("Run a task.\n\nPerform the work.\n\nDetails:\n    Keep relative indentation."),
        ),
        ("concatenated", Some("One description.")),
        ("raw", Some("Use \\n literally.")),
        ("not_documented", None),
        ("formatted", None),
        ("binary", None),
    ] {
        assert_eq!(
            named(&parsed, name).description.as_deref(),
            description,
            "{name}"
        );
        assert_eq!(
            parsed
                .callables
                .iter()
                .find(|c| c.name == name)
                .unwrap()
                .description
                .as_deref(),
            description,
            "{name}"
        );
    }
}

#[test]
fn data_and_markdown_comments_attach_to_structural_symbols_only() {
    for (path, source, expected) in [
        ("source.json", "{\n// A setting.\n\"run\": 1\n}", "run"),
        (
            "source.yaml",
            "parent:\n  # A setting.\n  run: 1\n",
            "parent.run",
        ),
        (
            "source.toml",
            "[parent]\n# A setting.\nrun = 1\n",
            "parent.run",
        ),
        (
            "source.tf",
            "service \"parent\" {\n/* A setting. */\nrun = 1\n}\n",
            "service.parent.run",
        ),
        (
            "source.xml",
            "<parent>\n<!-- A setting. -->\n<run/>\n</parent>",
            "parent.run",
        ),
        (
            "source.html",
            "<parent>\n<!-- A setting. -->\n<run></run>\n</parent>",
            "parent.run",
        ),
        (
            "source.md",
            "# Parent\n<!-- A setting. -->\n## Run\n",
            "Parent.Run",
        ),
    ] {
        let parsed = clean(path, source);
        assert_eq!(parsed.description, None, "{path}");
        assert_eq!(
            named(&parsed, expected).description.as_deref(),
            Some("A setting."),
            "{path}"
        );
    }
}

#[test]
fn literal_comment_text_and_shebangs_are_not_descriptions() {
    for (path, source, name) in [
        (
            "source.ts",
            "const text = '// fake';\nfunction run() {}",
            "run",
        ),
        ("source.py", "text = '# fake'\ndef run(): pass", "run"),
        (
            "source.sh",
            "#!/bin/bash\ncat <<'EOF'\n# fake\nEOF\nrun() { :; }",
            "run",
        ),
        ("source.yaml", "text: |\n  # fake\nrun: 1\n", "run"),
        ("source.toml", "text = '''\n# fake\n'''\nrun = 1\n", "run"),
        ("source.tf", "text = <<EOF\n# fake\nEOF\nrun = 1\n", "run"),
        (
            "source.html",
            "<script><!-- fake --></script>\n<run></run>",
            "run",
        ),
        (
            "source.xml",
            "<parent><![CDATA[<!-- fake -->]]>\n<run/></parent>",
            "parent.run",
        ),
        ("source.md", "```html\n<!-- fake -->\n```\n# Run\n", "Run"),
        ("source.md", "`<!-- fake -->`\n# Run\n", "Run"),
    ] {
        let parsed = clean(path, source);
        assert_eq!(parsed.description, None, "{path}");
        assert_eq!(named(&parsed, name).description, None, "{path}");
    }
}

#[test]
fn description_changes_update_callable_sources_hashes_and_embedding_inputs() {
    let before = clean("source.ts", "// Before.\nexport const run = () => 1;\n");
    let after = clean(
        "source.ts",
        "// Longer after description.\nexport const run = () => 1;\n",
    );
    assert_eq!(callables(&before), vec![("run", "function")]);
    let before = &before.callables[0];
    let after = &after.callables[0];
    assert_ne!(before.description, after.description);
    assert_ne!(before.source, after.source);
    assert_ne!(before.source_hash, after.source_hash);
    assert_ne!(before.embedding_input, after.embedding_input);
    assert_eq!(before.source, "// Before.\nexport const run = () => 1");
    assert_eq!(
        after.source,
        "// Longer after description.\nexport const run = () => 1"
    );
    for callable in [before, after] {
        assert_eq!(callable.source_hash, slopdex::hash(&callable.source));
        assert!(callable.embedding_input.ends_with(&callable.source));
        assert_eq!(callable.start_line, 1);
        assert_eq!(callable.line_count, 2);
    }
    assert_eq!(
        (
            before.start_line,
            before.start_column,
            before.end_line,
            before.end_column
        ),
        (
            after.start_line,
            after.start_column,
            after.end_line,
            after.end_column
        )
    );
}

#[test]
fn leading_comments_extend_code_ranges_without_changing_signatures() {
    for (path, comment, declaration, name) in [
        (
            "source.ts",
            "// First.\n// Second.",
            "export function run() {}",
            "run",
        ),
        (
            "source.tsx",
            "/** First.\n * Second. */",
            "const run = () => <div />;",
            "run",
        ),
        (
            "source.js",
            "// First.\n// Second.",
            "function run() {}",
            "run",
        ),
        (
            "source.jsx",
            "/** First.\n * Second. */",
            "const run = () => <div />;",
            "run",
        ),
        (
            "source.py",
            "# First.\n# Second.",
            "@decorate\ndef run(): pass",
            "run",
        ),
        (
            "source.rs",
            "/// First.\n/// Second.",
            "#[inline]\npub fn run() {}",
            "run",
        ),
        ("source.go", "// First.\n// Second.", "func run() {}", "run"),
        (
            "source.java",
            "/** First.\n * Second. */",
            "class Run {}",
            "Run",
        ),
        (
            "source.c",
            "/* First.\n * Second. */",
            "int run(void) { return 0; }",
            "run",
        ),
        ("source.sh", "# First.\n# Second.", "run() { :; }", "run"),
    ] {
        let bare = clean(path, &format!("{declaration}\n"));
        let source = format!("\n{comment}\n{declaration}\n");
        let parsed = clean(path, &source);
        let node = named(&parsed, name);
        assert_eq!(node.start_byte, 1, "{path}");
        assert_eq!(node.start_line, 2, "{path}");
        assert_eq!(node.declaration_start_line, Some(4), "{path}");
        assert_eq!(node.signature, named(&bare, name).signature, "{path}");
        assert_eq!(
            node.description.as_deref(),
            Some("First.\nSecond."),
            "{path}"
        );
        for callable in &parsed.callables {
            assert_eq!(callable.start_line, 2, "{path}");
            assert!(callable.source.starts_with(comment), "{path}");
            assert_eq!(callable.source_hash, slopdex::hash(&callable.source));
            assert!(callable.embedding_input.ends_with(&callable.source));
        }
    }
}

#[test]
fn detached_and_trailing_comments_stay_outside_following_code_ranges() {
    let source = "// Detached.\n\nfunction bare() {} // Trailing.\nfunction following() {}\n//\nfunction empty() {}\n";
    let parsed = clean("source.ts", source);
    for (name, line) in [("bare", 3), ("following", 4), ("empty", 5)] {
        let node = named(&parsed, name);
        assert_eq!(node.start_line, line);
        assert_eq!(node.description, None);
        let callable = parsed.callables.iter().find(|c| c.name == name).unwrap();
        assert_eq!(callable.start_line, line);
    }
    assert!(parsed.callables[2].source.starts_with("//\n"));
}

#[test]
fn optional_descriptions_are_backward_compatible_and_omitted_when_absent() {
    let parsed = clean("source.ts", "function run() {}\n");
    let json = serde_json::to_value(&parsed).unwrap();
    assert!(json.get("description").is_none());
    assert!(json["structure"]["nodes"][0].get("description").is_none());
    assert!(json["callables"][0].get("description").is_none());
    let restored: ParsedFile = serde_json::from_value(json).unwrap();
    assert_eq!(restored.description, None);
    assert_eq!(restored.structure.nodes[0].description, None);
    assert_eq!(restored.callables[0].description, None);

    let comments_only = parse("source.py", "\n# File documentation.\n").unwrap();
    assert_eq!(
        comments_only.description.as_deref(),
        Some("File documentation.")
    );
    assert!(comments_only.callables.is_empty());
    assert!(comments_only.structure.nodes.is_empty());
    assert_eq!(
        parse("source.txt", "// Not supported.")
            .unwrap()
            .description,
        None
    );
}
