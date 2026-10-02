#!/usr/bin/env python3
"""Paired, local OpenCode/slopdex navigation evaluation (Python 3.11+, stdlib)."""

from __future__ import annotations

import argparse
import collections
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import random
import re
import shutil
import signal
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
SUBMODULE = HERE / "repo"
ANSWER_FORMAT = """
Return your final answer as a single JSON object, without Markdown fences:
{"findings": [{"path": "repository/relative/file.go", "symbol": "unqualifiedFunctionName",
"line": 123, "explanation": "Describe this function's role and relevant behavior."}],
"flow": "Explain how the three required functions interact and answer the task's questions."}
Include exactly three findings, one for each requested implementation role. Cite the
first line of each function declaration (1-based), not a call site. Inspect the local
source and provide substantive explanations. Use repository-relative paths. Work
independently without delegating or using the network. Do not modify repository files.
"""
BASE_INSTRUCTIONS = """# Evaluation workspace

Investigate this checkout using local tools and cite source evidence. Do not modify
files, fetch external material, or delegate. Return the requested JSON answer.
"""
ARM_INSTRUCTIONS = {
    "off": "\nUse conventional local code navigation: glob, grep/rg, and targeted reads.\nSlopdex is unavailable in this trial; do not invoke it.\n",
    "slopdex": "\nUse slopdex for code navigation:\n- `slopdex search \"describe the implementation you need\" --threshold 0.5`\n- `slopdex map --private -g \"*.go\" -e \"symbol regex\" tsc/internal`\nInclude --private when mapping Go internals so unexported symbols are visible.\nThen read the relevant source lines to verify your findings. Vary queries if needed.\nThe index is prebuilt; do not rebuild it. Conventional local tools are also available.\n",
}


def read_json(path: Path):
    return json.loads(path.read_text())


def write_json(path: Path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def digest(value) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def command(argv, *, cwd=ROOT, env=None, timeout=120) -> str:
    result = subprocess.run(argv, cwd=cwd, env=env, text=True, capture_output=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f"Command failed ({result.returncode}): {argv!r}\n{result.stderr}")
    return result.stdout.strip()


def executable(name: str) -> str:
    found = shutil.which(name)
    if not found:
        raise ValueError(f"Required executable not found: {name}")
    return str(Path(found).absolute())


def load_inputs(config_path: Path):
    config, corpus = read_json(config_path), read_json(HERE / "tasks.json")
    if config.get("schema_version") != 1 or corpus.get("schema_version") != 1:
        raise ValueError("Expected config and tasks schema_version=1")
    if not re.fullmatch(r"[0-9a-f]{40}", corpus["repository_commit"]):
        raise ValueError("Tasks must pin a full repository commit")
    ids = [task["id"] for task in corpus["tasks"]]
    if len(ids) != len(set(ids)) or any(not re.fullmatch(r"[a-z0-9-]+", item) for item in ids):
        raise ValueError("Task IDs must be unique slugs")
    for task in corpus["tasks"]:
        if len(task["targets"]) != 3 or not task["prompt"]:
            raise ValueError(f"Task {task['id']} must have a prompt and three targets")
        for target in task["targets"]:
            path = Path(target["path"])
            if path.is_absolute() or ".." in path.parts:
                raise ValueError("Target paths must be repository-relative")
    for model in config["models"]:
        if not re.fullmatch(r"[^/\s]+/[^\s]+", model["model"]) or not model["variant"]:
            raise ValueError("Models need provider/model IDs and explicit variants")
        if not isinstance(model["options"], dict) or not model["options"]:
            raise ValueError("Specify provider reasoning options for every variant")
    if not config["models"]:
        raise ValueError("Configure at least one model")
    model_keys = [(model["model"], model["variant"]) for model in config["models"]]
    if len(model_keys) != len(set(model_keys)):
        raise ValueError("Configured model/variant combinations must be unique")
    for name in ("timeout_seconds", "max_steps", "index_timeout_seconds"):
        if not isinstance(config[name], int) or config[name] <= 0:
            raise ValueError(f"{name} must be a positive integer")
    return config, corpus


def ensure_source(commit: str):
    # No --remote: git's recorded gitlink, not a moving branch, is authoritative.
    recorded = command(["git", "ls-files", "--stage", "evals/typescript/repo"]).split()
    if len(recorded) < 2 or recorded[0] != "160000" or recorded[1] != commit:
        raise ValueError("Submodule gitlink does not match tasks.repository_commit")
    command(["git", "submodule", "update", "--init", "--depth", "1", "--", "evals/typescript/repo"], timeout=600)
    if command(["git", "rev-parse", "HEAD"], cwd=SUBMODULE) != commit:
        raise ValueError("TypeScript checkout does not match the pinned commit")
    if command(["git", "status", "--porcelain", "--untracked-files=no"], cwd=SUBMODULE):
        raise ValueError("TypeScript submodule has tracked changes; use a clean checkout")


def target_lines(task, source=SUBMODULE):
    result = {}
    for target in task["targets"]:
        symbol = re.escape(target["symbol"])
        pattern = re.compile(rf"^func\s+(?:\([^\n]*\)\s+)?{symbol}\s*(?:\[[^\n]*\])?\s*\(", re.M)
        text = (source / target["path"]).read_text()
        matches = list(pattern.finditer(text))
        if len(matches) != 1:
            raise ValueError(f"Expected one declaration for {target}, found {len(matches)}")
        result[(target["path"], target["symbol"])] = text.count("\n", 0, matches[0].start()) + 1
    return result


def clone(destination: Path, commit: str):
    command(["git", "clone", "--shared", "--no-checkout", str(SUBMODULE), str(destination)])
    command(["git", "checkout", "--detach", commit], cwd=destination)


def logged_process(argv, cwd: Path, env: dict, directory: Path, timeout: int):
    """Bound the whole process group, including any CLI/tool children."""
    started = time.monotonic()
    with (directory / "stdout.jsonl").open("w") as out, (directory / "stderr.log").open("w") as err:
        process = subprocess.Popen(argv, cwd=cwd, env=env, stdout=out, stderr=err, start_new_session=True)
        timed_out = False
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
            # The parent exiting does not imply that its tool children exited.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
        except BaseException:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
            raise
    return process.returncode, timed_out, time.monotonic() - started


def index_coverage(index: Path, tasks):
    """Permit parser recovery elsewhere, but require searchable vectors for the task."""
    with closing(sqlite3.connect(f"{index.as_uri()}?mode=ro", uri=True)) as db:
        operational = db.execute("SELECT path, code, message FROM diagnostics WHERE code != 'parse-error' LIMIT 5").fetchall()
        if operational:
            raise ValueError(f"Index has non-parser indexing failures: {operational}")
        profile_row = db.execute("SELECT value FROM metadata WHERE key='active_embedding_profile'").fetchone()
        if not profile_row:
            raise ValueError("Index has no active embedding profile")
        profile = json.loads(profile_row[0])
        missing = []
        targets = [target for task in tasks for target in task["targets"]]
        for target in targets:
            rows = db.execute(
                "SELECT u.data, u.embedding_input_hash FROM symbols s "
                "JOIN search_units u ON u.path=s.path AND u.symbol_id=s.id "
                "WHERE s.path=? AND s.name=? AND u.kind='function'",
                (target["path"], target["symbol"]),
            ).fetchall()
            available = False
            for data, input_hash in rows:
                embedding_input = json.loads(data).get("embeddingInput")
                if not isinstance(embedding_input, str) or hashlib.sha256(embedding_input.encode()).hexdigest() != input_hash:
                    continue
                # Schema 3 reads durable artifacts by profile/input, including after
                # a live-state reset when unit_embeddings links are not materialized.
                payload = json.dumps([profile, "document", embedding_input], sort_keys=True, separators=(",", ":"), ensure_ascii=False)
                key = hashlib.sha256(payload.encode()).hexdigest()
                if db.execute("SELECT 1 FROM embeddings WHERE key=?", (key,)).fetchone():
                    available = True
                    break
            if not available:
                missing.append(target)
        if missing:
            raise ValueError(f"Task targets are missing from the semantic index: {missing}")
        parser_errors = db.execute("SELECT count(*) FROM diagnostics WHERE code='parse-error'").fetchone()[0]
    return {"validated_targets": len(targets), "parser_diagnostics": parser_errors}


def prepare_index(config, commit: str, cache: Path, slopdex: str, tasks):
    version = command([slopdex, "--version"])
    identity = {"commit": commit, "slopdex_version": version, "config": config["slopdex"]}
    directory = cache / digest(identity)[:20]
    directory.mkdir(parents=True, exist_ok=True)
    index = directory / "index.sqlite"
    manifest = directory / "manifest.json"
    if index.exists() and manifest.exists() and read_json(manifest).get("identity") == identity:
        return directory, {**read_json(manifest), "coverage": index_coverage(index, tasks)}
    # A per-cache advisory lock prevents concurrent preparations from sharing SQLite.
    import fcntl
    with (directory / "prepare.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if index.exists() and manifest.exists() and read_json(manifest).get("identity") == identity:
            return directory, {**read_json(manifest), "coverage": index_coverage(index, tasks)}
        write_json(directory / "config.json", config["slopdex"])
        with tempfile.TemporaryDirectory(prefix="slopdex-ts-index-") as temporary:
            workspace = Path(temporary) / "repo"
            clone(workspace, commit)
            argv = [slopdex, "--root", str(workspace), "--config", str(directory / "config.json"), "--index", str(index)]
            # Preserve paid vector artifacts when retrying an interrupted preparation
            # at a fresh clone path; force resets live state, not reusable embeddings.
            retry = ["--force-reindex", "--yes-really-rebuild-the-index"] if index.exists() else []
            code, expired, elapsed = logged_process(argv + retry + ["update"], workspace, os.environ.copy(), directory, config["index_timeout_seconds"])
            if expired or code:
                raise RuntimeError(f"Index preparation failed; see {directory / 'stderr.log'}")
            status = json.loads(command(argv + ["--no-reindex", "status"], cwd=workspace))
            if not status.get("functionCount"):
                write_json(directory / "status.json", status)
                raise RuntimeError(f"Index is empty; see {directory / 'status.json'} and slopdex index-errors")
        metadata = {"identity": identity, "wall_seconds": elapsed, "status": status}
        write_json(manifest, metadata)
    coverage = index_coverage(index, tasks)
    if coverage["parser_diagnostics"]:
        print(f"Index: {coverage['parser_diagnostics']} parser diagnostics recorded; all {coverage['validated_targets']} task targets have searchable vectors.", flush=True)
    return directory, {**metadata, "coverage": coverage}


def opencode_config(model, arm: str, steps: int):
    provider, model_id = model["model"].split("/", 1)
    permission = {
        "*": "allow", "edit": "deny", "task": "deny", "question": "deny",
        "webfetch": "deny", "websearch": "deny", "skill": "deny", "external_directory": "deny",
    }
    if arm == "off":
        permission["bash"] = {"*": "allow", "*slopdex*": "deny"}
    return {
        "$schema": "https://opencode.ai/config.json", "model": model["model"],
        "small_model": model["model"], "share": "disabled", "autoupdate": False,
        "snapshot": False, "lsp": False, "formatter": False, "plugin": [],
        "permission": permission,
        "agent": {"build": {"steps": steps, "permission": permission}, "title": {"disable": True}, "summary": {"disable": True}},
        "provider": {provider: {"models": {model_id: {"variants": {model["variant"]: model["options"]}}}}},
    }


def isolated_environment(sandbox: Path, config: dict):
    env = os.environ.copy()
    # Copy only the selected provider's auth, not sessions or account/organization
    # configuration. Never put credentials in persistent harness artifacts.
    home = Path.home()
    old_data_home = Path(env.get("XDG_DATA_HOME", str(home / ".local/share")))
    data_home = sandbox / "data"
    auth_path = old_data_home / "opencode" / "auth.json"
    if auth_path.exists():
        auth = read_json(auth_path)
        provider = config["model"].split("/", 1)[0]
        credentials = auth.get(provider)
        if credentials:
            if credentials.get("type") == "wellknown":
                raise ValueError("Remote wellknown auth configuration is incompatible with isolated evals; supply an API key")
            new_auth = data_home / "opencode" / "auth.json"
            write_json(new_auth, {provider: credentials})
            new_auth.chmod(0o600)
    for key in tuple(env):
        if key.startswith("OPENCODE_") and key not in {"OPENCODE_API_KEY", "OPENCODE_GO_API_KEY"}:
            env.pop(key)
    config_home = sandbox / "config"
    config_home.mkdir()
    env.update({
        "HOME": str(sandbox), "XDG_CONFIG_HOME": str(config_home), "XDG_DATA_HOME": str(data_home),
        "XDG_STATE_HOME": str(sandbox / "state"),
        "OPENCODE_CONFIG_DIR": str(config_home / "opencode"),
        "OPENCODE_CONFIG_CONTENT": json.dumps(config), "OPENCODE_DISABLE_PROJECT_CONFIG": "1",
        "OPENCODE_DISABLE_EXTERNAL_SKILLS": "1", "OPENCODE_DISABLE_CLAUDE_CODE_SKILLS": "1",
        "OPENCODE_DISABLE_CLAUDE_CODE": "1", "OPENCODE_PURE": "1",
    })
    return env


def check_effective_config(opencode: str, workspace: Path, env: dict, expected: dict, directory: Path):
    """Use OpenCode itself to validate config shape and benchmark-critical overrides."""
    resolved = json.loads(command([opencode, "debug", "config"], cwd=workspace, env=env))

    def contains(actual, intended):
        if isinstance(intended, dict):
            return isinstance(actual, dict) and all(key in actual and contains(actual[key], value) for key, value in intended.items())
        return actual == intended

    keys = ("model", "small_model", "share", "autoupdate", "snapshot", "lsp", "formatter", "permission", "instructions")
    critical = {key: expected[key] for key in keys}
    critical["agent"] = {"build": expected["agent"]["build"]}
    critical["provider"] = expected["provider"]
    if not contains(resolved, critical):
        raise ValueError("OpenCode's effective configuration does not preserve the eval controls")
    # Save only secret-free, explicitly generated settings, never full resolved auth.
    write_json(directory / "verified-config.json", critical)


def copy_index(source_path: Path, destination: Path, workspace: Path, commit: str):
    """Relocate a schema-3 snapshot whose source paths are repository-relative."""
    with closing(sqlite3.connect(f"{source_path.as_uri()}?mode=ro", uri=True)) as source:
        with closing(sqlite3.connect(destination)) as target, target:
            source.backup(target)
            row = target.execute("SELECT value FROM metadata WHERE key='identity'").fetchone()
            checkpoint = target.execute("SELECT value FROM metadata WHERE key='checkpoint'").fetchone()
            identity = json.loads(row[0]) if row else {}
            if identity.get("schema") != 3 or checkpoint != (commit,):
                raise ValueError("Cached slopdex index has an unsupported schema or wrong commit; rebuild the cache")
            identity["root"] = str(workspace.resolve())
            target.execute("UPDATE metadata SET value=? WHERE key='identity'", (json.dumps(identity),))


def install_wrapper(sandbox: Path, workspace: Path, arm: str, binary: str | None, index_dir: Path | None, commit: str):
    bin_dir = sandbox / "bin"
    bin_dir.mkdir()
    log = sandbox / "slopdex-calls.jsonl"
    arguments = []
    if arm == "slopdex":
        # SQLite backup includes any WAL pages; each trial gets an independent index.
        destination = sandbox / "index.sqlite"
        copy_index(index_dir / "index.sqlite", destination, workspace, commit)
        arguments = [binary, "--root", str(workspace), "--config", str(index_dir / "config.json"), "--index", str(destination), "--no-reindex"]
    script = f"""#!{sys.executable}
import json, os, sys, time
with open({str(log)!r}, 'a') as stream:
    stream.write(json.dumps({{'argv': sys.argv[1:], 'time': time.time()}}) + '\\n')
arguments = {arguments!r}
if not arguments:
    sys.stderr.write('slopdex is unavailable in the baseline arm\\n')
    sys.exit(127)
os.execv(arguments[0], arguments + sys.argv[1:])
"""
    wrapper = bin_dir / "slopdex"
    wrapper.write_text(script)
    wrapper.chmod(0o755)
    return bin_dir, log


def parse_events(path: Path):
    text_by_message = collections.OrderedDict()
    tokens = collections.Counter()
    tools = collections.Counter()
    errors, sessions = [], set()
    cost, finishes, malformed = 0.0, 0, 0
    with path.open() as stream:
        for line in stream:
            if not line.strip():
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                malformed += 1
                continue
            if not isinstance(event, dict):
                malformed += 1
                continue
            if event.get("sessionID"):
                sessions.add(event["sessionID"])
            part = event.get("part") or {}
            kind = event.get("type")
            if kind == "text":
                key = part.get("messageID", "unknown")
                text_by_message[key] = text_by_message.get(key, "") + part.get("text", "")
            elif kind == "step_finish":
                finishes += 1
                cost += part.get("cost", 0) or 0
                usage = part.get("tokens") or {}
                for name in ("input", "output", "reasoning", "total"):
                    tokens[name] += usage.get(name, 0) or 0
                for name in ("read", "write"):
                    tokens[f"cache_{name}"] += (usage.get("cache") or {}).get(name, 0) or 0
            elif kind == "tool_use":
                tools[part.get("tool", "unknown")] += 1
            elif kind == "error":
                errors.append(event.get("error", event))
    return {
        "answer_text": next(reversed(text_by_message.values()), ""), "tokens": dict(tokens),
        "tools": dict(tools), "cost_usd": cost if finishes else None,
        "steps": finishes, "errors": errors, "sessions": sorted(sessions), "malformed_event_lines": malformed,
    }


def score_answer(text: str, task, gold: dict):
    cleaned = re.sub(r"^```(?:json)?\s*|\s*```$", "", text.strip()).strip()
    try:
        answer = json.loads(cleaned)
        findings = answer["findings"]
        if not isinstance(findings, list) or len(findings) != 3 or not isinstance(answer["flow"], str) or not answer["flow"].strip():
            raise ValueError("Expected three findings and a nonempty flow explanation")
        for finding in findings:
            if (not isinstance(finding, dict) or not isinstance(finding.get("path"), str)
                or not isinstance(finding.get("symbol"), str) or type(finding.get("line")) is not int
                or finding["line"] <= 0 or not isinstance(finding.get("explanation"), str)
                or not finding["explanation"].strip()):
                raise ValueError("Invalid finding: require path, symbol, positive line and explanation")
    except (json.JSONDecodeError, KeyError, TypeError, ValueError) as error:
        return {"valid_answer": False, "error": str(error), "recall": 0.0, "precision": 0.0, "f1": 0.0, "passed": False, "matches": []}
    found, cited, details = set(), set(), []
    for finding in findings:
        path = finding["path"].removeprefix("./")
        key = (path, finding["symbol"])
        identified = key in gold
        citation = identified and gold[key] <= finding["line"] <= gold[key] + 2
        if identified:
            found.add(key)
        if citation:
            cited.add(key)
        details.append({"path": path, "symbol": finding["symbol"], "identified": identified, "citation_valid": citation, "expected_line": gold.get(key)})
    recall, precision = len(cited) / len(gold), len(cited) / len(findings)
    return {
        "valid_answer": True, "symbol_recall": len(found) / len(gold), "recall": recall,
        "precision": precision, "f1": 2 * recall * precision / (recall + precision) if recall + precision else 0.0,
        "passed": len(cited) == len(gold), "matches": details,
    }


def run_trial(trial, task, gold, config, commit, directory, opencode, slopdex, index_dir):
    directory.mkdir(parents=True, exist_ok=True)
    prompt = task["prompt"] + "\n" + ANSWER_FORMAT
    (directory / "prompt.txt").write_text(prompt)
    with tempfile.TemporaryDirectory(prefix="slopdex-ts-trial-") as temporary:
        sandbox = Path(temporary)
        workspace = sandbox / "repo"
        clone(workspace, commit)
        instructions = BASE_INSTRUCTIONS + ARM_INSTRUCTIONS[trial["arm"]]
        (workspace / "AGENTS.md").write_text(instructions)
        (directory / "AGENTS.md").write_text(instructions)
        cli_config = opencode_config(trial["model_config"], trial["arm"], config["max_steps"])
        # Project-config discovery is disabled, so load arm guidance explicitly.
        cli_config["instructions"] = [str(workspace / "AGENTS.md")]
        write_json(directory / "opencode.json", cli_config)
        env = isolated_environment(sandbox, cli_config)
        env["PWD"] = str(workspace)
        bin_dir, calls = install_wrapper(sandbox, workspace, trial["arm"], slopdex, index_dir, commit)
        env["PATH"] = str(bin_dir) + os.pathsep + env.get("PATH", "")
        check_effective_config(opencode, workspace, env, cli_config, directory)
        argv = [opencode, "run", "--model", trial["model"], "--variant", trial["variant"],
                "--agent", "build", "--format", "json", "--dir", str(workspace), "--title", trial["id"], "--", prompt]
        write_json(directory / "command.json", argv)
        code, expired, elapsed = logged_process(argv, workspace, env, directory, config["timeout_seconds"])
        events = parse_events(directory / "stdout.jsonl")
        (directory / "answer.txt").write_text(events.pop("answer_text"))
        grade = score_answer((directory / "answer.txt").read_text(), task, gold)
        changes = command(["git", "diff", "--name-only", "HEAD"], cwd=workspace)
        untracked = command(["git", "ls-files", "--others", "--exclude-standard"], cwd=workspace).splitlines()
        violations = [path for path in changes.splitlines() + untracked if path != "AGENTS.md"]
        slopdex_calls = []
        if calls.exists():
            shutil.copyfile(calls, directory / "slopdex-calls.jsonl")
            slopdex_calls = [json.loads(line) for line in calls.read_text().splitlines()]
        status = "ok"
        if expired:
            status = "timeout"
        elif code or events["errors"]:
            status = "agent_error"
        elif violations or (trial["arm"] == "off" and slopdex_calls):
            status = "protocol_violation"
        elif not grade["valid_answer"]:
            status = "invalid_answer"
        if status != "ok":
            grade["f1"], grade["passed"] = 0.0, False
        result = {
            **{key: value for key, value in trial.items() if key != "model_config"},
            **events, "status": status, "exit_code": code, "timed_out": expired,
            "wall_seconds": elapsed, "grade": grade, "workspace_violations": violations,
            "slopdex_calls": len(slopdex_calls),
        }
        write_json(directory / "result.json", result)
    return result


def make_trials(config, tasks, arms, repeats, seed):
    pairs = [(model, task, repeat) for model in config["models"] for task in tasks for repeat in range(1, repeats + 1)]
    random.Random(seed).shuffle(pairs)
    trials = []
    for position, (model, task, repeat) in enumerate(pairs):
        order = arms if position % 2 == 0 else list(reversed(arms))
        for arm in order:
            key = f"{model['model']}@{model['variant']}:{task['id']}:{repeat}:{arm}"
            trials.append({"id": digest(key)[:16], "task": task["id"], "repeat": repeat,
                           "model": model["model"], "variant": model["variant"], "arm": arm, "model_config": model})
    return trials


def report(output: Path):
    manifest = read_json(output / "manifest.json")
    results = [read_json(output / "trials" / trial["id"] / "result.json") for trial in manifest["trials"]
               if (output / "trials" / trial["id"] / "result.json").exists()]
    groups, pairs = collections.defaultdict(list), collections.defaultdict(dict)
    for result in results:
        groups[(result["model"], result["variant"], result["arm"])].append(result)
        pairs[(result["model"], result["variant"], result["task"], result["repeat"])][result["arm"]] = result
    summary = {"completed": len(results), "planned": len(manifest["trials"]), "groups": [], "paired": []}
    lines = ["# TypeScript paired evaluation", "", f"Commit: `{manifest['commit']}`", "",
             f"Completed {len(results)}/{len(manifest['trials'])} trials. F1 includes failures as zero.", "",
             "| Model | Thinking | Arm | N | Passed | Mean F1 | Mean seconds | Input | Output | Reasoning | Cache read | Mean USD | Slopdex calls |",
             "|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"]
    for (model, variant, arm), rows in sorted(groups.items()):
        costs = [row["cost_usd"] for row in rows if row["cost_usd"] is not None]
        group = {"model": model, "variant": variant, "arm": arm, "n": len(rows),
                 "passed": sum(row["grade"]["passed"] for row in rows),
                 "mean_f1": statistics.mean(row["grade"]["f1"] for row in rows),
                 "mean_wall_seconds": statistics.mean(row["wall_seconds"] for row in rows),
                 "mean_cost_usd": statistics.mean(costs) if costs else None,
                 "statuses": dict(collections.Counter(row["status"] for row in rows)),
                 "mean_tokens": {name: statistics.mean(row["tokens"].get(name, 0) for row in rows) for name in ("input", "output", "reasoning", "cache_read", "cache_write")},
                 "slopdex_calls": sum(row["slopdex_calls"] for row in rows)}
        summary["groups"].append(group)
        usage = group["mean_tokens"]
        cost = f"{group['mean_cost_usd']:.5f}" if costs else "n/a"
        lines.append(f"| {model} | {variant} | {arm} | {len(rows)} | {group['passed']} | {group['mean_f1']:.3f} | {group['mean_wall_seconds']:.1f} | {usage['input']:.0f} | {usage['output']:.0f} | {usage['reasoning']:.0f} | {usage['cache_read']:.0f} | {cost} | {group['slopdex_calls']} |")
    differences = collections.defaultdict(list)
    for (model, variant, task, repeat), pair in sorted(pairs.items()):
        if not {"off", "slopdex"} <= pair.keys():
            continue
        off, on = pair["off"], pair["slopdex"]
        delta = {"model": model, "variant": variant, "task": task, "repeat": repeat,
                 "f1_delta": on["grade"]["f1"] - off["grade"]["f1"],
                 "both_ok": off["status"] == on["status"] == "ok"}
        # Time/cost improvements are only interpretable for completed protocol-valid pairs.
        if delta["both_ok"]:
            delta["seconds_delta"] = on["wall_seconds"] - off["wall_seconds"]
            delta["input_tokens_delta"] = on["tokens"].get("input", 0) - off["tokens"].get("input", 0)
            if on["cost_usd"] is not None and off["cost_usd"] is not None:
                delta["cost_delta_usd"] = on["cost_usd"] - off["cost_usd"]
        summary["paired"].append(delta)
        differences[(model, variant)].append(delta)
    lines += ["", "## Paired differences (slopdex − baseline)", "", "Positive F1 favors slopdex; negative time/token/cost differences favor slopdex.", ""]
    for (model, variant), rows in sorted(differences.items()):
        completed = [row for row in rows if row["both_ok"]]
        timing = f"{statistics.mean(row['seconds_delta'] for row in completed):+.1f}s" if completed else "n/a"
        lines.append(f"- **{model} / {variant}**: {len(rows)} pairs; mean F1 Δ {statistics.mean(row['f1_delta'] for row in rows):+.3f}; mean time Δ {timing} ({len(completed)} valid pairs).")
    lines += ["", "## Per-task pairs", "", "| Model | Thinking | Task | Repeat | F1 Δ | Both valid | Seconds Δ | Input Δ | USD Δ |", "|---|---|---|---:|---:|---|---:|---:|---:|"]
    for row in summary["paired"]:
        lines.append(f"| {row['model']} | {row['variant']} | {row['task']} | {row['repeat']} | {row['f1_delta']:+.3f} | {row['both_ok']} | {row.get('seconds_delta', 'n/a')} | {row.get('input_tokens_delta', 'n/a')} | {row.get('cost_delta_usd', 'n/a')} |")
    if manifest.get("index"):
        lines += ["", "## Index preparation", "", f"One-time preparation: {manifest['index']['wall_seconds']:.1f}s (cached across trials).", "Embedding/index preparation costs are not included in OpenCode's reported model cost."]
        coverage = manifest["index"].get("coverage", {})
        if coverage.get("parser_diagnostics"):
            lines += [f"Parser diagnostics elsewhere in the checkout: {coverage['parser_diagnostics']}; validated searchable task targets: {coverage['validated_targets']}."]
    lines += ["", "Scoring measures symbol identification with valid declaration citations. Explanation correctness requires manual review of `answer.txt`; no LLM judge is used.", ""]
    write_json(output / "summary.json", summary)
    (output / "report.md").write_text("\n".join(lines))
    return summary


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=HERE / "config.json")
    parser.add_argument("--cache", type=Path, default=HERE / "cache")
    sub = parser.add_subparsers(dest="action", required=True)
    sub.add_parser("list", help="List task prompts and default models without network access")
    sub.add_parser("validate", help="Initialize source and verify every gold declaration")
    sub.add_parser("prepare", help="Initialize shallow submodule and prebuild the slopdex index")
    run = sub.add_parser("run", help="Run paired trials; an existing output directory resumes")
    run.add_argument("--output", type=Path)
    run.add_argument("--task", action="append", help="Task ID; repeat to select several")
    run.add_argument("--model", action="append", help="Configured provider/model@variant; repeat to select several")
    run.add_argument("--arms", nargs="+", choices=list(ARM_INSTRUCTIONS), default=["off", "slopdex"])
    run.add_argument("--repeats", type=int, default=1)
    run.add_argument("--seed", type=int, default=0)
    run.add_argument("--dry-run", action="store_true", help="Print trial matrix without network, indexing or model calls")
    report_parser = sub.add_parser("report", help="Regenerate reports from saved trial results")
    report_parser.add_argument("output", type=Path)
    args = parser.parse_args(argv)
    if args.action == "report":
        print(json.dumps(report(args.output.resolve()), indent=2))
        return
    config, corpus = load_inputs(args.config.resolve())
    commit, tasks = corpus["repository_commit"], corpus["tasks"]
    if args.action == "list":
        for model in config["models"]:
            print(f"Model: {model['model']}@{model['variant']}")
        for task in tasks:
            print(f"\n{task['id']}: {task['title']}\n{task['prompt']}")
        return
    if args.action == "run":
        if args.repeats <= 0 or len(set(args.arms)) != len(args.arms):
            raise ValueError("Use positive repeats and unique arms")
        if args.task:
            unknown = set(args.task) - {task["id"] for task in tasks}
            if unknown:
                raise ValueError(f"Unknown tasks: {sorted(unknown)}")
            tasks = [task for task in tasks if task["id"] in args.task]
        if args.model:
            names = {f"{model['model']}@{model['variant']}" for model in config["models"]}
            if set(args.model) - names:
                raise ValueError(f"Unknown configured models: {sorted(set(args.model) - names)}")
            config["models"] = [model for model in config["models"] if f"{model['model']}@{model['variant']}" in args.model]
        trials = make_trials(config, tasks, args.arms, args.repeats, args.seed)
        if args.dry_run:
            print(json.dumps({"commit": commit, "trials": trials, "count": len(trials)}, indent=2))
            return
    if args.action in {"validate", "prepare"}:
        ensure_source(commit)
        gold = {task["id"]: target_lines(task) for task in tasks}
        if args.action == "validate":
            print(f"Validated {len(tasks)} tasks / {sum(map(len, gold.values()))} declarations at {commit}")
        else:
            directory, metadata = prepare_index(config, commit, args.cache.resolve(), executable("slopdex"), tasks)
            print(f"Index ready: {directory}\n{json.dumps(metadata, indent=2)}")
        return
    output = (args.output or HERE / "jobs" / f"{time.strftime('%Y%m%d-%H%M%S')}-{time.time_ns() % 1000000:06d}").resolve()
    settings = {"config": config, "tasks": tasks, "commit": commit, "trials": trials,
                "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    task_by_id = {task["id"]: task for task in tasks}
    # One runner owns an output directory at a time; interrupted runs can be resumed.
    import fcntl
    output.mkdir(parents=True, exist_ok=True)
    with (output / "run.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        saved = None
        if (output / "manifest.json").exists():
            saved = read_json(output / "manifest.json")
            if any(saved.get(key) != value for key, value in settings.items()):
                raise ValueError("Resume settings differ from the saved manifest; use a new --output directory")
        pending = [trial for trial in trials if not (output / "trials" / trial["id"] / "result.json").exists()]
        if saved and not pending:
            report(output)
            print(f"All trials already saved. Report: {output / 'report.md'}")
            return
        opencode = executable("opencode")
        slopdex = executable("slopdex") if "slopdex" in args.arms else None
        identity = {**settings, "opencode_version": command([opencode, "--version"]),
                    "slopdex_version": command([slopdex, "--version"]) if slopdex else None}
        if saved and saved["fingerprint"] != digest(identity):
            raise ValueError("Resume executable versions differ; use a new --output directory")
        ensure_source(commit)
        gold = {task["id"]: target_lines(task) for task in tasks}
        index_dir, metadata = (None, saved.get("index") if saved else None)
        if any(trial["arm"] == "slopdex" for trial in pending):
            index_dir, metadata = prepare_index(config, commit, args.cache.resolve(), slopdex, tasks)
        manifest = {**identity, "fingerprint": digest(identity), "index": metadata, "seed": args.seed}
        write_json(output / "manifest.json", manifest)
        for number, trial in enumerate(trials, 1):
            directory = output / "trials" / trial["id"]
            if (directory / "result.json").exists():
                continue
            print(f"[{number}/{len(trials)}] {trial['model']} / {trial['variant']} / {trial['task']} / {trial['arm']}", flush=True)
            result = run_trial(trial, task_by_id[trial["task"]], gold[trial["task"]], config, commit, directory, opencode, slopdex, index_dir)
            print(f"  {result['status']}: F1={result['grade']['f1']:.3f}, {result['wall_seconds']:.1f}s", flush=True)
            report(output)
        report(output)
    print(f"Report: {output / 'report.md'}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError, subprocess.TimeoutExpired) as error:
        print(f"eval: {error}", file=sys.stderr)
        sys.exit(1)
