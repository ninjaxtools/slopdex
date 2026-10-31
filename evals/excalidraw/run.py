#!/usr/bin/env python3
"""Paired, local OpenCode/slopdex navigation evaluation (Python 3.11+, stdlib)."""

from __future__ import annotations

import argparse
import collections
from contextlib import closing, contextmanager
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
import subprocess
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
SUBMODULE = HERE / "repo"
ANSWER_FORMAT = """
Return your final answer as a single JSON object, without Markdown fences:
{"findings": [{"path": "repository/relative/file.ts", "symbol": "unqualifiedFunctionName",
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
    # No arm-specific harness guidance: baseline gets only the base instructions, map
    # only adds the README snippet. Slopdex is still blocked in baseline by permissions.
    "off": "",
    "map": "",
}
DEFAULT_ARMS = ["off", "map"]
ARM_NAVIGATION = {"map": ("map",)}
# No slopdex command is mandatory: trials record which commands the agent chose to use.
ARM_REQUIRED = {}
ARM_OPTIONAL = {"map": ("faq", "refactor")}
# Trials copy the full semantic index (code/docs/descriptions/symbols ANN), which faq needs.
TRIAL_SYMBOL_ONLY = False


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


def arm_instructions(arm: str, *, map_private=None) -> str:
    guidance = ARM_INSTRUCTIONS[arm]
    if arm in ARM_NAVIGATION:
        snippets = "\n\n".join(readme_agent_instruction(command) for command in ARM_NAVIGATION[arm])
        if map_private is not None:
            if map_private:
                if not re.search(r" --private(?=[ \t`]|$)", snippets):
                    snippets = snippets.replace("`slopdex map ", "`slopdex map --private ")
            else:
                snippets = re.sub(r" --private(?=[ \t`]|$)", "", snippets)
            action = "include" if map_private else "omit"
            guidance += f"Required map visibility: {action} --private on every map invocation.\n"
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
    from saved import reject_credentials

    reject_credentials(config)
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
        if task.get("difficulty") == "expert" and (len(targets) < 7 or len({path for path, _ in targets}) < 4):
            raise ValueError(f"Expert task {task['id']} needs at least seven targets across four files")
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
    recorded = command(["git", "ls-files", "--stage", "evals/excalidraw/repo"]).split()
    if len(recorded) < 2 or recorded[0] != "160000" or recorded[1] != commit:
        raise ValueError("Submodule gitlink does not match tasks.repository_commit")
    command(["git", "submodule", "update", "--init", "--depth", "1", "--", "evals/excalidraw/repo"], timeout=600)
    if command(["git", "rev-parse", "HEAD"], cwd=SUBMODULE) != commit:
        raise ValueError("Excalidraw checkout does not match the pinned commit")
    if command(["git", "status", "--porcelain", "--untracked-files=no"], cwd=SUBMODULE):
        raise ValueError("Excalidraw submodule has tracked changes; use a clean checkout")


MODIFIERS = r"(?:(?:export|default|declare|public|private|protected|static|readonly|abstract|override|async|get|set)\s+)*"


def declaration_patterns(symbol: str):
    """TypeScript callable declarations: functions, arrow/function constants, and class members."""
    name = re.escape(symbol)
    generics = r"(?:<[^\n]*>)?"
    return [
        # function name(...) / export default async function* name<T>(...)
        re.compile(rf"^[ \t]*{MODIFIERS}function\s*\*?\s*{name}\s*{generics}\s*\(", re.M),
        # export const name = (...) => / = async <T>(...) => / = function / = memoize(
        re.compile(rf"^[ \t]*{MODIFIERS}(?:const|let|var)\s+{name}\s*(?::[^=\n]+)?=\s*(?:async\s+)?(?:function\b|{generics}\s*\(|[A-Za-z_$][\w$]*\s*=>)", re.M),
        # class member: name(...) {, private name = (...) =>, static async name<T>(
        re.compile(rf"^[ \t]+{MODIFIERS}{name}\s*(?:\?\s*)?(?:{generics}\s*\([^;\n]*(?:\)\s*(?::[^;\n]*)?\{{\s*$|,\s*$|$)|(?::[^=\n]+)?=\s*(?:async\s+)?(?:function\b|{generics}\s*\(|[A-Za-z_$][\w$]*\s*=>))", re.M),
    ]


def declaration_lines(text: str, symbol: str):
    lines, source_lines = set(), text.split("\n")
    for kind, pattern in enumerate(declaration_patterns(symbol)):
        for match in pattern.finditer(text):
            line = text.count("\n", 0, match.start())
            content = source_lines[line]
            if content.lstrip().startswith(("//", "*", "/*")):
                continue
            if kind == 2 and not class_member(source_lines, line):
                continue  # Indented calls/assignments in function bodies are not members.
            lines.add(line + 1)
    return sorted(lines)


def class_member(source_lines, line: int) -> bool:
    indent = len(source_lines[line]) - len(source_lines[line].lstrip())
    for previous in reversed(source_lines[:line]):
        stripped = previous.lstrip()
        if stripped and len(previous) - len(stripped) < indent:
            return re.match(rf"{MODIFIERS}class\b", stripped) is not None
    return False


def target_lines(task, source=SUBMODULE):
    result = {}
    for target in task["targets"]:
        text = (source / target["path"]).read_text()
        lines = declaration_lines(text, target["symbol"])
        if len(lines) != 1:
            raise ValueError(f"Expected one declaration for {target}, found {len(lines)} at lines {lines}")
        result[(target["path"], target["symbol"])] = lines[0]
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


@contextmanager
def open_index(index: Path):
    """Read workspace state and its bound schema-4 artifact store without writes."""
    with closing(sqlite3.connect(f"{index.as_uri()}?mode=ro", uri=True)) as db:
        binding = db.execute("SELECT value FROM metadata WHERE key='global_path'").fetchone()
        if binding:
            db.execute("ATTACH DATABASE ? AS global", (Path(binding[0]).as_uri() + "?mode=ro",))
        yield db


def index_config(config_path: Path, index: Path):
    settings = read_json(config_path)
    with open_index(index) as db:
        binding = db.execute("SELECT value FROM metadata WHERE key='global_path'").fetchone()
    if binding:
        settings["artifactCachePath"] = binding[0]
    return settings


def index_coverage(index: Path, tasks, *, semantic=True):
    """Permit parser recovery elsewhere, but require searchable vectors for the task."""
    with open_index(index) as db:
        shared_store = db.execute("SELECT value FROM metadata WHERE key='global_path'").fetchone() is not None
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
            content = ("SELECT c.value, u.embedding_input_hash, u.content_hash FROM symbols s " if shared_store else
                       "SELECT u.data, u.embedding_input_hash, NULL FROM symbols s ")
            rows = db.execute(
                content +
                "JOIN search_units u ON u.path=s.path AND u.symbol_id=s.id "
                + ("JOIN global.cache c ON c.kind='unit-content' AND c.key=u.content_hash " if shared_store else "") +
                "WHERE s.path=? AND s.name=? AND u.kind='function'",
                (target["path"], target["symbol"]),
            ).fetchall()
            available = False
            for data, input_hash, content_hash in rows:
                if content_hash is not None and hashlib.sha256(data.encode()).hexdigest() != content_hash:
                    continue
                embedding_input = json.loads(data).get("embeddingInput")
                if not isinstance(embedding_input, str) or hashlib.sha256(embedding_input.encode()).hexdigest() != input_hash:
                    continue
                # Read durable artifacts by profile/input, including after
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


def implementation_identity(binary: str):
    """Version alone cannot distinguish same-version or uncommitted builds."""
    paths = command(["git", "ls-files", "--cached", "--others", "--exclude-standard", "--",
                     "Cargo.toml", "Cargo.lock", "build.rs", ".cargo", "src", "native"]).splitlines()
    files = {path: file_digest(ROOT / path) if (ROOT / path).is_file() else None for path in sorted(set(paths))}
    return {"commit": command(["git", "rev-parse", "HEAD"]),
            "source_sha256": digest(files), "binary_sha256": file_digest(Path(binary)),
            "dirty": bool(command(["git", "status", "--porcelain", "--", "Cargo.toml", "Cargo.lock", ".cargo", "src"]))}


def shared_ann_snapshot(index: Path, metadata: dict, *, symbol_only):
    """Capture workspace pointers and every referenced immutable native base."""
    artifacts, shared = {}, {}
    directory = Path(metadata["global_path"] + ".indexes")
    profile = None if symbol_only else json.loads(metadata["active_embedding_profile"])
    for kind in (["symbols"] if symbol_only else ["code", "docs", "descriptions", "symbols"]):
        suffix = f".{kind}.shared.json"
        path = Path(str(index) + suffix)
        pointer = read_json(path)
        if (not isinstance(pointer, dict) or pointer.get("version") != 1
            or any(not isinstance(pointer.get(key), str) or not re.fullmatch(r"[0-9a-f]{64}", pointer[key])
                   for key in ("contract", "base_id", "fingerprint"))
            or pointer["fingerprint"] != metadata.get(f"index_state_{kind}")):
            raise ValueError(f"Shared ANN pointer is stale or corrupt: {path}")
        base_path = directory / pointer["contract"] / pointer["base_id"]
        base = read_json(Path(str(base_path) + ".base.json"))
        binary = Path(str(base_path) + ".usearch")
        native = read_json(Path(str(binary) + ".manifest.json"))
        if not isinstance(base, dict) or not isinstance(native, dict):
            raise ValueError(f"Shared ANN base must have object manifests: {base_path}")
        dimensions = base.get("dimensions")
        if (base.get("version") != 1 or base.get("id") != pointer["base_id"] or base.get("contract") != pointer["contract"]
            or not isinstance(dimensions, int) or dimensions <= 0
            or (profile is not None and kind != "symbols" and dimensions != profile["dimensions"])
            or native.get("version") != 1 or native.get("dimensions") != dimensions
            or native.get("binary_hash") != file_digest(binary)):
            raise ValueError(f"Shared ANN base is stale or corrupt: {base_path}")
        artifacts[suffix] = {"sha256": file_digest(path), "size_bytes": path.stat().st_size}
        for extension in (".base.json", ".usearch", ".usearch.manifest.json"):
            path = Path(str(base_path) + extension)
            shared[str(path.relative_to(directory))] = {"sha256": file_digest(path), "size_bytes": path.stat().st_size}
    return {"schema_version": 2, "generation": int(metadata["generation"]), "embedding_profile": profile,
            "descriptions_enabled": not symbol_only, "artifacts": artifacts, "shared_artifacts": shared}


def native_ann_paths(index: Path, snapshot: dict):
    paths = {suffix: Path(str(index) + suffix) for suffix in snapshot["artifacts"]}
    if snapshot.get("shared_artifacts"):
        with open_index(index) as db:
            binding = db.execute("SELECT value FROM metadata WHERE key='global_path'").fetchone()[0]
        paths.update({"shared/" + name: Path(binding + ".indexes") / name for name in snapshot["shared_artifacts"]})
    return paths


def native_ann_snapshot(index: Path, *, symbol_only=False):
    """Validate the native sidecars against their binaries and SQLite identity."""
    with closing(sqlite3.connect(f"{index.as_uri()}?mode=ro", uri=True)) as db:
        metadata = dict(db.execute("SELECT key, value FROM metadata"))
    if json.loads(metadata["identity"]).get("schema") == 4:
        return shared_ann_snapshot(index, metadata, symbol_only=symbol_only)
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
        actual = native_ann_snapshot(index, symbol_only=symbol_only)
        if actual["schema_version"] == 2 and saved.get("warmup_version") != 2:
            return False
        return {key: value for key, value in saved.items() if key not in {"wall_seconds", "warmup_version"}} == actual
    except (OSError, ValueError, KeyError, sqlite3.Error):
        return False


def offline_ann_command(binary: str, workspace: Path, config_path: Path, index: Path, *, symbol_only=False):
    # Preembed the warm-up query once; subsequent probes reuse it offline. Search
    # explicitly opens all content/description channels, even for empty groups.
    navigation = (["map", "--private", "-q", "slopdex eval warmup", "--symbol-threshold", "0", "-g", "*", "-g", "!*"] if symbol_only else
                  ["search", "slopdex eval warmup", "--code", "--docs", "--descriptions", "--threshold", "1", "--limit", "1"])
    return [binary, "--root", str(workspace), "--config", str(config_path), "--index", str(index),
            "--no-reindex", "--output", "json", *navigation]


def warm_native_ann(directory: Path, metadata: dict, binary: str, workspace: Path, timeout: int, *, symbol_only=False):
    index = directory / "index.sqlite"
    with closing(sqlite3.connect(index)) as db, db:
        relocate_index(db, workspace, metadata["identity"]["commit"])
    logs = directory / "ann-warmup"
    logs.mkdir(exist_ok=True)
    elapsed = 0.0
    # Content search warms code/docs/descriptions; symbol vectors are built lazily by map -q.
    for mode in ([True] if symbol_only else [False, True]):
        argv = offline_ann_command(binary, workspace, directory / "config.json", index, symbol_only=mode)
        code, expired, seconds = logged_process(argv, workspace, os.environ.copy(), logs, timeout)
        elapsed += seconds
        if code or expired:
            raise RuntimeError(f"Native ANN warm-up failed; see {logs / 'stderr.log'}")
    metadata["native_ann"] = {**native_ann_snapshot(index, symbol_only=symbol_only), "wall_seconds": elapsed, "warmup_version": 2}
    metadata["status"]["rootDir"] = str(workspace.resolve())
    write_json(directory / "manifest.json", metadata)
    return metadata


def prepare_map_index(config, commit: str, cache: Path, slopdex: str, tasks, *, implementation=None):
    """Build the full semantic index (code, docs, symbols) so map -q and faq work offline-indexed."""
    settings = {**config["slopdex"], "rerankingEnabled": False}
    identity = {"kind": "semantic", "commit": commit, "slopdex_version": command([slopdex, "--version"]), "config": settings}
    if implementation is not None:
        identity["implementation"] = implementation
    directory = cache / digest(identity)[:20]
    directory.mkdir(parents=True, exist_ok=True)
    index, manifest = directory / "index.sqlite", directory / "manifest.json"
    import fcntl
    with (directory / "prepare.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        metadata = read_json(manifest) if manifest.exists() else {}
        if not index.exists() or metadata.get("identity") != identity:
            write_json(directory / "config.json", {**settings, "artifactCachePath": str(directory / "artifacts.sqlite")})
            with tempfile.TemporaryDirectory(prefix="slopdex-eval-index-") as temporary:
                workspace = Path(temporary) / "repo"
                clone(workspace, commit)
                argv = [slopdex, "--root", str(workspace), "--config", str(directory / "config.json"), "--index", str(index)]
                retry = ["--force-reindex", "--yes-really-rebuild-the-index"] if index.exists() else []
                code, expired, elapsed = logged_process(argv + retry + ["update"], workspace, os.environ.copy(), directory, config["index_timeout_seconds"])
                if code or expired:
                    raise RuntimeError(f"Semantic index preparation failed; see {directory / 'stderr.log'}")
                status = json.loads(command(argv + ["--no-reindex", "status"], cwd=workspace))
            metadata = {"identity": identity, "wall_seconds": elapsed, "status": status}
            write_json(manifest, metadata)
        coverage = index_coverage(index, tasks, semantic=True)
        if not native_ann_ready(index, metadata):
            metadata = warm_native_ann(directory, metadata, slopdex, SUBMODULE, config["index_timeout_seconds"])
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


def eval_key_variable(provider: str) -> str:
    return re.sub(r"[^A-Z0-9]", "_", provider.upper()) + "_EVALS_KEY"


def isolated_environment(sandbox: Path, config: dict):
    env = os.environ.copy()
    # Copy only the selected provider's auth, not sessions or account/organization
    # configuration. Never put credentials in persistent harness artifacts.
    home = Path.home()
    old_data_home = Path(env.get("XDG_DATA_HOME", str(home / ".local/share")))
    data_home = sandbox / "data"
    auth_path = old_data_home / "opencode" / "auth.json"
    provider = config["model"].split("/", 1)[0]
    # A dedicated eval key (e.g. OPENCODE_GO_EVALS_KEY) takes precedence over stored auth.
    eval_key = env.get(eval_key_variable(provider))
    if eval_key or auth_path.exists():
        credentials = {"type": "api", "key": eval_key} if eval_key else read_json(auth_path).get(provider)
        if credentials:
            if credentials.get("type") == "wellknown":
                raise ValueError("Remote wellknown auth configuration is incompatible with isolated evals; supply an API key")
            new_auth = data_home / "opencode" / "auth.json"
            write_json(new_auth, {provider: credentials})
            new_auth.chmod(0o600)
    for key in tuple(env):
        if (key.startswith("OPENCODE_") and key not in {"OPENCODE_API_KEY", "OPENCODE_GO_API_KEY"}) or key.endswith("_EVALS_KEY"):
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
    if identity.get("schema") not in {3, 4} or checkpoint != (commit,):
        raise ValueError("Cached slopdex index has an unsupported schema or wrong commit; rebuild the cache")
    if identity["schema"] == 4 and not db.execute("SELECT value FROM metadata WHERE key='global_path'").fetchone():
        raise ValueError("Cached slopdex index has no artifact store binding; rebuild the cache")
    identity["root"] = str(workspace.resolve())
    db.execute("UPDATE metadata SET value=? WHERE key='identity'", (json.dumps(identity),))


def copy_index(source_path: Path, destination: Path, workspace: Path, commit: str, *, include_ann=True, symbol_only=False):
    """Relocate SQLite and its warmed ANN sidecars without changing vector identity."""
    native_ann = native_ann_snapshot(source_path, symbol_only=symbol_only) if include_ann else None
    with closing(sqlite3.connect(f"{source_path.as_uri()}?mode=ro", uri=True)) as source:
        with closing(sqlite3.connect(destination)) as target, target:
            source.backup(target)
            relocate_index(target, workspace, commit)
            binding = source.execute("SELECT value FROM metadata WHERE key='global_path'").fetchone()
            if binding:
                artifacts = Path(str(destination) + ".artifacts.sqlite")
                with closing(sqlite3.connect(Path(binding[0]).as_uri() + "?mode=ro", uri=True)) as store:
                    with closing(sqlite3.connect(artifacts)) as copied_store:
                        store.backup(copied_store)
                target.execute("UPDATE metadata SET value=? WHERE key='global_path'", (str(artifacts.resolve()),))
    if native_ann is not None:
        for suffix in native_ann["artifacts"]:
            shutil.copy2(Path(str(source_path) + suffix), Path(str(destination) + suffix))
        if native_ann.get("shared_artifacts"):
            source_files = native_ann_paths(source_path, native_ann)
            target_files = native_ann_paths(destination, native_ann)
            for key in source_files:
                if key.startswith("shared/"):
                    target_files[key].parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(source_files[key], target_files[key])
        if native_ann_snapshot(destination, symbol_only=symbol_only) != native_ann:
            raise ValueError("Copied ANN sidecars do not match the trial SQLite snapshot")


def verify_native_ann_reuse(binary: str, workspace: Path, config_path: Path, index: Path, directory: Path, timeout: int, *, symbol_only=False):
    """Prove the writable native loader accepts copied sidecars before agent timing."""
    before = native_ann_snapshot(index, symbol_only=symbol_only)
    stamps = {key: path.stat().st_mtime_ns for key, path in native_ann_paths(index, before).items()}
    logs = directory / "ann-validation"
    logs.mkdir(exist_ok=True)
    offline_config = logs / "config.json"
    settings = index_config(config_path, index)
    argv = offline_ann_command(binary, workspace, offline_config, index, symbol_only=symbol_only)
    env = os.environ.copy()
    symbol_query = None
    symbol_manifest = None
    with closing(sqlite3.connect(index)) as db, db:
        db.execute("DELETE FROM search_cache")
    for key in ("embeddingApiKey", "openaiApiKey", "jinaApiKey"):
        settings.pop(key, None)
    for key in ("OPENAI_API_KEY", "JINA_API_KEY"):
        env.pop(key, None)
    write_json(offline_config, settings)
    if symbol_only:
        if before["schema_version"] == 1:
            symbol_manifest = read_json(Path(str(index) + ".symbols.usearch.manifest.json"))
        # Force the actual map -q path through the graph rather than a cached
        # selector result. This is the trial's disposable copy, not the cache.
        with open_index(index) as db:
            embedding_count = db.execute("SELECT count(*) FROM embeddings").fetchone()[0]
        # With the root already relocated, force only selects the writable loader.
        # Any graph rebuild is observable in the sidecars. A structural refresh
        # may advance generation while retaining the identical vocabulary/graph;
        # that only updates manifest generation/fingerprint, not the binary.
        argv = [binary, "--root", str(workspace), "--config", str(offline_config), "--index", str(index),
                "--force-reindex", "--yes-really-rebuild-the-index", "--ignore-errors", "--output", "json", "map", "--private", "-k", "fns",
                "-q", "slopdex eval warmup", "--symbol-threshold", "0", "-g", "*", "-g", "!*"]
        symbol_query = {"command": "map -q", "provider_credentials_removed": True,
                        "selector_cache_cleared": True, "embedding_count": embedding_count}
    code, expired, elapsed = logged_process(argv, workspace, env, logs, timeout)
    if code or expired:
        raise RuntimeError(f"Trial ANN validation failed; see {logs / 'stderr.log'}")
    after = native_ann_snapshot(index, symbol_only=symbol_only)
    after_stamps = {key: path.stat().st_mtime_ns for key, path in native_ann_paths(index, after).items()}
    unchanged = after == before and after_stamps == stamps
    if symbol_only and before["schema_version"] == 2:
        unchanged = ({key: value for key, value in after.items() if key != "generation"}
                     == {key: value for key, value in before.items() if key != "generation"} and after_stamps == stamps)
        symbol_query["manifest_generation_updated"] = after["generation"] != before["generation"]
    elif symbol_only:
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
        with open_index(index) as db:
            after_count = db.execute("SELECT count(*) FROM embeddings").fetchone()[0]
        if after_count != embedding_count or "external model call" in (logs / "stderr.log").read_text():
            raise ValueError("Map -q did not reuse the prebuilt symbol vectors offline")
    result = {"validated": True, "wall_seconds": elapsed, "snapshot": after}
    if symbol_query is not None:
        result["symbol_query"] = symbol_query
    write_json(directory / "ann-validation.json", result)
    return result


def install_faq_agent(sandbox: Path, opencode: str) -> tuple[Path, Path]:
    """An `opencode`-named shim for slopdex's agentCommand that records nested faq usage.

    Slopdex appends `--model` for executables named opencode; the shim adds JSON events,
    returns the final answer text, and journals tokens/cost so trials can count them.
    """
    directory = sandbox / "faq-agent"
    directory.mkdir()
    log = sandbox / "faq-usage.jsonl"
    script = f"""#!{sys.executable}
import collections, json, subprocess, sys, tempfile, time
from pathlib import Path
{inspect.getsource(parse_events)}
argv = sys.argv[1:]
position = argv.index('--') if '--' in argv else len(argv)
argv = argv[:position] + ['--format', 'json'] + argv[position:]
started = time.time()
with tempfile.TemporaryDirectory() as temporary:
    events_path = Path(temporary) / 'events.jsonl'
    with events_path.open('w') as stream:
        code = subprocess.run([{opencode!r}, *argv], stdin=sys.stdin, stdout=stream).returncode
    events = parse_events(events_path)
with open({str(log)!r}, 'a') as stream:
    stream.write(json.dumps({{'exit_code': code, 'seconds': time.time() - started, 'tokens': events['tokens'],
                             'cost_usd': events['cost_usd'], 'steps': events['steps'], 'errors': len(events['errors'])}}) + '\\n')
sys.stdout.write(events['answer_text'])
sys.exit(code or (0 if events['answer_text'].strip() else 1))
"""
    shim = directory / "opencode"
    shim.write_text(script)
    shim.chmod(0o755)
    return shim, log


def faq_usage(log: Path):
    """Sum nested faq model calls; any call without cost telemetry makes the cost unknown."""
    calls = [json.loads(line) for line in log.read_text().splitlines() if line.strip()] if log.exists() else []
    tokens = collections.Counter()
    for call in calls:
        tokens.update({name: value for name, value in (call.get("tokens") or {}).items() if isinstance(value, (int, float))})
    costs = [call.get("cost_usd") for call in calls]
    return {"calls": len(calls), "seconds": sum(call.get("seconds", 0) for call in calls),
            "steps": sum(call.get("steps", 0) for call in calls), "tokens": dict(tokens),
            "cost_usd": None if any(cost is None for cost in costs) else sum(costs)}


def install_wrapper(sandbox: Path, workspace: Path, arm: str, binary: str | None, index_dir: Path | None, commit: str, *, map_private=None, agent_model=None, agent_command=None):
    bin_dir = sandbox / "bin"
    bin_dir.mkdir()
    log = sandbox / "slopdex-calls.jsonl"
    arguments = []
    if arm != "off":
        # SQLite backup includes any WAL pages; each trial gets an independent index.
        destination = sandbox / "index.sqlite"
        copy_index(index_dir / "index.sqlite", destination, workspace, commit, symbol_only=TRIAL_SYMBOL_ONLY)
        config_path = sandbox / "slopdex.json"
        settings = index_config(index_dir / "config.json", destination)
        # faq answers with the trial's own model through the sandboxed OpenCode.
        settings["agentCommand"] = agent_command or ["opencode", "run"]
        if agent_model:
            settings["agentModel"] = agent_model
        write_json(config_path, settings)
        arguments = [binary, "--root", str(workspace), "--config", str(config_path), "--index", str(destination), "--no-reindex"]
    script = f"""#!{sys.executable}
import json, subprocess, sys, time, uuid
ARM_NAVIGATION = {ARM_NAVIGATION!r}
ARM_OPTIONAL = {ARM_OPTIONAL!r}
{inspect.getsource(slopdex_command)}
{inspect.getsource(slopdex_call_allowed)}
call_id = uuid.uuid4().hex
command = slopdex_command(sys.argv[1:])
allowed = slopdex_call_allowed({arm!r}, sys.argv[1:], map_private={map_private!r})
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
        "descriptions", "reindex-files", "models", "config", "help", "faq", "refactor",
    }
    value_options = {
        "--root", "--config", "--index", "--provider", "--model", "--dimensions",
        "--description-provider", "--agent-command", "--agent-model", "--description-fallback-model",
        "--reranker-candidates", "--output", "--detail", "--expand-code-threshold",
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


def slopdex_call_allowed(arm: str, argv: list[str], *, map_private=None) -> bool:
    if arm in ARM_NAVIGATION:
        protected = {"--root", "--config", "--index", "--provider", "--model", "--dimensions",
                     "--description-provider", "--agent-command", "--agent-model", "--description-fallback-model",
                     "--reranker-candidates", "--force-reindex", "--yes-really-rebuild-the-index",
                     "--rebuild-on-divergence"}
        for arg in argv:
            if arg == "--":
                break
            if arg.split("=", 1)[0] in protected:
                return False
    command_name = slopdex_command(argv)
    if command_name == "map" and map_private is not None:
        options = argv
        if "--" in options:
            separator = options.index("--")
            # `-- map ...` names the command after the separator; after `map --`, everything is positional.
            command_after = "map" not in options[:separator] and options[separator + 1:separator + 2] == ["map"]
            options = options[separator + 1:] if command_after else options[:separator]
            if "--" in options:
                options = options[:options.index("--")]
        if ("--private" in options) != map_private:
            return False
    return arm in ARM_NAVIGATION and command_name in {*ARM_NAVIGATION[arm], *ARM_OPTIONAL.get(arm, ()), "help", "version"}


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


def slopdex_protocol_violations(arm: str, calls, *, map_private=None):
    violations = []
    for call in calls:
        if not slopdex_call_allowed(arm, call["argv"], map_private=map_private):
            violations.append({"reason": "command_not_allowed", "command": slopdex_command(call["argv"]), "argv": call["argv"]})
    for required in ARM_REQUIRED.get(arm, ()):
        if not any(slopdex_command(call["argv"]) == required and call.get("exit_code") == 0
                   and slopdex_call_allowed(arm, call["argv"], map_private=map_private) for call in calls):
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


def run_trial(trial, task, gold, config, commit, directory, opencode, slopdex, index_dir, *, instruction=None, map_private=None):
    directory.mkdir(parents=True, exist_ok=True)
    prompt = task_prompt(task)
    (directory / "prompt.txt").write_text(prompt)
    with tempfile.TemporaryDirectory(prefix="slopdex-eval-trial-") as temporary:
        sandbox = Path(temporary)
        workspace = sandbox / "repo"
        clone(workspace, commit)
        instructions = BASE_INSTRUCTIONS + (instruction if instruction is not None else arm_instructions(trial["arm"], map_private=map_private))
        (workspace / "AGENTS.md").write_text(instructions)
        (directory / "AGENTS.md").write_text(instructions)
        cli_config = opencode_config(trial["model_config"], trial["arm"], config["max_steps"])
        # Project-config discovery is disabled, so load arm guidance explicitly.
        cli_config["instructions"] = [str(workspace / "AGENTS.md")]
        write_json(directory / "opencode.json", cli_config)
        env = isolated_environment(sandbox, cli_config)
        env["PWD"] = str(workspace)
        faq_shim, faq_log = install_faq_agent(sandbox, opencode)
        bin_dir, calls = install_wrapper(sandbox, workspace, trial["arm"], slopdex, index_dir, commit, map_private=map_private,
                                         agent_model=trial["model"], agent_command=[str(faq_shim), "run"])
        env["PATH"] = str(bin_dir) + os.pathsep + env.get("PATH", "")
        native_ann = None
        if trial["arm"] == "map":
            native_ann = verify_native_ann_reuse(slopdex, workspace, index_dir / "config.json", sandbox / "index.sqlite", directory, config["index_timeout_seconds"], symbol_only=TRIAL_SYMBOL_ONLY)
        check_effective_config(opencode, workspace, env, cli_config, directory)
        argv = [opencode, "run", "--model", trial["model"], "--variant", trial["variant"],
                "--agent", "build", "--format", "json", "--dir", str(workspace), "--title", trial["id"], "--", prompt]
        write_json(directory / "command.json", argv)
        code, expired, elapsed = logged_process(argv, workspace, env, directory, config["timeout_seconds"])
        events = parse_events(directory / "stdout.jsonl")
        # Nested faq model calls are part of the arm's spend: fold them into the totals.
        faq = faq_usage(faq_log)
        if faq_log.exists():
            shutil.copyfile(faq_log, directory / "faq-usage.jsonl")
        events["agent_tokens"], events["agent_cost_usd"] = dict(events["tokens"]), events["cost_usd"]
        if faq["calls"]:
            events["tokens"] = {name: events["tokens"].get(name, 0) + faq["tokens"].get(name, 0)
                                for name in {*events["tokens"], *faq["tokens"]}}
            events["cost_usd"] = (None if events["cost_usd"] is None or faq["cost_usd"] is None
                                  else events["cost_usd"] + faq["cost_usd"])
        events["faq_usage"] = faq
        (directory / "answer.txt").write_text(events.pop("answer_text"))
        grade = score_answer((directory / "answer.txt").read_text(), task, gold)
        changes = command(["git", "diff", "--name-only", "HEAD"], cwd=workspace)
        untracked = command(["git", "ls-files", "--others", "--exclude-standard"], cwd=workspace).splitlines()
        violations = [path for path in changes.splitlines() + untracked if path != "AGENTS.md"]
        slopdex_calls = []
        if calls.exists():
            shutil.copyfile(calls, directory / "slopdex-calls.jsonl")
            slopdex_calls = read_slopdex_calls(calls)
        command_violations = slopdex_protocol_violations(trial["arm"], slopdex_calls, map_private=map_private)
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
    if not set(arms) <= set(DEFAULT_ARMS):
        raise ValueError("Only baseline (off) and README map arms are supported")
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
    from report import report as render

    return render(output)


def main(argv=None):
    from saved import compare_saved, load_records, restore_record, save_bundle, trial_controls

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=HERE / "config.json")
    parser.add_argument("--cache", type=Path, default=HERE / "cache")
    parser.add_argument("--results", type=Path, default=HERE / "results", help="Versioned baseline and latest-map records")
    sub = parser.add_subparsers(dest="action", required=True)
    sub.add_parser("list", help="List task prompts and default models without network access")
    sub.add_parser("validate", help="Initialize source and verify every gold declaration")
    sub.add_parser("prepare", help="Initialize source and prebuild the README map index")
    run = sub.add_parser("run", help="Reuse matching saved results and run only changed/missing trials")
    run.add_argument("--output", type=Path)
    run.add_argument("--task", action="append", help="Task ID; repeat to select several")
    run.add_argument("--model", action="append", help="Configured provider/model@variant; repeat to select several")
    run.add_argument("--arms", nargs="+", choices=list(ARM_INSTRUCTIONS), default=DEFAULT_ARMS)
    run.add_argument("--repeats", type=int, default=1)
    run.add_argument("--seed", type=int, default=0)
    run.add_argument("--map-private", action=argparse.BooleanOptionalAction, default=None,
                     help="Require --private on map calls, or forbid it with --no-map-private (default: README guidance)")
    run.add_argument("--dry-run", action="store_true", help="Print trial matrix without network, indexing or model calls")
    save_parser = sub.add_parser("save", help="Import a completed job into versioned baseline/latest-map records")
    save_parser.add_argument("output", type=Path)
    report_parser = sub.add_parser("report", help="Compare versioned results, or regenerate a job report")
    report_parser.add_argument("output", nargs="?", type=Path)
    args = parser.parse_args(argv)
    results = args.results.resolve()
    if args.action == "report":
        print(json.dumps(report(args.output.resolve()) if args.output else compare_saved(results), indent=2))
        return
    if args.action == "save":
        import fcntl
        results.mkdir(parents=True, exist_ok=True)
        with (results / "results.lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            saved_any = False
            for arm in DEFAULT_ARMS:
                path = save_bundle(args.output.resolve(), results, arm)
                if path:
                    saved_any = True
                    print(f"Saved {arm}: {path}")
            if not saved_any:
                raise ValueError("Job has no complete baseline/map dataset to save")
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
        if args.map_private is not None and "map" not in args.arms:
            raise ValueError("Map visibility requires the map arm")
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
        instructions_by_arm = {arm: arm_instructions(arm, map_private=args.map_private) for arm in args.arms}
        if args.dry_run:
            print(json.dumps({"commit": commit, "trials": trials, "count": len(trials), "arm_instructions": instructions_by_arm}, indent=2))
            return
    if args.action in {"validate", "prepare"}:
        ensure_source(commit)
        gold = {task["id"]: target_lines(task) for task in tasks}
        if args.action == "validate":
            print(f"Validated {len(tasks)} tasks / {sum(map(len, gold.values()))} declarations at {commit}")
        else:
            slopdex = executable("slopdex")
            directory, metadata = prepare_map_index(config, commit, args.cache.resolve(), slopdex, tasks,
                                                    implementation=implementation_identity(slopdex))
            print(f"Semantic index ready: {directory}\n{json.dumps(metadata, indent=2)}")
        return
    output = (args.output or HERE / "jobs" / f"{time.strftime('%Y%m%d-%H%M%S')}-{time.time_ns() % 1000000:06d}").resolve()
    settings = {"config": config, "tasks": tasks, "commit": commit, "trials": trials,
                 "arm_instructions": instructions_by_arm,
                 "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    if args.map_private is not None:
        settings["map_private"] = args.map_private
    task_by_id = {task["id"]: task for task in tasks}
    # One runner owns an output directory at a time; interrupted runs can be resumed.
    import fcntl
    output.mkdir(parents=True, exist_ok=True)
    results.mkdir(parents=True, exist_ok=True)
    with (output / "run.lock").open("w") as lock, (results / "results.lock").open("w") as result_lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fcntl.flock(result_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        saved = None
        if (output / "manifest.json").exists():
            saved = read_json(output / "manifest.json")
            if any(saved.get(key) != value for key, value in settings.items()):
                raise ValueError("Resume settings differ from the saved manifest; use a new --output directory")
        opencode = executable("opencode")
        slopdex = executable("slopdex") if "map" in args.arms else None
        identity = {**settings, "opencode_version": command([opencode, "--version"]),
                    "slopdex_version": command([slopdex, "--version"]) if slopdex else None,
                    "implementation": implementation_identity(slopdex) if slopdex else None}
        if saved and saved["fingerprint"] != digest(identity):
            raise ValueError("Resume executable/implementation identity differs; use a new --output directory")
        reusable = load_records(results)
        reused = dict(saved.get("reused_trials", {})) if saved else {}
        for trial in trials:
            directory = output / "trials" / trial["id"]
            if (directory / "result.json").exists():
                continue
            controls = trial_controls(identity, trial, prompt=task_prompt(task_by_id[trial["task"]]),
                                      instructions=BASE_INSTRUCTIONS + instructions_by_arm[trial["arm"]])
            match = reusable.get((trial["arm"], digest(controls)))
            if match:
                record, source = match
                restore_record(record, directory)
                reused[trial["id"]] = str(source.relative_to(results))
        pending = [trial for trial in trials if not (output / "trials" / trial["id"] / "result.json").exists()]
        print(f"Reusing {len(reused)} saved trials; {len(pending)} require execution.", flush=True)
        gold = {}
        if pending:
            ensure_source(commit)
            gold = {task["id"]: target_lines(task) for task in tasks}
        symbol_index_dir, symbol_metadata = (None, saved.get("map_symbol_index") if saved else None)
        if any(trial["arm"] == "map" for trial in pending):
            symbol_index_dir, symbol_metadata = prepare_map_index(config, commit, args.cache.resolve(), slopdex, tasks,
                                                                  implementation=identity["implementation"])
        manifest = {**identity, "fingerprint": digest(identity), "map_symbol_index": symbol_metadata, "seed": args.seed,
                    "reused_trials": reused,
                    "comparison_mode": "saved baseline/map (not interleaved)" if reused else "interleaved"}
        write_json(output / "manifest.json", manifest)
        for number, trial in enumerate(trials, 1):
            directory = output / "trials" / trial["id"]
            if (directory / "result.json").exists():
                continue
            print(f"[{number}/{len(trials)}] {trial['model']} / {trial['variant']} / {trial['task']} / {trial['arm']}", flush=True)
            result = run_trial(trial, task_by_id[trial["task"]], gold[trial["task"]], config, commit, directory, opencode, slopdex, symbol_index_dir,
                               instruction=instructions_by_arm[trial["arm"]],
                               **({"map_private": args.map_private} if args.map_private is not None else {}))
            print(f"  {result['status']}: F1={result['grade']['f1']:.3f}, {result['wall_seconds']:.1f}s", flush=True)
            report(output)
        report(output)
        for arm in args.arms:
            save_bundle(output, results, arm)
        if set(args.arms) == set(DEFAULT_ARMS):
            compare_saved(results)
    print(f"Report: {output / 'report.md'}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError, subprocess.TimeoutExpired) as error:
        print(f"eval: {error}", file=sys.stderr)
        sys.exit(1)
