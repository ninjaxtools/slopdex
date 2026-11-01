#!/usr/bin/env python3
"""FrontierHarness Eval for slopdex: OpenCode baseline versus the README map arm.

Runs the 30 published FrontierHarness v1 tasks (21 Terminal-Bench, 9 DeepSWE)
with local Harbor + Docker, the way maki's FrontierHarness run invoked them,
once without slopdex (`off`) and once with slopdex and the README map snippet
(`map`, the same instruction as the Excalidraw eval). See README.md.
"""

from __future__ import annotations

import argparse
import collections
import concurrent.futures
import hashlib
import importlib.util
import inspect
import json
import os
import random
import re
import shutil
import socket
import statistics
import subprocess
import sys
import tomllib
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
AGENT_FILES = (HERE / "fh_agent.py", HERE / "fh_shims.py")
AGENT_IMPORT = "fh_agent:FrontierOpenCode"
ARMS = ("off", "map")
PROTOCOL_VERSION = 1
# Infrastructure outcomes are retried on the next run instead of being scored.
SCORED_EXCEPTIONS = {"AgentTimeoutError", "NonZeroAgentExitCodeError"}


def load_excalidraw():
    """The Excalidraw harness owns the README snippet and slopdex command parser."""
    spec = importlib.util.spec_from_file_location("excalidraw_run", HERE.parent / "excalidraw" / "run.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


EXCALIDRAW = load_excalidraw()


def read_json(path: Path):
    return json.loads(path.read_text())


def write_json(path: Path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    os.replace(temporary, path)


def digest(value) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def tree_digest(directory: Path) -> str:
    hasher = hashlib.sha256()
    for path in sorted(p for p in directory.rglob("*") if p.is_file()):
        hasher.update(str(path.relative_to(directory)).encode() + b"\0" + path.read_bytes() + b"\0")
    return hasher.hexdigest()


def file_digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(argv, **kwargs):
    return subprocess.run(argv, check=True, text=True, capture_output=True, **kwargs).stdout


def map_instructions() -> str:
    """Exactly the Excalidraw map-arm guidance: the README map snippet, verbatim."""
    return EXCALIDRAW.arm_instructions("map")


# ---------------------------------------------------------------- sources


def checkout(url: str, commit: str, destination: Path) -> Path:
    if not (destination / ".git").exists():
        destination.parent.mkdir(parents=True, exist_ok=True)
        run(["git", "clone", "--quiet", "--filter=blob:none", url, str(destination)])
    if run(["git", "-C", str(destination), "rev-parse", "HEAD"]).strip() != commit:
        run(["git", "-C", str(destination), "fetch", "--quiet", "origin", commit])
        run(["git", "-C", str(destination), "checkout", "--quiet", "--detach", commit])
    return destination


def benchmark_tasks(frontier: Path) -> list[str]:
    return list(read_json(frontier / "benchmark.json")["task_ids"])


def split_task(task_id: str) -> tuple[str, str]:
    suite, name = task_id.split("/", 1)
    return suite, name


def agent_network_overlay(text: str) -> str:
    """Maki's Terminal-Bench overlay: no network for the agent phase only.

    Setup and the verifier keep the published baseline (they install packages);
    `--allow-agent-host` then opens just the model and embedding hosts for the
    agent, matching how DeepSWE tasks declare `[agent] network_mode`.
    """
    lines = [line for line in text.splitlines() if not re.match(r"\s*allow_internet\s*=", line)]
    try:
        index = next(i for i, line in enumerate(lines) if line.strip() == "[agent]")
    except StopIteration:
        lines += ["", "[agent]"]
        index = len(lines) - 1
    if not any(re.match(r"\s*network_mode\s*=", line) for line in lines[index + 1:next(
            (j for j in range(index + 1, len(lines)) if lines[j].lstrip().startswith("[")), len(lines))]):
        lines.insert(index + 1, 'network_mode = "no-network"')
    result = "\n".join(lines) + "\n"
    parsed = tomllib.loads(result)
    if parsed["agent"].get("network_mode") != "no-network" or "allow_internet" in parsed.get("environment", {}):
        raise ValueError("agent network overlay did not apply")
    return result


def freeze(config, cache: Path) -> tuple[Path, dict]:
    """Build the frozen task tree: FrontierHarness metadata over upstream task files."""
    sources = config["sources"]
    frontier = checkout(sources["frontierharness"]["repository"], sources["frontierharness"]["commit"],
                        cache / "frontierharness-eval")
    deep_swe = checkout(sources["deep_swe"]["repository"], sources["deep_swe"]["commit"], cache / "deep-swe")
    bench = cache / "terminal-bench" / sources["terminal_bench"]["dataset"].replace("/", "_")
    if not bench.exists():
        staging = bench.with_name(bench.name + ".tmp")
        shutil.rmtree(staging, ignore_errors=True)
        staging.mkdir(parents=True)
        run(["harbor", "download", sources["terminal_bench"]["dataset"], "-o", str(staging)])
        os.replace(staging, bench)
    (bench_root,) = [path for path in bench.iterdir() if path.is_dir()]
    identity = {"protocol": PROTOCOL_VERSION, "sources": sources, "overlay": digest(inspect.getsource(agent_network_overlay))}
    frozen = cache / "frozen" / digest(identity)[:16]
    manifest_path = frozen / "manifest.json"
    if manifest_path.exists():
        return frozen, read_json(manifest_path)
    staging = frozen.with_name(frozen.name + ".tmp")
    shutil.rmtree(staging, ignore_errors=True)
    tasks = {}
    for task_id in benchmark_tasks(frontier):
        suite, name = split_task(task_id)
        target = staging / suite / name
        if suite == "terminal-bench":
            shutil.copytree(bench_root / name, target)
            published = frontier / "tasks" / name
            shutil.copy(published / "instruction.md", target / "instruction.md")
            (target / "task.toml").write_text(agent_network_overlay((published / "task.toml").read_text()))
        elif suite == "datacurve":
            shutil.copytree(deep_swe / "tasks" / name, target)
        else:
            raise ValueError(f"Unknown FrontierHarness suite in {task_id}")
        tasks[task_id] = {"path": f"{suite}/{name}", "digest": tree_digest(target)}
    manifest = {"identity": identity, "tasks": tasks,
                "terminal_bench_files": tree_digest(bench_root), "created_at": now()}
    write_json(staging / "manifest.json", manifest)
    shutil.rmtree(frozen, ignore_errors=True)
    os.replace(staging, frozen)
    return frozen, manifest


# ---------------------------------------------------------------- slopdex build


def source_digest() -> str:
    """Content of everything that compiles into the binary, including dirty edits."""
    listed = run(["git", "-C", str(ROOT), "ls-files", "-co", "--exclude-standard", "--", "Cargo.toml", "Cargo.lock",
                  "build.rs", "src"]).split("\n")
    hasher = hashlib.sha256()
    for name in sorted(filter(None, listed)):
        path = ROOT / name
        if path.is_file():
            hasher.update(name.encode() + b"\0" + path.read_bytes() + b"\0")
    return hasher.hexdigest()


def build_slopdex(config, cache: Path) -> Path:
    """A portable (glibc 2.35) build of the working tree, cached by source digest."""
    builder = config["builder"]
    binary = cache / "bin" / f"slopdex-{source_digest()[:20]}"
    if binary.exists():
        return binary
    build = cache / "build"
    for name in ("target", "cargo-home"):
        (build / name).mkdir(parents=True, exist_ok=True)
    subprocess.run(["docker", "build", "-q", "--build-arg", f"RUST_VERSION={builder['rust']}", "-t", builder["image"],
                    str(HERE / "builder")], check=True)
    subprocess.run([
        "docker", "run", "--rm", "--user", f"{os.getuid()}:{os.getgid()}",
        "-e", "CARGO_HOME=/cargo-home", "-e", "CARGO_TARGET_DIR=/target",
        "-v", f"{ROOT}:/src:ro", "-v", f"{build / 'target'}:/target", "-v", f"{build / 'cargo-home'}:/cargo-home",
        "-w", "/src", builder["image"], "cargo", "build", "--release", "--locked", "--bin", "slopdex",
    ], check=True)
    binary.parent.mkdir(parents=True, exist_ok=True)
    temporary = binary.with_suffix(".tmp")
    shutil.copy2(build / "target" / "release" / "slopdex", temporary)
    os.replace(temporary, binary)
    return binary


# ---------------------------------------------------------------- trials


def now() -> str:
    return datetime.now(timezone.utc).isoformat()


def opencode_config(model) -> dict:
    provider, model_id = model["model"].split("/", 1)
    return {
        "share": "disabled", "autoupdate": False,
        "provider": {provider: {"models": {model_id: {"variants": {model["variant"]: model["options"]}}}}},
    }


def provider_key_variable(model) -> str:
    """Prefer the dedicated eval key (e.g. OPENCODE_GO_EVALS_KEY), like the Excalidraw harness."""
    eval_key = EXCALIDRAW.eval_key_variable(model["model"].split("/", 1)[0])
    return eval_key if os.environ.get(eval_key) else "OPENCODE_API_KEY"


def controls(config, arm: str, task: dict, binary: Path | None) -> dict:
    """Everything that can change a trial's outcome; reuse requires an exact match."""
    value = {
        "protocol": PROTOCOL_VERSION, "arm": arm, "task_digest": task["digest"], "model": config["model"],
        "opencode_version": config["opencode_version"], "agent_hosts": config["agent_hosts"],
        "agent_setup_timeout_multiplier": config["agent_setup_timeout_multiplier"],
        "harbor_version": harbor_version(), "agent_source": [file_digest(path) for path in AGENT_FILES],
    }
    if arm == "map":
        value |= {"slopdex": config["slopdex"], "instructions": map_instructions(), "binary": file_digest(binary)}
    return value


_HARBOR_VERSION = None


def harbor_version() -> str:
    global _HARBOR_VERSION
    if _HARBOR_VERSION is None:
        _HARBOR_VERSION = run(["harbor", "--version"]).strip()
    return _HARBOR_VERSION


def resolve_agent_hosts(hosts: list[str]) -> dict[str, list[str]]:
    return {host: sorted({info[4][0] for info in socket.getaddrinfo(host, 443, socket.AF_INET, socket.SOCK_STREAM)})
            for host in hosts}


def hosts_overlay(resolved: dict[str, list[str]]) -> str:
    """Pin allowlisted hosts in /etc/hosts of the egress sidecar's network namespace.

    In the agent phase Harbor's sidecar rejects all non-TCP egress, which also drops
    Docker's embedded resolver forwarding upstream from inside that namespace, so no
    name resolves. The sidecar owns the namespace (`extra_hosts` on `main` conflicts
    with its network mode); TLS still passes the sidecar's SNI allowlist.
    """
    entries = "".join(f'      - "{host}:{ip}"\n' for host, ips in resolved.items() for ip in ips)
    return f"services:\n  harbor-docker-egress-control-sidecar:\n    extra_hosts:\n{entries}"


def harbor_command(config, arm: str, task_dir: Path, jobs: Path, binary: Path | None,
                   overlay: Path | None = None) -> list[str]:
    model = config["model"]
    argv = [
        "harbor", "run", "-p", str(task_dir), "-a", AGENT_IMPORT, "-m", model["model"],
        "--ak", f"arm={arm}", "--ak", f"version={config['opencode_version']}", "--ak", f"variant={model['variant']}",
        "--ak", "opencode_config=" + json.dumps(opencode_config(model), separators=(",", ":")),
        "--ae", f"OPENCODE_API_KEY=${{{provider_key_variable(model)}}}",
        "--agent-setup-timeout-multiplier", str(config["agent_setup_timeout_multiplier"]),
        "-r", "0", "-n", "1", "-y", "-q", "--jobs-dir", str(jobs), "--job-name", "trial",
    ]
    for host in config["agent_hosts"]:
        argv += ["--allow-agent-host", host]
    if overlay is not None:
        argv += ["--extra-docker-compose", str(overlay)]
    if arm == "map":
        argv += [
            "--ak", f"slopdex_binary={binary}",
            "--ak", "slopdex_settings=" + json.dumps(config["slopdex"], separators=(",", ":")),
            "--ak", "instructions=" + json.dumps(map_instructions()),
            "--ae", "OPENAI_API_KEY=${OPENAI_API_KEY}",
        ]
    return argv


def seconds_between(timing) -> float | None:
    if not timing or not timing.get("started_at") or not timing.get("finished_at"):
        return None
    start, end = (datetime.fromisoformat(timing[key].replace("Z", "+00:00")) for key in ("started_at", "finished_at"))
    return (end - start).total_seconds()


def jsonl(path: Path) -> list[dict]:
    if not path.exists():
        return []
    rows = []
    for line in path.read_text().splitlines():
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    return rows


def slopdex_commands(agent_dir: Path) -> dict[str, int]:
    starts = [row for row in jsonl(agent_dir / "slopdex-calls.jsonl") if row.get("event") == "start"]
    return dict(sorted(collections.Counter(EXCALIDRAW.slopdex_command(row.get("argv") or []) for row in starts).items()))


def faq_usage(agent_dir: Path) -> dict:
    calls = jsonl(agent_dir / "faq-usage.jsonl")
    tokens = collections.Counter()
    for call in calls:
        tokens.update({k: v for k, v in (call.get("tokens") or {}).items() if isinstance(v, (int, float))})
    costs = [call.get("cost_usd") for call in calls]
    return {"calls": len(calls), "tokens": dict(tokens),
            "cost_usd": None if any(cost is None for cost in costs) else sum(costs)}


def parse_trial(jobs: Path) -> dict:
    """Score one Harbor trial directory; infrastructure failures are not task failures."""
    results = sorted(jobs.glob("trial/*/result.json"))
    if len(results) != 1:
        return {"status": "infra_invalid", "reason": f"expected one trial result, found {len(results)}"}
    trial_dir = results[0].parent
    result = read_json(results[0])
    exception = (result.get("exception_info") or {}).get("exception_type")
    reward = ((result.get("verifier_result") or {}).get("rewards") or {}).get("reward")
    agent = result.get("agent_result") or {}
    executed = bool((result.get("agent_execution") or {}).get("started_at"))
    # An agent that never reached its model (no tokens) failed on infrastructure, not the task.
    reached_model = bool(agent.get("n_input_tokens"))
    if reward is None or not executed or (exception and (exception not in SCORED_EXCEPTIONS or not reached_model)):
        status = "infra_invalid"
    else:
        status = "success" if reward >= 1.0 else "failure"
    faq = faq_usage(trial_dir / "agent")
    agent_cost = agent.get("cost_usd")
    cost = None if agent_cost is None or (faq["calls"] and faq["cost_usd"] is None) else agent_cost + (faq["cost_usd"] or 0)
    return {
        "status": status, "reward": reward, "exception": exception,
        "agent_seconds": seconds_between(result.get("agent_execution")),
        "setup_seconds": seconds_between(result.get("agent_setup")),
        "input_tokens": agent.get("n_input_tokens"), "cache_tokens": agent.get("n_cache_tokens"),
        "output_tokens": agent.get("n_output_tokens"), "agent_cost_usd": agent_cost, "cost_usd": cost,
        "faq_usage": faq, "slopdex_commands": slopdex_commands(trial_dir / "agent"),
        "opencode_version": (result.get("agent_info") or {}).get("version"),
        "trial_dir": str(trial_dir.relative_to(HERE)) if trial_dir.is_relative_to(HERE) else str(trial_dir),
    }


def record_path(results: Path, arm: str, task_id: str) -> Path:
    return results / "trials" / arm / (task_id.replace("/", "__") + ".json")


def run_trial(config, frozen: Path, task_id: str, task: dict, arm: str, binary, results: Path, jobs_root: Path,
              control: dict) -> dict:
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    jobs = jobs_root / arm / task_id.replace("/", "__") / stamp
    jobs.mkdir(parents=True)
    resolved = resolve_agent_hosts(config["agent_hosts"])
    overlay = jobs / "agent-hosts.yaml"
    overlay.write_text(hosts_overlay(resolved))
    argv = harbor_command(config, arm, frozen / task["path"], jobs, binary, overlay)
    (jobs / "command.json").write_text(json.dumps(argv, indent=2) + "\n")
    env = {**os.environ, "PYTHONPATH": os.pathsep.join(filter(None, [str(HERE), os.environ.get("PYTHONPATH")]))}
    with (jobs / "harbor.log").open("w") as log:
        code = subprocess.run(argv, cwd=HERE, env=env, stdout=log, stderr=subprocess.STDOUT).returncode
    record = {"task": task_id, "arm": arm, "controls": control, "controls_digest": digest(control),
              "harbor_exit_code": code, "finished_at": now(), "agent_host_addresses": resolved, **parse_trial(jobs)}
    write_json(record_path(results, arm, task_id), record)
    return record


def reusable(path: Path, control: dict) -> bool:
    if not path.exists():
        return False
    record = read_json(path)
    return record.get("controls_digest") == digest(control) and record.get("status") != "infra_invalid"


def make_trials(task_ids, arms, seed: int):
    order = list(task_ids)
    random.Random(seed).shuffle(order)
    # Alternate which arm goes first so neither systematically meets warmer image caches.
    return [(task_id, arm) for i, task_id in enumerate(order)
            for arm in (arms if i % 2 == 0 else tuple(reversed(arms)))]


# ---------------------------------------------------------------- report


def median(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def arm_summary(records: list[dict]) -> dict:
    valid = [r for r in records if r["status"] in ("success", "failure")]
    passed = [r for r in valid if r["status"] == "success"]
    costs = [r.get("cost_usd") for r in valid]
    known = [c for c in costs if c is not None]
    inputs = sum(r.get("input_tokens") or 0 for r in valid)
    commands = collections.Counter()
    for r in valid:
        commands.update(r.get("slopdex_commands") or {})
    return {
        "valid": len(valid), "passed": len(passed),
        "infra_invalid": sum(r["status"] == "infra_invalid" for r in records),
        "pass_rate": len(passed) / len(valid) if valid else None,
        "total_cost_usd": sum(known), "cost_coverage": len(known) / len(valid) if valid else None,
        "effective_cost_per_pass": sum(known) / len(passed) if passed else None,
        "median_cost_per_pass": median(r.get("cost_usd") for r in passed),
        "median_agent_seconds_pass": median(r.get("agent_seconds") for r in passed),
        "median_agent_seconds": median(r.get("agent_seconds") for r in valid),
        "cache_hit_rate": sum(r.get("cache_tokens") or 0 for r in valid) / inputs if inputs else None,
        "input_tokens": inputs, "output_tokens": sum(r.get("output_tokens") or 0 for r in valid),
        "faq_calls": sum((r.get("faq_usage") or {}).get("calls", 0) for r in valid),
        "slopdex_commands": dict(sorted(commands.items())),
    }


def summarize(results: Path, task_ids: list[str]) -> dict:
    records = {arm: {} for arm in ARMS}
    for arm in ARMS:
        for task_id in task_ids:
            path = record_path(results, arm, task_id)
            if path.exists():
                records[arm][task_id] = read_json(path)
    pairs = collections.Counter()
    for task_id in task_ids:
        statuses = tuple(records[arm].get(task_id, {}).get("status") for arm in ARMS)
        if all(s in ("success", "failure") for s in statuses):
            pairs[{("success", "success"): "both_pass", ("failure", "failure"): "both_fail",
                   ("success", "failure"): "off_only", ("failure", "success"): "map_only"}[statuses]] += 1
    return {"arms": {arm: arm_summary(list(records[arm].values())) for arm in ARMS},
            "paired": dict(pairs), "records": records}


def fmt(value, kind=""):
    if value is None:
        return "—"
    if kind == "pct":
        return f"{value:.1%}"
    if kind == "usd":
        return f"${value:.3f}"
    if kind == "s":
        minutes, seconds = divmod(int(round(value)), 60)
        return f"{minutes}m {seconds:02d}s"
    return f"{value:,}" if isinstance(value, int) else str(value)


def render_report(summary: dict, task_ids: list[str], config) -> str:
    arms = summary["arms"]
    rows = [
        ("Valid trials", "valid", ""), ("Passed", "passed", ""), ("Pass rate", "pass_rate", "pct"),
        ("Infra invalid (rerun)", "infra_invalid", ""), ("Effective cost per pass", "effective_cost_per_pass", "usd"),
        ("Median cost per pass", "median_cost_per_pass", "usd"), ("Total cost", "total_cost_usd", "usd"),
        ("Median agent time (passes)", "median_agent_seconds_pass", "s"),
        ("Median agent time (all)", "median_agent_seconds", "s"), ("Cache hit rate", "cache_hit_rate", "pct"),
        ("Input tokens", "input_tokens", ""), ("Output tokens", "output_tokens", ""), ("faq calls", "faq_calls", ""),
    ]
    lines = [
        "# FrontierHarness Eval: OpenCode versus slopdex map", "",
        f"Model `{config['model']['model']}` ({config['model']['variant']}), OpenCode {config['opencode_version']}, "
        "local Harbor + Docker. Generated by `run.py report`; see README.md for method and caveats.", "",
        "| Metric | off | map |", "| --- | ---: | ---: |",
    ]
    lines += [f"| {label} | {fmt(arms['off'][key], kind)} | {fmt(arms['map'][key], kind)} |" for label, key, kind in rows]
    paired = summary["paired"]
    lines += ["", f"Paired tasks: {paired.get('both_pass', 0)} both pass, {paired.get('map_only', 0)} map only, "
              f"{paired.get('off_only', 0)} off only, {paired.get('both_fail', 0)} both fail.", "",
              f"slopdex commands (map): {json.dumps(arms['map']['slopdex_commands']) or '{}'}", "",
              "Published FrontierHarness OpenCode 1.18.19 with Kimi K3 (a different model, on Runta): "
              "50.0% pass, $3.24 effective cost per pass.", "",
              "| Task | off | map | off cost | map cost | off time | map time |",
              "| --- | --- | --- | ---: | ---: | ---: | ---: |"]
    for task_id in task_ids:
        cells = [summary["records"][arm].get(task_id, {}) for arm in ARMS]
        lines.append(f"| {task_id} | " + " | ".join(c.get("status", "pending") for c in cells) + " | "
                     + " | ".join(fmt(c.get("cost_usd"), "usd") for c in cells) + " | "
                     + " | ".join(fmt(c.get("agent_seconds"), "s") for c in cells) + " |")
    return "\n".join(lines) + "\n"


def write_report(results: Path, task_ids: list[str], config) -> str:
    summary = summarize(results, task_ids)
    write_json(results / "summary.json", {"arms": summary["arms"], "paired": summary["paired"]})
    text = render_report(summary, task_ids, config)
    (results / "report.md").write_text(text)
    return text


# ---------------------------------------------------------------- CLI


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--config", type=Path, default=HERE / "config.json")
    parser.add_argument("--cache", type=Path, default=HERE / "cache")
    parser.add_argument("--results", type=Path, default=HERE / "results")
    parser.add_argument("--jobs", type=Path, default=HERE / "jobs")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("prepare", help="Freeze the task tree and build the portable slopdex binary")
    sub.add_parser("list", help="List the frozen tasks and their digests")
    run_parser = sub.add_parser("run", help="Run missing or changed trials")
    run_parser.add_argument("--arms", nargs="+", choices=ARMS, default=list(ARMS))
    run_parser.add_argument("--task", action="append", help="Task id (repeatable), e.g. terminal-bench/regex-log")
    run_parser.add_argument("--concurrency", type=int)
    run_parser.add_argument("--dry-run", action="store_true", help="Print the plan without running trials")
    sub.add_parser("report", help="Regenerate results/report.md from saved trials")
    args = parser.parse_args(argv)
    config = read_json(args.config)

    frozen, manifest = freeze(config, args.cache)
    task_ids = list(manifest["tasks"])
    if args.command == "list":
        for task_id, task in manifest["tasks"].items():
            print(f"{task_id}\t{task['digest'][:12]}")
        return 0
    if args.command == "report":
        print(write_report(args.results, task_ids, config), end="")
        return 0
    binary = build_slopdex(config, args.cache) if args.command == "prepare" or "map" in getattr(args, "arms", []) else None
    if args.command == "prepare":
        print(json.dumps({"frozen": str(frozen), "tasks": len(task_ids), "slopdex": str(binary)}, indent=2))
        return 0

    selected = args.task or task_ids
    unknown = sorted(set(selected) - set(task_ids))
    if unknown:
        parser.error(f"unknown task(s): {', '.join(unknown)}")
    plan = []
    for task_id, arm in make_trials(selected, tuple(args.arms), config["seed"]):
        control = controls(config, arm, manifest["tasks"][task_id], binary)
        if not reusable(record_path(args.results, arm, task_id), control):
            plan.append((task_id, arm, control))
    if args.dry_run:
        print(json.dumps({"frozen": str(frozen), "slopdex": str(binary) if binary else None,
                          "pending": [f"{arm} {task_id}" for task_id, arm, _ in plan],
                          "reused": len(selected) * len(args.arms) - len(plan),
                          "map_instructions": map_instructions()}, indent=2))
        return 0
    if not os.environ.get(provider_key_variable(config["model"])):
        parser.error(f"set {provider_key_variable(config['model'])} for the {config['model']['model']} provider")
    if "map" in args.arms and not os.environ.get("OPENAI_API_KEY"):
        parser.error("set OPENAI_API_KEY for slopdex embeddings in the map arm")
    workers = args.concurrency or config["concurrency"]
    print(f"{len(plan)} trial(s) to run, {workers} at a time", file=sys.stderr)
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        futures = {pool.submit(run_trial, config, frozen, task_id, manifest["tasks"][task_id], arm, binary,
                               args.results, args.jobs, control): (task_id, arm) for task_id, arm, control in plan}
        for future in concurrent.futures.as_completed(futures):
            task_id, arm = futures[future]
            try:
                record = future.result()
                print(f"[{now()}] {arm:3} {task_id}: {record['status']} "
                      f"({fmt(record.get('cost_usd'), 'usd')}, {fmt(record.get('agent_seconds'), 's')})", file=sys.stderr)
            except Exception as error:  # keep the sweep going; the trial stays pending
                print(f"[{now()}] {arm:3} {task_id}: harness error: {error}", file=sys.stderr)
    print(write_report(args.results, task_ids, config), end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
