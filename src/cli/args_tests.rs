use super::*;
use crate::cli::{CommandMode, test_support::parse};
use clap::CommandFactory;

#[test]
fn clap_definition_is_consistent() {
    Cli::command().debug_assert();
}

#[test]
fn refactor_uses_two_targets_and_structure_only_without_stale_snapshots() {
    let cli = parse(&["refactor", "src/a.ts::parse", "lib/b.ts::read", "--dry-run"]);
    assert_eq!(
        CommandMode::for_command(&cli.command),
        CommandMode::Structure
    );
    let Command::Refactor(args) = cli.command else {
        panic!("expected refactor");
    };
    assert_eq!(args.source, "src/a.ts::parse");
    assert_eq!(args.destination, "lib/b.ts::read");
    assert!(args.dry_run);
    let cli = Cli::try_parse_from(["slopdex", "refactor", "a.ts", "b.ts", "--no-reindex"]).unwrap();
    assert!(cli.validate().is_err());
    assert!(Cli::try_parse_from(["slopdex", "refactor", "a.ts"]).is_err());
}

#[test]
fn lines_selectors_exclusions_and_limits_do_not_become_declaration_limits() {
    let cli = parse(&[
        "lines",
        "src",
        "docs",
        "-g",
        "*.rs",
        "-g",
        "!generated.rs",
        "-e",
        "read",
        "-q",
        "parse source",
        "-i",
        "--formats",
        "all",
        "--exclude-comments",
        "--exclude-tests",
        "--include-blank-lines",
        "--limit",
        "10",
    ]);
    assert_eq!(CommandMode::for_command(&cli.command), CommandMode::Symbols);
    let Command::Lines(args) = cli.command else {
        panic!("expected lines")
    };
    assert_eq!(args.limit, Some(ResultLimit::Count(10)));
    let options = args.options(&args.paths);
    assert_eq!(options["paths"], json!(["src", "docs"]));
    assert_eq!(options["glob"], json!(["*.rs", "!generated.rs"]));
    assert_eq!(options["regexp"], json!(["read"]));
    assert_eq!(options["symbolQuery"], json!(["parse source"]));
    assert_eq!(options["formats"], json!(["all"]));
    assert_eq!(options["private"], true);
    assert_eq!(options["excludeComments"], true);
    assert_eq!(options["excludeTests"], true);
    assert_eq!(options["includeBlankLines"], true);
    assert!(options.get("limit").is_none());
    assert!(options.get("callers").is_none());
    let cli = parse(&["lines"]);
    assert_eq!(
        CommandMode::for_command(&cli.command),
        CommandMode::Structure
    );
    let Command::Lines(args) = cli.command else {
        panic!("expected lines")
    };
    assert!(args.limit.is_none());
    assert!(!args.include_blank_lines);
    assert_eq!(args.options(&args.paths)["includeBlankLines"], false);
    assert_eq!(args.paths, [PathBuf::from(".")]);
    for limit in ["0", "-1", "bad"] {
        assert!(Cli::try_parse_from(["slopdex", "lines", "--limit", limit]).is_err());
    }
    assert!(Cli::try_parse_from(["slopdex", "lines", "--limit", "none"]).is_ok());
}

#[test]
fn files_output_is_only_available_for_lines_with_global_flag_placement() {
    for args in [
        vec!["lines", "--output", "files", "--limit", "3"],
        vec!["--output", "files", "lines", "--limit", "3"],
    ] {
        let cli = parse(&args);
        assert_eq!(cli.global.format, Some(Format::Files));
        let Command::Lines(args) = cli.command else {
            panic!("expected lines")
        };
        assert_eq!(args.limit, Some(ResultLimit::Count(3)));
        assert!(args.options(&args.paths).get("limit").is_none());
    }
    for command in [
        vec!["map"],
        vec!["search", "query"],
        vec!["cross-search"],
        vec!["status"],
        vec!["config"],
    ] {
        let cli = Cli::try_parse_from(["slopdex", "--output", "files"].into_iter().chain(command))
            .unwrap();
        let error = cli.validate().unwrap_err().to_string();
        assert!(
            error.contains("--output files is only available for lines"),
            "{error}"
        );
    }
}

#[test]
fn embedding_setup_accepts_global_flags_before_and_after_commands() {
    for args in [
        vec![
            "update",
            "--setup",
            "--provider",
            "ollama",
            "--model",
            "qwen3-embedding:0.6b",
            "--pull",
        ],
        vec![
            "--provider",
            "ollama",
            "--model",
            "qwen3-embedding:4b",
            "--dimensions",
            "256",
            "--pull",
            "update",
            "--setup",
        ],
        vec![
            "config",
            "embeddings",
            "--provider",
            "ollama",
            "--model",
            "qwen3-embedding:8b",
            "--pull",
        ],
        vec!["--provider", "ollama", "--pull", "config", "embeddings"],
    ] {
        let cli = parse(&args);
        cli.validate().unwrap();
        assert_eq!(cli.global.provider.as_deref(), Some("ollama"));
        assert!(cli.global.pull);
        if let Command::Update(args) = cli.command {
            assert!(args.setup);
        } else if let Command::Config { args } = cli.command {
            assert_eq!(args, ["embeddings"]);
        } else {
            panic!("unexpected command");
        }
    }
    assert!(Cli::try_parse_from(["slopdex", "config", "--setup"]).is_err());
    assert!(Cli::try_parse_from(["slopdex", "search", "query", "--setup"]).is_err());
}

#[test]
fn pull_is_only_accepted_for_explicit_embedding_setup_and_ollama() {
    for args in [
        vec!["update", "--pull"],
        vec!["search", "query", "--provider", "ollama", "--pull"],
        vec!["config", "--pull"],
        vec!["config", "set", "model", "custom", "--pull"],
        vec!["config", "embeddings", "extra", "--pull"],
        vec!["update", "--setup", "--provider", "openai", "--pull"],
        vec!["config", "embeddings", "--provider", "jina", "--pull"],
    ] {
        let cli =
            Cli::try_parse_from(std::iter::once("slopdex").chain(args.iter().copied())).unwrap();
        assert!(
            cli.validate().unwrap_err().to_string().contains("--pull"),
            "{args:?}"
        );
    }
    // The selected saved provider is checked after loading config, not by Clap.
    parse(&[
        "update",
        "--setup",
        "--model",
        "qwen3-embedding:0.6b",
        "--pull",
    ])
    .validate()
    .unwrap();
    parse(&["update", "--provider", "ollama"])
        .validate()
        .unwrap();
}

#[test]
fn output_flag_is_canonical_and_format_flag_is_rejected() {
    for (value, expected) in [
        ("text", Format::Summary),
        ("json", Format::Json),
        ("clusters", Format::Clusters),
    ] {
        assert_eq!(
            parse(&["cross-search", "--output", value]).global.format,
            Some(expected)
        );
        assert_eq!(
            parse(&["--output", value, "cross-search"]).global.format,
            Some(expected)
        );
    }
    for command in [
        vec!["map"],
        vec!["search", "query"],
        vec!["cross-search"],
        vec!["config"],
    ] {
        let mut old = vec!["slopdex"];
        old.extend(command.iter().copied());
        old.extend(["--format", "json"]);
        assert!(Cli::try_parse_from(old).is_err());
        let help = Cli::try_parse_from(
            std::iter::once("slopdex")
                .chain(command.iter().copied())
                .chain(["--help"]),
        )
        .unwrap_err()
        .to_string();
        assert!(help.contains("--output"), "{help}");
        assert!(
            !help.split_whitespace().any(|word| word == "--format"),
            "{help}"
        );
    }
    let error = Cli::try_parse_from(["slopdex", "map", "--output", "invalid"])
        .unwrap_err()
        .to_string();
    assert!(error.contains("--output"), "{error}");
    assert!(!error.contains("--format"), "{error}");
    let cli = parse(&["map", "--formats", "code,docs", "--output", "json"]);
    let Command::Map(args) = cli.command else {
        panic!()
    };
    assert_eq!(args.selection.formats, ["code", "docs"]);
}

#[test]
fn help_describes_all_indexes_semantic_selection_and_explicit_generation() {
    let help = |args: &[&str]| {
        Cli::try_parse_from(std::iter::once("slopdex").chain(args.iter().copied()))
            .unwrap_err()
            .to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let top = help(&["--help"]);
    assert!(top.contains("Plain search includes all indexes"), "{top}");
    assert!(
        top.contains("queries and explicit generation may call providers"),
        "{top}"
    );
    assert!(!top.contains("opt-in"), "{top}");
    let search = help(&["search", "--help"]);
    assert!(
        search.contains("Semantic query over symbols, descriptions, and heading titles"),
        "{search}"
    );
    assert!(search.contains("callable-source"), "{search}");
    for index in [
        "callable code",
        "generated callable/file description",
        "documentation content",
        "symbol-name/alias and heading-title",
    ] {
        assert!(
            search.contains(&format!("Select the {index} index")),
            "{search}"
        );
    }
    let generation = help(&["generate", "descriptions", "--help"]);
    assert!(
        generation.contains("Generate missing or stale file and callable descriptions"),
        "{generation}"
    );
}

#[test]
fn description_generation_uses_content_mode_and_explicit_nested_action() {
    for argv in [
        vec!["generate", "descriptions"],
        vec![
            "generate",
            "descriptions",
            "--no-reindex",
            "--output",
            "json",
        ],
    ] {
        let cli = parse(&argv);
        assert!(matches!(
            cli.command,
            Command::Generate {
                action: GenerateAction::Descriptions
            }
        ));
        assert_eq!(CommandMode::for_command(&cli.command), CommandMode::Content);
    }
}

#[test]
fn shared_selection_arguments_reach_all_search_options() {
    for command in [
        "search",
        "search-code",
        "search-descriptions",
        "search-docs",
        "search-md",
        "search-symbols",
        "faq",
        "cross-search",
    ] {
        let mut argv = vec![command];
        if command != "cross-search" {
            argv.push("query");
        }
        argv.extend([
            "-g",
            "*.rs",
            "--glob",
            "!src/generated/**",
            "-g",
            "src/generated/keep.rs",
            "-e",
            "^Service\\.",
            "--regex",
            "Setup",
            "--regexp",
            "Guide",
            "-e",
            r"assert\(|throw new Error|ǃ\(",
            "-i",
            "-q",
            "validateSession",
            "--symbol-query",
            "load settings",
            "--symbol-threshold",
            "0.65",
        ]);
        let cli = parse(&argv);
        let options = match cli.command {
            Command::Search(args) => args.query.filters.options(),
            Command::SearchCode(args)
            | Command::SearchDescriptions(args)
            | Command::SearchMd(args)
            | Command::SearchSymbols(args) => args.filters.options(),
            Command::Faq(args) => args.query.filters.options(),
            Command::CrossSearch(args) => args.options(),
            _ => unreachable!(),
        };
        assert_eq!(
            options["glob"],
            json!(["*.rs", "!src/generated/**", "src/generated/keep.rs"]),
            "{command}"
        );
        assert_eq!(
            options["regexp"],
            json!([
                "^Service\\.",
                "Setup",
                "Guide",
                r"assert\(|throw new Error|ǃ\("
            ]),
            "{command}"
        );
        assert_eq!(options["ignoreCase"], true, "{command}");
        assert_eq!(
            options["symbolQuery"],
            json!(["validateSession", "load settings"]),
            "{command}"
        );
        assert_eq!(options["symbolThreshold"], 0.65, "{command}");
    }
}

#[test]
fn formats_are_shared_selection_and_do_not_replace_settings_path() {
    for command in [
        "map",
        "lines",
        "search",
        "search-code",
        "search-descriptions",
        "search-docs",
        "search-md",
        "search-symbols",
        "faq",
        "cross-search",
    ] {
        let mut argv = vec!["--config", "settings/custom.json", command];
        if !matches!(command, "map" | "lines" | "cross-search") {
            argv.push("query");
        }
        let options = |cli: Cli| {
            assert_eq!(
                cli.global.config,
                Some(PathBuf::from("settings/custom.json"))
            );
            match cli.command {
                Command::Map(args) => args.options(),
                Command::Lines(args) => args.options(&args.paths),
                Command::Search(args) => args.query.filters.options(),
                Command::SearchCode(args)
                | Command::SearchDescriptions(args)
                | Command::SearchMd(args)
                | Command::SearchSymbols(args) => args.filters.options(),
                Command::Faq(args) => args.query.filters.options(),
                Command::CrossSearch(args) => args.options(),
                _ => unreachable!(),
            }
        };
        let defaults = options(parse(&argv));
        assert!(defaults.get("formats").is_none(), "{command}: {defaults}");
        argv.extend(["--formats", "config,code", "--formats", "docs,markup"]);
        let selected = options(parse(&argv));
        assert_eq!(
            selected["formats"],
            json!(["config", "code", "docs", "markup"]),
            "{command}"
        );
        assert!(selected.get("config").is_none(), "{command}: {selected}");
        let mut all = argv[..argv.len() - 4].to_vec();
        all.extend(["--formats", "all"]);
        assert_eq!(options(parse(&all))["formats"], json!(["all"]), "{command}");
        for value in ["", "bad", "others", "md", "code,bad", "code,", ",docs"] {
            let mut invalid = all[..all.len() - 2].to_vec();
            invalid.extend(["--formats", value]);
            assert!(
                Cli::try_parse_from(std::iter::once("slopdex").chain(invalid)).is_err(),
                "{command}: {value}"
            );
        }
        let mut removed = all[..all.len() - 2].to_vec();
        removed.push("--include-config");
        assert!(Cli::try_parse_from(std::iter::once("slopdex").chain(removed)).is_err());
    }
}

#[test]
fn docs_command_and_selector_keep_md_as_a_visible_compatibility_alias() {
    for command in ["search-docs", "search-md"] {
        let cli = parse(&[command, "query"]);
        assert!(matches!(cli.command, Command::SearchMd(_)));
        let (_, kind, _, _) = cli.command.search_request().unwrap();
        assert_eq!(kind, "search-docs", "{command}");
    }
    for selector in ["--docs", "--md"] {
        let cli = parse(&["search", "query", selector]);
        let (_, _, options, _) = cli.command.search_request().unwrap();
        assert_eq!(options["docs"], true, "{selector}");
        assert!(options.get("md").is_none(), "{selector}: {options}");
    }
    let help = Cli::try_parse_from(["slopdex", "search", "--help"])
        .unwrap_err()
        .to_string();
    assert!(help.contains("--docs") && help.contains("--md"), "{help}");
    let help = Cli::try_parse_from(["slopdex", "--help"])
        .unwrap_err()
        .to_string();
    assert!(
        help.contains("search-docs") && help.contains("search-md"),
        "{help}"
    );
}

#[test]
fn search_commands_select_explicit_indexes_and_structural_mode() {
    let plain = parse(&["search", "query"]);
    let (_, kind, options, descriptions) = plain.command.search_request().unwrap();
    assert_eq!(kind, "search");
    assert!(!descriptions);
    for selector in ["code", "descriptions", "docs", "symbols"] {
        assert!(options.get(selector).is_none());
    }
    assert_eq!(
        CommandMode::for_command(&plain.command),
        CommandMode::Content
    );
    for argv in [
        vec!["search-symbols", "validate session"],
        vec!["search", "validate session", "--symbols"],
    ] {
        let cli = parse(&argv);
        let (args, kind, options, _) = cli.command.search_request().unwrap();
        assert_eq!(args.query, "validate session");
        assert_eq!(CommandMode::for_command(&cli.command), CommandMode::Symbols);
        if kind == "search" {
            assert_eq!(options["symbols"], true);
            for selector in ["code", "descriptions", "docs"] {
                assert_eq!(options[selector], false);
            }
        } else {
            assert_eq!(kind, "search-symbols");
        }
    }
    for selector in ["--code", "--docs", "--descriptions"] {
        let cli = parse(&["search", "query", "--symbols", selector]);
        let (_, _, options, _) = cli.command.search_request().unwrap();
        assert_eq!(options["symbols"], true);
        assert_eq!(options[selector.trim_start_matches('-')], true);
        assert_eq!(CommandMode::for_command(&cli.command), CommandMode::Content);
    }
    let cli = parse(&[
        "search-symbols",
        "rank names",
        "-q",
        "filter names",
        "--threshold",
        "0.3-0.8",
        "--symbol-threshold",
        "0.7",
        "--limit",
        "4",
    ]);
    let (_, _, options, _) = cli.command.search_request().unwrap();
    assert_eq!(options["minSimilarity"], 0.3);
    assert_eq!(options["maxSimilarity"], 0.8);
    assert_eq!(options["symbolQuery"], json!(["filter names"]));
    assert_eq!(options["symbolThreshold"], 0.7);
    assert_eq!(options["limit"], 4);
    assert!(Cli::try_parse_from(["slopdex", "search-symbols"]).is_err());
    assert!(Cli::try_parse_from(["slopdex", "search-symbols", "query", "--code"]).is_err());
}

#[test]
fn search_accepts_multiple_nonempty_queries_with_shared_options() {
    let cli = parse(&[
        "search",
        "validate session",
        "load settings",
        "--code",
        "--threshold",
        "0.6",
        "another query",
        "--limit",
        "5",
    ]);
    let Command::Search(args) = cli.command else {
        panic!()
    };
    assert_eq!(args.query.query, "validate session");
    assert_eq!(args.additional_queries, ["load settings", "another query"]);
    assert_eq!(args.options()["minSimilarity"], 0.6);
    assert_eq!(args.options()["limit"], 5);
    assert_eq!(args.options()["code"], true);
    let Command::Search(single) = parse(&["search", "one query"]).command else {
        panic!()
    };
    assert!(single.additional_queries.is_empty());
    for args in [
        vec!["slopdex", "search"],
        vec!["slopdex", "search", "query", ""],
        vec!["slopdex", "search", "query", "   "],
        vec!["slopdex", "faq", "first", "second"],
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn symbol_selection_defaults_and_scalar_threshold_validation() {
    for command in [
        "map",
        "search",
        "search-code",
        "search-descriptions",
        "search-md",
        "search-symbols",
        "faq",
        "cross-search",
    ] {
        let mut argv = vec![command];
        if !matches!(command, "map" | "cross-search") {
            argv.push("content query");
        }
        let options = |cli: Cli| match cli.command {
            Command::Map(args) => args.options(),
            Command::Search(args) => args.query.filters.options(),
            Command::SearchCode(args)
            | Command::SearchDescriptions(args)
            | Command::SearchMd(args)
            | Command::SearchSymbols(args) => args.filters.options(),
            Command::Faq(args) => args.query.filters.options(),
            Command::CrossSearch(args) => args.options(),
            _ => unreachable!(),
        };
        let defaults = options(parse(&argv));
        assert!(defaults.get("symbolQuery").is_none(), "{command}");
        assert!(defaults.get("symbolThreshold").is_none(), "{command}");
        let mut selected = argv.clone();
        selected.extend([
            "-q",
            "readFile",
            "--symbol-query",
            "write file",
            "-q",
            "-g glob filter matching",
            "--symbol-query",
            "--glob include matching",
        ]);
        let selected = options(parse(&selected));
        assert_eq!(
            selected["symbolQuery"],
            json!([
                "readFile",
                "write file",
                "-g glob filter matching",
                "--glob include matching"
            ]),
            "{command}"
        );
        assert!(selected.get("symbolThreshold").is_none());
        for flag in ["-q", "--symbol-query"] {
            let mut invalid = argv.clone();
            invalid.push(flag);
            assert!(
                Cli::try_parse_from(std::iter::once("slopdex").chain(invalid)).is_err(),
                "{command}: {flag}"
            );
        }
        for value in ["", "   ", "_::.!🙂", "-", "--"] {
            let mut invalid = argv.clone();
            invalid.extend(["-q", value]);
            assert!(
                Cli::try_parse_from(std::iter::once("slopdex").chain(invalid)).is_err(),
                "{command}: {value}"
            );
        }
        for value in ["NaN", "inf", "-inf", "-1.01", "1.01", "0.4-0.9", "no"] {
            let mut invalid = argv.clone();
            invalid.extend(["--symbol-threshold", value]);
            assert!(
                Cli::try_parse_from(std::iter::once("slopdex").chain(invalid)).is_err(),
                "{command}: {value}"
            );
        }
        for value in ["-1", "0", "1"] {
            let mut valid = argv.clone();
            valid.extend(["--symbol-threshold", value]);
            assert_eq!(
                options(parse(&valid))["symbolThreshold"],
                json!(value.parse::<f64>().unwrap())
            );
        }
    }
    let Command::Search(args) = parse(&[
        "search",
        "content query",
        "-q",
        "names",
        "--threshold",
        "0.2",
        "--symbol-threshold",
        "0.7",
    ])
    .command
    else {
        panic!()
    };
    assert_eq!(args.query.query, "content query");
    assert_eq!(args.query.filters.options()["minSimilarity"], 0.2);
    assert_eq!(args.query.filters.options()["symbolThreshold"], 0.7);
}

#[test]
fn map_symbol_queries_accept_leading_hyphens() {
    let Command::Map(args) = parse(&[
        "map",
        "-g",
        "**/*.rs",
        "-i",
        "-e",
        "glob|globset|include",
        "-q",
        "-g glob filter matching",
        "-q",
        "typescript file patterns",
        "--private",
        "--callers",
        "2",
        "--callees",
        "2",
        "src",
        "tests",
    ])
    .command
    else {
        panic!()
    };
    assert_eq!(
        args.options(),
        json!({
            "paths": ["src", "tests"],
            "glob": ["**/*.rs"],
            "ignoreCase": true,
            "regexp": ["glob|globset|include"],
            "symbolQuery": ["-g glob filter matching", "typescript file patterns"],
            "private": true,
            "callers": 2,
            "callees": 2,
            "refs": "cross-file"
        })
    );
}

#[test]
fn reference_modes_parse_for_all_call_aware_commands() {
    for command in [
        vec!["map"],
        vec!["search", "query"],
        vec!["search-code", "query"],
        vec!["search-descriptions", "query"],
        vec!["search-md", "query"],
        vec!["search-symbols", "query"],
        vec!["faq", "query"],
        vec!["cross-search"],
    ] {
        for mode in ["none", "all", "cross-file"] {
            let mut args = command.clone();
            args.extend(["--refs", mode]);
            let cli = parse(&args);
            let calls = match cli.command {
                Command::Map(args) => args.calls,
                Command::Search(args) => args.query.calls,
                Command::SearchCode(args)
                | Command::SearchDescriptions(args)
                | Command::SearchMd(args)
                | Command::SearchSymbols(args) => args.calls,
                Command::Faq(args) => args.query.calls,
                Command::CrossSearch(args) => args.calls,
                _ => unreachable!(),
            };
            assert_eq!(
                calls.resolve(Detail::Compact).refs,
                mode.parse::<Refs>().unwrap()
            );
            assert_eq!(
                calls.resolve(Detail::Expanded).refs,
                mode.parse::<Refs>().unwrap()
            );
            assert_eq!(
                calls.resolve_faq_context().refs,
                mode.parse::<Refs>().unwrap()
            );
        }
        for mode in ["", "local", "cross_file", "ALL"] {
            let mut args = command.clone();
            args.extend(["--refs", mode]);
            assert!(Cli::try_parse_from(std::iter::once("slopdex").chain(args)).is_err());
        }
    }
}

#[test]
fn reference_defaults_follow_effective_call_depths() {
    for (args, detail, expected) in [
        (vec![], Detail::Compact, Refs::None),
        (vec![], Detail::Standard, Refs::None),
        (vec![], Detail::Expanded, Refs::CrossFile),
        (vec!["--callers", "1"], Detail::Compact, Refs::CrossFile),
        (vec!["--callees", "2"], Detail::Compact, Refs::CrossFile),
        (
            vec!["--expand-callers", "1"],
            Detail::Compact,
            Refs::CrossFile,
        ),
        (
            vec!["--expand-callables", "1"],
            Detail::Compact,
            Refs::CrossFile,
        ),
        (
            vec!["--callers", "0", "--callees", "0"],
            Detail::Expanded,
            Refs::None,
        ),
        (
            vec!["--callers", "1", "--refs", "none"],
            Detail::Compact,
            Refs::None,
        ),
        (vec!["--refs", "all"], Detail::Compact, Refs::All),
    ] {
        let mut argv = vec!["map"];
        argv.extend(args);
        let Command::Map(args) = parse(&argv).command else {
            unreachable!()
        };
        assert_eq!(args.calls.resolve(detail).refs, expected, "{argv:?}");
        let options = args.options_for(detail);
        assert_eq!(
            Refs::resolve(
                options["refs"].as_str(),
                options["callers"].as_u64().unwrap_or(0) as usize,
                options["callees"].as_u64().unwrap_or(0) as usize
            )
            .unwrap(),
            expected,
            "{argv:?}"
        );
    }
}

#[test]
fn map_paths_kinds_and_shared_selection_parse() {
    let cli = parse(&[
        "--root",
        "/repo",
        "map",
        "src",
        "/repo/docs",
        "-g",
        "*.rs",
        "-g",
        "!test.rs",
        "-e",
        "Service",
        "--regex",
        "Guide",
        "--ignore-case",
        "-k",
        "Functions,Types",
        "--kind",
        "Methods",
        "--kinds",
        "imports",
        "--private",
        "--output",
        "json",
        "--no-reindex",
    ]);
    assert_eq!(cli.global.format, Some(Format::Json));
    assert!(cli.global.no_reindex);
    let Command::Map(args) = cli.command else {
        panic!()
    };
    assert_eq!(
        args.options(),
        json!({
            "paths": ["src", "/repo/docs"],
            "glob": ["*.rs", "!test.rs"],
            "regexp": ["Service", "Guide"],
            "ignoreCase": true,
            "kinds": ["functions", "types", "method", "import"],
            "private": true
        })
    );
    let Command::Map(args) = parse(&["map"]).command else {
        panic!()
    };
    assert_eq!(args.options(), json!({"paths": ["."]}));
    for args in [
        vec!["map", "-k", "unknown"],
        vec!["map", "-k", "functions,"],
        vec!["map", "-g", "["],
        vec!["map", "-e", "["],
        vec!["map", "--threshold", "0.5"],
    ] {
        assert!(Cli::try_parse_from(std::iter::once("slopdex").chain(args)).is_err());
    }
}

#[test]
fn readme_commands_parse() {
    for args in [
        vec!["search", "validate an authenticated session"],
        vec!["search", "keep the repository index synchronized"],
        vec!["search-code", "configure the embedding provider"],
        vec!["search-md", "configure the embedding provider"],
        vec!["generate", "descriptions"],
        vec![
            "search-descriptions",
            "keep the repository index synchronized",
        ],
        vec!["help", "models", "opencode-go"],
        vec!["config", "set", "agentModel", "gpt-5.6-luna"],
        vec!["config", "set", "agentModel", "muse-spark-1.3-contributor"],
        vec!["config"],
        vec!["faq", "I want to implement a new rpc endpoint"],
        vec![
            "cross-search",
            "--cross-file-only",
            "--lines",
            "4",
            "--threshold",
            "0.9",
        ],
        vec![
            "cross-search",
            "--cross-file-only",
            "--lines",
            "4",
            "--threshold",
            "0.85-0.9",
        ],
        vec![
            "cross-search",
            "--uncommitted",
            "--cross-file-only",
            "--lines",
            "4",
            "--threshold",
            "0.9",
        ],
        vec![
            "cross-search",
            "--changed-since",
            "origin/main",
            "--threshold",
            "0.9",
        ],
        vec![
            "cross-search",
            "--source-path",
            "src/services",
            "-e",
            r"^UserService\.",
            "--threshold",
            "0.9",
        ],
        vec![
            "cross-search",
            "--cross-file-only",
            "--cohesion",
            "--threshold",
            "0.8",
        ],
        vec!["search", "...", "--threshold", "0.5"],
        vec!["cross-search", "--uncommitted", "--threshold", "0.8"],
        vec!["config", "set", "rerankerProvider", "cohere"],
        vec!["config", "set", "rerankerProvider", "jina"],
        vec!["config", "set", "rerankerProvider", "openai"],
        vec![
            "cross-search",
            "--target-root",
            "/path/to/other/repo",
            "--target-index",
            "/path/to/other/repo/.slopdex/index.sqlite",
            "--threshold",
            "0.9",
        ],
        vec!["status"],
        vec!["index", "errors", "--output", "summary"],
    ] {
        parse(&args);
    }
    assert_eq!(
        Cli::try_parse_from(["slopdex", "--version"])
            .unwrap_err()
            .kind(),
        clap::error::ErrorKind::DisplayVersion
    );
    assert_eq!(
        Cli::try_parse_from(["slopdex", "--help"])
            .unwrap_err()
            .kind(),
        clap::error::ErrorKind::DisplayHelp
    );
}

#[test]
fn threshold_ranges_include_negative_endpoints_and_exponents() {
    for (value, min, max) in [
        ("0.3", 0.3, None),
        ("-1", -1.0, None),
        ("1", 1.0, None),
        ("0.85-0.9", 0.85, Some(0.9)),
        ("-0.8--0.2", -0.8, Some(-0.2)),
        ("-1-1", -1.0, Some(1.0)),
        ("1e-2-9e-1", 0.01, Some(0.9)),
        ("-1e-1--1e-2", -0.1, Some(-0.01)),
    ] {
        let cli = parse(&["cross-search", "--threshold", value]);
        let Command::CrossSearch(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.filters.threshold, Threshold { min, max });
        let options = args.options();
        assert_eq!(options["minSimilarity"], json!(min));
        assert_eq!(options.get("maxSimilarity"), max.map(|n| json!(n)).as_ref());
    }
    for value in [
        "NaN",
        "inf",
        "-inf",
        "1.01",
        "-1.01",
        "0.9-0.9",
        "0.9-0.8",
        "-0.1--0.8",
        "0.2-NaN",
        "0.1-1.1",
        "0.1-",
        "",
    ] {
        assert!(
            Cli::try_parse_from(["slopdex", "search", "query", "--threshold", value]).is_err(),
            "accepted {value}"
        );
    }
}

#[test]
fn line_ranges_have_an_exclusive_maximum() {
    for (value, min, max) in [("1", 1, None), ("4", 4, None), ("4-10", 4, Some(10))] {
        let cli = parse(&["cross-search", "--lines", value]);
        let Command::CrossSearch(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.lines, LineRange { min, max });
        let options = args.options();
        assert_eq!(options["minLines"], json!(min));
        assert_eq!(options.get("maxLines"), max.map(|n| json!(n)).as_ref());
    }
    for value in ["0", "-1", "1-1", "2-1", "1-", "1-2-3", "1.5", ""] {
        assert!(
            Cli::try_parse_from(["slopdex", "cross-search", "--lines", value]).is_err(),
            "accepted {value}"
        );
    }

    let Command::CrossSearch(args) = parse(&["cross-search", "--min-lines", "4-10"]).command else {
        panic!()
    };
    assert_eq!(
        args.lines,
        LineRange {
            min: 4,
            max: Some(10)
        }
    );
}

#[test]
fn cross_search_has_a_selective_default_without_changing_query_defaults() {
    for command in [
        "search",
        "search-code",
        "search-descriptions",
        "search-md",
        "search-symbols",
        "faq",
    ] {
        let cli = parse(&[command, "query"]);
        let filters = match cli.command {
            Command::Search(args) => args.query.filters,
            Command::SearchCode(args)
            | Command::SearchDescriptions(args)
            | Command::SearchMd(args)
            | Command::SearchSymbols(args) => args.filters,
            Command::Faq(args) => args.query.filters,
            _ => unreachable!(),
        };
        assert_eq!(filters.options()["minSimilarity"], 0.3, "{command}");
        let help = Cli::try_parse_from(["slopdex", command, "--help"])
            .unwrap_err()
            .to_string();
        assert!(help.contains("[default: 0.3]"), "{command}: {help}");
    }
    for args in [vec!["cross-search"], vec!["cross-search", "--cohesion"]] {
        let Command::CrossSearch(args) = parse(&args).command else {
            unreachable!()
        };
        assert_eq!(args.options()["minSimilarity"], 0.8);
    }
    let help = Cli::try_parse_from(["slopdex", "cross-search", "--help"])
        .unwrap_err()
        .to_string();
    assert!(help.contains("[default: 0.8]"), "{help}");
    assert!(!help.contains("[default: 0.3]"), "{help}");
}

#[test]
fn limits_accept_none_preserve_unspecified_and_resolve_config_defaults() {
    for command in [
        "search",
        "search-code",
        "search-descriptions",
        "search-docs",
        "search-symbols",
        "faq",
        "map",
        "cross-search",
    ] {
        for (limit, expected) in [
            (None, None),
            (Some("7"), Some(json!(7))),
            (Some("none"), Some(json!("none"))),
        ] {
            let mut argv = vec![command];
            if !matches!(command, "map" | "cross-search") {
                argv.push("query");
            }
            if let Some(limit) = limit {
                argv.extend(["--limit", limit]);
            }
            let cli = parse(&argv);
            let options = match cli.command {
                Command::Map(args) => {
                    let options = args.options();
                    assert!(options.get("minSimilarity").is_none());
                    options
                }
                Command::CrossSearch(args) => {
                    assert!(args.options().get("limit").is_none());
                    args.filters.options()
                }
                Command::Faq(args) => args.query.filters.options(),
                command => command.search_request().unwrap().2,
            };
            assert_eq!(options.get("limit"), expected.as_ref(), "{argv:?}");
            for default in [json!(11), json!("none")] {
                assert_eq!(
                    crate::limits::resolve(&json!({"defaultLimit": default}), &options)
                        .unwrap()
                        .value(),
                    expected.clone().unwrap_or(default),
                    "{argv:?}"
                );
            }
            assert_eq!(
                crate::limits::resolve(&json!({}), &options)
                    .unwrap()
                    .value(),
                expected.unwrap_or(json!(4096))
            );
        }
        for invalid in ["0", "-1", "1.5", "NaN", "all"] {
            let mut argv = vec!["slopdex", command];
            if !matches!(command, "map" | "cross-search") {
                argv.push("query");
            }
            argv.extend(["--limit", invalid]);
            assert!(Cli::try_parse_from(argv).is_err(), "{command}: {invalid}");
        }
    }
}

#[test]
fn options_defaults_and_limit_validation() {
    let cli = parse(&["cross-search"]);
    let Command::CrossSearch(args) = cli.command else {
        panic!()
    };
    assert_eq!(args.matches, 5);
    assert_eq!(args.lines, LineRange { min: 2, max: None });
    assert_eq!(args.filters.threshold.min, 0.8);
    assert!(args.filters.limit.is_none());
    let cli = parse(&["cross-search", "--limit", "1"]);
    let Command::CrossSearch(args) = cli.command else {
        panic!()
    };
    assert!(args.options().get("limit").is_none());
    for option in ["--limit", "--matches", "--lines", "--min-lines"] {
        for value in ["0", "-1", "1.5", "NaN"] {
            assert!(Cli::try_parse_from(["slopdex", "cross-search", option, value]).is_err());
        }
    }
    assert!(
        Cli::try_parse_from([
            "slopdex",
            "faq",
            "query",
            "--describe-full-file-threshold",
            "0.8"
        ])
        .is_err()
    );
}

#[test]
fn validates_combinations_before_opening_an_index() {
    for args in [
        vec!["cross-search", "--target-root", "other"],
        vec!["cross-search", "--target-index", "other.sqlite"],
        vec!["cross-search", "--target-config", "other.json"],
        vec!["status", "--force-reindex"],
        vec!["search", "query", "--regexp", "["],
    ] {
        assert!(Cli::try_parse_from(std::iter::once("slopdex").chain(args)).is_err());
    }
    for args in [
        vec!["search", "query", "--output", "clusters"],
        vec!["map", "--output", "clusters"],
        vec!["cross-search", "--cohesion", "--output", "clusters"],
    ] {
        let cli = Cli::try_parse_from(std::iter::once("slopdex").chain(args)).unwrap();
        assert!(cli.validate().is_err());
    }
    parse(&[
        "--root",
        "/tmp/repo",
        "search",
        "query",
        "--no-reindex",
        "--output",
        "json",
    ]);
    parse(&["config", "set", "agentModel", "model"]);
    parse(&["config", "set", "rerankerCandidates", "100"]);
    parse(&[
        "update",
        "--force-reindex",
        "--yes-really-rebuild-the-index",
    ]);
    parse(&["generate", "descriptions"]);
    parse(&["search", "query", "--code", "--md", "--regex", "foo"]);
}

#[test]
fn argument_errors_cover_global_constraints_and_conflicting_model_selections() {
    for args in [
        vec!["status", "--rebuild-on-divergence"],
        vec![
            "status",
            "--no-reindex",
            "--force-reindex",
            "--yes-really-rebuild-the-index",
        ],
        vec!["refresh", "--target", "main"],
        vec!["search", " \n\t"],
        vec!["status", "--model", " "],
        vec!["status", "--dimensions", "18446744073709551616"],
        vec!["index-errors"],
        vec!["reindex-files"],
        vec!["index", "reindex-files"],
        vec!["index", "reindex-files", "--callables"],
        vec!["generate"],
        vec!["generate", "descriptions", "--callables"],
        vec!["descriptions", "enable"],
        vec!["models"],
        vec!["help", "models", "--description-provider", "openai"],
        vec!["status", "--description-fallback-model", "old-model"],
        vec!["search-code", "query", "--regexp", "(?=lookahead)"],
        vec!["search-code", "query", "--regexp", r"(a)\1"],
    ] {
        let error = Cli::try_parse_from(std::iter::once("slopdex").chain(args.iter().copied()))
            .unwrap_err();
        assert_eq!(error.exit_code(), 2, "{args:?}");
    }
    parse(&["help", "models", "opencode"]);
    let cli = parse(&[
        "status",
        "--agent-command",
        "[\"opencode\",\"run\"]",
        "--agent-model",
        "opencode-go/deepseek-v4.1-flash",
        "--trust-repo-commands",
    ]);
    assert_eq!(
        cli.global.agent_command.as_deref(),
        Some("[\"opencode\",\"run\"]")
    );
    assert_eq!(
        cli.global.agent_model.as_deref(),
        Some("opencode-go/deepseek-v4.1-flash")
    );
    parse(&["config", "set", "agentModel", "same"]);
    parse(&[
        "update",
        "--target",
        "HEAD",
        "--rebuild-on-divergence",
        "--yes-really-rebuild-the-index",
    ]);
    let cli = parse(&["search-descriptions", "--", "--literal query"]);
    let Command::SearchDescriptions(args) = cli.command else {
        panic!()
    };
    assert_eq!(args.query, "--literal query");
}

#[test]
fn every_argument_has_help_text() {
    fn check(command: &clap::Command, path: &str) {
        for arg in command.get_arguments() {
            let id = arg.get_id().as_str();
            if matches!(id, "help" | "version") {
                continue;
            }
            let help = arg.get_help().map(ToString::to_string).unwrap_or_default();
            assert!(!help.trim().is_empty(), "{path} --{id} has no help text");
        }
        for subcommand in command.get_subcommands() {
            check(subcommand, &format!("{path} {}", subcommand.get_name()));
        }
    }
    check(&Cli::command(), "slopdex");
}

#[test]
fn help_resolves_command_paths_and_aliases_before_running() {
    for path in [
        vec!["search"],
        vec!["search-md"],
        vec!["index", "errors"],
        vec!["generate", "descriptions"],
        vec!["cross-search"],
    ] {
        let path: Vec<String> = path.into_iter().map(String::from).collect();
        let mut command = help_command(&path).unwrap();
        let mut rendered = Vec::new();
        command.write_long_help(&mut rendered).unwrap();
        let rendered = String::from_utf8(rendered).unwrap();
        assert!(rendered.contains("Usage: slopdex "), "{path:?}: {rendered}");
        let cli = parse(
            &[
                &["help"],
                path.iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .as_slice(),
            ]
            .concat(),
        );
        assert!(matches!(
            cli.command,
            Command::Help {
                topic: Some(HelpTopic::Command(ref found))
            } if *found == path
        ));
    }
    assert!(help_command(&["nonexistent".to_owned()]).is_err());
    assert!(help_command(&["index".to_owned(), "missing".to_owned()]).is_err());
    let cli = Cli::try_parse_from(["slopdex", "help", "nonexistent"]).unwrap();
    assert!(
        cli.validate()
            .unwrap_err()
            .to_string()
            .contains("nonexistent")
    );
    assert!(matches!(
        parse(&["help", "models"]).command,
        Command::Help {
            topic: Some(HelpTopic::Models { provider: None })
        }
    ));
    assert!(matches!(
        parse(&["help"]).command,
        Command::Help { topic: None }
    ));
}

#[test]
fn repository_endpoint_trust_is_an_explicit_global_flag() {
    assert!(!parse(&["status"]).global.trust_repo_endpoints);
    assert!(
        parse(&["--trust-repo-endpoints", "status"])
            .global
            .trust_repo_endpoints
    );
    assert!(
        parse(&["search", "q", "--trust-repo-endpoints"])
            .global
            .trust_repo_endpoints
    );
}
