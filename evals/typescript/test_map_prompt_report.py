"""Meaningful offline accounting/preservation fixtures; run with python3 -B -m unittest.

From the repository root:
python3 -B -m unittest discover -s evals/typescript -p test_map_prompt_report.py
"""

import json
from contextlib import closing
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

import map_prompt_report as report


class MapPromptReportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="map-report-")
        self.addCleanup(self.temp.cleanup)
        self.job = Path(self.temp.name)
        self.task = {"id": "fixture", "prompt": "Find implementation", "targets": [{"path": "tsc/internal/a.go", "symbol": "work"}]}
        self.trials = [{"id": arm, "arm": arm, "model": "fixture/model", "variant": "medium", "task": "fixture", "repeat": 1} for arm in ("off", "map-first")]
        self.manifest = {"trials": self.trials, "tasks": [self.task], "index": None, "commit": "a" * 40,
                         "seed": 42, "slopdex_version": "slopdex 0.29.0", "opencode_version": "1.18.34"}
        self.write("manifest.json", self.manifest)

    def write(self, name, value):
        path = self.job / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value) if not isinstance(value, str) else value)

    def event(self, kind, part, timestamp=1000):
        return {"type": kind, "part": part, "timestamp": timestamp}

    def tool(self, name, inputs, output, call_id, *, time=2000, status="completed", metadata=None):
        return self.event("tool_use", {"callID": call_id, "messageID": "step", "tool": name,
                                       "state": {"input": inputs, "output": output, "status": status,
                                                 "metadata": metadata or {}, "time": {"start": time, "end": time + 50}}}, time + 50)

    def trace(self, arm, records, suffix=""):
        self.write(f"trials/{arm}/stdout.jsonl", "\n".join(json.dumps(record) for record in records) + "\n" + suffix)

    def result(self, arm, *, status="ok", steps=2, tokens=None, cost=.02, seconds=20):
        trial = next(t for t in self.trials if t["id"] == arm)
        result = {**trial, "status": status, "steps": steps, "tokens": tokens or {"input": 100, "cache_read": 300, "cache_write": 70, "output": 11, "reasoning": 7, "total": 488},
                  "tools": {"read": 1, "bash": 1}, "cost_usd": cost, "wall_seconds": seconds, "workspace_violations": [],
                  "slopdex_calls": 0 if arm == "off" else 1, "slopdex_commands": {} if arm == "off" else {"map": 1},
                  "grade": {"valid_answer": True, "f1": 1 if status == "ok" else 0, "passed": status == "ok", "precision": 1, "recall": 1,
                            "matches": [{"path": "tsc/internal/a.go", "symbol": "work", "expected_line": 10, "citation_valid": True}]}}
        self.write(f"trials/{arm}/result.json", result)
        self.write(f"trials/{arm}/answer.txt", {"findings": [{"path": "tsc/internal/a.go", "symbol": "work", "line": 10, "explanation": "Returns a value."}], "flow": "work returns"})
        return result

    def test_distinct_tokens_dedup_output_partial_events_and_protocol_quality(self):
        self.result("map-first", status="protocol_violation")
        finish = self.event("step_finish", {"id": "finish", "cost": .02, "tokens": {"input": 100, "cache": {"read": 300, "write": 70}, "output": 11, "reasoning": 7, "total": 488}}, 4000)
        mapping = self.tool("bash", {"command": "slopdex map --private -k fns -g '!**/*_test.go' -e work tsc/internal"}, "@@ 10-14 @@\nfunc work()", "map", time=1500)
        read = self.tool("read", {"filePath": "/sandbox/repo/tsc/internal/a.go", "offset": 10, "limit": 5}, "<content>\n10: func work(\n11:     x int,\n12: ) int {\n13:     return x\n14: }\n</content>", "read", time=2500,
                         metadata={"preview": "THIS MUST NOT COUNT", "display": {"text": "THIS MUST NOT COUNT"}})
        self.trace("map-first", [self.event("step_start", {"messageID": "step"}), mapping,
                                 self.tool("read", {}, "", "read", status="running"), read, finish, finish,
                                 self.event("tool_use", []), self.event("step_finish", "bad")], suffix='not json\n[]\n{"type":')
        self.write("trials/map-first/slopdex-calls.jsonl", "\n".join(json.dumps(record) for record in [
            {"event": "start", "id": "map", "argv": ["map", "--private", "-k", "fns", "-g", "!**/*_test.go"], "time": 1.5},
            {"event": "finish", "id": "map", "exit_code": 0, "end_time": 1.6}]))
        before = report.snapshot(self.job)[0]
        metrics, audit = report.report(self.job)
        row = metrics["trials"][1]
        self.assertEqual(row["tokens"]["output"], 11)
        self.assertEqual(row["tokens"]["reasoning"], 7)
        self.assertEqual(row["input_presentations"], 470)
        self.assertEqual(row["peak_step_input_presentations"], 470)
        self.assertEqual(row["trace_accounting"]["steps"], 1)
        self.assertEqual(row["trace_accounting"]["tool_calls"], 2)
        self.assertEqual(row["trace"]["malformed_lines"], 3)
        self.assertEqual(row["trace"]["malformed_parts"], 2)
        self.assertEqual(row["saved_tool_output_chars_by_kind"]["read"], len(read["part"]["state"]["output"]))
        self.assertEqual(row["strict_f1"], 0)
        self.assertFalse(row["strict_passed"])
        self.assertEqual(row["citation_f1"], 1)
        self.assertTrue(row["valid_answer"])
        self.assertTrue(row["discovery"]["first_map_before_first_successful_source_read"])
        self.assertEqual(row["discovery"]["tools_to_first_source_read"], 2)
        self.assertAlmostEqual(row["discovery"]["seconds_to_first_source_read_from_first_event"], 1.5)
        self.assertEqual(row["source_verification"][0]["signature_end_line_heuristic"], 12)
        self.assertTrue(row["source_verification"][0]["implementation_beyond_signature_exposed_heuristic"])
        self.assertTrue(row["map_argv"][0]["test_exclusion_heuristic"])
        self.assertEqual(row["journal"]["exit_statuses"], {"0": 1})
        self.assertTrue(audit["preservation"]["stable"])
        self.assertEqual(before, report.snapshot(self.job)[0])
        self.assertEqual(metrics["completed"], 1)

    def test_pair_cohorts_ratios_and_invalid_json_not_salvaged(self):
        self.result("off", steps=4, cost=.04, seconds=40, tokens={"input": 200, "cache_read": 600, "cache_write": 140, "output": 22, "reasoning": 14})
        self.result("map-first", status="protocol_violation")
        metrics, _ = report.report(self.job)
        cohorts = metrics["paired_comparisons"][0]["cohorts"]
        self.assertEqual(cohorts["all_available"]["n"], 1)
        self.assertEqual(cohorts["both_status_ok"]["n"], 0)
        self.assertEqual(cohorts["both_valid_answer"]["n"], 1)
        self.assertEqual(cohorts["both_status_ok_and_valid_answer"]["n"], 0)
        metric = cohorts["all_available"]["metrics"]["input_presentations"]
        self.assertEqual(metric["mean_delta"], -470)
        self.assertEqual(metric["mean_case_ratio"], .5)
        self.assertEqual(metric["mean_ratio_effect_pct"], -50)
        self.assertEqual(metrics["paired_cases"][0]["metrics"]["strict_f1"]["delta"], -1)
        self.assertEqual(metrics["paired_cases"][0]["metrics"]["citation_f1"]["delta"], 0)
        self.write("trials/map-first/answer.txt", 'noise {"findings": []}')
        metrics, _ = report.report(self.job)
        self.assertFalse(metrics["trials"][1]["valid_answer"])
        self.assertEqual(metrics["trials"][1]["citation_f1"], 0)
        self.assertEqual(metrics["paired_comparisons"][0]["cohorts"]["both_valid_answer"]["n"], 0)

    def test_shell_reads_mixed_outputs_and_bare_declarations(self):
        self.result("map-first")
        records = [self.tool("bash", {"command": "slopdex map -e work tsc/internal/a.go"}, "@@ 10-14 @@\nfunc work() int", "map"),
                   self.tool("bash", {"command": "slopdex map tsc/internal; sed -n '10,14p' tsc/internal/a.go"}, "map and source\nreturn x", "mixed"),
                   self.tool("read", {"filePath": "/sandbox/repo/tsc/internal/a.go", "offset": 10, "limit": 1}, "<content>\n10: func work(x int) int {\n</content>", "signature"),
                   self.tool("bash", {"command": "sed -n '11,14p' tsc/internal/a.go"}, "    return x\n}\n", "body", time=3000),
                   self.tool("bash", {"command": "sed -n '10,14p' tsc/internal/a.go"}, "missing", "failure", metadata={"exit": 1}),
                   self.tool("bash", {"command": "cat /home/slopdex/work/repo/README.md"}, "not a slopdex command", "path")]
        self.trace("map-first", records)
        metrics, _ = report.report(self.job)
        row = metrics["trials"][1]
        self.assertEqual(row["successful_source_reads_by_kind"], {"read": 1, "bash": 1})
        self.assertEqual(row["saved_bash_output_chars_by_kind"]["mixed_map_source"], len("map and source\nreturn x"))
        self.assertTrue(row["source_verification"][0]["implementation_beyond_signature_exposed_heuristic"])
        self.trace("map-first", records[:3])
        metrics, _ = report.report(self.job)
        self.assertFalse(metrics["trials"][1]["source_verification"][0]["implementation_beyond_signature_exposed_heuristic"])

    def test_malformed_journals_missing_results_and_require_complete(self):
        self.result("map-first")
        self.write("trials/map-first/slopdex-calls.jsonl", '\n'.join(json.dumps(r) for r in [
            {"event": "finish", "id": "orphan", "exit_code": 0},
            {"event": "start", "id": "call", "argv": ["map", "--private"]},
            {"event": "start", "id": "invalid", "argv": 12}]) + '\n{"partial":\n[]')
        self.write("trials/off/result.json", '{"partial":')
        with patch("subprocess.run", side_effect=AssertionError("External command prohibited")), patch("socket.socket", side_effect=AssertionError("Network prohibited")):
            self.assertEqual(report.main([str(self.job), "--require-complete"]), 2)
        metrics = json.loads((self.job / "metrics.json").read_text())
        calls = metrics["trials"][1]["journal"]
        self.assertEqual(calls["counts"], {"map": 1, "unknown": 1})
        self.assertEqual(calls["exit_statuses"], {"unfinished": 2})
        self.assertEqual(calls["orphan_finishes"], 1)
        self.assertEqual(calls["malformed_lines"], 2)
        self.assertEqual(metrics["completed"], 1)
        self.assertEqual(metrics["paired_cases"], [])
        audit = json.loads((self.job / "efficiency-audit.json").read_text())
        self.assertIsNone(audit["trials"][1]["checks"]["no_semantic_or_index_journal_calls"])

    def test_immutable_cache_end_ranges_and_checkpoint(self):
        cache = self.job / "structural.sqlite"
        with closing(sqlite3.connect(cache)) as db, db:
            db.executescript("CREATE TABLE metadata(key TEXT, value TEXT); CREATE TABLE symbols(path TEXT,name TEXT,kind TEXT,start_line INT,end_line INT,signature TEXT); CREATE TABLE embeddings(id INT); CREATE TABLE unit_embeddings(id INT);")
            db.execute("INSERT INTO metadata VALUES('checkpoint',?)", (self.manifest["commit"],))
            db.execute("INSERT INTO symbols VALUES(?,?,?,?,?,?)", ("tsc/internal/a.go", "work", "function", 10, 12, "func work() int"))
        self.manifest["map_index"] = {"status": {"indexPath": str(cache)}}
        self.write("manifest.json", self.manifest)
        self.result("map-first")
        self.trace("map-first", [self.tool("read", {"filePath": "tsc/internal/a.go"}, "<content>\n10: func work() int {\n11: return 1\n12: }\n13: unrelated()\n</content>", "read")])
        before = cache.read_bytes()
        metrics, audit = report.report(self.job)
        cover = metrics["trials"][1]["source_verification"][0]
        self.assertEqual(cover["end_line"], 12)
        self.assertEqual(cover["body_lines_exposed_heuristic"], 1)
        self.assertTrue(audit["structural_cache"]["unchanged"])
        self.assertEqual(before, cache.read_bytes())
        with closing(sqlite3.connect(cache)) as db, db:
            db.execute("UPDATE metadata SET value='wrong'")
        _, audit = report.report(self.job)
        self.assertFalse(audit["structural_cache"]["used"])
        self.assertIn("checkpoint", audit["structural_cache"]["reason"].lower())

    def test_zero_denominator_and_saved_grade_without_answer(self):
        self.result("off", cost=0)
        self.result("map-first", status="protocol_violation")
        (self.job / "trials/map-first/answer.txt").unlink()
        metrics, _ = report.report(self.job)
        self.assertEqual(metrics["trials"][1]["citation_f1"], 1)
        self.assertEqual(metrics["trials"][1]["citation_score"]["method"], "saved_valid_grade_precision_recall_answer_missing")
        metric = metrics["paired_comparisons"][0]["cohorts"]["all_available"]["metrics"]["cost_usd"]
        self.assertEqual(metric["n"], 1)
        self.assertEqual(metric["ratio_n"], 0)
        self.assertIsNone(metric["mean_case_ratio"])
        result = json.loads((self.job / "trials/off/result.json").read_text())
        result["grade"]["matches"] = []
        self.write("trials/off/result.json", result)
        comparison = json.loads((self.job / "trials/map-first/result.json").read_text())
        comparison["grade"]["matches"] = []
        self.write("trials/map-first/result.json", comparison)
        metrics, _ = report.report(self.job)
        self.assertEqual(metrics["trials"][0]["citation_f1"], 1)
        self.assertEqual(metrics["trials"][0]["citation_score"]["method"], "saved_valid_grade_precision_recall_gold_unavailable")

    def test_frozen_controls_subset_and_multi_map_journal_order(self):
        self.result("map-first")
        config = {"model": "fixture/model", "small_model": "fixture/model", "share": "disabled", "autoupdate": False,
                  "snapshot": False, "lsp": False, "formatter": False, "plugin": [], "instructions": ["/sandbox/repo/AGENTS.md"],
                  "permission": {key: "deny" for key in ("edit", "task", "question", "webfetch", "websearch", "skill", "external_directory")},
                  "provider": {"fixture": {"models": {"model": {"variants": {"medium": {"reasoningEffort": "medium"}}}}}}}
        config["agent"] = {"build": {"permission": config["permission"], "steps": 40}, "title": {"disable": True}, "summary": {"disable": True}}
        verified = {key: value for key, value in config.items() if key != "plugin"}
        verified["agent"] = {"build": config["agent"]["build"]}
        self.trials[1]["model_config"] = {"options": {"reasoningEffort": "medium"}}
        self.manifest["config"] = {"max_steps": 40, "models": [self.trials[1]["model_config"]]}
        self.manifest["arm_instructions"] = {"map-first": "frozen guidance\n"}
        self.write("manifest.json", self.manifest)
        self.write("trials/map-first/opencode.json", config)
        self.write("trials/map-first/verified-config.json", verified)
        self.write("trials/map-first/AGENTS.md", report.BASE_INSTRUCTIONS + "frozen guidance\n")
        self.write("trials/map-first/prompt.txt", self.task["prompt"] + "\nJSON schema")
        self.trace("map-first", [self.tool("read", {"filePath": "tsc/internal/a.go"}, "<content>\n10: func work() int {\n11: return 1\n12: }\n</content>", "read", time=3000)])
        self.write("trials/map-first/slopdex-calls.jsonl", "\n".join(json.dumps(record) for record in [
            {"event": "start", "id": "first", "argv": ["map"], "time": 2},
            {"event": "finish", "id": "first", "exit_code": 0},
            {"event": "start", "id": "second", "argv": ["map", "-e", "other"], "time": 4},
            {"event": "start", "id": "third", "argv": ["map", "-e", "third"], "time": 5}]))
        metrics, audit = report.report(self.job)
        checks = audit["trials"][1]["checks"]
        self.assertTrue(checks["verified_config_matches_generated"])
        self.assertTrue(checks["frozen_instructions_match"])
        self.assertTrue(checks["config_controls_match"])
        discovery = metrics["trials"][1]["discovery"]
        self.assertTrue(discovery["first_map_before_first_successful_source_read"])
        self.assertTrue(discovery["first_successful_map_before_first_successful_source_read"])
        self.assertEqual(discovery["maps_after_or_at_first_source_tool"], 2)
        self.assertEqual(discovery["late_maps_after_last_source_tool"], 2)
        verified["permission"] = {"edit": "allow"}
        self.write("trials/map-first/verified-config.json", verified)
        _, audit = report.report(self.job)
        self.assertFalse(audit["trials"][1]["checks"]["verified_config_matches_generated"])

    def test_concurrent_changes_are_audited_and_outputs_do_not_affect_hashes(self):
        self.result("off")
        original = report.snapshot

        def concurrent_snapshot(job, capture=False):
            if not capture:
                self.write("trials/map-first/stdout.jsonl", '{"type":"step_start"}\n')
            return original(job, capture)

        with patch.object(report, "snapshot", side_effect=concurrent_snapshot):
            _, audit = report.report(self.job)
        self.assertFalse(audit["preservation"]["stable"])
        self.assertEqual(audit["preservation"]["added"], ["trials/map-first/stdout.jsonl"])
        _, audit = report.report(self.job)
        self.assertTrue(audit["preservation"]["stable"])

    def test_malformed_event_shapes_do_not_fabricate_peak_context(self):
        self.result("map-first")
        self.trace("map-first", [self.event("step_start", {"messageID": []}),
                                 self.event("tool_use", {"messageID": [], "tool": "read", "state": {"input": [], "output": None}}),
                                 self.event("step_finish", {"id": [], "tokens": "bad", "cost": "bad"}),
                                 self.event("text", {"messageID": [], "text": []})])
        metrics, _ = report.report(self.job)
        row = metrics["trials"][1]
        self.assertIsNone(row["peak_step_input_presentations"])
        self.assertEqual(row["trace"]["steps_missing_input_accounting"], 1)
        self.assertEqual(row["successful_source_read_calls"], 0)


if __name__ == "__main__":
    unittest.main()
