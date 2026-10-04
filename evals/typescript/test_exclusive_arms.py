"""Offline exclusive-arm regressions; invoke only a local fake CLI."""

import collections
from contextlib import closing
import hashlib
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
        self.root = Path(temporary.name)

    def write_calls(self, records):
        path = self.root / "calls.jsonl"
        path.write_text("\n" + "\n".join(json.dumps(record) for record in records) + "\n\n")
        return path

    def test_default_arms_and_exact_readme_instructions(self):
        self.assertEqual(runner.DEFAULT_ARMS, ["map", "search"])
        readme = (runner.ROOT / "README.md").read_text()
        blocks = ["\n".join(line.removeprefix("> ") for line in block.splitlines())
                  for block in re.findall(r"^> ```text\n(.*?)^> ```$", readme, re.M | re.S)]
        for arm in ("map", "search"):
            with self.subTest(arm=arm):
                snippets = [block for block in blocks if f"`slopdex {arm} " in block]
                self.assertEqual(len(snippets), 1)
                instructions = runner.arm_instructions(arm)
                self.assertEqual(instructions.count(snippets[0]), 1)
                self.assertEqual(runner.readme_agent_instruction(arm), snippets[0])
                self.assertEqual(set(re.findall(r"`slopdex ([\w-]+)", instructions)), {arm})

    def test_instructions_read_current_readme_verbatim(self):
        snippets = {
            "map": '- custom structural guidance: `slopdex map -e "exact regex" src`\n- preserve this second line',
            "search": '- custom semantic guidance: `slopdex search "exact query" --threshold 0.7`',
        }
        (self.root / "README.md").write_text("\n".join(
            "> ```text\n" + "\n".join("> " + line for line in snippet.splitlines()) + "\n> ```"
            for snippet in snippets.values()))
        with patch.object(runner, "ROOT", self.root):
            for arm, snippet in snippets.items():
                with self.subTest(arm=arm):
                    self.assertEqual(runner.readme_agent_instruction(arm), snippet)
                    self.assertIn(snippet, runner.arm_instructions(arm))
                    self.assertIn(snippet, runner.arm_instructions("map-search"))

    def test_combined_arm_requires_successful_map_and_search_and_enforces_controls(self):
        map_call = {"argv": ["map", "--private", "target.go"], "exit_code": 0}
        search_call = {"argv": ["search", "query", "--threshold", "0.3", "--limit", "50"], "exit_code": 0}
        for call in (map_call, search_call):
            self.assertTrue(runner.slopdex_call_allowed("map-search", call["argv"]))
        self.assertEqual(runner.slopdex_protocol_violations("map-search", [map_call, search_call]), [])
        for calls, missing in (([], ["map", "search"]), ([map_call], ["search"]),
                               ([search_call], ["map"]),
                               ([map_call, {**search_call, "exit_code": 1}], ["search"]),
                               ([map_call, {"argv": ["search", "--help"], "exit_code": 0}], ["search"])):
            with self.subTest(calls=calls):
                self.assertEqual(runner.slopdex_protocol_violations("map-search", calls), [
                    {"reason": "required_command_not_used_successfully", "command": command} for command in missing])
        for argv in (["status"], ["search-code", "query"], ["help", "models"],
                     ["map", "--index", "other.sqlite"], ["search", "--root=other", "query"]):
            self.assertFalse(runner.slopdex_call_allowed("map-search", argv))
            self.assertEqual(runner.slopdex_protocol_violations("map-search", [map_call, search_call, {"argv": argv}]), [
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
        for arm, opposite in (("map", "search"), ("search", "map")):
            allowed = ([arm, "target"], ["--no-reindex", arm, "target"],
                       ["--format", "json", arm, "target"],
                       ["--", arm, "target"], [], ["--help"], ["-h"], ["--version"], ["-V"],
                       ["help", opposite], [arm, "--help"], [opposite, "--help"],
                       [arm, "--version"])
            for argv in allowed:
                with self.subTest(arm=arm, argv=argv):
                    self.assertTrue(runner.slopdex_call_allowed(arm, argv))
            for command in (opposite, *blocked):
                for argv in ([command, "target"], ["--root", arm, "--no-reindex", command, "target"]):
                    with self.subTest(arm=arm, argv=argv):
                        self.assertFalse(runner.slopdex_call_allowed(arm, argv))
            for argv in (["--unsupported", arm], ["--root"], [opposite, "--", "--help"]):
                with self.subTest(arm=arm, argv=argv):
                    self.assertFalse(runner.slopdex_call_allowed(arm, argv))

    def test_missing_required_command_for_unused_failed_partial_or_help_only(self):
        for arm in ("map", "search"):
            examples = ([], [{"argv": [arm, "target"], "exit_code": 9}],
                        [{"argv": [arm, "target"], "event": "start"}],
                        [{"argv": [arm, "legacy target"]}],
                         [{"argv": ["--help"], "exit_code": 0},
                          {"argv": [arm, "--help"], "exit_code": 0},
                          {"argv": ["--version"], "exit_code": 0}],
                         [{"argv": [arm, "-ih"], "exit_code": 0}])
            for calls in examples:
                with self.subTest(arm=arm, calls=calls):
                    self.assertEqual(runner.slopdex_protocol_violations(arm, calls), [
                        {"reason": "required_command_not_used_successfully", "command": arm}])
            self.assertEqual(runner.slopdex_protocol_violations(arm, [
                {"argv": ["--no-reindex", arm, "target"], "exit_code": 0}]), [])

    def test_protected_index_configuration_cannot_be_overridden(self):
        options = ("--root", "--config", "--index", "--provider", "--model", "--dimensions",
                   "--description-provider", "--description-model", "--description-fallback-model",
                   "--reranker-candidates")
        controls = ("--force-reindex", "--yes-really-rebuild-the-index", "--rebuild-on-divergence")
        for arm in ("map", "search"):
            for option in options:
                for argv in ([option, "value", arm], [arm, option, "value"], [arm, f"{option}=value"]):
                    with self.subTest(arm=arm, argv=argv):
                        self.assertFalse(runner.slopdex_call_allowed(arm, argv))
            for option in controls:
                self.assertFalse(runner.slopdex_call_allowed(arm, [arm, option]))
            self.assertTrue(runner.slopdex_call_allowed(arm, [arm, "--", "--index"]))
            self.assertTrue(runner.slopdex_call_allowed(arm, [arm, "Explain --index handling"]))
        self.assertTrue(runner.slopdex_call_allowed("slopdex", ["map", "--root", "value"]))

    def test_short_help_clusters_and_catalog_help_do_not_count_as_navigation(self):
        for argv in (["map", "-ih"], ["search", "query", "-ih"], ["map", "-hi"]):
            self.assertEqual(runner.slopdex_command(argv), "help")
        self.assertEqual(runner.slopdex_command(["map", "-ehelper"]), "map")
        self.assertEqual(runner.slopdex_command(["map", "--", "-ih"]), "map")
        for argv in (["help", "models"], ["help", "models", "opencode-go"], ["help", "--format=json", "models"]):
            self.assertEqual(runner.slopdex_command(argv), "models")
            for arm in ("map", "search"):
                self.assertFalse(runner.slopdex_call_allowed(arm, argv))

    def test_wrong_command_violates_protocol_even_after_success(self):
        for arm, opposite in (("map", "search"), ("search", "map")):
            for command in (opposite, "search-code", "cross-search", "status"):
                for exit_code in (0, 126):
                    with self.subTest(arm=arm, command=command, exit_code=exit_code):
                        calls = [{"argv": [arm, "target"], "exit_code": 0},
                                 {"argv": [command], "allowed": False, "exit_code": exit_code}]
                        self.assertEqual(runner.slopdex_protocol_violations(arm, calls), [
                            {"reason": "command_not_allowed", "command": command, "argv": [command]}])

    def create_index(self, arm):
        directory = self.root / "cache"
        directory.mkdir()
        runner.write_json(directory / "config.json", {})
        index = directory / "index.sqlite"
        metadata = {"identity": json.dumps({"schema": 3, "root": "/original/root"}),
                    "checkpoint": "fixture-commit"}
        if arm in runner.SEMANTIC_ARMS:
            metadata.update({"generation": "1", "descriptions_enabled": "false",
                             "active_embedding_profile": json.dumps({"dimensions": 3, "model": "fixture"})})
        with closing(sqlite3.connect(index)) as db, db:
            db.execute("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT)")
            db.executemany("INSERT INTO metadata VALUES (?, ?)", metadata.items())
        if arm in runner.SEMANTIC_ARMS:
            for kind in ("code", "markdown"):
                binary = Path(str(index) + f".{kind}.usearch")
                binary.write_bytes(f"fake warm ANN for {kind}".encode())
                runner.write_json(Path(str(binary) + ".manifest.json"), {
                    "version": 1, "generation": 1, "dimensions": 3,
                    "binary_hash": hashlib.sha256(binary.read_bytes()).hexdigest(), "vectors": {},
                })
        return directory

    def exercise_wrapper(self, arm):
        directory = self.create_index(arm)
        source = directory / "index.sqlite"
        source_bytes = source.read_bytes()
        sandbox, workspace = self.root / "sandbox", self.root / "workspace"
        sandbox.mkdir()
        workspace.mkdir()
        executions = self.root / "fake-executions.jsonl"
        fake_cli = self.root / "fake-slopdex"
        fake_cli.write_text(f"""#!{sys.executable}
import json, sys
with open({str(executions)!r}, 'a') as stream:
    stream.write(json.dumps(sys.argv[1:]) + '\\n')
print('fake CLI executed')
sys.exit(9 if '--fixture-fail' in sys.argv[1:] else 0)
""")
        fake_cli.chmod(0o755)
        bin_dir, log = runner.install_wrapper(sandbox, workspace, arm, str(fake_cli),
                                              directory, "fixture-commit")
        copied = sandbox / "index.sqlite"
        with closing(sqlite3.connect(copied)) as db:
            metadata = dict(db.execute("SELECT key, value FROM metadata"))
        self.assertEqual(json.loads(metadata["identity"])["root"], str(workspace.resolve()))
        self.assertEqual(metadata["checkpoint"], "fixture-commit")
        self.assertEqual(source.read_bytes(), source_bytes)
        if arm in runner.MAP_ARMS:
            self.assertNotIn("active_embedding_profile", metadata)
            self.assertEqual(list(sandbox.glob("index.sqlite.*usearch*")), [])
        else:
            before = runner.native_ann_snapshot(source)
            self.assertEqual(runner.native_ann_snapshot(copied), before)
            for suffix in before["artifacts"]:
                original, destination = Path(str(source) + suffix), Path(str(copied) + suffix)
                self.assertEqual(original.read_bytes(), destination.read_bytes())
                self.assertEqual(original.stat().st_mtime_ns, destination.stat().st_mtime_ns)
                self.assertNotEqual(original.stat().st_ino, destination.stat().st_ino)

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
            prefix = ["--root", str(workspace), "--config", str(directory / "config.json"),
                      "--index", str(copied), "--no-reindex"]
            self.assertEqual(actual_executions, [prefix + arguments for arguments in forwarded])

        primary = runner.ARM_NAVIGATION[arm][0]
        opposite = "search" if primary == "map" else "map"
        blocked = "status" if arm == "map-search" else opposite
        invoke([blocked, "target"], blocked, False, 126)
        self.assertFalse(executions.exists())
        invoke([primary, "target"], primary, True, 0)
        if arm == "map-search":
            invoke(["search", "query", "--limit", "50"], "search", True, 0)
        invoke([primary, "--index", "other.sqlite"], primary, False, 126)
        invoke(["help", "models"], "models", False, 126)
        for command in ("search-code", "search-md", "search-descriptions", "cross-search", "status", "update"):
            invoke([command, "target"], command, False, 126)
        invoke(["--root", arm, "--format=json", opposite, "target"], opposite, False, 126)
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
        self.assertEqual(runner.slopdex_protocol_violations(arm, calls), [
            {"reason": "command_not_allowed", "command": command, "argv": argv}
            for argv, command, allowed, _ in expected if not allowed])
        self.assertEqual(source.read_bytes(), source_bytes)
        if arm in runner.SEMANTIC_ARMS:
            self.assertEqual(runner.native_ann_snapshot(copied), before)

    def test_generated_map_wrapper_blocks_executes_and_journals_with_structural_only_snapshot(self):
        self.exercise_wrapper("map")

    def test_generated_search_wrapper_blocks_executes_and_journals_with_warm_ann_snapshot(self):
        self.exercise_wrapper("search")

    def test_generated_combined_wrapper_allows_both_commands_with_warm_ann_snapshot(self):
        self.exercise_wrapper("map-search")

    def test_map_first_wrapper_uses_only_structural_snapshot(self):
        self.exercise_wrapper("map-first")

    def test_map_follow_wrapper_uses_only_structural_snapshot(self):
        self.exercise_wrapper("map-follow")

    def test_map_verify_wrapper_uses_only_structural_snapshot(self):
        self.exercise_wrapper("map-verify")


if __name__ == "__main__":
    unittest.main()
