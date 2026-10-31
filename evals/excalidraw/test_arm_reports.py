"""Offline no-map / README-guided map reporting regressions."""

import copy
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import report as reporting
import run as runner


class ArmReportTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="typescript-arm-reports-")
        self.addCleanup(self.temporary.cleanup)
        self.output = Path(self.temporary.name)
        self.trials, self.results = [], []

    def add_trial(self, arm, *, task="fixture", repeat=1, model="test/model", variant="medium",
                  f1=1.0, seconds=10.0, tokens=None, cost=0.01, steps=2,
                  status="ok", commands=None, completed=True):
        trial = {"id": f"trial-{len(self.trials)}", "model": model, "variant": variant,
                 "arm": arm, "task": task, "repeat": repeat}
        self.trials.append(trial)
        if not completed:
            return trial
        commands = dict(commands or {})
        result = {
            **trial, "tokens": dict(tokens if tokens is not None else {
                "input": 100, "output": 20, "reasoning": 5, "cache_read": 3, "cache_write": 2, "total": 0}),
            "cost_usd": cost, "steps": steps, "status": status, "wall_seconds": seconds,
            "grade": {"valid_answer": True, "f1": f1, "passed": f1 == 1},
            "slopdex_calls": sum(commands.values()), "slopdex_commands": commands,
        }
        self.results.append(copy.deepcopy(result))
        runner.write_json(self.output / "trials" / trial["id"] / "result.json", result)
        return trial

    def manifest(self, **metadata):
        return {"commit": "a" * 40, "trials": self.trials, **metadata}

    def saved_report(self, **metadata):
        runner.write_json(self.output / "manifest.json", self.manifest(**metadata))
        originals = {path: runner.read_json(path) for path in (self.output / "trials").glob("*/result.json")}
        with patch.object(runner, "command", side_effect=AssertionError("external command")), \
                patch.object(runner, "run_trial", side_effect=AssertionError("model call")), \
                patch.object(runner, "prepare_map_index", side_effect=AssertionError("index preparation")):
            summary = reporting.report(self.output)
        self.assertEqual(summary, runner.read_json(self.output / "summary.json"))
        for path, original in originals.items():
            updated = runner.read_json(path)
            original.pop("slopdex_commands", None)
            updated.pop("slopdex_commands", None)
            self.assertEqual(original, updated)
        return summary, (self.output / "report.md").read_text()

    def test_full_off_map_metrics_and_report(self):
        self.add_trial("off", f1=.5, seconds=20, steps=4, cost=.04)
        self.add_trial("map", seconds=10, steps=2, cost=.02,
                       tokens={"input": 50, "output": 10, "reasoning": 3, "cache_read": 2, "cache_write": 1, "total": 66},
                       commands={"map": 3, "help": 1})
        summary, text = self.saved_report()
        self.assertEqual(summary["comparison_mode"], "interleaved")
        self.assertEqual((summary["completed"], summary["planned"]), (2, 2))
        baseline, mapped = summary["groups"]
        self.assertEqual(baseline["mean_tokens"]["total"], 130)
        self.assertEqual(mapped["mean_tokens"]["total"], 66)
        self.assertEqual(mapped["mean_steps"], 2)
        self.assertEqual(mapped["total_cost_usd"], .02)
        self.assertEqual(mapped["slopdex_command_trials"], {"help": 1, "map": 1})
        self.assertEqual(set(mapped["mean_tokens"]), set(reporting.TOKEN_NAMES))
        case = summary["paired"][0]
        self.assertEqual((case["reference_arm"], case["comparison_arm"]), ("off", "map"))
        self.assertEqual(case["f1_delta"], .5)
        self.assertEqual(case["seconds_delta"], -10)
        self.assertEqual(case["steps_delta"], -2)
        self.assertEqual(case["total_tokens_delta"], -64)
        metric = summary["paired_summary"][0]["cohorts"]["all_pairs"]["metrics"]["wall_seconds"]
        self.assertEqual((metric["mean_delta"], metric["mean_delta_pct"], metric["aggregate_ratio"]), (-10, -50, .5))
        self.assertIn("Side-by-side means and accuracy", text)
        self.assertIn("Cache write", text)
        self.assertIn("Observed execution and spend totals", text)
        self.assertIn("`map` × 3", text)
        self.assertEqual(summary["slopdex_commands"], {"help": 1, "map": 3})

    def test_failure_consumption_is_kept_but_valid_cohorts_are_separate(self):
        self.add_trial("off", repeat=1, seconds=100, steps=10, cost=1)
        self.add_trial("map", repeat=1, seconds=1, steps=1, cost=.01, status="agent_error", f1=1)
        self.add_trial("off", repeat=2, seconds=20, steps=4, cost=.2, f1=.5)
        self.add_trial("map", repeat=2, seconds=10, steps=2, cost=.1)
        self.add_trial("off", repeat=3, seconds=30, steps=6, cost=.3)
        self.add_trial("map", repeat=3, seconds=25, steps=5, cost=.25)
        summary, text = self.saved_report()
        failed = summary["paired"][0]
        self.assertTrue(failed["failure_shortened_possible"])
        self.assertFalse(failed["both_ok"])
        self.assertFalse(failed["both_passed"])
        self.assertEqual(failed["f1_delta"], -1)
        self.assertEqual(failed["seconds_delta"], -99)
        self.assertAlmostEqual(failed["cost_delta_usd"], -.99)
        cohorts = summary["paired_summary"][0]["cohorts"]
        self.assertEqual({key: value["n"] for key, value in cohorts.items()}, {"all_pairs": 3, "both_ok": 2, "both_passed": 1})
        self.assertEqual(cohorts["all_pairs"]["failure_shortened_possible_n"], 1)
        self.assertEqual(cohorts["both_ok"]["metrics"]["wall_seconds"]["mean_delta"], -7.5)
        self.assertEqual(cohorts["both_passed"]["metrics"]["wall_seconds"]["mean_delta"], -5)
        mapped = summary["groups"][1]
        self.assertEqual(mapped["passed"], 2)
        self.assertAlmostEqual(mapped["mean_f1"], 2 / 3)
        self.assertIn("failure-shortened runs can look cheaper/faster", text)
        self.assertIn("survivor bias", text)

    def test_missing_telemetry_total_fallback_and_reported_total_never_double_count(self):
        self.add_trial("off", cost=None, tokens={"input": 100, "output": 20, "reasoning": 5, "cache_read": 3, "cache_write": 2})
        self.add_trial("map", tokens={"input": 100, "output": 20, "reasoning": 5, "cache_read": 3, "cache_write": 2, "total": 125})
        self.add_trial("off", repeat=2, tokens={}, seconds=None, steps=None)
        self.add_trial("map", repeat=2, tokens={"input": 0, "output": 0, "reasoning": 0, "cache_read": 0, "cache_write": 0, "total": 0})
        summary, text = self.saved_report()
        first, second = summary["paired"]
        self.assertEqual(first["total_tokens_sources"], {"off": "category_sum", "map": "reported"})
        self.assertEqual(first["total_tokens_delta"], -5)
        self.assertIsNone(first["cost_delta_usd"])
        self.assertIsNone(second["total_tokens_delta"])
        baseline, mapped = summary["groups"]
        self.assertEqual(baseline["mean_tokens"]["total"], 130)
        self.assertEqual(mapped["mean_tokens"]["total"], 62.5)
        self.assertEqual(mapped["total_tokens_sources"], {"category_sum": 1, "reported": 1})
        self.assertEqual(baseline["missing_telemetry"]["cost_usd"], 1)
        self.assertEqual(baseline["missing_telemetry"]["total_tokens"], 1)
        metrics = summary["paired_summary"][0]["cohorts"]["all_pairs"]["metrics"]
        self.assertEqual((metrics["cost_usd"]["n"], metrics["cost_usd"]["missing_n"]), (1, 1))
        self.assertEqual(metrics["total_tokens"]["off_missing_n"], 1)
        self.assertEqual(metrics["total_tokens"]["sum_map"], 125)
        self.assertIn("reasoning is distinct", text)
        self.assertIn("missing is not zero", text)

    def test_zero_denominators_and_case_vs_aggregate_effects(self):
        for repeat, seconds, cost in ((1, 0, 0), (2, 10, .1), (3, 100, 1)):
            self.add_trial("off", repeat=repeat, seconds=seconds, cost=cost)
            self.add_trial("map", repeat=repeat, seconds=20, cost=.2)
        summary = reporting.build_summary(self.manifest(), self.results)
        metric = summary["paired_summary"][0]["cohorts"]["all_pairs"]["metrics"]["wall_seconds"]
        self.assertEqual((metric["n"], metric["ratio_n"], metric["zero_reference_n"]), (3, 2, 1))
        self.assertAlmostEqual(metric["mean_case_ratio"], 1.1)
        self.assertAlmostEqual(metric["mean_delta_pct"], 10)
        self.assertAlmostEqual(metric["aggregate_ratio"], 60 / 110)
        self.assertIsNone(summary["paired"][0]["metrics"]["cost_usd"]["delta_pct"])

    def test_partial_and_single_arm_runs_match_task_and_repeat(self):
        self.add_trial("off", task="first")
        self.add_trial("map", task="first", completed=False)
        self.add_trial("off", task="second", repeat=2, completed=False)
        self.add_trial("map", task="second", repeat=2)
        self.add_trial("off", task="third")
        self.add_trial("map", task="third")
        summary, text = self.saved_report()
        self.assertEqual((summary["completed"], summary["planned"]), (4, 6))
        self.assertEqual([row["task"] for row in summary["paired"]], ["third"])
        comparison = summary["paired_summary"][0]
        self.assertEqual((comparison["available_pairs"], comparison["planned_pairs"], comparison["missing_pairs"]), (1, 3, 2))
        self.assertEqual(comparison["unmatched_results"], {"off": 1, "map": 1})
        self.assertIn("Partial run", text)
        empty = reporting.build_summary(self.manifest(), [])
        self.assertEqual(empty["paired"], [])
        self.assertEqual(empty["groups"][0]["n"], 0)
        self.assertIsNone(empty["groups"][0]["total_cost_usd"])
        single = reporting.build_summary({"trials": self.trials[:1]}, self.results[:1])
        self.assertEqual(single["paired"], [])
        self.assertEqual(single["paired_summary"][0]["planned_pairs"], 0)

    def test_model_variant_pairing_is_deterministic_and_only_off_map(self):
        for model, variant in (("test/two", "low"), ("test/one", "high"), ("test/one", "low")):
            for repeat in (2, 1):
                self.add_trial("map", model=model, variant=variant, repeat=repeat)
                self.add_trial("off", model=model, variant=variant, repeat=repeat)
        self.add_trial("search")
        self.add_trial("off", model="test/unmatched", variant="high")
        self.add_trial("map", model="test/unmatched", variant="low")
        before = copy.deepcopy((self.trials, self.results))
        summary = reporting.build_summary(self.manifest(), self.results)
        reverse = reporting.build_summary({"trials": list(reversed(self.trials))}, list(reversed(self.results)))
        self.assertEqual(summary, reverse)
        self.assertEqual((self.trials, self.results), before)
        self.assertEqual(len(summary["paired"]), 6)
        self.assertEqual(summary["ignored_planned_trials"], 1)
        self.assertEqual({row["arm"] for row in summary["groups"]}, {"off", "map"})
        self.assertIn("1 historical trials from other arms are excluded", reporting.markdown(self.manifest(), summary))
        self.assertEqual([(row["model"], row["variant"]) for row in summary["paired_summary"]],
                         [("test/one", "high"), ("test/one", "low"), ("test/two", "low"), ("test/unmatched", "high"), ("test/unmatched", "low")])

    def test_saved_historical_mode_and_sources_with_preparation(self):
        self.add_trial("off")
        self.add_trial("map")
        metadata = {"comparison_mode": "historical_saved", "sources": {"off": "baseline.json", "map": "map.json"},
                    "baseline_source": "older-job", "map_symbol_index": {"wall_seconds": 2, "native_ann": {"wall_seconds": 3}}}
        summary, text = self.saved_report(**metadata)
        self.assertEqual(summary["comparison_mode"], "historical_saved")
        self.assertEqual(summary["sources"]["baseline_source"], "older-job")
        self.assertIn("not an interleaved experiment", text)
        self.assertIn("baseline.json", text)
        self.assertIn("Map symbol-index preparation", text)
        self.assertIn("symbol embedding/ANN preparation: 3.0s", text)
        self.assertEqual(text, reporting.markdown(self.manifest(**metadata), summary))

    def test_command_backfill_prefers_journal_then_saved_then_unknown(self):
        self.add_trial("off")
        logged = self.add_trial("map", commands={"map": 99})
        directory = self.output / "trials" / logged["id"]
        (directory / "slopdex-calls.jsonl").write_text('{"argv":["map"],"exit_code":0}\n{"argv":["--help"]}\n')
        legacy = self.add_trial("map", repeat=2, commands={"map": 2})
        path = self.output / "trials" / legacy["id"] / "result.json"
        result = runner.read_json(path)
        result.pop("slopdex_commands")
        runner.write_json(path, result)
        summary, text = self.saved_report()
        self.assertEqual(summary["slopdex_commands"], {"help": 1, "map": 1, "unknown": 2})
        logged_row = next(row for row in summary["trials"] if row["id"] == logged["id"])
        self.assertEqual(logged_row["slopdex_calls"], 2)
        self.assertEqual(logged_row["saved_slopdex_calls"], 99)
        self.assertEqual(runner.read_json(directory / "result.json")["slopdex_commands"], {"help": 1, "map": 1})
        self.assertEqual(runner.read_json(path)["grade"], result["grade"])
        self.assertEqual(runner.read_json(path)["slopdex_commands"], {"unknown": 2})
        self.assertIn("Older count-only artifacts", text)
        self.assertIn("By task and trial", text)

    def test_duplicate_or_mismatched_identities_are_not_arbitrarily_paired(self):
        self.add_trial("off")
        self.add_trial("map")
        with self.assertRaisesRegex(ValueError, "Duplicate result"):
            reporting.build_summary(self.manifest(), self.results + self.results[:1])
        with self.assertRaisesRegex(ValueError, "Duplicate planned"):
            reporting.build_summary({"trials": self.trials + self.trials[:1]}, self.results)
        changed = copy.deepcopy(self.results)
        changed[0]["variant"] = "other"
        with self.assertRaisesRegex(ValueError, "does not match"):
            reporting.build_summary(self.manifest(), changed)

    def test_zero_call_map_trial_is_still_listed(self):
        self.add_trial("off")
        self.add_trial("map")
        _, text = self.saved_report(map_index={"wall_seconds": 2})
        usage = text.split("### By task and trial", 1)[1]
        self.assertIn("| test/model | medium | map | fixture | 1 | 0 | — |", usage)
        self.assertNotIn("| test/model | medium | off | fixture", usage)
        self.assertIn("Structural-index preparation", text)


if __name__ == "__main__":
    unittest.main()
