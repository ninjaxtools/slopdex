"""Offline exclusive-arm regressions; invoke only a local fake CLI."""

import collections
from contextlib import closing
import hashlib
import io
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import run as runner


class ExclusiveArmTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="typescript-exclusive-arms-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()

    def write_calls(self, records):
        path = self.root / "calls.jsonl"
        path.write_text("\n" + "\n".join(json.dumps(record) for record in records) + "\n\n")
        return path

    def test_default_arms_and_exact_readme_instructions(self):
        self.assertEqual(runner.DEFAULT_ARMS, ["off", "map"])
        self.assertEqual(set(runner.ARM_INSTRUCTIONS), {"off", "map"})
        self.assertEqual(runner.ARM_NAVIGATION, {"map": ("map",)})
        readme = (runner.ROOT / "README.md").read_text()
        blocks = ["\n".join(line.removeprefix("> ") for line in block.splitlines())
                  for block in re.findall(r"^> ```text\n(.*?)^> ```$", readme, re.M | re.S)]
        snippets = [block for block in blocks if "`slopdex map " in block]
        self.assertEqual(len(snippets), 1)
        instructions = runner.arm_instructions("map")
        self.assertEqual(instructions.count(snippets[0]), 1)
        self.assertEqual(runner.readme_agent_instruction("map"), snippets[0])
        self.assertEqual(set(re.findall(r"`slopdex ([\w-]+)", instructions)), {"map", "faq", "refactor"})
        self.assertEqual(runner.ARM_OPTIONAL, {"map": ("faq", "refactor")})
        for argv in (["faq", "How are types checked?"], ["refactor", "a.ts::f", "b.ts::g"]):
            with self.subTest(argv=argv):
                self.assertTrue(runner.slopdex_call_allowed("map", argv, map_private=True))
                self.assertFalse(runner.slopdex_call_allowed("off", argv))
                # No command is mandatory by default; faq/refactor alone are valid trials.
                self.assertEqual(runner.ARM_REQUIRED, {})
                self.assertEqual(runner.slopdex_protocol_violations("map", [{"argv": argv, "exit_code": 0}]), [])

    def test_instructions_read_current_readme_verbatim(self):
        snippet = '- custom symbol guidance: `slopdex map -q "exact concept" src`\n- preserve this second line'
        (self.root / "README.md").write_text(
            "> ```text\n" + "\n".join("> " + line for line in snippet.splitlines()) + "\n> ```")
        with patch.object(runner, "ROOT", self.root):
            self.assertEqual(runner.readme_agent_instruction("map"), snippet)
            self.assertIn(snippet, runner.arm_instructions("map"))
            self.assertNotIn(snippet, runner.arm_instructions("off"))

    def test_map_visibility_instructions_preserve_default_and_override_examples(self):
        default = runner.arm_instructions("map")
        self.assertEqual(runner.arm_instructions("map", map_private=None), default)
        self.assertIn(runner.readme_agent_instruction("map"), default)
        for private in (True, False):
            with self.subTest(map_private=private):
                instructions = runner.arm_instructions("map", map_private=private)
                examples = re.findall(r"`slopdex map [^`]+`", instructions)
                self.assertTrue(examples)
                self.assertTrue(all(("--private" in example) == private for example in examples))
                action = "include" if private else "omit"
                self.assertRegex(instructions, rf"(?i)(?:required|always)[^\n]*{action}[^\n]*--private")
                self.assertEqual(runner.arm_instructions("off", map_private=private),
                                 runner.arm_instructions("off"))

    def test_map_visibility_overrides_follow_changed_readme_without_private(self):
        snippet = '- custom navigation: `slopdex map -q "exact concept" src`\n- keep this guidance'
        (self.root / "README.md").write_text(
            "> ```text\n" + "\n".join("> " + line for line in snippet.splitlines()) + "\n> ```")
        with patch.object(runner, "ROOT", self.root):
            self.assertIn(snippet, runner.arm_instructions("map", map_private=None))
            self.assertIn('`slopdex map --private -q "exact concept" src`',
                          runner.arm_instructions("map", map_private=True))
            self.assertIn(snippet, runner.arm_instructions("map", map_private=False))

    def test_baseline_disallows_every_slopdex_invocation(self):
        self.assertEqual(runner.slopdex_protocol_violations("off", []), [])
        for argv in ([], ["--help"], ["--version"], ["map", "target.go"], ["search", "query"]):
            with self.subTest(argv=argv):
                self.assertFalse(runner.slopdex_call_allowed("off", argv))
                self.assertEqual(runner.slopdex_protocol_violations("off", [{"argv": argv, "exit_code": 0}]), [
                    {"reason": "command_not_allowed", "command": runner.slopdex_command(argv), "argv": argv}])

    def test_mixed_legacy_and_interleaved_events_count_partial_start_once(self):
        legacy = {"argv": ["map", "legacy.go"], "time": 1}
        records = [
            legacy,
            {"event": "start", "id": "first", "argv": ["search", "query"],
             "command": "search", "allowed": True, "time": 2},
            {"event": "start", "id": "second", "argv": ["status"],
             "command": "status", "allowed": False, "time": 3},
            {"event": "finish", "id": "second", "exit_code": 126, "end_time": 4},
            {"argv": ["--version"], "exit_code": 0, "time": 5},
            {"event": "start", "id": "partial", "argv": ["search", "unfinished"],
             "command": "search", "allowed": True, "time": 6},
            {"event": "finish", "id": "first", "exit_code": 0, "end_time": 7},
        ]
        calls = runner.read_slopdex_calls(self.write_calls(records))
        self.assertEqual(len(calls), 5)
        self.assertEqual(calls[0], legacy)
        self.assertEqual([call["argv"] for call in calls],
                         [record["argv"] for record in records if "argv" in record])
        self.assertEqual(calls[1]["exit_code"], 0)
        self.assertEqual(calls[1]["end_time"], 7)
        self.assertTrue(calls[1]["allowed"])
        self.assertEqual(calls[2]["exit_code"], 126)
        self.assertFalse(calls[2]["allowed"])
        self.assertNotIn("exit_code", calls[-1])
        self.assertEqual(runner.slopdex_command_counts(calls),
                         {"map": 1, "search": 2, "status": 1, "version": 1})

    def test_flat_legacy_logs_remain_supported(self):
        records = [{"argv": ["map", "src"]}, {"argv": ["search", "query"], "exit_code": 0}]
        self.assertEqual(runner.read_slopdex_calls(self.write_calls(records)), records)

    def test_orphan_finish_is_rejected(self):
        path = self.write_calls([{"event": "finish", "id": "missing", "exit_code": 0, "end_time": 1}])
        with self.assertRaisesRegex(ValueError, "no start"):
            runner.read_slopdex_calls(path)

    def test_policy_allows_only_assigned_navigation_and_help_or_version(self):
        blocked = ("search-code", "search-md", "search-descriptions", "cross-search",
                   "status", "index-errors", "update", "refresh", "describe", "descriptions",
                   "reindex-files", "models", "config", "typo")
        allowed = (["map", "target"], ["--no-reindex", "map", "target"],
                   ["--output", "json", "map", "target"], ["--output=json", "map", "target"],
                   ["--", "map", "target"], [], ["--help"], ["-h"], ["--version"], ["-V"],
                   ["help", "search"], ["map", "--help"], ["search", "--help"],
                   ["map", "--version"])
        for argv in allowed:
            with self.subTest(argv=argv):
                self.assertTrue(runner.slopdex_call_allowed("map", argv))
        for command in ("search", *blocked):
            for argv in ([command, "target"], ["--root", "map", "--no-reindex", command, "target"]):
                with self.subTest(argv=argv):
                    self.assertFalse(runner.slopdex_call_allowed("map", argv))
        for argv in (["--unsupported", "map"], ["--format", "json", "map"],
                     ["--format=json", "map"], ["--root"], ["search", "--", "--help"]):
            with self.subTest(argv=argv):
                self.assertFalse(runner.slopdex_call_allowed("map", argv))

    def test_map_visibility_policy_checks_options_not_separator_positionals(self):
        examples = (
            (["map", "target.go"], False),
            (["map", "--private", "target.go"], True),
            (["--output=json", "map", "target.go", "--private"], True),
            (["map", "--", "--private"], False),
            (["--", "map", "--private"], True),
            (["map", "--", "map", "--private"], False),
            (["map", "--private", "--", "--private"], True),
        )
        for argv, has_private in examples:
            with self.subTest(argv=argv):
                self.assertTrue(runner.slopdex_call_allowed("map", argv))
                self.assertTrue(runner.slopdex_call_allowed("map", argv, map_private=None))
                for private in (True, False):
                    self.assertEqual(runner.slopdex_call_allowed("map", argv, map_private=private),
                                     private == has_private)
        for private in (None, True, False):
            for argv in ([], ["--help"], ["--version"], ["map", "--help"],
                         ["map", "--private", "--help"], ["map", "--private", "--version"],
                         ["map", "-ih"], ["search", "--help"], ["help", "map"]):
                with self.subTest(map_private=private, information_argv=argv):
                    self.assertTrue(runner.slopdex_call_allowed("map", argv, map_private=private))
                    self.assertFalse(runner.slopdex_call_allowed("off", argv, map_private=private))
            for argv in (["search", "--private"], ["map", "--private", "--index", "other.sqlite"]):
                self.assertFalse(runner.slopdex_call_allowed("map", argv, map_private=private))

    @patch.object(runner, "ARM_REQUIRED", {"map": ("map",)})  # Optional requirement machinery.
    def test_map_visibility_protocol_requires_success_under_matching_policy(self):
        missing = {"reason": "required_command_not_used_successfully", "command": "map"}
        for private in (True, False):
            matching = ["map", "--private", "target.go"] if private else ["map", "target.go"]
            mismatching = ["map", "target.go"] if private else ["map", "--private", "target.go"]
            denied = {"reason": "command_not_allowed", "command": "map", "argv": mismatching}
            with self.subTest(map_private=private):
                self.assertEqual(runner.slopdex_protocol_violations("map", [
                    {"argv": matching, "exit_code": 0}], map_private=private), [])
                self.assertEqual(runner.slopdex_protocol_violations("map", [
                    {"argv": mismatching, "exit_code": 0}], map_private=private), [denied, missing])
                self.assertEqual(runner.slopdex_protocol_violations("map", [
                    {"argv": matching, "exit_code": 0}, {"argv": mismatching, "exit_code": 126}],
                    map_private=private), [denied])
                for calls in ([], [{"argv": matching, "exit_code": 9}], [{"argv": matching}],
                              [{"argv": ["map", "--private", "--help"], "exit_code": 0}]):
                    self.assertEqual(runner.slopdex_protocol_violations("map", calls,
                                                                      map_private=private), [missing])

    def test_cli_visibility_dry_runs_are_offline_and_leave_stores_untouched(self):
        cache, results, output = (self.root / name for name in ("cache", "results", "job"))
        with patch.object(runner, "executable", side_effect=AssertionError("external executable")), \
                patch.object(runner, "command", side_effect=AssertionError("external command")), \
                patch.object(runner, "prepare_map_index", side_effect=AssertionError("index preparation")), \
                patch.object(runner, "run_trial", side_effect=AssertionError("model call")):
            for private, flags in ((None, []), (True, ["--map-private"]), (False, ["--no-map-private"])):
                with self.subTest(map_private=private), patch("sys.stdout", new=io.StringIO()) as stdout:
                    runner.main(["--cache", str(cache), "--results", str(results), "run",
                                 "--output", str(output), "--dry-run", *flags])
                    plan = json.loads(stdout.getvalue())
                self.assertEqual(plan["arm_instructions"], {
                    arm: runner.arm_instructions(arm, map_private=private) for arm in ("off", "map")})
                self.assertEqual({trial["arm"] for trial in plan["trials"]}, {"off", "map"})
                self.assertEqual(plan["count"], len(plan["trials"]))
                for path in (cache, results, output):
                    self.assertFalse(path.exists())
            for flag in ("--map-private", "--no-map-private"):
                with self.subTest(flag=flag), self.assertRaisesRegex(ValueError, "requires the map arm"):
                    runner.main(["--results", str(results), "run", "--arms", "off", "--dry-run", flag])

    @patch.object(runner, "ARM_REQUIRED", {"map": ("map",)})  # Optional requirement machinery.
    def test_missing_required_command_for_unused_failed_partial_or_help_only(self):
        examples = ([], [{"argv": ["map", "target"], "exit_code": 9}],
                    [{"argv": ["map", "target"], "event": "start"}],
                    [{"argv": ["map", "legacy target"]}],
                    [{"argv": ["--help"], "exit_code": 0},
                     {"argv": ["map", "--help"], "exit_code": 0},
                     {"argv": ["--version"], "exit_code": 0}],
                    [{"argv": ["map", "-ih"], "exit_code": 0}])
        for calls in examples:
            with self.subTest(calls=calls):
                self.assertEqual(runner.slopdex_protocol_violations("map", calls), [
                    {"reason": "required_command_not_used_successfully", "command": "map"}])
        self.assertEqual(runner.slopdex_protocol_violations("map", [
            {"argv": ["--no-reindex", "map", "target"], "exit_code": 0}]), [])

    def test_protected_index_configuration_cannot_be_overridden(self):
        options = ("--root", "--config", "--index", "--provider", "--model", "--dimensions",
                   "--description-provider", "--agent-command", "--agent-model", "--description-fallback-model",
                   "--reranker-candidates")
        controls = ("--force-reindex", "--yes-really-rebuild-the-index", "--rebuild-on-divergence")
        for option in options:
            for argv in ([option, "value", "map"], ["map", option, "value"], ["map", f"{option}=value"]):
                with self.subTest(argv=argv):
                    self.assertFalse(runner.slopdex_call_allowed("map", argv))
        for option in controls:
            self.assertFalse(runner.slopdex_call_allowed("map", ["map", option]))
        self.assertTrue(runner.slopdex_call_allowed("map", ["map", "--", "--index"]))
        self.assertTrue(runner.slopdex_call_allowed("map", ["map", "Explain --index handling"]))

    def test_short_help_clusters_and_catalog_help_do_not_count_as_navigation(self):
        for argv in (["map", "-ih"], ["search", "query", "-ih"], ["map", "-hi"]):
            self.assertEqual(runner.slopdex_command(argv), "help")
        self.assertEqual(runner.slopdex_command(["map", "-ehelper"]), "map")
        self.assertEqual(runner.slopdex_command(["map", "--", "-ih"]), "map")
        for argv in (["help", "models"], ["help", "models", "opencode-go"], ["help", "--output=json", "models"]):
            self.assertEqual(runner.slopdex_command(argv), "models")
            for arm in ("off", "map"):
                self.assertFalse(runner.slopdex_call_allowed(arm, argv))

    def test_wrong_command_violates_protocol_even_after_success(self):
        for command in ("search", "search-code", "cross-search", "status"):
            for exit_code in (0, 126):
                with self.subTest(command=command, exit_code=exit_code):
                    calls = [{"argv": ["map", "target"], "exit_code": 0},
                             {"argv": [command], "allowed": False, "exit_code": exit_code}]
                    self.assertEqual(runner.slopdex_protocol_violations("map", calls), [
                        {"reason": "command_not_allowed", "command": command, "argv": [command]}])

    def create_index(self):
        directory = self.root / "cache"
        directory.mkdir()
        runner.write_json(directory / "config.json", {})
        index = directory / "index.sqlite"
        metadata = {"identity": json.dumps({"schema": 3, "root": "/original/root"}),
                     "checkpoint": "fixture-commit", "generation": "1", "descriptions_enabled": "false"}
        with closing(sqlite3.connect(index)) as db, db:
            db.execute("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT)")
            db.executemany("INSERT INTO metadata VALUES (?, ?)", metadata.items())
        binary = Path(str(index) + ".symbols.usearch")
        binary.write_bytes(b"fake warm symbol ANN")
        runner.write_json(Path(str(binary) + ".manifest.json"), {
            "version": 1, "generation": 1, "dimensions": 3,
            "binary_hash": hashlib.sha256(binary.read_bytes()).hexdigest(), "vectors": {},
        })
        return directory

    @patch.object(runner, "TRIAL_SYMBOL_ONLY", True)  # Fixture builds symbol-only sidecars.
    def test_generated_map_wrapper_blocks_executes_and_journals_with_symbol_snapshot(self):
        directory = self.create_index()
        source = directory / "index.sqlite"
        source_bytes = source.read_bytes()
        sandbox, workspace = self.root / "sandbox", self.root / "workspace"
        sandbox.mkdir()
        if os.name == "posix":
            physical_workspace = self.root / "physical-workspace"
            physical_workspace.mkdir()
            workspace.symlink_to(physical_workspace, target_is_directory=True)
        else:
            workspace.mkdir()
        executions = self.root / "fake-executions.jsonl"
        fake_cli = self.root / "fake-slopdex"
        fake_cli.write_text(f"""#!{sys.executable}
import json, pathlib, sys
assert pathlib.Path(sys.argv[sys.argv.index('--root') + 1]).resolve() == pathlib.Path.cwd().resolve()
with open({str(executions)!r}, 'a') as stream:
    stream.write(json.dumps(sys.argv[1:]) + '\\n')
print('fake CLI executed')
sys.exit(9 if '--fixture-fail' in sys.argv[1:] else 0)
""")
        fake_cli.chmod(0o755)
        bin_dir, log = runner.install_wrapper(sandbox, workspace, "map", str(fake_cli),
                                              directory, "fixture-commit")
        copied = sandbox / "index.sqlite"
        with closing(sqlite3.connect(copied)) as db:
            metadata = dict(db.execute("SELECT key, value FROM metadata"))
        self.assertEqual(Path(json.loads(metadata["identity"])["root"]).resolve(), workspace.resolve())
        self.assertEqual(metadata["checkpoint"], "fixture-commit")
        self.assertEqual(source.read_bytes(), source_bytes)
        self.assertNotIn("active_embedding_profile", metadata)
        before = runner.native_ann_snapshot(source, symbol_only=True)
        self.assertEqual(runner.native_ann_snapshot(copied, symbol_only=True), before)
        for suffix in before["artifacts"]:
            original, destination = Path(str(source) + suffix), Path(str(copied) + suffix)
            self.assertEqual(original.read_bytes(), destination.read_bytes())
            self.assertEqual(original.stat().st_mtime_ns, destination.stat().st_mtime_ns)
            self.assertFalse(os.path.samefile(original, destination))

        expected, forwarded = [], []
        env = {**os.environ, "PATH": str(bin_dir) + os.pathsep + os.environ.get("PATH", "")}

        def invoke(argv, command, allowed, exit_code):
            result = subprocess.run(["slopdex", *argv], cwd=workspace, env=env,
                                    text=True, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, exit_code, result.stderr)
            if allowed:
                forwarded.append(argv)
                self.assertIn("fake CLI executed", result.stdout)
            else:
                self.assertNotIn("fake CLI executed", result.stdout)
                self.assertIn("unavailable", result.stderr)
            expected.append((argv, command, allowed, exit_code))
            actual_executions = [json.loads(line) for line in executions.read_text().splitlines()] if executions.exists() else []
            prefix = ["--root", str(workspace), "--config", str(sandbox / "slopdex.json"),
                      "--index", str(copied), "--no-reindex"]
            self.assertEqual(actual_executions, [prefix + arguments for arguments in forwarded])

        primary, opposite = "map", "search"
        invoke([opposite, "target"], opposite, False, 126)
        self.assertFalse(executions.exists())
        invoke([primary, "target"], primary, True, 0)
        invoke(["map", "--private", "-g", "*.go", "-q", "validate session"], "map", True, 0)
        invoke([primary, "--index", "other.sqlite"], primary, False, 126)
        invoke(["help", "models"], "models", False, 126)
        for command in ("search-code", "search-md", "search-descriptions", "cross-search", "status", "update"):
            invoke([command, "target"], command, False, 126)
        invoke(["--root", "map", "--output=json", opposite, "target"], opposite, False, 126)
        for argv, command in ((["--help"], "help"), (["help", opposite], "help"),
                               ([opposite, "--help"], "help"), ([primary, "-ih"], "help"), (["--version"], "version")):
            invoke(argv, command, True, 0)
        invoke([primary, "--fixture-fail"], primary, True, 9)

        events = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertEqual(len(events), 2 * len(expected))
        self.assertEqual(len({event["id"] for event in events}), len(expected))
        for index, (argv, command, allowed, exit_code) in enumerate(expected):
            start, finish = events[2 * index:2 * index + 2]
            self.assertEqual(start["event"], "start")
            self.assertEqual(start["argv"], argv)
            self.assertEqual(start["command"], command)
            self.assertIs(start["allowed"], allowed)
            self.assertEqual(finish["event"], "finish")
            self.assertEqual(finish["id"], start["id"])
            self.assertEqual(finish["exit_code"], exit_code)
            self.assertGreaterEqual(finish["end_time"], start["time"])
        calls = runner.read_slopdex_calls(log)
        self.assertEqual(len(calls), len(expected))
        for call, (argv, command, allowed, exit_code) in zip(calls, expected):
            self.assertEqual((call["argv"], call["command"], call["allowed"], call["exit_code"]),
                             (argv, command, allowed, exit_code))
        counts = dict(collections.Counter(command for _, command, _, _ in expected))
        self.assertEqual(runner.slopdex_command_counts(calls), counts)
        self.assertEqual(runner.saved_slopdex_commands(sandbox, {"slopdex_calls": 999}), counts)
        self.assertEqual(runner.slopdex_protocol_violations("map", calls), [
            {"reason": "command_not_allowed", "command": command, "argv": argv}
            for argv, command, allowed, _ in expected if not allowed])
        self.assertEqual(source.read_bytes(), source_bytes)
        self.assertEqual(runner.native_ann_snapshot(copied, symbol_only=True), before)

    @unittest.skipUnless(os.name == "posix", "fake CLI execution uses a shebang")
    @patch.object(runner, "TRIAL_SYMBOL_ONLY", True)  # Fixture builds symbol-only sidecars.
    def test_generated_wrapper_enforces_map_visibility_and_journals_original_argv(self):
        directory = self.create_index()
        physical_workspace = self.root / "physical-workspace"
        physical_workspace.mkdir()
        workspace = self.root / "workspace"
        workspace.symlink_to(physical_workspace, target_is_directory=True)
        executions = self.root / "executions.jsonl"
        fake_cli = self.root / "fake-slopdex"
        fake_cli.write_text(f"""#!{sys.executable}
import json, pathlib, sys
assert pathlib.Path(sys.argv[sys.argv.index('--root') + 1]).resolve() == pathlib.Path.cwd().resolve()
with open({str(executions)!r}, 'a') as stream:
    stream.write(json.dumps(sys.argv[1:]) + '\\n')
""")
        fake_cli.chmod(0o755)
        forwarded = []
        for private in (True, False):
            sandbox = self.root / ("private" if private else "public")
            sandbox.mkdir()
            bin_dir, log = runner.install_wrapper(sandbox, workspace, "map", str(fake_cli),
                                                  directory, "fixture-commit", map_private=private)
            expected = []
            for argv, has_private in ((["map", "target.go"], False),
                                      (["map", "--private", "target.go"], True),
                                      (["map", "--", "--private"], False),
                                      (["map", "--private", "--", "--private"], True),
                                      (["map", "--private", "--help"], None)):
                allowed = has_private is None or has_private == private
                code = 0 if allowed else 126
                result = subprocess.run([str(bin_dir / "slopdex"), *argv], cwd=workspace,
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, code, result.stderr)
                expected.append((argv, allowed, code))
                if allowed:
                    forwarded.append((sandbox, argv))
                else:
                    self.assertIn("unavailable", result.stderr)
            calls = runner.read_slopdex_calls(log)
            self.assertEqual([(call["argv"], call["allowed"], call["exit_code"]) for call in calls], expected)
            events = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertEqual(len(events), 2 * len(expected))
            for index, (argv, allowed, code) in enumerate(expected):
                start, finish = events[2 * index:2 * index + 2]
                self.assertEqual(start["argv"], argv)
                self.assertIs(start["allowed"], allowed)
                self.assertEqual((start["event"], finish["event"]), ("start", "finish"))
                self.assertEqual(start["id"], finish["id"])
                self.assertEqual(finish["exit_code"], code)
            self.assertEqual(runner.slopdex_protocol_violations("map", calls, map_private=private), [
                {"reason": "command_not_allowed", "command": "map", "argv": argv}
                for argv, allowed, _ in expected if not allowed])
        actual = [json.loads(line) for line in executions.read_text().splitlines()]
        self.assertEqual(len(actual), len(forwarded))
        for execution, (sandbox, argv) in zip(actual, forwarded):
            self.assertEqual(execution[0:7:2], ["--root", "--config", "--index", "--no-reindex"])
            for position, path in ((1, workspace), (3, sandbox / "slopdex.json"), (5, sandbox / "index.sqlite")):
                self.assertEqual(Path(execution[position]).resolve(), path.resolve())
            # Argument spelling is deliberately preserved, unlike filesystem identity.
            self.assertEqual(execution[7:], argv)


if __name__ == "__main__":
    unittest.main()
