use slopdex::parse::{ImportBinding, parse};

fn imports(path: &str, source: &str) -> Vec<ImportBinding> {
    let parsed = parse(path, source).unwrap();
    assert!(parsed.errors.is_empty(), "{path}: {:?}", parsed.errors);
    parsed
        .structure
        .nodes
        .into_iter()
        .flat_map(|node| node.imports)
        .collect()
}

fn expected(
    path: &str,
    source: Option<&str>,
    name: Option<&str>,
    alias: Option<&str>,
    wildcard: bool,
) -> ImportBinding {
    ImportBinding {
        path: path.into(),
        source: source.map(str::to_owned),
        name: name.map(str::to_owned),
        alias: alias.map(str::to_owned),
        wildcard,
    }
}

#[test]
fn rust_use_lists_ignore_comment_extras() {
    let source = r#"
use std::{
    /* before */ io::{self, // between
        Read as Reader, /* after */},
    fmt::*, // trailing
};
use std::{/* empty */};
extern crate core as renamed;
"#;
    assert_eq!(
        imports("imports.rs", source),
        [
            expected("std::io", None, Some("io"), None, false),
            expected("std::io::Read", None, Some("Read"), Some("Reader"), false),
            expected("std::fmt::*", None, Some("*"), None, true),
            expected("core", None, Some("core"), Some("renamed"), false),
        ]
    );
}

#[test]
fn rust_use_paths_omit_layout_and_comments() {
    let source = r#"
use std /* scope */ :: io :: Read as Reader;
use std /* scope */ :: {io /* nested */ :: {Read}, fmt :: /* star */ *};
use ::std :: io :: Read;
"#;
    assert_eq!(
        imports("imports.rs", source),
        [
            expected("std::io::Read", None, Some("Read"), Some("Reader"), false),
            expected("std::io::Read", None, Some("Read"), None, false),
            expected("std::fmt::*", None, Some("*"), None, true),
            expected("::std::io::Read", None, Some("Read"), None, false),
        ]
    );
}

#[test]
fn rust_absolute_use_lists_keep_the_root_prefix() {
    let source = "use ::{std::io::{self, Read as Reader}, std::fmt::*};";
    assert_eq!(
        imports("imports.rs", source),
        [
            expected("::std::io", None, Some("io"), None, false),
            expected("::std::io::Read", None, Some("Read"), Some("Reader"), false),
            expected("::std::fmt::*", None, Some("*"), None, true),
        ]
    );
}

#[test]
fn python_future_imports_supply_the_implicit_module() {
    let source = "from __future__ import (annotations, # between\n    generator_stop)\n";
    assert_eq!(
        imports("imports.py", source),
        [
            expected(
                "__future__.annotations",
                Some("__future__"),
                Some("annotations"),
                None,
                false,
            ),
            expected(
                "__future__.generator_stop",
                Some("__future__"),
                Some("generator_stop"),
                None,
                false,
            ),
        ]
    );
}

#[test]
fn python_relative_wildcards_keep_the_relative_depth() {
    assert_eq!(
        imports(
            "imports.py",
            "from . import *\nfrom .. import *\nfrom ...pkg import *\nfrom . import ( # note\n    Thing as Local,)\n",
        ),
        [
            expected(".*", Some("."), Some("*"), None, true),
            expected("..*", Some(".."), Some("*"), None, true),
            expected("...pkg.*", Some("...pkg"), Some("*"), None, true),
            expected(".Thing", Some("."), Some("Thing"), Some("Local"), false),
        ]
    );
}

#[test]
fn python_dotted_imports_omit_layout() {
    assert_eq!(
        imports(
            "imports.py",
            "import os . path as path\nfrom .. pkg . inner import Thing as Local\n",
        ),
        [
            expected("os.path", None, Some("os.path"), Some("path"), false),
            expected(
                "..pkg.inner.Thing",
                Some("..pkg.inner"),
                Some("Thing"),
                Some("Local"),
                false,
            ),
        ]
    );
}

#[test]
fn javascript_imports_and_reexports_ignore_comments_and_unquote_aliases() {
    let source = r#"
import /* before */ Default, { /* inside */ Thing as Local, "two words" as Words } from 'pkg';
import * /* star */ as Namespace from 'space';
export { /* before */ Thing as "two words", "quoted name" as "quoted alias", default as Default } from 'other';
export * /* star */ from 'all';
export * as "space name" from 'space';
export * as default from 'defaults';
"#;
    for path in ["imports.js", "imports.jsx", "imports.ts", "imports.tsx"] {
        assert_eq!(
            imports(path, source),
            [
                expected(
                    "pkg.default",
                    Some("pkg"),
                    Some("default"),
                    Some("Default"),
                    false
                ),
                expected(
                    "pkg.Thing",
                    Some("pkg"),
                    Some("Thing"),
                    Some("Local"),
                    false
                ),
                expected(
                    "pkg.two words",
                    Some("pkg"),
                    Some("two words"),
                    Some("Words"),
                    false
                ),
                expected("space.*", Some("space"), Some("*"), Some("Namespace"), true),
                expected(
                    "other.Thing",
                    Some("other"),
                    Some("Thing"),
                    Some("two words"),
                    false
                ),
                expected(
                    "other.quoted name",
                    Some("other"),
                    Some("quoted name"),
                    Some("quoted alias"),
                    false
                ),
                expected(
                    "other.default",
                    Some("other"),
                    Some("default"),
                    Some("Default"),
                    false
                ),
                expected("all.*", Some("all"), Some("*"), None, true),
                expected(
                    "space.*",
                    Some("space"),
                    Some("*"),
                    Some("space name"),
                    true
                ),
                expected(
                    "defaults.*",
                    Some("defaults"),
                    Some("*"),
                    Some("default"),
                    true
                ),
            ],
            "{path}",
        );
    }
}

#[test]
fn javascript_literal_contents_are_not_delimiters_or_wildcards() {
    let source = r#"
import "'pkg'";
import '<pkg>';
import '`pkg`';
import 'pkg*';
import { '*' as Star, "'name'" as Quoted } from 'names';
export { Thing as "'alias'" } from 'aliases';
"#;
    for path in ["imports.js", "imports.jsx", "imports.ts", "imports.tsx"] {
        assert_eq!(
            imports(path, source),
            [
                expected("'pkg'", Some("'pkg'"), None, None, false),
                expected("<pkg>", Some("<pkg>"), None, None, false),
                expected("`pkg`", Some("`pkg`"), None, None, false),
                expected("pkg*", Some("pkg*"), None, None, false),
                expected("names.*", Some("names"), Some("*"), Some("Star"), false),
                expected(
                    "names.'name'",
                    Some("names"),
                    Some("'name'"),
                    Some("Quoted"),
                    false
                ),
                expected(
                    "aliases.Thing",
                    Some("aliases"),
                    Some("Thing"),
                    Some("'alias'"),
                    false
                ),
            ],
            "{path}",
        );
    }
}

#[test]
fn typescript_import_equals_reads_the_target_not_the_local_name() {
    let source = r#"
import /* before */ Local = Namespace;
import Nested = Namespace /* scope */ . Inner;
import Required /* alias */ = require(/* module */ 'pkg');
import { /* before */ type Thing as LocalThing, type Other } from 'types';
import type DefaultType from 'types';
export type { Thing as Exported } from 'types';
"#;
    for path in ["imports.ts", "imports.tsx"] {
        assert_eq!(
            imports(path, source),
            [
                expected("Namespace", None, None, Some("Local"), false),
                expected("Namespace.Inner", None, None, Some("Nested"), false),
                expected("pkg", Some("pkg"), None, Some("Required"), false),
                expected(
                    "types.Thing",
                    Some("types"),
                    Some("Thing"),
                    Some("LocalThing"),
                    false
                ),
                expected("types.Other", Some("types"), Some("Other"), None, false),
                expected(
                    "types.default",
                    Some("types"),
                    Some("default"),
                    Some("DefaultType"),
                    false
                ),
                expected(
                    "types.Thing",
                    Some("types"),
                    Some("Thing"),
                    Some("Exported"),
                    false
                ),
            ],
            "{path}",
        );
    }
}

#[test]
fn go_import_aliases_and_raw_paths_preserve_literal_contents() {
    let source = r#"
package imports
import (
    /* before */ renamed /* alias */ "example.com/pkg"
    . `example.com/raw`
    _ "example.com/side"
    "io"
)
"#;
    assert_eq!(
        imports("imports.go", source),
        [
            expected(
                "example.com/pkg",
                Some("example.com/pkg"),
                None,
                Some("renamed"),
                false
            ),
            expected(
                "example.com/raw",
                Some("example.com/raw"),
                None,
                Some("."),
                false
            ),
            expected(
                "example.com/side",
                Some("example.com/side"),
                None,
                Some("_"),
                false
            ),
            expected("io", Some("io"), None, None, false),
        ]
    );
}

#[test]
fn java_import_paths_omit_comment_extras_and_layout() {
    let source = r#"
import java /* scope */ . util . List;
import static java . util /* scope */ . Collections . /* star */ *;
"#;
    assert_eq!(
        imports("Imports.java", source),
        [
            expected("java.util.List", None, Some("List"), None, false),
            expected("java.util.Collections.*", None, Some("*"), None, true),
        ]
    );
}

#[test]
fn c_include_paths_only_strip_the_outer_delimiters() {
    let source = "#include /* before */ <stdio.h>\n#include \"<local>.h\"\n#include HEADER\n";
    assert_eq!(
        imports("imports.c", source),
        [
            expected("stdio.h", Some("stdio.h"), None, None, false),
            expected("<local>.h", Some("<local>.h"), None, None, false),
            expected("HEADER", Some("HEADER"), None, None, false),
        ]
    );
}
