"""Offline tests for the FrontierHarness harness: no Docker, network, or model calls."""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import fh_shims  # noqa: E402
import run  # noqa: E402

CONFIG = run.read_json(run.HERE / "config.json")
NODE = shutil.which("node")

TASK_TOML = """version = "1.1"

[verifier]
timeout_sec = 900.0

[agent]
timeout_sec = 900.0

[environment]
docker_image = "example:1"
allow_internet = false

[environment.env]
"""


def write_trial(jobs: Path, result: dict, *, calls=(), faq=()) -> Path:
    trial = jobs / "trial" / "task__abc"
    (trial / "agent").mkdir(parents=True)
    (trial / "result.json").write_text(json.dumps(result))
    (trial / "agent" / "slopdex-calls.jsonl").write_text("".join(json.dumps(c) + "\n" for c in calls))
    (trial / "agent" / "faq-usage.jsonl").write_text("".join(json.dumps(c) + "\n" for c in faq))
    return trial


def result(reward=1.0, exception=None, executed=True, cost=0.5):
    return {
        "verifier_result": None if reward is None else {"rewards": {"reward": reward}},
        "exception_info": exception and {"exception_type": exception},
        "agent_execution": {"started_at": "2026-11-01T00:00:00Z", "finished_at": "2026-11-01T00:02:30Z"} if executed else None,
        "agent_setup": {"started_at": "2026-11-01T00:00:00Z", "finished_at": "2026-11-01T00:00:10Z"},
        "agent_result": {"n_input_tokens": 1000, "n_cache_tokens": 800, "n_output_tokens": 50, "cost_usd": cost},
        "agent_info": {"version": "1.18.34"},
    }


class OverlayTest(unittest.TestCase):
    def test_closes_agent_network_and_keeps_baseline_public(self):
        parsed = tomllib.loads(run.agent_network_overlay(TASK_TOML))
        self.assertEqual(parsed["agent"], {"network_mode": "no-network", "timeout_sec": 900.0})
        self.assertNotIn("allow_internet", parsed["environment"])
        self.assertEqual(parsed["verifier"], {"timeout_sec": 900.0})

    def test_is_idempotent_and_adds_missing_agent_section(self):
        once = run.agent_network_overlay(TASK_TOML)
        self.assertEqual(run.agent_network_overlay(once), once)
        without_agent = run.agent_network_overlay('[environment]\ndocker_image = "x"\n')
        self.assertEqual(tomllib.loads(without_agent)["agent"], {"network_mode": "no-network"})


class InstructionTest(unittest.TestCase):
    def test_map_arm_uses_the_excalidraw_readme_snippet(self):
        text = run.map_instructions()
        self.assertEqual(text, run.EXCALIDRAW.arm_instructions("map"))
        self.assertIn("## Code navigation", text)
        self.assertIn("`slopdex map ", text)


class CommandTest(unittest.TestCase):
    def setUp(self):
        patcher = mock.patch.dict(os.environ, {"OPENCODE_GO_EVALS_KEY": "secret-value"})
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_off_arm_passes_key_as_template_and_no_slopdex(self):
        argv = run.harbor_command(CONFIG, "off", Path("/tasks/t"), Path("/jobs"), None)
        self.assertNotIn("secret-value", " ".join(argv))
        self.assertIn("OPENCODE_API_KEY=${OPENCODE_GO_EVALS_KEY}", argv)
        self.assertNotIn("OPENAI_API_KEY=${OPENAI_API_KEY}", argv)
        self.assertFalse(any(a.startswith("slopdex_") for a in argv))
        hosts = [argv[i + 1] for i, a in enumerate(argv) if a == "--allow-agent-host"]
        self.assertEqual(hosts, CONFIG["agent_hosts"])

    def test_map_arm_passes_binary_settings_and_instructions(self):
        argv = run.harbor_command(CONFIG, "map", Path("/tasks/t"), Path("/jobs"), Path("/bin/slopdex-x"))
        kwargs = dict(argv[i + 1].split("=", 1) for i, a in enumerate(argv) if a == "--ak")
        self.assertEqual(kwargs["slopdex_binary"], "/bin/slopdex-x")
        self.assertEqual(json.loads(kwargs["instructions"]), run.map_instructions())
        self.assertEqual(json.loads(kwargs["slopdex_settings"]), CONFIG["slopdex"])
        self.assertEqual(json.loads(kwargs["opencode_config"])["provider"]["opencode-go"]["models"]
                         ["deepseek-v4.1-flash"]["variants"]["medium"], {"reasoningEffort": "medium"})

    def test_hosts_overlay_pins_addresses_on_the_sidecar(self):
        overlay = run.hosts_overlay({"opencode.ai": ["1.2.3.4", "5.6.7.8"]})
        self.assertEqual(overlay, 'services:\n  harbor-docker-egress-control-sidecar:\n    extra_hosts:\n'
                                  '      - "opencode.ai:1.2.3.4"\n      - "opencode.ai:5.6.7.8"\n')
        argv = run.harbor_command(CONFIG, "off", Path("/t"), Path("/j"), None, Path("/j/hosts.yaml"))
        self.assertEqual(argv[argv.index("--extra-docker-compose") + 1], "/j/hosts.yaml")

    def test_trial_order_is_seeded_and_alternates_arms(self):
        trials = run.make_trials(["a", "b", "c"], ("off", "map"), 7)
        self.assertEqual(trials, run.make_trials(["a", "b", "c"], ("off", "map"), 7))
        self.assertEqual([arm for _, arm in trials], ["off", "map", "map", "off", "off", "map"])


class ParseTrialTest(unittest.TestCase):
    def setUp(self):
        self.directory = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.directory)

    def parse(self, value, **extra):
        jobs = self.directory / str(len(list(self.directory.iterdir())))
        write_trial(jobs, value, **extra)
        return run.parse_trial(jobs)

    def test_statuses(self):
        self.assertEqual(self.parse(result(1.0))["status"], "success")
        self.assertEqual(self.parse(result(0.0))["status"], "failure")
        self.assertEqual(self.parse(result(0.0, "AgentTimeoutError"))["status"], "failure")
        self.assertEqual(self.parse(result(None, "AgentSetupTimeoutError", executed=False))["status"], "infra_invalid")
        self.assertEqual(self.parse(result(None))["status"], "infra_invalid")
        self.assertEqual(self.parse(result(1.0, "DockerError"))["status"], "infra_invalid")
        unreachable = result(0.0, "NonZeroAgentExitCodeError", cost=None)
        unreachable["agent_result"] = {"n_input_tokens": None, "cost_usd": None}
        self.assertEqual(self.parse(unreachable)["status"], "infra_invalid")

    def test_missing_trial_is_infra(self):
        self.assertEqual(run.parse_trial(self.directory / "missing")["status"], "infra_invalid")

    def test_faq_cost_and_commands(self):
        record = self.parse(result(1.0, cost=0.5),
                            calls=[{"event": "start", "argv": ["map", "-q", "x", "src"]},
                                   {"event": "finish", "exit_code": 0},
                                   {"event": "start", "argv": ["faq", "how?"]}],
                            faq=[{"tokens": {"input": 10}, "cost_usd": 0.25}])
        self.assertEqual(record["cost_usd"], 0.75)
        self.assertEqual(record["agent_cost_usd"], 0.5)
        self.assertEqual(record["agent_seconds"], 150)
        self.assertEqual(record["slopdex_commands"], {"faq": 1, "map": 1})

    def test_faq_without_cost_makes_cost_unknown(self):
        record = self.parse(result(1.0), faq=[{"tokens": {}, "cost_usd": None}])
        self.assertIsNone(record["cost_usd"])


class SummaryTest(unittest.TestCase):
    def test_effective_cost_counts_failures(self):
        records = [
            {"status": "success", "cost_usd": 1.0, "agent_seconds": 10, "input_tokens": 100, "cache_tokens": 50},
            {"status": "failure", "cost_usd": 3.0, "agent_seconds": 30, "input_tokens": 100, "cache_tokens": 50},
            {"status": "infra_invalid", "cost_usd": 9.0},
        ]
        summary = run.arm_summary(records)
        self.assertEqual((summary["valid"], summary["passed"], summary["infra_invalid"]), (2, 1, 1))
        self.assertEqual(summary["pass_rate"], 0.5)
        self.assertEqual(summary["effective_cost_per_pass"], 4.0)
        self.assertEqual(summary["median_cost_per_pass"], 1.0)
        self.assertEqual(summary["cache_hit_rate"], 0.5)

    def test_reuse_requires_matching_controls_and_valid_status(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "r.json"
            control = {"arm": "off"}
            run.write_json(path, {"controls_digest": run.digest(control), "status": "failure"})
            self.assertTrue(run.reusable(path, control))
            self.assertFalse(run.reusable(path, {"arm": "map"}))
            run.write_json(path, {"controls_digest": run.digest(control), "status": "infra_invalid"})
            self.assertFalse(run.reusable(path, control))


@unittest.skipUnless(NODE, "node is required to execute the container shims")
class ShimTest(unittest.TestCase):
    def setUp(self):
        self.directory = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.directory)

    def script(self, name: str, text: str) -> Path:
        path = self.directory / name
        path.write_text(text)
        path.chmod(0o755)
        return path

    def test_wrapper_prefixes_journals_and_blocks_overrides(self):
        real = self.script("real", "#!/bin/sh\nprintf '%s\\n' \"$@\"\n")
        log = self.directory / "calls.jsonl"
        wrapper = self.script("slopdex", fh_shims.render_js(
            fh_shims.WRAPPER_JS, NODE=NODE, BIN=json.dumps(str(real)),
            PREFIX=json.dumps(["--root", "/app"]), LOG=json.dumps(str(log))))
        ok = subprocess.run([wrapper, "map", "-q", "x"], capture_output=True, text=True)
        self.assertEqual((ok.returncode, ok.stdout.split()), (0, ["--root", "/app", "map", "-q", "x"]))
        blocked = subprocess.run([wrapper, "--index=/tmp/other", "status"], capture_output=True, text=True)
        self.assertEqual((blocked.returncode, blocked.stdout), (126, ""))
        events = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertEqual([(e["event"], e.get("allowed"), e.get("exit_code")) for e in events],
                         [("start", True, None), ("finish", None, 0), ("start", False, None), ("finish", None, 126)])

    def test_faq_shim_adds_json_format_and_journals_usage(self):
        events = [
            {"type": "text", "part": {"messageID": "m1", "text": "draft"}},
            {"type": "step_finish", "part": {"cost": 0.01, "tokens": {"input": 5, "output": 2, "cache": {"read": 3}}}},
            {"type": "text", "part": {"messageID": "m2", "text": "final answer"}},
            {"type": "step_finish", "part": {"cost": 0.02, "tokens": {"input": 1, "output": 1}}},
        ]
        argv_log = self.directory / "argv"
        real = self.script("opencode-real", f"#!/bin/sh\nprintf '%s\\n' \"$@\" > {argv_log}\ncat <<'EOF'\n"
                           + "\n".join(json.dumps(e) for e in events) + "\nEOF\n")
        log = self.directory / "faq.jsonl"
        shim = self.script("opencode", fh_shims.render_js(
            fh_shims.FAQ_SHIM_JS, NODE=NODE, OPENCODE=json.dumps(str(real)), LOG=json.dumps(str(log))))
        out = subprocess.run([shim, "run", "--model", "m", "--", "question"], capture_output=True, text=True)
        self.assertEqual((out.returncode, out.stdout), (0, "final answer"))
        self.assertEqual(argv_log.read_text().split(), ["run", "--model", "m", "--format", "json", "--", "question"])
        (usage,) = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertEqual(usage["steps"], 2)
        self.assertAlmostEqual(usage["cost_usd"], 0.03)
        self.assertEqual((usage["tokens"]["input"], usage["tokens"]["cache_read"]), (6, 3))


if __name__ == "__main__":
    unittest.main()
