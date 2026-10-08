use super::*;
use crate::cli::{CommandMode, test_support::parse};
use clap::CommandFactory;

#[test]
fn clap_definition_is_consistent() {
    Cli::command().debug_assert();
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
    for index in [
        "callable code",
        "generated callable/file description",
        "Markdown content",
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
            "--format",
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
        "search-md",
        "search-symbols",
        "describe",
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
            Command::Describe(args) => args.query.filters.options(),
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
            json!(["^Service\\.", "Setup", "Guide"]),
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
fn search_commands_select_explicit_indexes_and_structural_mode() {
    let plain = parse(&["search", "query"]);
    let (_, kind, options, descriptions) = plain.command.search_request().unwrap();
    assert_eq!(kind, "search");
    assert!(!descriptions);
    for selector in ["code", "descriptions", "md", "symbols"] {
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
            for selector in ["code", "descriptions", "md"] {
                assert_eq!(options[selector], false);
            }
        } else {
            assert_eq!(kind, "search-symbols");
        }
    }
    for selector in ["--code", "--md", "--descriptions"] {
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
fn symbol_selection_defaults_and_scalar_threshold_validation() {
    for command in [
        "map",
        "search",
        "search-code",
        "search-descriptions",
        "search-md",
        "search-symbols",
        "describe",
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
            Command::Describe(args) => args.query.filters.options(),
            Command::CrossSearch(args) => args.options(),
            _ => unreachable!(),
        };
        let defaults = options(parse(&argv));
        assert!(defaults.get("symbolQuery").is_none(), "{command}");
        assert!(defaults.get("symbolThreshold").is_none(), "{command}");
        let mut selected = argv.clone();
        selected.extend(["-q", "readFile", "--symbol-query", "write file"]);
        let selected = options(parse(&selected));
        assert_eq!(selected["symbolQuery"], json!(["readFile", "write file"]));
        assert!(selected.get("symbolThreshold").is_none());
        for value in ["", "   ", "_::.!🙂"] {
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
        "--format",
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
    assert_eq!(args.options(), json!({}));
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
        vec!["config", "set", "descriptionModel", "gpt-5.6-luna"],
        vec![
            "config",
            "set",
            "descriptionFallbackModel",
            "muse-spark-1.3-contributor",
        ],
        vec!["config"],
        vec!["describe", "I want to implement a new rpc endpoint"],
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
        vec!["index", "errors", "--format", "summary"],
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
        "describe",
    ] {
        let cli = parse(&[command, "query"]);
        let filters = match cli.command {
            Command::Search(args) => args.query.filters,
            Command::SearchCode(args)
            | Command::SearchDescriptions(args)
            | Command::SearchMd(args)
            | Command::SearchSymbols(args) => args.filters,
            Command::Describe(args) => args.query.filters,
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
            "describe",
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
        vec!["search", "query", "--format", "clusters"],
        vec!["map", "--format", "clusters"],
        vec!["cross-search", "--cohesion", "--format", "clusters"],
        vec!["help", "models", "--description-provider", "openai"],
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
        "--format",
        "json",
    ]);
    parse(&["config", "set", "descriptionModel", "model"]);
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
        vec!["search-code", "query", "--regexp", "(?=lookahead)"],
        vec!["search-code", "query", "--regexp", r"(a)\1"],
    ] {
        let error = Cli::try_parse_from(std::iter::once("slopdex").chain(args.iter().copied()))
            .unwrap_err();
        assert_eq!(error.exit_code(), 2, "{args:?}");
    }
    for (args, message) in [
        (
            vec![
                "help",
                "models",
                "opencode",
                "--description-provider",
                "opencode-go",
            ],
            "must match",
        ),
        (
            vec!["help", "models", "--description-provider", "openai"],
            "opencode or opencode-go",
        ),
    ] {
        let cli =
            Cli::try_parse_from(std::iter::once("slopdex").chain(args.iter().copied())).unwrap();
        assert!(
            cli.validate().unwrap_err().to_string().contains(message),
            "{args:?}"
        );
    }
    parse(&[
        "help",
        "models",
        "opencode",
        "--description-provider",
        "opencode",
    ]);
    parse(&["config", "set", "descriptionModel", "same"]);
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
