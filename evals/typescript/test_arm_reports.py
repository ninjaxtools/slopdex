"""Offline report regressions using synthetic completed trial artifacts."""

import copy
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import run as runner


class ArmReportTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="typescript-arm-reports-")
        self.addCleanup(self.temporary.cleanup)
        self.output = Path(self.temporary.name)
        self.trials = []
        self.results = {}

    def add_trial(self, arm, *, task="fixture", repeat=1, model="test/model", variant="medium",
                  f1=1.0, seconds=10.0, input_tokens=100, cost=0.01, status="ok",
                  commands=None, completed=True):
        trial = {"id": f"trial-{len(self.trials)}", "model": model, "variant": variant,
                 "arm": arm, "task": task, "repeat": repeat}
        self.trials.append(trial)
        if not completed:
            return trial
        commands = dict(commands or {})
        result = {
            **trial, "tokens": {"input": input_tokens, "output": 20, "reasoning": 5,
                                "total": input_tokens + 25, "cache_read": 3, "cache_write": 2},
            "tools": {"bash": 1}, "cost_usd": cost, "steps": 1, "errors": [],
            "sessions": ["fixture-session"], "malformed_event_lines": 0,
            "status": status, "exit_code": 0, "timed_out": status == "timeout",
            "wall_seconds": seconds,
            "grade": {"valid_answer": True, "symbol_recall": f1, "recall": f1,
                      "precision": f1, "f1": f1, "passed": f1 == 1,
                      "matches": [{"path": "fixture.go", "symbol": "fixture", "identified": True,
                                   "citation_valid": True, "expected_line": 1}]},
            "workspace_violations": [], "slopdex_calls": sum(commands.values()),
            "slopdex_commands": commands, "native_ann": None,
        }
        self.results[trial["id"]] = copy.deepcopy(result)
        runner.write_json(self.output / "trials" / trial["id"] / "result.json", result)
        return trial

    def report(self, **metadata):
        runner.write_json(self.output / "manifest.json", {"commit": "a" * 40,
                                                       "trials": self.trials, **metadata})
        # Reports must be entirely offline, even with preparation metadata present.
        with patch.object(runner, "command", side_effect=AssertionError("external command")), \
                patch.object(runner, "run_trial", side_effect=AssertionError("model call")), \
                patch.object(runner, "prepare_index", side_effect=AssertionError("index preparation")):
            summary = runner.report(self.output)
        self.assertEqual(summary, runner.read_json(self.output / "summary.json"))
        for trial_id, original in self.results.items():
            result = runner.read_json(self.output / "trials" / trial_id / "result.json")
            self.assertEqual(result, {**original, "slopdex_commands": result["slopdex_commands"]})
        return summary, (self.output / "report.md").read_text()

    def assert_pair(self, row, reference, comparison, *, f1, seconds, tokens, cost):
        self.assertEqual((row["reference_arm"], row["comparison_arm"]), (reference, comparison))
        self.assertAlmostEqual(row["f1_delta"], f1)
        self.assertTrue(row["both_ok"])
        self.assertAlmostEqual(row["seconds_delta"], seconds)
        self.assertEqual(row["input_tokens_delta"], tokens)
        self.assertAlmostEqual(row["cost_delta_usd"], cost)

    def test_map_search_deltas_and_invocation_counters(self):
        self.add_trial("search", f1=1, seconds=8, input_tokens=70, cost=.02, commands={"search": 2})
        self.add_trial("map", f1=.5, seconds=12, input_tokens=110, cost=.03, commands={"map": 3})
        summary, text = self.report()
        self.assertEqual(len(summary["paired"]), 1)
        self.assert_pair(summary["paired"][0], "map", "search", f1=.5, seconds=-4, tokens=-40, cost=-.01)
        self.assertEqual(summary["slopdex_commands"], {"map": 3, "search": 2})
        for group in summary["groups"]:
            command = group["arm"]
            self.assertEqual(group["slopdex_command_trials"], {command: 1})
            self.assertEqual(group["slopdex_commands"], {command: 3 if command == "map" else 2})
        self.assertIn("**test/model / medium / search − map**: 1 pairs; mean F1 Δ +0.500", text)
        self.assertIn("| test/model | medium | search − map | fixture | 1 | +0.500 |", text)

    def test_legacy_off_slopdex_and_count_only_backfill(self):
        self.add_trial("off", f1=.25, seconds=15, input_tokens=200, cost=.04)
        trial = self.add_trial("slopdex", f1=.75, seconds=10, input_tokens=120, cost=.01,
                               commands={"map": 2})
        path = self.output / "trials" / trial["id"] / "result.json"
        result = runner.read_json(path)
        result.pop("slopdex_commands")
        runner.write_json(path, result)
        summary, text = self.report()
        self.assert_pair(summary["paired"][0], "off", "slopdex", f1=.5, seconds=-5, tokens=-80, cost=-.03)
        self.assertEqual(summary["slopdex_commands"], {"unknown": 2})
        self.assertEqual(runner.read_json(path)["grade"], result["grade"])
        self.assertIn("slopdex − off", text)

    def test_combined_navigation_is_compared_with_both_single_tool_arms(self):
        self.add_trial("map-search", f1=.8, seconds=90, input_tokens=120, cost=.02)
        self.add_trial("search", f1=.9, seconds=100, input_tokens=140, cost=.03)
        self.add_trial("map", f1=.7, seconds=50, input_tokens=100, cost=.01)
        summary, text = self.report()
        self.assertEqual(len(summary["paired"]), 3)
        self.assert_pair(summary["paired"][0], "map", "search", f1=.2, seconds=50, tokens=40, cost=.02)
        self.assert_pair(summary["paired"][1], "map", "map-search", f1=.1, seconds=40, tokens=20, cost=.01)
        self.assert_pair(summary["paired"][2], "search", "map-search", f1=-.1, seconds=-10, tokens=-20, cost=-.01)
        for reference, comparison in (("map", "search"), ("map", "map-search"), ("search", "map-search")):
            self.assertIn(f"**test/model / medium / {comparison} − {reference}**", text)

    def test_partial_runs_pair_only_matching_completed_trials(self):
        self.add_trial("search", task="first", f1=.75)
        self.add_trial("off", task="first", f1=.25)
        self.add_trial("map", task="first", completed=False)
        self.add_trial("map", task="second", repeat=2)
        self.add_trial("search", task="second", repeat=1)
        self.add_trial("off", task="second", repeat=2, completed=False)
        summary, _ = self.report()
        self.assertEqual((summary["completed"], summary["planned"]), (4, 6))
        self.assertEqual(len(summary["paired"]), 1)
        self.assertEqual(summary["paired"][0]["task"], "first")
        self.assertEqual((summary["paired"][0]["reference_arm"], summary["paired"][0]["comparison_arm"]),
                         ("off", "search"))

    def test_single_arm_and_empty_results_have_no_pairs(self):
        self.add_trial("map")
        summary, text = self.report()
        self.assertEqual(summary["paired"], [])
        self.assertIn("| test/model | medium | map | fixture | 1 | 0 | — |", text)
        (self.output / "trials" / self.trials[0]["id"] / "result.json").unlink()
        self.results.clear()
        summary, _ = self.report()
        self.assertEqual(summary["completed"], 0)
        self.assertEqual(summary["paired"], [])

    def test_three_and_four_arms_generate_every_combination_in_order(self):
        expected_three = [("off", "map"), ("off", "search"), ("map", "search")]
        expected_four = [("off", "map"), ("off", "search"), ("off", "slopdex"),
                         ("map", "search"), ("map", "slopdex"), ("search", "slopdex")]
        for arms, expected in ((["search", "map", "off"], expected_three),
                               (["slopdex", "search", "map", "off"], expected_four)):
            with self.subTest(arms=arms):
                self.trials.clear()
                self.results.clear()
                for arm in arms:
                    self.add_trial(arm, f1={"off": .1, "map": .3, "search": .7, "slopdex": 1}[arm])
                summary, text = self.report()
                actual = [(row["reference_arm"], row["comparison_arm"]) for row in summary["paired"]]
                self.assertEqual(actual, expected)
                headings = [text.index(f"**test/model / medium / {comparison} − {reference}**")
                            for reference, comparison in expected]
                self.assertEqual(headings, sorted(headings))
                for row in summary["paired"]:
                    self.assertGreater(row["f1_delta"], 0)

    def test_pair_summary_separates_models_variants_and_comparisons(self):
        for model, variant in (("test/one", "low"), ("test/one", "high"), ("test/two", "low")):
            for repeat in (1, 2):
                for arm, f1 in (("off", .25), ("map", .5), ("search", 1)):
                    self.add_trial(arm, model=model, variant=variant, repeat=repeat, f1=f1)
        summary, text = self.report()
        self.assertEqual(len(summary["paired"]), 18)
        self.assertEqual(text.count("**: 2 pairs;"), 9)
        self.assertIn("**test/one / low / search − map**: 2 pairs; mean F1 Δ +0.500", text)
        self.assertIn("**test/two / low / search − off**: 2 pairs; mean F1 Δ +0.750", text)

    def test_invalid_pairs_and_missing_cost_preserve_legacy_delta_rules(self):
        self.add_trial("map", f1=1, seconds=8, cost=None)
        self.add_trial("search", f1=0, status="timeout", seconds=100)
        self.add_trial("map", repeat=2, f1=.5, cost=None)
        self.add_trial("search", repeat=2, f1=1, seconds=6)
        summary, text = self.report()
        invalid, valid = summary["paired"]
        self.assertEqual(invalid["f1_delta"], -1)
        self.assertFalse(invalid["both_ok"])
        for key in ("seconds_delta", "input_tokens_delta", "cost_delta_usd"):
            self.assertNotIn(key, invalid)
        self.assertTrue(valid["both_ok"])
        self.assertEqual(valid["seconds_delta"], -4)
        self.assertNotIn("cost_delta_usd", valid)
        self.assertIn("mean F1 Δ -0.250; mean time Δ -4.0s (1 valid pairs)", text)

    def test_zero_call_treatment_rows_and_both_preparation_sections(self):
        for arm in ("off", "map", "search", "map-search", "slopdex"):
            self.add_trial(arm)
        summary, text = self.report(index={"wall_seconds": 12, "native_ann": {"wall_seconds": 3},
                                          "coverage": {"parser_diagnostics": 2, "validated_targets": 4}},
                                    map_index={"wall_seconds": 5})
        self.assertEqual(summary["slopdex_commands"], {})
        usage = text.split("### By task and trial", 1)[1]
        for arm in ("map", "search", "map-search", "slopdex"):
            self.assertIn(f"| test/model | medium | {arm} | fixture | 1 | 0 | — |", usage)
        self.assertNotIn("| test/model | medium | off | fixture", usage)
        self.assertIn("## Index preparation", text)
        self.assertIn("One-time preparation: 12.0s", text)
        self.assertIn("Native ANN warm-up: 3.0s", text)
        self.assertIn("Search and combined (`map-search` and `slopdex`) trials", text)
        self.assertIn("## Structural-index preparation", text)
        self.assertIn("One-time structural-index preparation: 5.0s", text)
        self.assertIn("validated searchable task targets: 4", text)

    def test_structural_only_preparation_has_no_ann_statement(self):
        self.add_trial("map")
        _, text = self.report(map_index={"wall_seconds": 2})
        self.assertIn("## Structural-index preparation", text)
        self.assertNotIn("Native ANN warm-up", text)


if __name__ == "__main__":
    unittest.main()
