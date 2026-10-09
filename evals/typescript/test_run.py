"""Offline regression tests: python3 -m unittest discover -s evals/typescript."""

import copy
import collections
from contextlib import closing
import fnmatch
import hashlib
import io
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import run as runner


class EvaluationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="typescript-eval-test-")
        self.root = Path(self.temporary.name)
        self.task = {"id": "fixture", "prompt": "Find the three implementation roles.", "targets": [
            {"path": "source.go", "symbol": name} for name in ("alpha", "beta", "gamma")
        ]}
        self.gold = {("source.go", name): line for name, line in zip(("alpha", "beta", "gamma"), (1, 4, 7))}
        self.answer = {"findings": [
            {"path": path, "symbol": symbol, "line": line, "explanation": f"{symbol} implements its required role."}
            for (path, symbol), line in self.gold.items()
        ], "flow": "The implementation flows from alpha to beta to gamma."}
        self.config, _ = runner.load_inputs(runner.HERE / "config.json")

    def tearDown(self):
        self.temporary.cleanup()

    def grade(self, answer):
        return runner.score_answer(json.dumps(answer), self.task, self.gold)

    def test_duplicate_symbols_and_call_sites_do_not_earn_full_credit(self):
        self.assertTrue(self.grade(self.answer)["passed"])
        duplicate = copy.deepcopy(self.answer)
        duplicate["findings"][1] = duplicate["findings"][0]
        grade = self.grade(duplicate)
        self.assertAlmostEqual(grade["recall"], 2 / 3)
        self.assertFalse(grade["passed"])
        call_site = copy.deepcopy(self.answer)
        call_site["findings"][0]["line"] = 100
        self.assertAlmostEqual(self.grade(call_site)["recall"], 2 / 3)
        self.assertEqual(self.grade(call_site)["symbol_recall"], 1)

    def test_invalid_answers_and_shotgun_findings_are_rejected(self):
        for text in ("not JSON", "null", "[]", '{"findings":null}', '{"findings":[]}'):
            self.assertFalse(runner.score_answer(text, self.task, self.gold)["valid_answer"])
        too_many = copy.deepcopy(self.answer)
        too_many["findings"].append(too_many["findings"][0])
        self.assertFalse(self.grade(too_many)["valid_answer"])
        boolean_line = copy.deepcopy(self.answer)
        boolean_line["findings"][0]["line"] = True
        self.assertFalse(self.grade(boolean_line)["valid_answer"])

    def advanced_fixture(self):
        task = dict(self.task, difficulty="advanced", prompt="Trace the five implementation stages.",
                    targets=self.task["targets"] + [{"path": "cache.go", "symbol": "delta"}, {"path": "mapping.go", "symbol": "epsilon"}])
        gold = {**self.gold, ("cache.go", "delta"): 10, ("mapping.go", "epsilon"): 13}
        answer = copy.deepcopy(self.answer)
        answer["findings"] += [
            {"path": path, "symbol": symbol, "line": line, "explanation": f"{symbol} implements its required stage."}
            for (path, symbol), line in list(gold.items())[3:]
        ]
        answer["flow"] = "The five stages connect request handling, cache ownership, and mapping."
        return task, gold, answer

    def test_advanced_tasks_require_and_score_five_findings(self):
        task, gold, answer = self.advanced_fixture()
        result = runner.score_answer(json.dumps(answer), task, gold)
        self.assertTrue(result["passed"])
        self.assertEqual(result["f1"], 1)
        self.assertEqual(len(result["matches"]), 5)
        incomplete = runner.score_answer(json.dumps(self.answer), task, gold)
        self.assertFalse(incomplete["valid_answer"])
        self.assertIn("Expected 5 findings", incomplete["error"])
        duplicate = copy.deepcopy(answer)
        duplicate["findings"][-1] = duplicate["findings"][-2]
        partial = runner.score_answer(json.dumps(duplicate), task, gold)
        self.assertTrue(partial["valid_answer"])
        self.assertFalse(partial["passed"])
        self.assertAlmostEqual(partial["f1"], .8)
        answer["findings"].append(answer["findings"][0])
        self.assertFalse(runner.score_answer(json.dumps(answer), task, gold)["valid_answer"])

    def test_task_prompts_use_each_tasks_own_finding_count(self):
        advanced, _, _ = self.advanced_fixture()
        for task, count in ((self.task, 3), (advanced, 5)):
            with self.subTest(count=count):
                prompt = runner.task_prompt(task)
                self.assertIn(f"Include exactly {count} findings", prompt)
                self.assertNotIn("{finding_count}", prompt)
                self.assertIn('{"findings":', prompt)
                self.assertTrue(prompt.startswith(task["prompt"]))

    def test_advanced_input_validation_enforces_unique_cross_file_targets(self):
        advanced, _, _ = self.advanced_fixture()
        corpus = {"schema_version": 1, "repository_commit": "a" * 40, "tasks": [advanced]}
        with patch.object(runner, "read_json", side_effect=[self.config, corpus]):
            _, loaded = runner.load_inputs(self.root / "config.json")
            self.assertEqual(len(loaded["tasks"][0]["targets"]), 5)
        duplicate = copy.deepcopy(advanced)
        duplicate["targets"][-1] = duplicate["targets"][-2]
        single_file = copy.deepcopy(advanced)
        for target in single_file["targets"]:
            target["path"] = "source.go"
        too_few = dict(advanced, targets=advanced["targets"][:3])
        for task in (duplicate, single_file, too_few):
            with self.subTest(task=task), patch.object(runner, "read_json", side_effect=[self.config, dict(corpus, tasks=[task])]), self.assertRaises(ValueError):
                runner.load_inputs(self.root / "config.json")

    def test_events_select_final_message_and_capture_usage_and_errors(self):
        events = [
            {"type": "text", "part": {"messageID": "earlier", "text": "Investigating..."}},
            {"type": "tool_use", "part": {"tool": "bash"}},
            {"type": "step_finish", "part": {"cost": .02, "tokens": {"input": 100, "output": 25, "reasoning": 10, "cache": {"read": 5, "write": 3}}}},
            {"type": "text", "sessionID": "session-1", "part": {"messageID": "final", "text": '{"findings":'}},
            {"type": "text", "part": {"messageID": "final", "text": "[]}"}},
            {"type": "error", "error": {"name": "APIError", "message": "Unavailable"}},
        ]
        path = self.root / "events.jsonl"
        path.write_text("startup noise\n" + "\n".join(json.dumps(event) for event in events))
        result = runner.parse_events(path)
        self.assertEqual(result["answer_text"], '{"findings":[]}')
        self.assertEqual(result["tokens"]["input"], 100)
        self.assertEqual(result["tokens"]["cache_read"], 5)
        self.assertEqual(result["tools"], {"bash": 1})
        self.assertEqual(result["cost_usd"], .02)
        self.assertEqual(result["malformed_event_lines"], 1)
        self.assertEqual(len(result["errors"]), 1)
        path.write_text("")
        self.assertIsNone(runner.parse_events(path)["cost_usd"])

    def test_slopdex_command_classification_handles_options_and_help(self):
        examples = [
            ([], "help"),
            (["--help"], "help"),
            (["-h"], "help"),
            (["--version"], "version"),
            (["-V"], "version"),
            (["help", "search"], "help"),
            (["map", "--help"], "help"),
            (["search", "--", "--help"], "search"),
            (["--root", "map", "--model", "search", "search-code", "map"], "search-code"),
            (["--config=config.json", "--output", "json", "--no-reindex", "map", "--private"], "map"),
            (["--output=json", "map", "--private"], "map"),
            (["--format", "json", "map"], "unknown"),
            (["--format=json", "map"], "unknown"),
            (["--root", "/repo"], "help"),
            (["--", "search", "query"], "search"),
            (["descriptions", "enable"], "descriptions"),
            (["refresh"], "refresh"),
            (["typo", "search"], "unknown"),
            (["--unsupported", "map"], "unknown"),
            (["--root"], "unknown"),
        ]
        for argv, expected in examples:
            with self.subTest(argv=argv):
                self.assertEqual(runner.slopdex_command(argv), expected)

    def test_slopdex_command_counts_use_logged_invocations_and_legacy_fallback(self):
        calls = [{"argv": argv} for argv in (["search", "map"], ["map", "--private"], ["search", "query"], ["--help"], ["map", "--help"])]
        self.assertEqual(runner.slopdex_command_counts(calls), {"help": 2, "map": 1, "search": 2})
        self.assertEqual(runner.saved_slopdex_commands(self.root, {"slopdex_calls": 3}), {"unknown": 3})
        self.assertEqual(runner.saved_slopdex_commands(self.root, {"slopdex_calls": 0}), {})
        self.assertEqual(runner.saved_slopdex_commands(self.root, {"slopdex_commands": {"map": 2}}), {"map": 2})
        (self.root / "slopdex-calls.jsonl").write_text("\n" + "\n".join(json.dumps(call) for call in calls) + "\n")
        self.assertEqual(runner.saved_slopdex_commands(self.root, {"slopdex_commands": {"map": 99}}), {"help": 2, "map": 1, "search": 2})
        for call in ({}, {"argv": "search query"}, {"argv": ["search", 1]}, None):
            with self.subTest(call=call), self.assertRaises(ValueError):
                runner.slopdex_command_counts([call])

    def create_index(self):
        directory = self.root / "cache"
        directory.mkdir()
        runner.write_json(directory / "config.json", {})
        with closing(sqlite3.connect(directory / "index.sqlite")) as db, db:
            db.execute("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT)")
            db.execute("INSERT INTO metadata VALUES ('identity', ?)", (json.dumps({"schema": 3, "root": "/old/root"}),))
            db.execute("INSERT INTO metadata VALUES ('checkpoint', 'commit')")
            db.execute("INSERT INTO metadata VALUES ('generation', '1')")
            db.execute("INSERT INTO metadata VALUES ('active_embedding_profile', ?)", (json.dumps({"dimensions": 3, "model": "fixture", "provider": "openai"}),))
            db.execute("INSERT INTO metadata VALUES ('descriptions_enabled', 'false')")
        self.write_native_sidecars(directory / "index.sqlite")
        return directory

    def write_native_sidecars(self, index, *, descriptions=False):
        kinds = {"code": 1, "markdown": 1}
        if descriptions:
            kinds.update({"descriptions": 2, "combined": 3})
        for kind, parts in kinds.items():
            binary = Path(str(index) + f".{kind}.usearch")
            binary.write_bytes(f"native fixture for {kind}".encode())
            runner.write_json(Path(str(binary) + ".manifest.json"), {
                "version": 1, "generation": 1, "dimensions": 3 * parts,
                "binary_hash": runner.file_digest(binary), "vectors": {},
            })

    def test_index_copy_relocates_root_without_mutating_cache(self):
        directory = self.create_index()
        destination = self.root / "copy.sqlite"
        runner.copy_index(directory / "index.sqlite", destination, self.root, "commit")
        self.assertEqual(runner.native_ann_snapshot(destination), runner.native_ann_snapshot(directory / "index.sqlite"))
        for suffix in runner.native_ann_snapshot(destination)["artifacts"]:
            source_file, copied_file = Path(str(directory / "index.sqlite") + suffix), Path(str(destination) + suffix)
            self.assertNotEqual(source_file.stat().st_ino, copied_file.stat().st_ino)
            self.assertEqual(source_file.stat().st_mtime_ns, copied_file.stat().st_mtime_ns)
        with closing(sqlite3.connect(destination)) as db:
            identity = json.loads(db.execute("SELECT value FROM metadata WHERE key='identity'").fetchone()[0])
            self.assertEqual(identity["root"], str(self.root))
        with closing(sqlite3.connect(directory / "index.sqlite")) as db:
            identity = json.loads(db.execute("SELECT value FROM metadata WHERE key='identity'").fetchone()[0])
            self.assertEqual(identity["root"], "/old/root")
        with self.assertRaises(ValueError):
            runner.copy_index(directory / "index.sqlite", self.root / "bad.sqlite", self.root, "other-commit")

    def test_native_cache_validation_rejects_corruption_staleness_and_old_metadata(self):
        directory = self.create_index()
        index = directory / "index.sqlite"
        metadata = {"native_ann": {**runner.native_ann_snapshot(index), "wall_seconds": .1}}
        self.assertTrue(runner.native_ann_ready(index, metadata))
        self.assertFalse(runner.native_ann_ready(index, {}))
        self.assertFalse(runner.native_ann_ready(index, {"native_ann": None}))
        self.assertFalse(runner.native_ann_ready(index, {"native_ann": []}))
        manifest_path = Path(str(index) + ".code.usearch.manifest.json")
        manifest = manifest_path.read_text()
        for malformed in (None, [], 42, "corrupt"):
            runner.write_json(manifest_path, malformed)
            self.assertFalse(runner.native_ann_ready(index, metadata))
        manifest_path.write_text(manifest)
        binary = Path(str(index) + ".code.usearch")
        original = binary.read_bytes()
        binary.write_bytes(b"corrupt")
        self.assertFalse(runner.native_ann_ready(index, metadata))
        binary.write_bytes(original)
        with closing(sqlite3.connect(index)) as db, db:
            db.execute("UPDATE metadata SET value='2' WHERE key='generation'")
        self.assertFalse(runner.native_ann_ready(index, metadata))
        with closing(sqlite3.connect(index)) as db, db:
            db.execute("UPDATE metadata SET value='1' WHERE key='generation'")
        binary.unlink()
        self.assertFalse(runner.native_ann_ready(index, metadata))
        with self.assertRaises(FileNotFoundError):
            runner.copy_index(index, self.root / "missing.sqlite", self.root, "commit")

    def test_description_and_combined_sidecars_are_copied_when_enabled(self):
        directory = self.create_index()
        index = directory / "index.sqlite"
        with closing(sqlite3.connect(index)) as db, db:
            db.execute("UPDATE metadata SET value='true' WHERE key='descriptions_enabled'")
        self.write_native_sidecars(index, descriptions=True)
        destination = self.root / "descriptions.sqlite"
        runner.copy_index(index, destination, self.root, "commit")
        copied = runner.native_ann_snapshot(destination)
        self.assertEqual(len(copied["artifacts"]), 8)
        self.assertEqual(copied, runner.native_ann_snapshot(index))

    def test_prepare_upgrades_and_repairs_native_cache_without_reembedding(self):
        directory = self.create_index()
        config = copy.deepcopy(self.config)
        config["slopdex"].update({"model": "fixture", "dimensions": 3, "include": ["*.go"]})
        metadata = {"identity": {"commit": "commit", "slopdex_version": "fixture-version", "config": config["slopdex"]},
                    "wall_seconds": .1, "status": {"functionCount": 3, "descriptionsEnabled": False}}
        runner.write_json(directory / "manifest.json", metadata)

        def warm(path, saved, binary, workspace, timeout):
            self.write_native_sidecars(path / "index.sqlite")
            saved["native_ann"] = {**runner.native_ann_snapshot(path / "index.sqlite"), "wall_seconds": .2}
            runner.write_json(path / "manifest.json", saved)
            return saved

        coverage = {"validated_targets": 3, "parser_diagnostics": 0}
        with patch.object(runner, "digest", return_value="cache"), patch.object(runner, "command", return_value="fixture-version"), patch.object(runner, "index_coverage", return_value=coverage), patch.object(runner, "warm_native_ann", side_effect=warm) as warming, patch.object(runner, "logged_process") as process:
            path, result = runner.prepare_index(config, "commit", self.root, "fixture-slopdex", [self.task])
            self.assertEqual(path, directory)
            self.assertEqual(result["coverage"], coverage)
            self.assertTrue(runner.native_ann_ready(directory / "index.sqlite", result))
            self.assertEqual(warming.call_count, 1)
            runner.prepare_index(config, "commit", self.root, "fixture-slopdex", [self.task])
            self.assertEqual(warming.call_count, 1)
            Path(str(directory / "index.sqlite") + ".code.usearch").unlink()
            runner.prepare_index(config, "commit", self.root, "fixture-slopdex", [self.task])
            self.assertEqual(warming.call_count, 2)
            runner.write_json(Path(str(directory / "index.sqlite") + ".code.usearch.manifest.json"), None)
            runner.prepare_index(config, "commit", self.root, "fixture-slopdex", [self.task])
            self.assertEqual(warming.call_count, 3)
            process.assert_not_called()

    def test_index_coverage_allows_unrelated_parser_errors_but_requires_target_vectors(self):
        index = self.root / "coverage.sqlite"
        with closing(sqlite3.connect(index)) as db, db:
            db.executescript("""
                CREATE TABLE metadata(key TEXT, value TEXT);
                CREATE TABLE diagnostics(path TEXT, code TEXT, message TEXT);
                CREATE TABLE symbols(id INTEGER, path TEXT, name TEXT);
                CREATE TABLE search_units(id INTEGER, path TEXT, symbol_id INTEGER, kind TEXT, data TEXT, embedding_input_hash TEXT);
                CREATE TABLE unit_embeddings(unit_id INTEGER, role TEXT, embedding_key TEXT);
                CREATE TABLE embeddings(key TEXT);
                INSERT INTO diagnostics VALUES('unrelated.go', 'parse-error', 'Recovered source region');
            """)
            profile = {"model": "fixture", "provider": "openai", "dimensions": 3}
            db.execute("INSERT INTO metadata VALUES('active_embedding_profile', ?)", (json.dumps(profile),))
            keys = []
            for number, target in enumerate(self.task["targets"]):
                db.execute("INSERT INTO symbols VALUES(?, ?, ?)", (number, target["path"], target["symbol"]))
                text = f"func {target['symbol']}() {{}}"
                payload = json.dumps([profile, "document", text], sort_keys=True, separators=(",", ":"), ensure_ascii=False)
                key = hashlib.sha256(payload.encode()).hexdigest()
                keys.append(key)
                db.execute("INSERT INTO search_units VALUES(?, ?, ?, 'function', ?, ?)", (number, target["path"], number, json.dumps({"embeddingInput": text}), hashlib.sha256(text.encode()).hexdigest()))
                db.execute("INSERT INTO embeddings VALUES(?)", (key,))
        self.assertEqual(runner.index_coverage(index, [self.task]), {"validated_targets": 3, "parser_diagnostics": 1})
        with closing(sqlite3.connect(index)) as db, db:
            db.execute("DELETE FROM embeddings WHERE key=?", (keys[-1],))
        with self.assertRaisesRegex(ValueError, "Task targets are missing"):
            runner.index_coverage(index, [self.task])
        with closing(sqlite3.connect(index)) as db, db:
            db.execute("INSERT INTO diagnostics VALUES('source.go', 'read-error', 'Cannot read source')")
        with self.assertRaisesRegex(ValueError, "non-parser indexing failures"):
            runner.index_coverage(index, [self.task])

    def test_isolation_keeps_auth_but_removes_inherited_opencode_settings(self):
        with patch.dict(os.environ, {"OPENCODE_CONFIG": "/user/config", "OPENCODE_CONFIG_CONTENT": "inherited", "OPENCODE_API_KEY": "fixture-key", "XDG_DATA_HOME": "/auth/data"}):
            config = runner.opencode_config(self.config["models"][0], "off", 10)
            env = runner.isolated_environment(self.root, config)
        self.assertNotIn("OPENCODE_CONFIG", env)
        self.assertEqual(env["XDG_DATA_HOME"], str(self.root / "data"))
        self.assertEqual(env["OPENCODE_API_KEY"], "fixture-key")
        self.assertEqual(json.loads(env["OPENCODE_CONFIG_CONTENT"]), config)
        self.assertEqual(config["provider"]["opencode-go"]["models"]["deepseek-v4.1-flash"]["variants"]["medium"]["reasoningEffort"], "medium")

    def test_only_selected_provider_credentials_are_copied(self):
        original = self.root / "original"
        runner.write_json(original / "opencode" / "auth.json", {
            "opencode-go": {"type": "api", "key": "selected-secret"},
            "other-provider": {"type": "api", "key": "unrelated-secret"},
        })
        sandbox = self.root / "sandbox"
        sandbox.mkdir()
        config = runner.opencode_config(self.config["models"][0], "off", 10)
        with patch.dict(os.environ, {"XDG_DATA_HOME": str(original)}):
            env = runner.isolated_environment(sandbox, config)
        copied = runner.read_json(Path(env["XDG_DATA_HOME"]) / "opencode" / "auth.json")
        self.assertEqual(set(copied), {"opencode-go"})
        self.assertEqual(copied["opencode-go"]["key"], "selected-secret")

    def test_paired_schedule_is_reproducible_and_counterbalanced(self):
        tasks = [dict(self.task, id=f"task-{index}") for index in range(10)]
        schedule = runner.make_trials(self.config, tasks, ["off", "slopdex"], 2, 7)
        self.assertEqual(schedule, runner.make_trials(self.config, tasks, ["off", "slopdex"], 2, 7))
        self.assertEqual(len(schedule), 80)
        self.assertEqual(len({trial["id"] for trial in schedule}), 80)
        first_arms = [schedule[index]["arm"] for index in range(0, len(schedule), 2)]
        self.assertEqual(first_arms.count("off"), first_arms.count("slopdex"))
        for index in range(0, len(schedule), 2):
            a, b = schedule[index:index + 2]
            self.assertEqual((a["task"], a["model"], a["repeat"]), (b["task"], b["model"], b["repeat"]))
            self.assertNotEqual(a["arm"], b["arm"])

    def test_completed_resume_and_incompatible_settings_do_not_prepare_an_index(self):
        corpus = {"repository_commit": "a" * 40, "tasks": [self.task]}
        trials = runner.make_trials(self.config, corpus["tasks"], ["off", "slopdex"], 1, 0)
        output = self.root / "completed"
        settings = {"config": self.config, "tasks": corpus["tasks"], "commit": corpus["repository_commit"],
                    "trials": trials, "arm_instructions": {arm: runner.arm_instructions(arm) for arm in ("off", "slopdex")},
                    "harness_sha256": hashlib.sha256(Path(runner.__file__).read_bytes()).hexdigest()}
        runner.write_json(output / "manifest.json", {**settings, "fingerprint": "saved"})
        for trial in trials:
            runner.write_json(output / "trials" / trial["id"] / "result.json", {})
        with patch.object(runner, "load_inputs", return_value=(self.config, corpus)), patch.object(runner, "report") as report, patch.object(runner, "ensure_source") as source, patch.object(runner, "prepare_index") as index, patch.object(runner, "executable") as binary:
            runner.main(["run", "--arms", "off", "slopdex", "--output", str(output)])
            report.assert_called_once_with(output)
            source.assert_not_called()
            index.assert_not_called()
            binary.assert_not_called()
            changed = copy.deepcopy(self.config)
            changed["timeout_seconds"] += 1
            with patch.object(runner, "load_inputs", return_value=(changed, corpus)), self.assertRaises(ValueError):
                runner.main(["run", "--arms", "off", "slopdex", "--output", str(output)])
            source.assert_not_called()
            index.assert_not_called()

    def test_map_prompt_arms_share_structural_preparation_without_semantic_index(self):
        corpus = {"repository_commit": "a" * 40, "tasks": [self.task]}
        structural = self.root / "structural"
        with patch.object(runner, "load_inputs", return_value=(self.config, corpus)), \
                patch.object(runner, "ensure_source"), patch.object(runner, "target_lines", return_value=self.gold), \
                patch.object(runner, "executable", side_effect=lambda name: name), \
                patch.object(runner, "command", return_value="fixture-version"), \
                patch.object(runner, "prepare_index", side_effect=AssertionError("semantic preparation")), \
                patch.object(runner, "prepare_map_index", return_value=(structural, {})) as prepare, \
                patch.object(runner, "report"), \
                patch.object(runner, "run_trial", return_value={"status": "ok", "grade": {"f1": 1}, "wall_seconds": 1}) as trial, \
                patch("sys.stdout", new=io.StringIO()):
            runner.main(["run", "--arms", "map-first", "map-follow", "--output", str(self.root / "profiles")])
        prepare.assert_called_once()
        self.assertEqual(trial.call_count, 4)
        for call in trial.call_args_list:
            self.assertEqual(call.args[8], structural)
            self.assertEqual(call.kwargs["instruction"], runner.arm_instructions(call.args[0]["arm"]))

    def test_readme_map_arm_prepares_symbol_vectors_without_content_index(self):
        corpus = {"repository_commit": "a" * 40, "tasks": [self.task]}
        symbols = self.root / "symbols"
        with patch.object(runner, "load_inputs", return_value=(self.config, corpus)), \
                patch.object(runner, "ensure_source"), patch.object(runner, "target_lines", return_value=self.gold), \
                patch.object(runner, "executable", side_effect=lambda name: name), \
                patch.object(runner, "command", return_value="fixture-version"), \
                patch.object(runner, "prepare_index", side_effect=AssertionError("content preparation")), \
                patch.object(runner, "prepare_map_index", return_value=(symbols, {})) as prepare, \
                patch.object(runner, "report"), \
                patch.object(runner, "run_trial", return_value={"status": "ok", "grade": {"f1": 1}, "wall_seconds": 1}) as trial, \
                patch("sys.stdout", new=io.StringIO()):
            runner.main(["run", "--arms", "map", "--output", str(self.root / "symbol-map")])
        prepare.assert_called_once()
        self.assertEqual(prepare.call_args.kwargs, {"symbols": True})
        self.assertEqual(trial.call_count, 2)
        for call in trial.call_args_list:
            self.assertEqual(call.args[8], symbols)
            self.assertIn('-q "<short symbol concept>"', call.kwargs["instruction"])

    def test_multi_arm_order_balances_every_position(self):
        arms = ["off", "map", "map-first", "map-follow"]
        tasks = [dict(self.task, id=f"task-{index}") for index in range(4)]
        trials = runner.make_trials(self.config, tasks, arms, 2, 42)
        self.assertEqual(len({trial["id"] for trial in trials}), 64)
        counts = collections.Counter((trial["arm"], index % 4) for index, trial in enumerate(trials))
        self.assertEqual(set(counts.values()), {4})
        for index in range(0, len(trials), 4):
            group = trials[index:index + 4]
            self.assertEqual({row["arm"] for row in group}, set(arms))
            self.assertEqual(len({(row["model"], row["task"], row["repeat"]) for row in group}), 1)

    def test_baseline_guard_allows_workspace_paths_and_denies_slopdex_commands(self):
        rules = runner.opencode_config(self.config["models"][0], "off", 40)["permission"]["bash"]
        denied = [pattern for pattern, action in rules.items() if action == "deny"]
        for command in ("slopdex map", "/home/leto/.cargo/bin/slopdex map", "env X=1 slopdex"):
            self.assertTrue(any(fnmatch.fnmatchcase(command, pattern) for pattern in denied), command)
        for command in ("cat /home/leto/r/slopdex/jobs/tmp/slopdex-ts-trial/repo/file.go",
                        "git -C /home/leto/r/slopdex/jobs/tmp/slopdex-ts-trial/repo status"):
            self.assertFalse(any(fnmatch.fnmatchcase(command, pattern) for pattern in denied), command)

    def test_timeout_is_classified_and_process_stopped(self):
        code, expired, elapsed = runner.logged_process([sys.executable, "-c", "import time; time.sleep(30)"], self.root, os.environ.copy(), self.root, 1)
        self.assertTrue(expired)
        self.assertNotEqual(code, 0)
        self.assertLess(elapsed, 10)

    def test_timeout_kills_child_even_when_parent_exits_on_term(self):
        pid_file = self.root / "child.pid"
        script = "import subprocess, sys, time; subprocess.Popen([sys.executable, '-c', \"import os, pathlib, signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); pathlib.Path(%r).write_text(str(os.getpid())); time.sleep(30)\"]); time.sleep(30)" % str(pid_file)
        _, expired, _ = runner.logged_process([sys.executable, "-c", script], self.root, os.environ.copy(), self.root, 1)
        self.assertTrue(expired)
        pid = int(pid_file.read_text())
        # Linux may retain a killed child briefly as a zombie until reaped by init.
        stat = Path(f"/proc/{pid}/stat")
        deadline = time.monotonic() + 2
        while stat.exists() and stat.read_text().split()[2] != "Z" and time.monotonic() < deadline:
            time.sleep(.01)
        if stat.exists():
            self.assertEqual(stat.read_text().split()[2], "Z")
        else:
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)

    def test_offline_paired_trials_and_report(self):
        """Exercise CLI spawning, both wrappers, scoring, artifacts and pairing."""
        cache = self.create_index()
        fake = self.root / "fake-opencode"
        fake.write_text(f'''#!{sys.executable}
import json, os, pathlib, subprocess, sys
workspace = pathlib.Path.cwd()
config = json.loads(os.environ['OPENCODE_CONFIG_CONTENT'])
if sys.argv[1:3] == ['debug', 'config']:
    print(json.dumps(config))
    sys.exit(0)
assert sys.argv[sys.argv.index('--variant') + 1] == 'medium'
assert sys.argv[sys.argv.index('--dir') + 1] == str(workspace) == os.environ['PWD']
assert config['instructions'] == [str(workspace / 'AGENTS.md')]
assert config['permission']['task'] == 'deny'
if 'Use slopdex for code navigation' in (workspace / 'AGENTS.md').read_text():
    subprocess.run(['slopdex', 'map'], check=True, stdout=subprocess.DEVNULL)
print(json.dumps({{'type': 'text', 'part': {{'messageID': 'final', 'text': os.environ['TEST_EVAL_ANSWER']}}}}))
print(json.dumps({{'type': 'step_finish', 'part': {{'cost': 0.01, 'tokens': {{'input': 100, 'output': 20}}}}}}))
''')
        fake.chmod(0o755)
        dex = self.root / "fake-slopdex"
        dex.write_text(f'''#!{sys.executable}
import json, pathlib, sqlite3, sys
root = sys.argv[sys.argv.index('--root') + 1]
index = sys.argv[sys.argv.index('--index') + 1]
with sqlite3.connect(index) as db:
    identity = json.loads(db.execute("SELECT value FROM metadata WHERE key='identity'").fetchone()[0])
    assert identity['root'] == str(pathlib.Path.cwd()) == root
assert '--no-reindex' in sys.argv
print('alpha beta gamma')
''')
        dex.chmod(0o755)
        source = self.root / "source"
        source.mkdir()
        (source / "source.go").write_text("func alpha() {}\n\n\nfunc beta() {}\n\n\nfunc gamma() {}\n")
        subprocess.run(["git", "init", "-q", str(source)], check=True)
        subprocess.run(["git", "add", "source.go"], cwd=source, check=True)
        subprocess.run(["git", "-c", "user.name=Eval Test", "-c", "user.email=eval@example.invalid", "commit", "-qm", "fixture"], cwd=source, check=True)
        commit = runner.command(["git", "rev-parse", "HEAD"], cwd=source)
        with closing(sqlite3.connect(cache / "index.sqlite")) as db, db:
            db.execute("UPDATE metadata SET value=? WHERE key='checkpoint'", (commit,))
        config = dict(self.config, models=self.config["models"][:1])
        trials = runner.make_trials(config, [self.task], ["off", "slopdex"], 1, 0)
        output = self.root / "output"
        runner.write_json(output / "manifest.json", {"commit": commit, "trials": trials})
        with patch.object(runner, "SUBMODULE", source), patch.dict(os.environ, {"TEST_EVAL_ANSWER": json.dumps(self.answer)}):
            for trial in trials:
                result = runner.run_trial(trial, self.task, self.gold, config, commit, output / "trials" / trial["id"], str(fake), str(dex), cache)
                self.assertEqual(result["status"], "ok")
                self.assertEqual(result["grade"]["f1"], 1)
                self.assertEqual(result["slopdex_calls"], int(trial["arm"] == "slopdex"))
                self.assertEqual(result["slopdex_commands"], {"map": 1} if trial["arm"] == "slopdex" else {})
                if trial["arm"] == "slopdex":
                    self.assertTrue(result["native_ann"]["validated"])
                    self.assertTrue((output / "trials" / trial["id"] / "ann-validation.json").exists())
                else:
                    self.assertIsNone(result["native_ann"])
        # Simulate older completed results to exercise report-time backfilling.
        grades = {}
        for trial in trials:
            path = output / "trials" / trial["id"] / "result.json"
            result = runner.read_json(path)
            grades[trial["id"]] = result["grade"]
            result.pop("slopdex_commands")
            runner.write_json(path, result)
        summary = runner.report(output)
        self.assertEqual(summary["completed"], 2)
        self.assertEqual(len(summary["paired"]), 1)
        self.assertEqual(summary["paired"][0]["f1_delta"], 0)
        self.assertEqual(summary["slopdex_commands"], {"map": 1})
        self.assertEqual(len(summary["trials"]), 2)
        group = next(group for group in summary["groups"] if group["arm"] == "slopdex")
        self.assertEqual(group["slopdex_commands"], {"map": 1})
        self.assertEqual(group["slopdex_command_trials"], {"map": 1})
        for trial in trials:
            result = runner.read_json(output / "trials" / trial["id"] / "result.json")
            self.assertEqual(result["grade"], grades[trial["id"]])
            self.assertEqual(result["slopdex_commands"], {"map": 1} if trial["arm"] == "slopdex" else {})
        self.assertTrue((output / "report.md").exists())
        self.assertIn("## Slopdex command usage", (output / "report.md").read_text())
        self.assertIn("`map` × 1", (output / "report.md").read_text())
        treatment = next(trial for trial in trials if trial["arm"] == "slopdex")
        path = output / "trials" / treatment["id"] / "result.json"
        result = runner.read_json(path)
        result.pop("slopdex_commands")
        runner.write_json(path, result)
        (path.parent / "slopdex-calls.jsonl").unlink()
        self.assertEqual(runner.report(output)["slopdex_commands"], {"unknown": 1})
        self.assertEqual(runner.read_json(path)["grade"], grades[treatment["id"]])


@unittest.skipUnless(os.environ.get("SLOPDEX_EVAL_CHECK_OPENCODE") == "1", "Optional installed-OpenCode config smoke check")
class OpenCodeCompatibilityTests(unittest.TestCase):
    def test_installed_cli_accepts_both_models_and_arm_configs(self):
        config, _ = runner.load_inputs(runner.HERE / "config.json")
        opencode = runner.executable("opencode")
        for model in config["models"]:
            for arm in ("off", "map", "search", "slopdex"):
                with self.subTest(model=model["model"], arm=arm), tempfile.TemporaryDirectory(prefix="typescript-opencode-smoke-") as temporary:
                    sandbox = Path(temporary)
                    workspace = sandbox / "repo"
                    workspace.mkdir()
                    subprocess.run(["git", "init", "-q", str(workspace)], check=True)
                    (workspace / "AGENTS.md").write_text(runner.BASE_INSTRUCTIONS + runner.arm_instructions(arm))
                    expected = runner.opencode_config(model, arm, config["max_steps"])
                    expected["instructions"] = [str(workspace / "AGENTS.md")]
                    env = runner.isolated_environment(sandbox, expected)
                    env["PWD"] = str(workspace)
                    runner.check_effective_config(opencode, workspace, env, expected, sandbox)


if __name__ == "__main__":
    unittest.main()
