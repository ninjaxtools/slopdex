#!/usr/bin/env python3
"""Paired, local OpenCode/slopdex navigation evaluation (Python 3.11+, stdlib)."""

from __future__ import annotations

import argparse
import collections
from contextlib import closing
import hashlib
import inspect
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
"flow": "Explain how the required functions interact and answer the task's questions."}
Include exactly {finding_count} findings, one for each requested implementation role. Cite the
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
    "map": "\nUse `slopdex map` successfully; other slopdex navigation is blocked.\nCheckout and symbol vectors are ready: skip setup/help; never override root/index/config.\nDiscover with --ignore-errors -g '*.go' -g '!**/*_test.go'; then read source.\n",
    "search": "\nOnly `slopdex search` is available for slopdex navigation in this trial.\nUse it successfully at least once; other slopdex navigation commands are blocked.\nRead the relevant source lines to verify your findings.\nThe semantic index is prebuilt; do not rebuild it.\nConventional local tools remain available for finding paths and verifying source.\n",
    "map-search": "\nOnly `slopdex map` and `slopdex search` are available for slopdex navigation in this trial.\nUse both successfully at least once; other slopdex navigation commands are blocked.\nInclude --private when mapping Go internals so unexported symbols are visible.\nRead the relevant source lines to verify your findings.\nThe semantic and structural index is prebuilt; do not rebuild it.\nConventional local tools remain available for finding paths and verifying source.\n",
    "slopdex": "\nUse slopdex for code navigation:\n- `slopdex search \"describe the implementation you need\" --threshold 0.3 --limit 50`\n- `slopdex map --private -g \"*.go\" -e \"symbol regex\" tsc/internal`\nInclude --private when mapping Go internals so unexported symbols are visible.\nMap's -e matches symbol names and qualified names, not declaration text or function bodies.\nNarrow paths and symbol filters before reading the returned implementation line ranges.\nLimit search candidates before rendering; avoid head as a result selector.\nThen read the relevant source lines to verify your findings. Vary queries if needed.\nThe index is prebuilt; do not rebuild it. Conventional local tools are also available.\n",
}
for _arm in ("map-first", "map-follow", "map-verify"):
    ARM_INSTRUCTIONS[_arm] = "\nOnly `slopdex map` is available for slopdex navigation in this trial.\nUse it successfully at least once; other slopdex navigation commands are blocked.\nInclude --private when mapping Go internals so unexported symbols are visible.\nThe structural index is prebuilt; do not rebuild it.\nConventional local tools remain available for finding paths and verifying source.\n"
DEFAULT_ARMS = ["map", "search"]
ARM_NAVIGATION = {"map": ("map",), "map-first": ("map",), "map-follow": ("map",), "map-verify": ("map",),
                  "search": ("search",), "map-search": ("map", "search")}
MAP_ARMS = {arm for arm, commands in ARM_NAVIGATION.items() if commands == ("map",)}
STRUCTURAL_MAP_ARMS = MAP_ARMS - {"map"}
MAP_PROMPTS = {arm: HERE / "prompts" / f"{arm}.md" for arm in ("map-first", "map-follow", "map-verify")}
SEMANTIC_ARMS = {"search", "map-search", "slopdex"}


def readme_agent_instruction(command_name: str) -> str:
    """Read the advertised AGENTS.md snippet verbatim from the project's README."""
    blocks, current = [], None
    for line in (ROOT / "README.md").read_text().splitlines():
        if line == "> ```text":
            current = []
        elif current is not None and line == "> ```":
            blocks.append("\n".join(current))
            current = None
        elif current is not None:
            current.append(line.removeprefix("> ").removeprefix(">"))
    matches = [block for block in blocks if f"`slopdex {command_name} " in block]
    if len(matches) != 1:
        raise ValueError(f"Expected one {command_name} agent instruction block in {ROOT / 'README.md'}")
    return matches[0]


def arm_instructions(arm: str) -> str:
    guidance = ARM_INSTRUCTIONS[arm]
    if arm in MAP_PROMPTS:
        return "\n## Code navigation\n\n" + MAP_PROMPTS[arm].read_text().strip() + "\n" + guidance
    if arm in ARM_NAVIGATION:
        snippets = "\n\n".join(readme_agent_instruction(command) for command in ARM_NAVIGATION[arm])
        return "\n## Code navigation\n\n" + snippets + "\n" + guidance
    return guidance


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
        if not isinstance(task["targets"], list) or len(task["targets"]) < 3 or not task["prompt"]:
            raise ValueError(f"Task {task['id']} must have a prompt and at least three targets")
        targets = [(target["path"], target["symbol"]) for target in task["targets"]]
        if len(targets) != len(set(targets)):
            raise ValueError(f"Task {task['id']} must have distinct path/symbol targets")
        if task.get("difficulty") == "advanced" and (len(targets) < 5 or len({path for path, _ in targets}) < 3):
            raise ValueError(f"Advanced task {task['id']} needs at least five targets across three files")
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


def index_coverage(index: Path, tasks, *, semantic=True):
    """Permit parser recovery elsewhere, but require searchable vectors for the task."""
    with closing(sqlite3.connect(f"{index.as_uri()}?mode=ro", uri=True)) as db:
        operational = db.execute("SELECT path, code, message FROM diagnostics WHERE code != 'parse-error' LIMIT 5").fetchall()
        if operational:
            raise ValueError(f"Index has non-parser indexing failures: {operational}")
        if semantic:
            profile_row = db.execute("SELECT value FROM metadata WHERE key='active_embedding_profile'").fetchone()
            if not profile_row:
                raise ValueError("Index has no active embedding profile")
            profile = json.loads(profile_row[0])
        missing = []
        targets = [target for task in tasks for target in task["targets"]]
        for target in targets:
            if not semantic:
                if not db.execute("SELECT 1 FROM symbols WHERE path=? AND name=? AND kind IN ('function', 'method')",
                                  (target["path"], target["symbol"])).fetchone():
                    missing.append(target)
                continue
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
            raise ValueError(f"Task targets are missing from the {'semantic' if semantic else 'structural'} index: {missing}")
        parser_errors = db.execute("SELECT count(*) FROM diagnostics WHERE code='parse-error'").fetchone()[0]
    return {"validated_targets": len(targets), "parser_diagnostics": parser_errors}


def file_digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def native_ann_snapshot(index: Path, *, symbol_only=False):
    """Validate the native sidecars against their binaries and SQLite identity."""
    with closing(sqlite3.connect(f"{index.as_uri()}?mode=ro", uri=True)) as db:
        metadata = dict(db.execute("SELECT key, value FROM metadata"))
    generation = int(metadata["generation"])
    profile = None if symbol_only else json.loads(metadata["active_embedding_profile"])
    descriptions = metadata.get("descriptions_enabled") == "true"
    kinds = {"symbols": 1} if symbol_only else {"code": 1, "markdown": 1}
    if descriptions and not symbol_only:
        kinds.update({"descriptions": 2, "combined": 3})
    artifacts = {}
    for kind, parts in kinds.items():
        suffix = f".{kind}.usearch"
        binary = Path(str(index) + suffix)
        manifest_path = Path(str(binary) + ".manifest.json")
        manifest = read_json(manifest_path)
        if not isinstance(manifest, dict):
            raise ValueError(f"Native ANN manifest must be an object: {manifest_path}")
        binary_hash = file_digest(binary)
        if (manifest.get("version") != 1 or manifest.get("generation") != generation
            or (not symbol_only and manifest.get("dimensions") != profile["dimensions"] * parts)
            or (symbol_only and (not isinstance(manifest.get("dimensions"), int) or manifest["dimensions"] <= 0))
            or manifest.get("binary_hash") != binary_hash):
            raise ValueError(f"Native ANN sidecar is stale or corrupt: {binary}")
        artifacts[suffix] = {"sha256": binary_hash, "size_bytes": binary.stat().st_size}
        artifacts[suffix + ".manifest.json"] = {"sha256": file_digest(manifest_path), "size_bytes": manifest_path.stat().st_size}
    return {"schema_version": 1, "generation": generation, "embedding_profile": profile,
            "descriptions_enabled": descriptions, "artifacts": artifacts}


def native_ann_ready(index: Path, metadata: dict, *, symbol_only=False) -> bool:
    saved = metadata.get("native_ann", {})
    if not isinstance(saved, dict):
        return False
    try:
        return {key: value for key, value in saved.items() if key != "wall_seconds"} == native_ann_snapshot(index, symbol_only=symbol_only)
    except (OSError, ValueError, KeyError, sqlite3.Error):
        return False


def offline_ann_command(binary: str, workspace: Path, config_path: Path, index: Path, *, symbol_only=False):
    # Cross-search opens the semantic engine for writing, persisting/reusing every
    # ANN index. An impossible source regex avoids comparisons and provider calls.
    # Symbol probes preembed vocabulary/query once, then reuse those cached vectors;
    # search-symbols always opens the graph, bypassing map's selector-result cache.
    navigation = ["search-symbols", "slopdex eval warmup", "-e", "a^"] if symbol_only else ["cross-search", "-e", "a^"]
    return [binary, "--root", str(workspace), "--config", str(config_path), "--index", str(index),
            "--no-reindex", "--format", "json", *navigation]


def warm_native_ann(directory: Path, metadata: dict, binary: str, workspace: Path, timeout: int, *, symbol_only=False):
    index = directory / "index.sqlite"
    with closing(sqlite3.connect(index)) as db, db:
        relocate_index(db, workspace, metadata["identity"]["commit"])
    logs = directory / "ann-warmup"
    logs.mkdir(exist_ok=True)
    argv = offline_ann_command(binary, workspace, directory / "config.json", index, symbol_only=symbol_only)
    code, expired, elapsed = logged_process(argv, workspace, os.environ.copy(), logs, timeout)
    if code or expired:
        raise RuntimeError(f"Native ANN warm-up failed; see {logs / 'stderr.log'}")
    metadata["native_ann"] = {**native_ann_snapshot(index, symbol_only=symbol_only), "wall_seconds": elapsed}
    metadata["status"]["rootDir"] = str(workspace.resolve())
    write_json(directory / "manifest.json", metadata)
    return metadata


def prepare_index(config, commit: str, cache: Path, slopdex: str, tasks):
    version = command([slopdex, "--version"])
    identity = {"commit": commit, "slopdex_version": version, "config": config["slopdex"]}
    directory = cache / digest(identity)[:20]
    directory.mkdir(parents=True, exist_ok=True)
    index = directory / "index.sqlite"
    manifest = directory / "manifest.json"
    # A per-cache advisory lock prevents concurrent preparations from sharing SQLite.
    import fcntl
    with (directory / "prepare.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        metadata = read_json(manifest) if manifest.exists() else {}
        if not index.exists() or metadata.get("identity") != identity:
            write_json(directory / "config.json", config["slopdex"])
            with tempfile.TemporaryDirectory(prefix="slopdex-ts-index-") as temporary:
                workspace = Path(temporary) / "repo"
                clone(workspace, commit)
                argv = [slopdex, "--root", str(workspace), "--config", str(directory / "config.json"), "--index", str(index)]
                # A live-state reset retains reusable paid vector artifacts.
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
        if not native_ann_ready(index, metadata):
            # Upgrade existing embedding caches in place, without regenerating vectors.
            metadata = warm_native_ann(directory, metadata, slopdex, SUBMODULE, config["index_timeout_seconds"])
    if coverage["parser_diagnostics"]:
        print(f"Index: {coverage['parser_diagnostics']} parser diagnostics recorded; all {coverage['validated_targets']} task targets have searchable vectors.", flush=True)
    return directory, {**metadata, "coverage": coverage}


def prepare_map_index(config, commit: str, cache: Path, slopdex: str, tasks, *, symbols=False):
    """Build structure, optionally preembedding names for the README's -q workflow."""
    settings = {**config["slopdex"], "descriptionsEnabled": False, "rerankingEnabled": False}
    identity = {"kind": "map-symbols" if symbols else "map", "commit": commit, "slopdex_version": command([slopdex, "--version"]), "config": settings}
    directory = cache / digest(identity)[:20]
    directory.mkdir(parents=True, exist_ok=True)
    index, manifest = directory / "index.sqlite", directory / "manifest.json"
    import fcntl
    with (directory / "prepare.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        metadata = read_json(manifest) if manifest.exists() else {}
        if not index.exists() or metadata.get("identity") != identity:
            write_json(directory / "config.json", settings)
            with tempfile.TemporaryDirectory(prefix="slopdex-ts-map-") as temporary:
                workspace = Path(temporary) / "repo"
                clone(workspace, commit)
                argv = [slopdex, "--root", str(workspace), "--config", str(directory / "config.json"), "--index", str(index)]
                retry = ["--force-reindex", "--yes-really-rebuild-the-index"]
                if not index.exists():
                    # Map without an index now parses directly without persisting.
                    # An empty SQLite file opts into indexed structural preparation.
                    with closing(sqlite3.connect(index)):
                        pass
                code, expired, elapsed = logged_process(argv + retry + ["map", "--private", "-e", "a^"], workspace, os.environ.copy(), directory, config["index_timeout_seconds"])
                if code or expired:
                    raise RuntimeError(f"Structural index preparation failed; see {directory / 'stderr.log'}")
                status = json.loads(command(argv + ["--no-reindex", "status"], cwd=workspace))
            metadata = {"identity": identity, "wall_seconds": elapsed, "status": status}
            write_json(manifest, metadata)
        coverage = index_coverage(index, tasks, semantic=False)
        if symbols and not native_ann_ready(index, metadata, symbol_only=True):
            metadata = warm_native_ann(directory, metadata, slopdex, SUBMODULE, config["index_timeout_seconds"], symbol_only=True)
    return directory, {**metadata, "coverage": coverage}


def opencode_config(model, arm: str, steps: int):
    provider, model_id = model["model"].split("/", 1)
    permission = {
        "*": "allow", "edit": "deny", "task": "deny", "question": "deny",
        "webfetch": "deny", "websearch": "deny", "skill": "deny", "external_directory": "deny",
    }
    if arm == "off":
        permission["bash"] = {"*": "allow", "*slopdex *": "deny", "*slopdex": "deny"}
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


def relocate_index(db, workspace: Path, commit: str):
    row = db.execute("SELECT value FROM metadata WHERE key='identity'").fetchone()
    checkpoint = db.execute("SELECT value FROM metadata WHERE key='checkpoint'").fetchone()
    identity = json.loads(row[0]) if row else {}
    if identity.get("schema") != 3 or checkpoint != (commit,):
        raise ValueError("Cached slopdex index has an unsupported schema or wrong commit; rebuild the cache")
    identity["root"] = str(workspace.resolve())
    db.execute("UPDATE metadata SET value=? WHERE key='identity'", (json.dumps(identity),))


def copy_index(source_path: Path, destination: Path, workspace: Path, commit: str, *, include_ann=True, symbol_only=False):
    """Relocate SQLite and its warmed ANN sidecars without changing vector identity."""
    native_ann = native_ann_snapshot(source_path, symbol_only=symbol_only) if include_ann else None
    with closing(sqlite3.connect(f"{source_path.as_uri()}?mode=ro", uri=True)) as source:
        with closing(sqlite3.connect(destination)) as target, target:
            source.backup(target)
            relocate_index(target, workspace, commit)
    if native_ann is not None:
        for suffix in native_ann["artifacts"]:
            shutil.copy2(Path(str(source_path) + suffix), Path(str(destination) + suffix))
        if native_ann_snapshot(destination, symbol_only=symbol_only) != native_ann:
            raise ValueError("Copied ANN sidecars do not match the trial SQLite snapshot")


def verify_native_ann_reuse(binary: str, workspace: Path, config_path: Path, index: Path, directory: Path, timeout: int, *, symbol_only=False):
    """Prove the writable native loader accepts copied sidecars before agent timing."""
    before = native_ann_snapshot(index, symbol_only=symbol_only)
    stamps = {suffix: Path(str(index) + suffix).stat().st_mtime_ns for suffix in before["artifacts"]}
    logs = directory / "ann-validation"
    logs.mkdir(exist_ok=True)
    argv = offline_ann_command(binary, workspace, config_path, index, symbol_only=symbol_only)
    env = os.environ.copy()
    symbol_query = None
    symbol_manifest = None
    if symbol_only:
        symbol_manifest = read_json(Path(str(index) + ".symbols.usearch.manifest.json"))
        # Force the actual map -q path through the graph rather than a cached
        # selector result. This is the trial's disposable copy, not the cache.
        with closing(sqlite3.connect(index)) as db, db:
            db.execute("DELETE FROM search_cache")
            embedding_count = db.execute("SELECT count(*) FROM embeddings").fetchone()[0]
        settings = read_json(config_path)
        for key in ("embeddingApiKey", "openaiApiKey", "jinaApiKey"):
            settings.pop(key, None)
        for key in ("OPENAI_API_KEY", "JINA_API_KEY"):
            env.pop(key, None)
        offline_config = logs / "config.json"
        write_json(offline_config, settings)
        # With the root already relocated, force only selects the writable loader.
        # Any graph rebuild is observable in the sidecars. A structural refresh
        # may advance generation while retaining the identical vocabulary/graph;
        # that only updates manifest generation/fingerprint, not the binary.
        argv = [binary, "--root", str(workspace), "--config", str(offline_config), "--index", str(index),
                "--force-reindex", "--yes-really-rebuild-the-index", "--ignore-errors", "--format", "json", "map", "--private", "-k", "fns",
                "-q", "slopdex eval warmup", "--symbol-threshold", "0", "-e", "a^"]
        symbol_query = {"command": "map -q", "provider_credentials_removed": True,
                        "selector_cache_cleared": True, "embedding_count": embedding_count}
    code, expired, elapsed = logged_process(argv, workspace, env, logs, timeout)
    if code or expired:
        raise RuntimeError(f"Trial ANN validation failed; see {logs / 'stderr.log'}")
    after_stamps = {suffix: Path(str(index) + suffix).stat().st_mtime_ns for suffix in before["artifacts"]}
    after = native_ann_snapshot(index, symbol_only=symbol_only)
    unchanged = after == before and after_stamps == stamps
    if symbol_only:
        current_manifest = read_json(Path(str(index) + ".symbols.usearch.manifest.json"))
        graph = ".symbols.usearch"
        generation_only = current_manifest["generation"] != symbol_manifest["generation"]
        comparable = lambda manifest: {key: value for key, value in manifest.items() if key not in {"generation", "fingerprint"}}
        unchanged = (comparable(current_manifest) == comparable(symbol_manifest)
                     and after["artifacts"][graph] == before["artifacts"][graph]
                     and after_stamps[graph] == stamps[graph]
                     and (generation_only or after_stamps == stamps))
        symbol_query["manifest_generation_updated"] = generation_only
    if not unchanged:
        raise ValueError("Slopdex rebuilt a trial's copied ANN index; preparation is incompatible")
    if symbol_only:
        with closing(sqlite3.connect(index)) as db:
            after_count = db.execute("SELECT count(*) FROM embeddings").fetchone()[0]
        if after_count != embedding_count or "external model call" in (logs / "stderr.log").read_text():
            raise ValueError("Map -q did not reuse the prebuilt symbol vectors offline")
    result = {"validated": True, "wall_seconds": elapsed, "snapshot": after}
    if symbol_query is not None:
        result["symbol_query"] = symbol_query
    write_json(directory / "ann-validation.json", result)
    return result


def install_wrapper(sandbox: Path, workspace: Path, arm: str, binary: str | None, index_dir: Path | None, commit: str):
    bin_dir = sandbox / "bin"
    bin_dir.mkdir()
    log = sandbox / "slopdex-calls.jsonl"
    arguments = []
    if arm != "off":
        # SQLite backup includes any WAL pages; each trial gets an independent index.
        destination = sandbox / "index.sqlite"
        copy_index(index_dir / "index.sqlite", destination, workspace, commit,
                   include_ann=arm in SEMANTIC_ARMS or arm == "map", symbol_only=arm == "map")
        arguments = [binary, "--root", str(workspace), "--config", str(index_dir / "config.json"), "--index", str(destination), "--no-reindex"]
    script = f"""#!{sys.executable}
import json, subprocess, sys, time, uuid
ARM_NAVIGATION = {ARM_NAVIGATION!r}
{inspect.getsource(slopdex_command)}
{inspect.getsource(slopdex_call_allowed)}
call_id = uuid.uuid4().hex
command = slopdex_command(sys.argv[1:])
allowed = slopdex_call_allowed({arm!r}, sys.argv[1:])
def record(value):
    with open({str(log)!r}, 'a') as stream:
        stream.write(json.dumps(value) + '\\n')
record({{'event': 'start', 'id': call_id, 'argv': sys.argv[1:], 'command': command,
        'allowed': allowed, 'time': time.time()}})
arguments = {arguments!r}
if not allowed or not arguments:
    sys.stderr.write('slopdex command is unavailable in this evaluation arm\\n')
    code = 126
else:
    try:
        code = subprocess.run(arguments + sys.argv[1:]).returncode
    except OSError as error:
        sys.stderr.write(str(error) + '\\n')
        code = 127
record({{'event': 'finish', 'id': call_id, 'exit_code': code, 'end_time': time.time()}})
sys.exit(code)
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


def slopdex_command(argv: list[str]) -> str:
    """Identify top-level commands without mistaking global option values for them."""
    commands = {
        "search", "search-code", "search-descriptions", "search-md", "describe",
        "cross-search", "map", "status", "index-errors", "update", "refresh",
        "descriptions", "reindex-files", "models", "config", "help",
    }
    value_options = {
        "--root", "--config", "--index", "--provider", "--model", "--dimensions",
        "--description-provider", "--description-model", "--description-fallback-model",
        "--reranker-candidates", "--format", "--detail", "--expand-code-threshold",
    }
    flag_options = {"--no-reindex", "--force-reindex", "--rebuild-on-divergence",
                    "--yes-really-rebuild-the-index", "--ignore-errors", "--verbose"}

    def information_flag(arg):
        if arg == "--help":
            return "help"
        if arg == "--version":
            return "version"
        if arg.startswith("-") and not arg.startswith("--"):
            for letter in arg[1:]:
                if letter in {"g", "e", "k", "q"}:
                    break  # The rest of a value-taking short option is its value.
                if letter == "h":
                    return "help"
                if letter == "V":
                    return "version"
                if letter != "i":
                    break
        return None

    index = 0
    while index < len(argv):
        arg = argv[index]
        if information_flag(arg):
            return information_flag(arg)
        if arg == "--":
            index += 1
            return argv[index] if index < len(argv) and argv[index] in commands else "unknown"
        if arg.split("=", 1)[0] in value_options:
            if "=" not in arg and index + 1 >= len(argv):
                return "unknown"
            index += 1 if "=" in arg else 2
        elif arg in flag_options:
            index += 1
        elif arg.startswith("-"):
            return "unknown"
        else:
            if arg not in commands:
                return "unknown"
            if arg == "help" and "models" in argv[index + 1:]:
                return "models"  # `help models` fetches catalogs, rather than local help.
            # Help/version requests do not execute the named search/map command.
            for trailing in argv[index + 1:]:
                if trailing == "--":
                    break
                if information_flag(trailing):
                    return information_flag(trailing)
            return arg
    return "help"


def slopdex_command_counts(calls) -> dict[str, int]:
    counts = collections.Counter()
    for call in calls:
        argv = call.get("argv") if isinstance(call, dict) else None
        if not isinstance(argv, list) or any(not isinstance(arg, str) for arg in argv):
            raise ValueError("Invalid slopdex invocation: expected a string argv array")
        counts[slopdex_command(argv)] += 1
    return dict(sorted(counts.items()))


def slopdex_call_allowed(arm: str, argv: list[str]) -> bool:
    if arm in ARM_NAVIGATION:
        protected = {"--root", "--config", "--index", "--provider", "--model", "--dimensions",
                     "--description-provider", "--description-model", "--description-fallback-model",
                     "--reranker-candidates", "--force-reindex", "--yes-really-rebuild-the-index",
                     "--rebuild-on-divergence"}
        for arg in argv:
            if arg == "--":
                break
            if arg.split("=", 1)[0] in protected:
                return False
    command_name = slopdex_command(argv)
    return arm == "slopdex" or (arm in ARM_NAVIGATION and command_name in {*ARM_NAVIGATION[arm], "help", "version"})


def read_slopdex_calls(path: Path):
    """Merge invocation start/finish events; preserve historical one-record logs."""
    calls, pending = [], {}
    for line in path.read_text().splitlines():
        if not line.strip():
            continue
        record = json.loads(line)
        event = record.get("event")
        if event == "finish":
            if record["id"] not in pending:
                raise ValueError(f"Slopdex finish event has no start in {path}")
            pending[record["id"]].update({"exit_code": record["exit_code"], "end_time": record["end_time"]})
        else:
            calls.append(record)
            if event == "start":
                pending[record["id"]] = record
    return calls


def slopdex_protocol_violations(arm: str, calls):
    violations = []
    for call in calls:
        if not slopdex_call_allowed(arm, call["argv"]):
            violations.append({"reason": "command_not_allowed", "command": slopdex_command(call["argv"]), "argv": call["argv"]})
    for required in ARM_NAVIGATION.get(arm, ()):
        if not any(slopdex_command(call["argv"]) == required and call.get("exit_code") == 0 for call in calls):
            violations.append({"reason": "required_command_not_used_successfully", "command": required})
    return violations


def saved_slopdex_commands(directory: Path, result: dict) -> dict[str, int]:
    calls = directory / "slopdex-calls.jsonl"
    if calls.exists():
        return slopdex_command_counts(read_slopdex_calls(calls))
    if "slopdex_commands" in result:
        return result["slopdex_commands"]
    # Preserve older count-only artifacts when the original invocation log is absent.
    return {"unknown": result["slopdex_calls"]} if result.get("slopdex_calls") else {}


def score_answer(text: str, task, gold: dict):
    expected_findings = len(task["targets"])
    cleaned = re.sub(r"^```(?:json)?\s*|\s*```$", "", text.strip()).strip()
    try:
        answer = json.loads(cleaned)
        findings = answer["findings"]
        if not isinstance(findings, list) or len(findings) != expected_findings or not isinstance(answer["flow"], str) or not answer["flow"].strip():
            raise ValueError(f"Expected {expected_findings} findings and a nonempty flow explanation")
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


def task_prompt(task) -> str:
    return task["prompt"] + "\n" + ANSWER_FORMAT.replace("{finding_count}", str(len(task["targets"])))


def run_trial(trial, task, gold, config, commit, directory, opencode, slopdex, index_dir, *, instruction=None):
    directory.mkdir(parents=True, exist_ok=True)
    prompt = task_prompt(task)
    (directory / "prompt.txt").write_text(prompt)
    with tempfile.TemporaryDirectory(prefix="slopdex-ts-trial-") as temporary:
        sandbox = Path(temporary)
        workspace = sandbox / "repo"
        clone(workspace, commit)
        instructions = BASE_INSTRUCTIONS + (instruction if instruction is not None else arm_instructions(trial["arm"]))
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
        native_ann = None
        if trial["arm"] in SEMANTIC_ARMS:
            native_ann = verify_native_ann_reuse(slopdex, workspace, index_dir / "config.json", sandbox / "index.sqlite", directory, config["index_timeout_seconds"])
        elif trial["arm"] == "map":
            native_ann = verify_native_ann_reuse(slopdex, workspace, index_dir / "config.json", sandbox / "index.sqlite", directory, config["index_timeout_seconds"], symbol_only=True)
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
            slopdex_calls = read_slopdex_calls(calls)
        command_violations = slopdex_protocol_violations(trial["arm"], slopdex_calls)
        status = "ok"
        if expired:
            status = "timeout"
        elif code or events["errors"]:
            status = "agent_error"
        elif violations or command_violations:
            status = "protocol_violation"
        elif not grade["valid_answer"]:
            status = "invalid_answer"
        if status != "ok":
            grade["f1"], grade["passed"] = 0.0, False
        result = {
            **{key: value for key, value in trial.items() if key != "model_config"},
            **events, "status": status, "exit_code": code, "timed_out": expired,
            "wall_seconds": elapsed, "grade": grade, "workspace_violations": violations,
            "slopdex_protocol_violations": command_violations,
            "slopdex_calls": len(slopdex_calls), "slopdex_commands": slopdex_command_counts(slopdex_calls),
            "native_ann": native_ann,
        }
        write_json(directory / "result.json", result)
    return result


def make_trials(config, tasks, arms, repeats, seed):
    pairs = [(model, task, repeat) for model in config["models"] for task in tasks for repeat in range(1, repeats + 1)]
    random.Random(seed).shuffle(pairs)
    trials = []
    for position, (model, task, repeat) in enumerate(pairs):
        if len(arms) <= 2:
            order = arms if position % 2 == 0 else list(reversed(arms))
        else:
            offset = position % len(arms)
            order = arms[offset:] + arms[:offset]
            if (position // len(arms)) % 2:
                order = list(reversed(order))
        for arm in order:
            key = f"{model['model']}@{model['variant']}:{task['id']}:{repeat}:{arm}"
            trials.append({"id": digest(key)[:16], "task": task["id"], "repeat": repeat,
                           "model": model["model"], "variant": model["variant"], "arm": arm, "model_config": model})
    return trials


def arm_comparisons(arms):
    """Return every selected arm pair in reference/comparison order."""
    selected = set(arms)
    ordered = [arm for arm in ARM_INSTRUCTIONS if arm in selected]
    return [(reference, comparison) for position, reference in enumerate(ordered)
            for comparison in ordered[position + 1:]]


def report(output: Path):
    manifest = read_json(output / "manifest.json")
    results = []
    for trial in manifest["trials"]:
        directory = output / "trials" / trial["id"]
        path = directory / "result.json"
        if not path.exists():
            continue
        result = read_json(path)
        counts = saved_slopdex_commands(directory, result)
        if result.get("slopdex_commands") != counts:
            result["slopdex_commands"] = counts
            write_json(path, result)
        results.append(result)
    groups, pairs = collections.defaultdict(list), collections.defaultdict(dict)
    for result in results:
        groups[(result["model"], result["variant"], result["arm"])].append(result)
        pairs[(result["model"], result["variant"], result["task"], result["repeat"])][result["arm"]] = result
    all_commands = collections.Counter()
    for result in results:
        all_commands.update(result["slopdex_commands"])
    summary = {"completed": len(results), "planned": len(manifest["trials"]), "groups": [], "paired": [],
               "slopdex_commands": dict(sorted(all_commands.items())),
               "trials": [{key: result[key] for key in ("id", "model", "variant", "arm", "task", "repeat", "slopdex_calls", "slopdex_commands")} for result in results]}
    lines = ["# TypeScript paired evaluation", "", f"Commit: `{manifest['commit']}`", "",
             f"Completed {len(results)}/{len(manifest['trials'])} trials. F1 includes failures as zero.", "",
             "| Model | Thinking | Arm | N | Passed | Mean F1 | Mean seconds | Input | Output | Reasoning | Cache read | Mean USD | Slopdex calls |",
             "|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"]
    for (model, variant, arm), rows in sorted(groups.items()):
        costs = [row["cost_usd"] for row in rows if row["cost_usd"] is not None]
        commands, command_trials = collections.Counter(), collections.Counter()
        for row in rows:
            commands.update(row["slopdex_commands"])
            command_trials.update(name for name, count in row["slopdex_commands"].items() if count)
        group = {"model": model, "variant": variant, "arm": arm, "n": len(rows),
                 "passed": sum(row["grade"]["passed"] for row in rows),
                 "mean_f1": statistics.mean(row["grade"]["f1"] for row in rows),
                 "mean_wall_seconds": statistics.mean(row["wall_seconds"] for row in rows),
                 "mean_cost_usd": statistics.mean(costs) if costs else None,
                 "statuses": dict(collections.Counter(row["status"] for row in rows)),
                 "mean_tokens": {name: statistics.mean(row["tokens"].get(name, 0) for row in rows) for name in ("input", "output", "reasoning", "cache_read", "cache_write")},
                 "slopdex_calls": sum(row["slopdex_calls"] for row in rows),
                 "slopdex_commands": dict(sorted(commands.items())),
                 "slopdex_command_trials": dict(sorted(command_trials.items()))}
        summary["groups"].append(group)
        usage = group["mean_tokens"]
        cost = f"{group['mean_cost_usd']:.5f}" if costs else "n/a"
        lines.append(f"| {model} | {variant} | {arm} | {len(rows)} | {group['passed']} | {group['mean_f1']:.3f} | {group['mean_wall_seconds']:.1f} | {usage['input']:.0f} | {usage['output']:.0f} | {usage['reasoning']:.0f} | {usage['cache_read']:.0f} | {cost} | {group['slopdex_calls']} |")
    comparisons = arm_comparisons(trial["arm"] for trial in manifest["trials"])
    differences = collections.defaultdict(list)
    for (model, variant, task, repeat), pair in sorted(pairs.items()):
        for reference_arm, comparison_arm in comparisons:
            if reference_arm not in pair or comparison_arm not in pair:
                continue
            reference, comparison = pair[reference_arm], pair[comparison_arm]
            delta = {"model": model, "variant": variant, "task": task, "repeat": repeat,
                     "reference_arm": reference_arm, "comparison_arm": comparison_arm,
                     "f1_delta": comparison["grade"]["f1"] - reference["grade"]["f1"],
                     "both_ok": reference["status"] == comparison["status"] == "ok"}
            # Time/cost improvements are only interpretable for completed protocol-valid pairs.
            if delta["both_ok"]:
                delta["seconds_delta"] = comparison["wall_seconds"] - reference["wall_seconds"]
                delta["input_tokens_delta"] = comparison["tokens"].get("input", 0) - reference["tokens"].get("input", 0)
                if comparison["cost_usd"] is not None and reference["cost_usd"] is not None:
                    delta["cost_delta_usd"] = comparison["cost_usd"] - reference["cost_usd"]
            summary["paired"].append(delta)
            differences[(model, variant, reference_arm, comparison_arm)].append(delta)
    lines += ["", "## Paired differences (comparison − reference)", "", "Positive F1 favors the comparison arm; negative time/token/cost differences favor the comparison arm.", ""]
    for (model, variant, reference_arm, comparison_arm), rows in sorted(
            differences.items(), key=lambda item: (item[0][:2], comparisons.index(item[0][2:]))):
        completed = [row for row in rows if row["both_ok"]]
        timing = f"{statistics.mean(row['seconds_delta'] for row in completed):+.1f}s" if completed else "n/a"
        lines.append(f"- **{model} / {variant} / {comparison_arm} − {reference_arm}**: {len(rows)} pairs; mean F1 Δ {statistics.mean(row['f1_delta'] for row in rows):+.3f}; mean time Δ {timing} ({len(completed)} valid pairs).")
    lines += ["", "## Per-task pairs (comparison − reference)", "", "| Model | Thinking | Comparison − reference | Task | Repeat | F1 Δ | Both valid | Seconds Δ | Input Δ | USD Δ |", "|---|---|---|---|---:|---:|---|---:|---:|---:|"]
    for row in summary["paired"]:
        lines.append(f"| {row['model']} | {row['variant']} | {row['comparison_arm']} − {row['reference_arm']} | {row['task']} | {row['repeat']} | {row['f1_delta']:+.3f} | {row['both_ok']} | {row.get('seconds_delta', 'n/a')} | {row.get('input_tokens_delta', 'n/a')} | {row.get('cost_delta_usd', 'n/a')} |")
    lines += ["", "## Slopdex command usage", "",
              "Counts are agent invocation attempts recorded by the PATH wrapper. Help/version requests are separate from commands; index preparation and ANN validation are excluded.",
              "Older count-only artifacts without logs are labeled `unknown`.", "",
              "| Command | Total invocations |", "|---|---:|"]
    for name, count in summary["slopdex_commands"].items():
        lines.append(f"| {name} | {count} |")
    lines += ["", "### By model and thinking mode", "",
              "| Model | Thinking | Arm | Command | Invocations | Trials using command |",
              "|---|---|---|---|---:|---:|"]
    for group in summary["groups"]:
        for name, count in group["slopdex_commands"].items():
            lines.append(f"| {group['model']} | {group['variant']} | {group['arm']} | {name} | {count} | {group['slopdex_command_trials'][name]}/{group['n']} |")
    lines += ["", "### By task and trial", "",
              "| Model | Thinking | Arm | Task | Repeat | Invocations | Commands |",
              "|---|---|---|---|---:|---:|---|"]
    for row in summary["trials"]:
        if row["arm"] == "off" and not row["slopdex_calls"]:
            continue
        commands = ", ".join(f"`{name}` × {count}" for name, count in row["slopdex_commands"].items()) or "—"
        lines.append(f"| {row['model']} | {row['variant']} | {row['arm']} | {row['task']} | {row['repeat']} | {row['slopdex_calls']} | {commands} |")
    if manifest.get("index"):
        lines += ["", "## Index preparation", "", f"One-time preparation: {manifest['index']['wall_seconds']:.1f}s (cached across trials).", "Embedding/index preparation costs are not included in OpenCode's reported model cost."]
        native_ann = manifest["index"].get("native_ann")
        if native_ann:
            lines += [f"Native ANN warm-up: {native_ann['wall_seconds']:.1f}s (cached). Search and combined (`map-search` and `slopdex`) trials receive the native binaries and manifests and verify reuse before timing; setup/validation time is excluded from agent wall time."]
        coverage = manifest["index"].get("coverage", {})
        if coverage.get("parser_diagnostics"):
            lines += [f"Parser diagnostics elsewhere in the checkout: {coverage['parser_diagnostics']}; validated searchable task targets: {coverage['validated_targets']}."]
    if manifest.get("map_index"):
        lines += ["", "## Structural-index preparation", "",
                  f"One-time structural-index preparation: {manifest['map_index']['wall_seconds']:.1f}s (cached across map trials).",
                  "Structural-index preparation is excluded from agent wall time and reported model cost."]
    if manifest.get("map_symbol_index"):
        metadata = manifest["map_symbol_index"]
        lines += ["", "## Map symbol-index preparation", "",
                  f"One-time structural preparation: {metadata['wall_seconds']:.1f}s; symbol embedding/ANN preparation: {metadata['native_ann']['wall_seconds']:.1f}s (cached).",
                  "Map trials receive normalized-name vectors and the symbol ANN sidecar, with reuse validated before timing. Preparation and validation are excluded from agent wall time and reported model cost."]
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
    preparation = sub.add_parser("prepare", help="Initialize source and prebuild the selected arm indexes")
    preparation.add_argument("--arms", nargs="+", choices=[arm for arm in ARM_INSTRUCTIONS if arm != "off"], default=DEFAULT_ARMS)
    run = sub.add_parser("run", help="Run paired trials; an existing output directory resumes")
    run.add_argument("--output", type=Path)
    run.add_argument("--task", action="append", help="Task ID; repeat to select several")
    run.add_argument("--model", action="append", help="Configured provider/model@variant; repeat to select several")
    run.add_argument("--arms", nargs="+", choices=list(ARM_INSTRUCTIONS), default=DEFAULT_ARMS)
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
            print(f"\n{task['id']} [{task.get('difficulty', 'intermediate')}, {len(task['targets'])} findings]: {task['title']}\n{task['prompt']}")
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
        instructions_by_arm = {arm: arm_instructions(arm) for arm in args.arms}
        if args.dry_run:
            print(json.dumps({"commit": commit, "trials": trials, "count": len(trials), "arm_instructions": instructions_by_arm}, indent=2))
            return
    if args.action in {"validate", "prepare"}:
        ensure_source(commit)
        gold = {task["id"]: target_lines(task) for task in tasks}
        if args.action == "validate":
            print(f"Validated {len(tasks)} tasks / {sum(map(len, gold.values()))} declarations at {commit}")
        else:
            if len(set(args.arms)) != len(args.arms):
                raise ValueError("Use unique preparation arms")
            slopdex = executable("slopdex")
            if set(args.arms) & STRUCTURAL_MAP_ARMS:
                directory, metadata = prepare_map_index(config, commit, args.cache.resolve(), slopdex, tasks)
                print(f"Structural index ready: {directory}\n{json.dumps(metadata, indent=2)}")
            if "map" in args.arms:
                directory, metadata = prepare_map_index(config, commit, args.cache.resolve(), slopdex, tasks, symbols=True)
                print(f"Map symbol index ready: {directory}\n{json.dumps(metadata, indent=2)}")
            if set(args.arms) & SEMANTIC_ARMS:
                directory, metadata = prepare_index(config, commit, args.cache.resolve(), slopdex, tasks)
                print(f"Semantic index ready: {directory}\n{json.dumps(metadata, indent=2)}")
        return
    output = (args.output or HERE / "jobs" / f"{time.strftime('%Y%m%d-%H%M%S')}-{time.time_ns() % 1000000:06d}").resolve()
    settings = {"config": config, "tasks": tasks, "commit": commit, "trials": trials,
                "arm_instructions": instructions_by_arm,
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
        slopdex = executable("slopdex") if any(arm != "off" for arm in args.arms) else None
        identity = {**settings, "opencode_version": command([opencode, "--version"]),
                    "slopdex_version": command([slopdex, "--version"]) if slopdex else None}
        if saved and saved["fingerprint"] != digest(identity):
            raise ValueError("Resume executable versions differ; use a new --output directory")
        ensure_source(commit)
        gold = {task["id"]: target_lines(task) for task in tasks}
        index_dir, metadata = (None, saved.get("index") if saved else None)
        map_index_dir, map_metadata = (None, saved.get("map_index") if saved else None)
        symbol_index_dir, symbol_metadata = (None, saved.get("map_symbol_index") if saved else None)
        if any(trial["arm"] in SEMANTIC_ARMS for trial in pending):
            index_dir, metadata = prepare_index(config, commit, args.cache.resolve(), slopdex, tasks)
        if any(trial["arm"] in STRUCTURAL_MAP_ARMS for trial in pending):
            map_index_dir, map_metadata = prepare_map_index(config, commit, args.cache.resolve(), slopdex, tasks)
        if any(trial["arm"] == "map" for trial in pending):
            symbol_index_dir, symbol_metadata = prepare_map_index(config, commit, args.cache.resolve(), slopdex, tasks, symbols=True)
        manifest = {**identity, "fingerprint": digest(identity), "index": metadata, "map_index": map_metadata,
                    "map_symbol_index": symbol_metadata, "seed": args.seed}
        write_json(output / "manifest.json", manifest)
        for number, trial in enumerate(trials, 1):
            directory = output / "trials" / trial["id"]
            if (directory / "result.json").exists():
                continue
            print(f"[{number}/{len(trials)}] {trial['model']} / {trial['variant']} / {trial['task']} / {trial['arm']}", flush=True)
            trial_index = symbol_index_dir if trial["arm"] == "map" else map_index_dir if trial["arm"] in STRUCTURAL_MAP_ARMS else index_dir
            result = run_trial(trial, task_by_id[trial["task"]], gold[trial["task"]], config, commit, directory, opencode, slopdex, trial_index,
                               instruction=instructions_by_arm[trial["arm"]])
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
