"""Offline regression tests: python3 -m unittest discover -s evals/typescript."""

import copy
from contextlib import closing
import hashlib
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

    def create_index(self):
        directory = self.root / "cache"
        directory.mkdir()
        runner.write_json(directory / "config.json", {})
        with closing(sqlite3.connect(directory / "index.sqlite")) as db, db:
            db.execute("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT)")
            db.execute("INSERT INTO metadata VALUES ('identity', ?)", (json.dumps({"schema": 3, "root": "/old/root"}),))
            db.execute("INSERT INTO metadata VALUES ('checkpoint', 'commit')")
        return directory

    def test_index_copy_relocates_root_without_mutating_cache(self):
        directory = self.create_index()
        destination = self.root / "copy.sqlite"
        runner.copy_index(directory / "index.sqlite", destination, self.root, "commit")
        with closing(sqlite3.connect(destination)) as db:
            identity = json.loads(db.execute("SELECT value FROM metadata WHERE key='identity'").fetchone()[0])
            self.assertEqual(identity["root"], str(self.root))
        with closing(sqlite3.connect(directory / "index.sqlite")) as db:
            identity = json.loads(db.execute("SELECT value FROM metadata WHERE key='identity'").fetchone()[0])
            self.assertEqual(identity["root"], "/old/root")
        with self.assertRaises(ValueError):
            runner.copy_index(directory / "index.sqlite", self.root / "bad.sqlite", self.root, "other-commit")

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
                    "trials": trials, "harness_sha256": hashlib.sha256(Path(runner.__file__).read_bytes()).hexdigest()}
        runner.write_json(output / "manifest.json", {**settings, "fingerprint": "saved"})
        for trial in trials:
            runner.write_json(output / "trials" / trial["id"] / "result.json", {})
        with patch.object(runner, "load_inputs", return_value=(self.config, corpus)), patch.object(runner, "report") as report, patch.object(runner, "ensure_source") as source, patch.object(runner, "prepare_index") as index, patch.object(runner, "executable") as binary:
            runner.main(["run", "--output", str(output)])
            report.assert_called_once_with(output)
            source.assert_not_called()
            index.assert_not_called()
            binary.assert_not_called()
            changed = copy.deepcopy(self.config)
            changed["timeout_seconds"] += 1
            with patch.object(runner, "load_inputs", return_value=(changed, corpus)), self.assertRaises(ValueError):
                runner.main(["run", "--output", str(output)])
            source.assert_not_called()
            index.assert_not_called()

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
        summary = runner.report(output)
        self.assertEqual(summary["completed"], 2)
        self.assertEqual(len(summary["paired"]), 1)
        self.assertEqual(summary["paired"][0]["f1_delta"], 0)
        self.assertTrue((output / "report.md").exists())


@unittest.skipUnless(os.environ.get("SLOPDEX_EVAL_CHECK_OPENCODE") == "1", "Optional installed-OpenCode config smoke check")
class OpenCodeCompatibilityTests(unittest.TestCase):
    def test_installed_cli_accepts_both_models_and_arm_configs(self):
        config, _ = runner.load_inputs(runner.HERE / "config.json")
        opencode = runner.executable("opencode")
        for model in config["models"]:
            for arm in ("off", "slopdex"):
                with self.subTest(model=model["model"], arm=arm), tempfile.TemporaryDirectory(prefix="typescript-opencode-smoke-") as temporary:
                    sandbox = Path(temporary)
                    workspace = sandbox / "repo"
                    workspace.mkdir()
                    subprocess.run(["git", "init", "-q", str(workspace)], check=True)
                    (workspace / "AGENTS.md").write_text(runner.BASE_INSTRUCTIONS + runner.ARM_INSTRUCTIONS[arm])
                    expected = runner.opencode_config(model, arm, config["max_steps"])
                    expected["instructions"] = [str(workspace / "AGENTS.md")]
                    env = runner.isolated_environment(sandbox, expected)
                    env["PWD"] = str(workspace)
                    runner.check_effective_config(opencode, workspace, env, expected, sandbox)


if __name__ == "__main__":
    unittest.main()
