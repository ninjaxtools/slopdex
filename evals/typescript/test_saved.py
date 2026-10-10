"""Offline regressions for durable eval records and per-trial reuse controls."""

import copy
from contextlib import ExitStack
import io
import json
import os
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest.mock import patch

import run as runner
import saved


class SavedFixture(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="typescript-saved-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.results = self.root / "results"
        self.config = {
            "schema_version": 1,
            "models": [{"model": "fixture/model", "variant": "medium",
                        "options": {"reasoningEffort": "medium", "temperature": .2}}],
            "timeout_seconds": 30, "max_steps": 10, "index_timeout_seconds": 60,
            "slopdex": {"provider": "openai", "model": "fixture-embedding", "dimensions": 3,
                        "include": ["*.go"], "rerankingEnabled": False},
        }
        self.task = {"id": "fixture", "title": "Fixture task", "prompt": "Find all three roles.",
                     "targets": [{"path": "fixture.go", "symbol": name}
                                 for name in ("alpha", "beta", "gamma")]}

    def manifest(self, *, config=None, tasks=None, arms=("off", "map"), repeats=1):
        config = copy.deepcopy(config if config is not None else self.config)
        tasks = copy.deepcopy(tasks if tasks is not None else [self.task])
        return {
            "config": config, "tasks": tasks, "commit": "a" * 40,
            "trials": runner.make_trials(config, tasks, list(arms), repeats, 0),
            "arm_instructions": {arm: runner.arm_instructions(arm) for arm in arms},
            "opencode_version": "fixture-opencode-1", "slopdex_version": "fixture-slopdex-1",
            "implementation": {"commit": "b" * 40, "binary_sha256": "c" * 64},
            "harness_sha256": "d" * 64, "seed": 0,
        }

    @staticmethod
    def changed(value, path, replacement):
        result = copy.deepcopy(value)
        target = result
        for key in path[:-1]:
            target = target[key]
        target[path[-1]] = replacement
        return result

    def controls(self, manifest, trial, *, prompt=None, instructions=None):
        task = next(task for task in manifest["tasks"] if task["id"] == trial["task"])
        return saved.trial_controls(
            manifest, trial,
            prompt=prompt if prompt is not None else runner.task_prompt(task),
            instructions=instructions if instructions is not None else
            runner.BASE_INSTRUCTIONS + runner.arm_instructions(trial["arm"]),
        )

    def fingerprint(self, manifest, trial, **kwargs):
        return runner.digest(self.controls(manifest, trial, **kwargs))

    def job(self, name, manifest=None, *, missing=()):
        manifest = copy.deepcopy(manifest if manifest is not None else self.manifest())
        output = self.root / name
        runner.write_json(output / "manifest.json", manifest)
        for trial in manifest["trials"]:
            if trial["id"] in missing:
                continue
            task = next(task for task in manifest["tasks"] if task["id"] == trial["task"])
            self.write_trial(output / "trials" / trial["id"], trial, task)
        return output

    def write_trial(self, directory, trial, task, *, instruction=None):
        f1 = 1.0 if trial["arm"] == "map" else .5
        commands = {"map": 1} if trial["arm"] == "map" else {}
        result = {
            **{key: value for key, value in trial.items() if key != "model_config"},
            "status": "ok", "exit_code": 0, "timed_out": False, "wall_seconds": 5.0,
            "tokens": {"input": 100, "output": 20, "reasoning": 5,
                       "cache_read": 0, "cache_write": 0, "total": 125},
            "tools": {"bash": 1}, "cost_usd": .01, "steps": 1, "errors": [],
            "sessions": ["fixture-session"], "malformed_event_lines": 0,
            "grade": {"valid_answer": True, "symbol_recall": f1, "recall": f1,
                      "precision": f1, "f1": f1, "passed": f1 == 1, "matches": []},
            "workspace_violations": [], "slopdex_protocol_violations": [],
            "slopdex_calls": sum(commands.values()), "slopdex_commands": commands,
            "native_ann": None,
        }
        runner.write_json(directory / "result.json", result)
        (directory / "prompt.txt").write_text(runner.task_prompt(task))
        guidance = instruction if instruction is not None else runner.arm_instructions(trial["arm"])
        (directory / "AGENTS.md").write_text(runner.BASE_INSTRUCTIONS + guidance)
        (directory / "answer.txt").write_text(json.dumps({"findings": [], "flow": "Fixture answer"}))
        return result


class TrialControlTests(SavedFixture):
    def test_controls_include_full_task_and_selected_model(self):
        manifest = self.manifest()
        for trial in manifest["trials"]:
            with self.subTest(arm=trial["arm"]):
                controls = self.controls(manifest, trial)
                self.assertEqual(controls["task"], self.task)
                self.assertEqual(controls["model"], self.config["models"][0])
                self.assertEqual(controls["protocol_version"], saved.PROTOCOL_VERSION)
                self.assertEqual(controls["opencode_config"], runner.opencode_config(
                    trial["model_config"], trial["arm"], self.config["max_steps"]))
                for key in ("slopdex_config", "slopdex_version", "implementation", "harness_sha256"):
                    self.assertEqual(key in controls, trial["arm"] == "map")

    def test_fingerprints_ignore_other_trials_models_tasks_and_scheduling(self):
        manifest = self.manifest()
        changed = copy.deepcopy(manifest)
        changed["config"]["models"].append({"model": "other/model", "variant": "high", "options": {}})
        changed["tasks"].append({**self.task, "id": "other-task", "prompt": "Another prompt"})
        changed["trials"] = runner.make_trials(changed["config"], changed["tasks"], ["map", "off"], 3, 91)
        changed.update({"seed": 91, "fingerprint": "whole-run-changed", "reused_trials": ["other"],
                        "index": {"wall_seconds": 123}, "map_symbol_index": {"wall_seconds": 456}})
        for trial in manifest["trials"]:
            with self.subTest(arm=trial["arm"]):
                self.assertEqual(self.fingerprint(manifest, trial), self.fingerprint(changed, trial))

    def test_slopdex_changes_invalidate_map_but_not_baseline(self):
        manifest = self.manifest()
        changes = [
            (("config", "slopdex", "model"), "different-embedding"),
            (("config", "slopdex", "include"), ["internal/**/*.go"]),
            (("slopdex_version",), "fixture-slopdex-2"),
            (("implementation", "commit"), "e" * 40),
            (("implementation", "binary_sha256"), "f" * 64),
            (("harness_sha256",), "0" * 64),
        ]
        for path, value in changes:
            changed = self.changed(manifest, path, value)
            for trial in manifest["trials"]:
                with self.subTest(control=path, arm=trial["arm"]):
                    if trial["arm"] == "off":
                        self.assertEqual(self.fingerprint(manifest, trial), self.fingerprint(changed, trial))
                    else:
                        self.assertNotEqual(self.fingerprint(manifest, trial), self.fingerprint(changed, trial))

    def test_execution_and_task_settings_invalidate_each_arm(self):
        manifest = self.manifest()
        changes = [
            (("commit",), "e" * 40),
            (("tasks", 0, "prompt"), "Changed task question"),
            (("tasks", 0, "targets", 0, "symbol"), "differentSymbol"),
            (("config", "timeout_seconds"), 31),
            (("config", "max_steps"), 11),
            (("opencode_version",), "fixture-opencode-2"),
        ]
        for path, value in changes:
            changed = self.changed(manifest, path, value)
            for trial in manifest["trials"]:
                with self.subTest(control=path, arm=trial["arm"]):
                    self.assertNotEqual(self.fingerprint(manifest, trial), self.fingerprint(changed, trial))

    def test_model_options_variant_repeat_and_actual_prompt_invalidate_each_arm(self):
        manifest = self.manifest()
        changes = [
            (("model_config", "model"), "fixture/other-model"),
            (("model_config", "variant"), "high"),
            (("model_config", "options", "reasoningEffort"), "high"),
            (("model_config", "options", "temperature"), .7),
            (("repeat",), 2),
        ]
        for trial in manifest["trials"]:
            original = self.fingerprint(manifest, trial)
            for path, value in changes:
                with self.subTest(control=path, arm=trial["arm"]):
                    self.assertNotEqual(original, self.fingerprint(manifest, self.changed(trial, path, value)))
            for kwargs in ({"prompt": "A different final prompt"}, {"instructions": "Different agent guidance"}):
                with self.subTest(control=kwargs, arm=trial["arm"]):
                    self.assertNotEqual(original, self.fingerprint(manifest, trial, **kwargs))

    def test_effective_opencode_controls_invalidate_each_arm(self):
        manifest = self.manifest()
        for trial in manifest["trials"]:
            with self.subTest(arm=trial["arm"]):
                original = self.fingerprint(manifest, trial)
                effective = runner.opencode_config(trial["model_config"], trial["arm"], 10)
                effective["permission"]["webfetch"] = "allow"
                with patch.object(runner, "opencode_config", return_value=effective):
                    self.assertNotEqual(original, self.fingerprint(manifest, trial))

    def test_index_preparation_timeout_and_json_key_order_are_not_trial_controls(self):
        manifest = self.manifest()
        changed = self.changed(manifest, ("config", "index_timeout_seconds"), 999)
        for trial in manifest["trials"]:
            reordered = copy.deepcopy(trial)
            reordered["model_config"]["options"] = dict(reversed(list(trial["model_config"]["options"].items())))
            with self.subTest(arm=trial["arm"]):
                self.assertEqual(self.fingerprint(manifest, trial), self.fingerprint(changed, reordered))

    def test_protocol_changes_invalidate_each_arm(self):
        manifest = self.manifest()
        for trial in manifest["trials"]:
            with self.subTest(arm=trial["arm"]):
                original = self.fingerprint(manifest, trial)
                with patch.object(saved, "PROTOCOL_VERSION", saved.PROTOCOL_VERSION + 1):
                    self.assertNotEqual(original, self.fingerprint(manifest, trial))


class DurableRecordTests(SavedFixture):
    def test_save_load_and_restore_survive_source_job_removal_offline(self):
        manifest = self.manifest(repeats=2)
        manifest.update({key: {"host_path": str(self.root / "private-cache")}
                         for key in ("index", "map_index", "map_symbol_index", "reused_trials")})
        output = self.job("historical-job", manifest)
        paths = {arm: saved.save_bundle(output, self.results, arm) for arm in ("off", "map")}
        self.assertEqual(paths["map"], self.results / "latest-map.json")
        originals = saved.load_records(self.results)
        self.assertEqual(len(originals), 4)
        for arm, path in paths.items():
            bundle = runner.read_json(path)
            self.assertEqual(bundle["schema_version"], 1)
            self.assertEqual(bundle["arm"], arm)
            self.assertEqual(bundle["source_job"], output.name)
            self.assertEqual(bundle["manifest"]["config"], manifest["config"])
            self.assertEqual(bundle["manifest"]["tasks"], manifest["tasks"])
            self.assertEqual(bundle["manifest"]["trials"], [trial for trial in manifest["trials"] if trial["arm"] == arm])
            for key in ("index", "map_index", "map_symbol_index", "reused_trials"):
                self.assertNotIn(key, bundle["manifest"])
            self.assertNotIn(str(self.root), path.read_text())
            if arm == "off":
                self.assertEqual(path.name, runner.digest(sorted(record["fingerprint"] for record in bundle["records"])) + ".json")
        shutil.rmtree(output)
        with patch.object(runner, "command", side_effect=AssertionError("external command")), \
                patch.object(runner, "run_trial", side_effect=AssertionError("model call")), \
                patch.object(runner, "prepare_map_index", side_effect=AssertionError("index preparation")):
            loaded = saved.load_records(self.results)
            self.assertEqual(loaded, originals)
            for (arm, fingerprint), (record, path) in loaded.items():
                self.assertEqual(record["fingerprint"], fingerprint)
                self.assertEqual(path, paths[arm])
                directory = self.root / "restored" / record["trial"]["id"]
                saved.restore_record(record, directory)
                self.assertEqual(runner.read_json(directory / "result.json"), record["result"])
                for filename, key in (("prompt.txt", "prompt"), ("AGENTS.md", "instructions"), ("answer.txt", "answer")):
                    self.assertEqual((directory / filename).read_text(), record[key])
                self.assertEqual(saved.make_record(manifest, record["trial"], directory), record)

    def test_baselines_are_immutable_even_when_imported_again_from_another_job(self):
        manifest = self.manifest(arms=("off",))
        first = self.job("first-baseline", manifest)
        path = saved.save_bundle(first, self.results, "off")
        original, modified = path.read_bytes(), path.stat().st_mtime_ns
        second = self.job("second-baseline", manifest)
        directory = second / "trials" / manifest["trials"][0]["id"]
        result = runner.read_json(directory / "result.json")
        result["wall_seconds"] = 123
        runner.write_json(directory / "result.json", result)
        (directory / "answer.txt").write_text("Another answer for identical controls")
        self.assertEqual(saved.save_bundle(second, self.results, "off"), path)
        self.assertEqual(path.read_bytes(), original)
        self.assertEqual(path.stat().st_mtime_ns, modified)
        self.assertEqual(runner.read_json(path)["source_job"], "first-baseline")

    def test_different_baseline_controls_coexist_without_id_collision(self):
        first_manifest = self.manifest(arms=("off",))
        second_config = self.changed(self.config, ("models", 0, "options", "temperature"), .7)
        second_manifest = self.manifest(config=second_config, arms=("off",))
        self.assertEqual(first_manifest["trials"][0]["id"], second_manifest["trials"][0]["id"])
        first = saved.save_bundle(self.job("first", first_manifest), self.results, "off")
        original = first.read_bytes()
        second = saved.save_bundle(self.job("second", second_manifest), self.results, "off")
        self.assertNotEqual(first, second)
        self.assertEqual(first.read_bytes(), original)
        self.assertEqual(len(saved.load_records(self.results)), 2)

    def test_baseline_subset_import_does_not_duplicate_existing_dataset(self):
        tasks = [self.task, {**self.task, "id": "second-task", "prompt": "Find another flow."}]
        full = self.manifest(tasks=tasks, arms=("off",), repeats=2)
        path = saved.save_bundle(self.job("full-baseline", full), self.results, "off")
        original, modified = path.read_bytes(), path.stat().st_mtime_ns
        subset = self.manifest(arms=("off",))
        subset_path = saved.save_bundle(self.job("selected-subset", subset), self.results, "off")
        self.assertEqual(subset_path, path)
        self.assertEqual(set((self.results / "baselines").glob("*.json")), {path})
        self.assertEqual(path.read_bytes(), original)
        self.assertEqual(path.stat().st_mtime_ns, modified)
        self.assertEqual(len(saved.load_records(self.results)), 4)

    def test_identical_complete_map_preserves_provenance_but_changed_map_replaces_it(self):
        manifest = self.manifest(arms=("map",))
        path = saved.save_bundle(self.job("first-map", manifest), self.results, "map")
        original, modified = path.read_bytes(), path.stat().st_mtime_ns
        self.assertEqual(saved.save_bundle(self.job("reimport-map", manifest), self.results, "map"), path)
        self.assertEqual(path.read_bytes(), original)
        self.assertEqual(path.stat().st_mtime_ns, modified)
        changed = self.changed(manifest, ("implementation", "commit"), "e" * 40)
        self.assertEqual(saved.save_bundle(self.job("new-map", changed), self.results, "map"), path)
        self.assertNotEqual(path.read_bytes(), original)
        self.assertEqual(runner.read_json(path)["source_job"], "new-map")
        self.assertEqual(len(saved.load_records(self.results)), 1)

    def test_incomplete_run_cannot_replace_latest_map_or_create_baseline(self):
        manifest = self.manifest(repeats=2)
        path = saved.save_bundle(self.job("complete", manifest), self.results, "map")
        original = path.read_bytes()
        for arm in ("off", "map"):
            missing = next(trial["id"] for trial in manifest["trials"] if trial["arm"] == arm)
            output = self.job(f"partial-{arm}", manifest, missing=(missing,))
            with self.subTest(arm=arm):
                self.assertIsNone(saved.save_bundle(output, self.results, arm))
                self.assertEqual(path.read_bytes(), original)
        self.assertFalse((self.results / "baselines").exists())
        no_map = self.job("only-baseline", self.manifest(arms=("off",)))
        self.assertIsNone(saved.save_bundle(no_map, self.results, "map"))
        self.assertEqual(path.read_bytes(), original)

    def test_new_map_result_replaces_previous_run_without_changing_reuse_key(self):
        manifest = self.manifest(arms=("map",))
        path = saved.save_bundle(self.job("first-map", manifest), self.results, "map")
        first_record = runner.read_json(path)["records"][0]
        output = self.job("new-map", manifest)
        directory = output / "trials" / manifest["trials"][0]["id"]
        result = runner.read_json(directory / "result.json")
        result["wall_seconds"] = 123
        runner.write_json(directory / "result.json", result)
        (directory / "answer.txt").write_text("New completed answer")
        self.assertEqual(saved.save_bundle(output, self.results, "map"), path)
        bundle = runner.read_json(path)
        self.assertEqual(bundle["source_job"], "new-map")
        self.assertEqual(bundle["records"][0]["fingerprint"], first_record["fingerprint"])
        self.assertEqual(bundle["records"][0]["result"], result)
        self.assertEqual(bundle["records"][0]["answer"], "New completed answer")

    def test_missing_required_record_artifacts_do_not_replace_latest_map(self):
        manifest = self.manifest(arms=("map",), repeats=2)
        path = saved.save_bundle(self.job("complete-map", manifest), self.results, "map")
        original = path.read_bytes()
        for filename in ("prompt.txt", "AGENTS.md", "answer.txt"):
            with self.subTest(filename=filename):
                output = self.job(filename.replace(".", "-"), manifest)
                (output / "trials" / manifest["trials"][-1]["id"] / filename).unlink()
                with self.assertRaises(FileNotFoundError):
                    saved.save_bundle(output, self.results, "map")
                self.assertEqual(path.read_bytes(), original)

    def test_make_record_rejects_every_result_identity_mismatch(self):
        manifest = self.manifest(arms=("map",))
        output = self.job("mismatches", manifest)
        trial = manifest["trials"][0]
        directory = output / "trials" / trial["id"]
        original = runner.read_json(directory / "result.json")
        for key in ("id", "arm", "model", "variant", "task", "repeat"):
            with self.subTest(key=key):
                runner.write_json(directory / "result.json", {**original, key: "wrong"})
                with self.assertRaisesRegex(ValueError, "does not match its trial"):
                    saved.make_record(manifest, trial, directory)

    def test_make_record_backfills_command_usage_from_legacy_logs(self):
        manifest = self.manifest(arms=("map",))
        output = self.job("legacy", manifest)
        trial = manifest["trials"][0]
        directory = output / "trials" / trial["id"]
        result = runner.read_json(directory / "result.json")
        result.pop("slopdex_commands")
        result["slopdex_calls"] = 2
        runner.write_json(directory / "result.json", result)
        (directory / "slopdex-calls.jsonl").write_text('\n'.join(json.dumps({"argv": argv}) for argv in (["map"], ["--help"])))
        record = saved.make_record(manifest, trial, directory)
        self.assertEqual(record["result"]["slopdex_commands"], {"help": 1, "map": 1})
        (directory / "slopdex-calls.jsonl").unlink()
        self.assertEqual(saved.make_record(manifest, trial, directory)["result"]["slopdex_commands"], {"unknown": 2})

    def test_make_record_accepts_matching_opencode_config_without_persisting_workspace_path(self):
        manifest = self.manifest(arms=("map",))
        output = self.job("opencode-config", manifest)
        trial = manifest["trials"][0]
        directory = output / "trials" / trial["id"]
        original = saved.make_record(manifest, trial, directory)
        actual = runner.opencode_config(trial["model_config"], trial["arm"], manifest["config"]["max_steps"])
        actual["instructions"] = [str(self.root / "disposable-workspace" / "AGENTS.md")]
        runner.write_json(directory / "opencode.json", actual)
        self.assertEqual(saved.make_record(manifest, trial, directory), original)
        self.assertEqual(runner.read_json(directory / "opencode.json"), actual)
        path = saved.save_bundle(output, self.results, "map")
        self.assertNotIn(str(self.root), path.read_text())

    def test_mismatched_job_opencode_controls_cannot_replace_latest_map(self):
        manifest = self.manifest(arms=("map",))
        path = saved.save_bundle(self.job("complete", manifest), self.results, "map")
        original = path.read_bytes()
        trial = manifest["trials"][0]
        actual = runner.opencode_config(trial["model_config"], trial["arm"], manifest["config"]["max_steps"])
        changes = [(("agent", "build", "steps"), 999), (("permission", "edit"), "allow"),
                   (("model",), "fixture/other-model"), (("plugin",), ["uncontrolled-plugin"])]
        for keys, value in changes:
            with self.subTest(control=keys):
                output = self.job("invalid-opencode-" + "-".join(keys), manifest)
                runner.write_json(output / "trials" / trial["id"] / "opencode.json", self.changed(actual, keys, value))
                with self.assertRaisesRegex(ValueError, "OpenCode config differs"):
                    saved.save_bundle(output, self.results, "map")
                self.assertEqual(path.read_bytes(), original)

    def test_recursive_config_credentials_cannot_be_saved(self):
        manifest = self.manifest()
        output = self.job("clean", manifest)
        paths = [saved.save_bundle(output, self.results, arm) for arm in ("off", "map")]
        originals = {path: path.read_bytes() for path in paths}
        credentials = [
            {"provider": {"options": {"apiKey": "fixture-credential"}}},
            {"providers": [{"api_key": "fixture-credential"}]},
            {"Authorization": "Bearer fixture-credential"},
            {"nested": {"password": "fixture-credential"}},
            {"nested": [{"clientSecret": "fixture-credential"}]},
            {"nested": [{"accessToken": "fixture-credential"}]},
        ]
        for number, value in enumerate(credentials):
            sensitive = copy.deepcopy(manifest)
            sensitive["config"]["extra_settings"] = value
            output = self.job(f"credential-{number}", sensitive)
            for arm in ("off", "map"):
                with self.subTest(credentials=value, arm=arm):
                    with self.assertRaisesRegex(ValueError, "contains credentials"):
                        saved.save_bundle(output, self.results, arm)
            for path, original in originals.items():
                self.assertEqual(path.read_bytes(), original)
        self.assertEqual(self.config, manifest["config"])
        self.assertEqual(set((self.results / "baselines").glob("*.json")), {paths[0]})

    def test_empty_credential_placeholders_and_token_limits_are_not_secrets(self):
        saved.reject_credentials({"apiKey": None, "nested": [{"secret": "", "password": False}],
                                  "options": {"max_tokens": 100, "reasoningEffort": "high"}})

    def test_imported_trial_model_credentials_cannot_bypass_manifest_config_guard(self):
        # JSON imports have separate model-config snapshots, unlike make_trials'
        # in-memory references to config.models. Check the settings actually saved.
        manifest = json.loads(json.dumps(self.manifest(arms=("off",))))
        manifest["trials"][0]["model_config"]["options"]["apiKey"] = "fixture-credential"
        self.assertNotIn("apiKey", manifest["config"]["models"][0]["options"])
        output = self.job("credential-in-trial", manifest)
        with self.assertRaisesRegex(ValueError, "contains credentials"):
            saved.save_bundle(output, self.results, "off")
        self.assertFalse((self.results / "baselines").exists())

    def test_load_empty_store_does_not_create_files(self):
        self.assertEqual(saved.load_records(self.results), {})
        self.assertFalse(self.results.exists())

    def test_old_records_remain_readable_but_cannot_match_upgraded_protocol(self):
        manifest = self.manifest()
        output = self.job("old-protocol", manifest)
        for arm in ("off", "map"):
            saved.save_bundle(output, self.results, arm)
        original = saved.load_records(self.results)
        with patch.object(saved, "PROTOCOL_VERSION", saved.PROTOCOL_VERSION + 1):
            loaded = saved.load_records(self.results)
            self.assertEqual(loaded, original)
            for record, _ in loaded.values():
                current = self.fingerprint(manifest, record["trial"])
                self.assertNotEqual(current, record["fingerprint"])
                self.assertNotIn((record["trial"]["arm"], current), loaded)
            with self.assertRaisesRegex(ValueError, "No matching saved baseline"):
                saved.compare_saved(self.results)

    def test_old_records_remain_readable_but_cannot_match_changed_opencode_controls(self):
        manifest = self.manifest()
        output = self.job("old-opencode-controls", manifest)
        for arm in ("off", "map"):
            saved.save_bundle(output, self.results, arm)
        original = saved.load_records(self.results)
        actual_config = runner.opencode_config

        def changed_config(model, arm, steps):
            result = actual_config(model, arm, steps)
            result["permission"]["webfetch"] = "allow"
            return result

        with patch.object(runner, "opencode_config", side_effect=changed_config):
            loaded = saved.load_records(self.results)
            self.assertEqual(loaded, original)
            for record, _ in loaded.values():
                current = self.fingerprint(manifest, record["trial"])
                self.assertNotEqual(current, record["fingerprint"])
                self.assertNotIn((record["trial"]["arm"], current), loaded)
            with self.assertRaisesRegex(ValueError, "No matching saved baseline"):
                saved.compare_saved(self.results)

    def test_load_rejects_unsupported_schema_arms_and_corrupt_controls(self):
        path = saved.save_bundle(self.job("map", self.manifest(arms=("map",))), self.results, "map")
        original = runner.read_json(path)
        changes = [
            (("schema_version",), 99, "Unsupported saved evaluation"),
            (("arm",), "search", "Unsupported saved evaluation"),
            (("records", 0, "trial", "arm"), "off", "mismatched arm"),
            (("records", 0, "fingerprint"), "not-a-fingerprint", "controls are corrupt"),
            (("records", 0, "prompt"), "different prompt", "controls are corrupt"),
            (("records", 0, "instructions"), "different instructions", "controls are corrupt"),
            (("records", 0, "controls", "repeat"), 99, "controls are corrupt"),
            (("records", 0, "controls", "protocol_version"), 99, "controls are corrupt"),
            (("records", 0, "controls", "opencode_config", "permission", "edit"), "allow", "controls are corrupt"),
            (("manifest", "config", "timeout_seconds"), 99, "controls are corrupt"),
            (("manifest", "implementation", "commit"), "e" * 40, "controls are corrupt"),
        ]
        changes += [(("records", 0, "result", key), "wrong", "result is mismatched")
                    for key in ("id", "arm", "model", "variant", "task", "repeat")]
        for keys, value, message in changes:
            with self.subTest(control=keys):
                runner.write_json(path, self.changed(original, keys, value))
                with self.assertRaisesRegex(ValueError, message):
                    saved.load_records(self.results)


class SavedComparisonTests(SavedFixture):
    def test_comparison_uses_matching_baseline_offline_after_slopdex_changes(self):
        baseline = saved.save_bundle(self.job("baseline", self.manifest(arms=("off",))), self.results, "off")
        original = baseline.read_bytes()
        manifest = self.manifest(arms=("map",))
        manifest["implementation"]["commit"] = "e" * 40
        manifest["harness_sha256"] = "f" * 64
        manifest["slopdex_version"] = "fixture-slopdex-2"
        manifest["config"]["slopdex"]["model"] = "new-embedding-model"
        saved.save_bundle(self.job("treatment", manifest), self.results, "map")
        with patch.object(runner, "command", side_effect=AssertionError("external command")), \
                patch.object(runner, "run_trial", side_effect=AssertionError("model call")), \
                patch.object(runner, "prepare_map_index", side_effect=AssertionError("index preparation")):
            summary = saved.compare_saved(self.results)
        self.assertEqual((summary["completed"], summary["planned"]), (2, 2))
        self.assertEqual(len(summary["paired"]), 1)
        self.assertEqual(summary["paired"][0]["f1_delta"], .5)
        self.assertEqual(summary["slopdex_commands"], {"map": 1})
        self.assertEqual(runner.read_json(self.results / "summary.json"), summary)
        text = (self.results / "report.md").read_text()
        self.assertIn("not interleaved", text)
        self.assertIn(str(baseline.relative_to(self.results)), text)
        self.assertEqual(baseline.read_bytes(), original)

    def test_comparison_matches_individual_trials_from_multiple_baseline_jobs(self):
        second_config = copy.deepcopy(self.config)
        second_config["models"][0]["model"] = "fixture/other-model"
        paths = [saved.save_bundle(self.job("baseline-1", self.manifest(arms=("off",))), self.results, "off"),
                 saved.save_bundle(self.job("baseline-2", self.manifest(config=second_config, arms=("off",))), self.results, "off")]
        combined = copy.deepcopy(self.config)
        combined["models"] += second_config["models"]
        saved.save_bundle(self.job("map", self.manifest(config=combined, arms=("map",))), self.results, "map")
        summary = saved.compare_saved(self.results)
        self.assertEqual((summary["completed"], summary["planned"]), (4, 4))
        self.assertEqual(len(summary["paired"]), 2)
        text = (self.results / "report.md").read_text()
        for path in paths:
            self.assertIn(str(path.relative_to(self.results)), text)

    def test_comparison_rejects_any_mismatch_in_baseline_settings(self):
        baseline_manifest = self.manifest(arms=("off",))
        saved.save_bundle(self.job("baseline", baseline_manifest), self.results, "off")
        map_manifest = self.manifest(arms=("map",))
        changes = [
            (("commit",), "e" * 40),
            (("tasks", 0, "prompt"), "Different task prompt"),
            (("tasks", 0, "targets", 0, "symbol"), "differentSymbol"),
            (("config", "timeout_seconds"), 31),
            (("config", "max_steps"), 11),
            (("opencode_version",), "fixture-opencode-2"),
            (("config", "models", 0, "model"), "fixture/other-model"),
            (("config", "models", 0, "variant"), "high"),
            (("config", "models", 0, "options", "reasoningEffort"), "high"),
            (("config", "models", 0, "options", "temperature"), .7),
        ]
        for keys, value in changes:
            with self.subTest(control=keys):
                changed = self.changed(map_manifest, keys, value)
                changed["trials"] = runner.make_trials(changed["config"], changed["tasks"], ["map"], 1, 0)
                saved.save_bundle(self.job("different-" + "-".join(map(str, keys)), changed), self.results, "map")
                with self.assertRaisesRegex(ValueError, "No matching saved baseline"):
                    saved.compare_saved(self.results)
                self.assertFalse((self.results / "summary.json").exists())
                self.assertFalse((self.results / "report.md").exists())

    def test_comparison_rejects_changed_baseline_guidance_or_effective_opencode_controls(self):
        saved.save_bundle(self.job("baseline", self.manifest(arms=("off",))), self.results, "off")
        saved.save_bundle(self.job("map", self.manifest(arms=("map",))), self.results, "map")
        with patch.object(runner, "BASE_INSTRUCTIONS", runner.BASE_INSTRUCTIONS + "New restriction\n"):
            with self.assertRaisesRegex(ValueError, "No matching saved baseline"):
                saved.compare_saved(self.results)
        with patch.object(runner, "ANSWER_FORMAT", runner.ANSWER_FORMAT + "New answer contract\n"):
            with self.assertRaisesRegex(ValueError, "No matching saved baseline"):
                saved.compare_saved(self.results)
        actual_config = runner.opencode_config

        def changed_config(model, arm, steps):
            result = actual_config(model, arm, steps)
            if arm == "off":
                result["permission"]["webfetch"] = "allow"
            return result

        # Imported baselines keep their original effective controls. A current
        # control change must not silently match the historical baseline.
        with patch.object(runner, "opencode_config", side_effect=changed_config):
            with self.assertRaisesRegex(ValueError, "No matching saved baseline"):
                saved.compare_saved(self.results)

    def test_comparison_requires_every_requested_repeat(self):
        saved.save_bundle(self.job("baseline", self.manifest(arms=("off",))), self.results, "off")
        saved.save_bundle(self.job("map", self.manifest(arms=("map",), repeats=2)), self.results, "map")
        with self.assertRaisesRegex(ValueError, "No matching saved baseline"):
            saved.compare_saved(self.results)

    def test_comparison_without_latest_map_is_actionable(self):
        saved.save_bundle(self.job("baseline", self.manifest(arms=("off",))), self.results, "off")
        with self.assertRaisesRegex(ValueError, "No saved map evaluation"):
            saved.compare_saved(self.results)


@unittest.skipUnless(os.name == "posix", "CLI job locking requires fcntl")
class SavedCliTests(SavedFixture):
    def current_manifest(self, **kwargs):
        manifest = self.manifest(**kwargs)
        manifest["harness_sha256"] = runner.file_digest(Path(runner.__file__))
        return manifest

    def save_history(self, manifest=None):
        manifest = manifest if manifest is not None else self.current_manifest()
        output = self.job("history", manifest)
        for arm in ("off", "map"):
            saved.save_bundle(output, self.results, arm)
        return manifest

    def run_cli(self, name, *, config=None, tasks=None, implementation=None, extra=()):
        config = copy.deepcopy(config if config is not None else self.config)
        tasks = copy.deepcopy(tasks if tasks is not None else [self.task])
        implementation = implementation if implementation is not None else self.manifest()["implementation"]
        output = self.root / name

        def fake_run(trial, task, gold, config, commit, directory, opencode, slopdex, index, *, instruction):
            return self.write_trial(directory, trial, task, instruction=instruction)

        def version(argv):
            self.assertEqual(argv[1:], ["--version"])
            return "fixture-opencode-1" if argv[0] == "fixture-opencode" else "fixture-slopdex-1"

        with ExitStack() as stack:
            stack.enter_context(patch.object(runner, "load_inputs", return_value=(config, {"repository_commit": "a" * 40, "tasks": tasks})))
            stack.enter_context(patch.object(runner, "executable", side_effect=lambda name: "fixture-" + name))
            stack.enter_context(patch.object(runner, "command", side_effect=version))
            stack.enter_context(patch.object(runner, "implementation_identity", return_value=implementation))
            calls = {name: stack.enter_context(patch.object(runner, name, **options)) for name, options in {
                "ensure_source": {}, "target_lines": {"return_value": {}},
                "prepare_map_index": {"return_value": (self.root / "symbols", {"wall_seconds": 1, "native_ann": {"wall_seconds": 2}})},
                "run_trial": {"side_effect": fake_run},
            }.items()}
            calls["load_records"] = stack.enter_context(patch.object(saved, "load_records", wraps=saved.load_records))
            calls["restore_record"] = stack.enter_context(patch.object(saved, "restore_record", wraps=saved.restore_record))
            stack.enter_context(patch("sys.stdout", new=io.StringIO()))
            runner.main(["--cache", str(self.root / "cache"), "--results", str(self.results),
                         "run", "--output", str(output), *extra])
        return output, calls

    def test_save_and_report_cli_use_durable_records_without_inputs_or_external_tools(self):
        output = self.job("historical")
        with patch.object(runner, "load_inputs", side_effect=AssertionError("current input config")), \
                patch.object(runner, "executable", side_effect=AssertionError("external executable")), \
                patch.object(runner, "command", side_effect=AssertionError("external command")), \
                patch("sys.stdout", new=io.StringIO()):
            runner.main(["--results", str(self.results), "save", str(output)])
            self.assertEqual(len(saved.load_records(self.results)), 2)
            shutil.rmtree(output)
            runner.main(["--results", str(self.results), "report"])
        self.assertEqual(runner.read_json(self.results / "summary.json")["completed"], 2)
        self.assertIn("not interleaved", (self.results / "report.md").read_text())

    def test_explicit_job_report_does_not_require_or_modify_durable_results(self):
        output = self.job("historical")
        with patch.object(saved, "compare_saved", side_effect=AssertionError("durable comparison")), \
                patch.object(runner, "load_inputs", side_effect=AssertionError("current input config")), \
                patch("sys.stdout", new=io.StringIO()):
            runner.main(["--results", str(self.results), "report", str(output)])
        self.assertEqual(runner.read_json(output / "summary.json")["completed"], 2)
        self.assertFalse(self.results.exists())

    def test_default_run_restores_every_matching_trial_without_source_or_index_preparation(self):
        historical = self.save_history()
        original = saved.load_records(self.results)
        output, calls = self.run_cli("reused", extra=("--seed", "91"))
        calls["ensure_source"].assert_not_called()
        calls["target_lines"].assert_not_called()
        calls["prepare_map_index"].assert_not_called()
        calls["run_trial"].assert_not_called()
        self.assertEqual(calls["restore_record"].call_count, 2)
        manifest = runner.read_json(output / "manifest.json")
        self.assertEqual({trial["arm"] for trial in manifest["trials"]}, {"off", "map"})
        self.assertEqual(set(manifest["reused_trials"]), {trial["id"] for trial in historical["trials"]})
        for record, _ in original.values():
            directory = output / "trials" / record["trial"]["id"]
            self.assertEqual(saved.make_record(manifest, record["trial"], directory), record)

    def test_only_missing_map_trial_executes_and_prepares_an_index(self):
        tasks = [self.task, {**self.task, "id": "second-task", "prompt": "Find another flow."}]
        baseline_manifest = self.current_manifest(tasks=tasks, arms=("off",))
        baseline = saved.save_bundle(self.job("baseline", baseline_manifest), self.results, "off")
        original_baseline = baseline.read_bytes()
        map_manifest = self.current_manifest(tasks=tasks, arms=("map",))
        map_manifest["trials"] = [trial for trial in map_manifest["trials"] if trial["task"] == "fixture"]
        saved.save_bundle(self.job("partial-history", map_manifest), self.results, "map")
        output, calls = self.run_cli("pending-map", tasks=tasks)
        self.assertEqual(calls["restore_record"].call_count, 3)
        calls["ensure_source"].assert_called_once_with("a" * 40)
        calls["prepare_map_index"].assert_called_once()
        calls["run_trial"].assert_called_once()
        trial = calls["run_trial"].call_args.args[0]
        self.assertEqual((trial["task"], trial["arm"]), ("second-task", "map"))
        self.assertEqual(baseline.read_bytes(), original_baseline)
        self.assertEqual(runner.read_json(output / "summary.json")["completed"], 4)

    def test_changed_implementation_reuses_baseline_and_runs_only_map(self):
        self.save_history()
        changed = {**self.manifest()["implementation"], "binary_sha256": "e" * 64}
        _, calls = self.run_cli("changed-build", implementation=changed)
        self.assertEqual(calls["restore_record"].call_count, 1)
        calls["prepare_map_index"].assert_called_once()
        self.assertEqual(calls["prepare_map_index"].call_args.kwargs["implementation"], changed)
        calls["run_trial"].assert_called_once()
        self.assertEqual(calls["run_trial"].call_args.args[0]["arm"], "map")

    def test_changed_harness_reuses_baseline_and_runs_only_map(self):
        manifest = self.current_manifest()
        manifest["harness_sha256"] = "e" * 64
        self.save_history(manifest)
        _, calls = self.run_cli("changed-harness")
        self.assertEqual(calls["restore_record"].call_count, 1)
        calls["prepare_map_index"].assert_called_once()
        calls["run_trial"].assert_called_once()
        self.assertEqual(calls["run_trial"].call_args.args[0]["arm"], "map")

    def test_changed_model_options_rerun_only_that_models_two_arms(self):
        config = copy.deepcopy(self.config)
        config["models"].append({"model": "fixture/other-model", "variant": "medium", "options": {}})
        self.save_history(self.current_manifest(config=config))
        changed = self.changed(config, ("models", 0, "options", "reasoningEffort"), "high")
        _, calls = self.run_cli("changed-model", config=changed)
        self.assertEqual(calls["restore_record"].call_count, 2)
        calls["prepare_map_index"].assert_called_once()
        self.assertEqual(calls["run_trial"].call_count, 2)
        for call in calls["run_trial"].call_args_list:
            self.assertEqual(call.args[0]["model"], "fixture/model")
            self.assertEqual(call.args[0]["model_config"]["options"]["reasoningEffort"], "high")

    def test_imported_unknown_implementation_cannot_match_current_known_map_build(self):
        for missing in (False, True):
            with self.subTest(missing_implementation=missing):
                self.results = self.root / ("results-missing-identity" if missing else "results-null-identity")
                manifest = self.current_manifest()
                if missing:
                    manifest.pop("implementation")
                else:
                    manifest["implementation"] = None
                self.save_history(manifest)
                bundle = runner.read_json(self.results / "latest-map.json")
                self.assertIsNone(bundle["manifest"]["implementation"])
                self.assertIsNone(bundle["records"][0]["controls"]["implementation"])
                self.assertIn("not eligible", bundle["manifest"]["provenance_note"])
                _, calls = self.run_cli("known-build-" + str(missing))
                self.assertEqual(calls["restore_record"].call_count, 1)
                calls["prepare_map_index"].assert_called_once()
                calls["run_trial"].assert_called_once()
                self.assertEqual(calls["run_trial"].call_args.args[0]["arm"], "map")

    def test_default_resume_does_not_execute_completed_trials_again(self):
        output, first = self.run_cli("resumed")
        self.assertEqual(first["run_trial"].call_count, 2)
        same_output, second = self.run_cli("resumed")
        self.assertEqual(same_output, output)
        for name in ("ensure_source", "target_lines", "prepare_map_index", "run_trial", "restore_record"):
            second[name].assert_not_called()


if __name__ == "__main__":
    unittest.main()
