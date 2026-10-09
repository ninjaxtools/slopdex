#!/usr/bin/env python3
"""Offline map-prompt efficiency report (Python 3.11+, standard library).

Usage: python3 evals/typescript/map_prompt_report.py JOB_DIR [--require-complete]
Writes only metrics.json, efficiency.md and efficiency-audit.json in JOB_DIR.
Never imports the runner, runs commands, contacts providers, or rebuilds indexes.
--require-complete still writes the snapshot report, then exits 2 if results are
missing/unreadable. Concurrent runner activity is recorded, not attributed to
this reader. Re-run on a quiescent job for a fully stable preservation audit.

Reported tokens/cost are provider accounting, not estimated billing. Input
presentations include repeated context across steps. Source coverage measures
exposure, not comprehension or explanation correctness; review answers manually.
"""

from __future__ import annotations

import argparse
from collections import Counter, defaultdict
from contextlib import closing
import hashlib
import json
import math
from pathlib import Path
import re
import shlex
import sqlite3
import statistics
import sys


OUTPUTS = {"metrics.json", "efficiency.md", "efficiency-audit.json"}
INPUTS = {"manifest.json", "result.json", "stdout.jsonl", "answer.txt",
          "slopdex-calls.jsonl", "AGENTS.md", "prompt.txt", "opencode.json",
          "verified-config.json", "command.json"}
TOKEN_NAMES = ("input", "cache_read", "cache_write", "output", "reasoning", "total")
SEMANTIC_OR_INDEX = {"search", "search-code", "search-descriptions", "search-md",
                     "cross-search", "describe", "descriptions", "update", "refresh",
                     "reindex-files", "models"}
VALUE_OPTIONS = {"--root", "--config", "--index", "--provider", "--model",
                 "--dimensions", "--description-provider", "--description-model",
                 "--description-fallback-model", "--reranker-candidates", "--output",
                 "--detail", "--expand-code-threshold"}
COMMANDS = SEMANTIC_OR_INDEX | {"map", "status", "index-errors", "config", "help"}
BASE_INSTRUCTIONS = ("# Evaluation workspace\n\nInvestigate this checkout using local tools and cite source evidence. Do not modify\n"
                     "files, fetch external material, or delegate. Return the requested JSON answer.\n")


def sha(data):
    return hashlib.sha256(data).hexdigest()


def digest(value):
    return sha(json.dumps(value, sort_keys=True).encode())


def obj(value):
    return value if isinstance(value, dict) else {}


def array(value):
    return value if isinstance(value, list) else []


def contains(actual, intended):
    """Verified configs intentionally contain only the runner's critical subset."""
    if isinstance(intended, dict):
        return isinstance(actual, dict) and all(key in actual and contains(actual[key], value) for key, value in intended.items())
    return actual == intended


def number(value):
    return value if type(value) in (int, float) and math.isfinite(value) else None


def average(values):
    values = [value for value in values if number(value) is not None]
    return statistics.mean(values) if values else None


def relative_source(path):
    """Normalize saved sandbox paths without matching 'slopdex' in workspace names."""
    if not isinstance(path, str):
        return ""
    path = path.replace("\\", "/")
    if "/repo/" in path:
        path = path.split("/repo/", 1)[1]
    return path.removeprefix("./")


def snapshot(job, capture=False):
    hashes, data, errors = {}, {}, []
    for path in sorted(job.rglob("*")):
        relative = path.relative_to(job).as_posix()
        if relative in OUTPUTS:
            continue
        try:
            if path.is_symlink():
                hashes[relative] = {"symlink": str(path.readlink())}
            elif path.is_file():
                # Hash all originals, but retain only report inputs in memory.
                if capture and path.name in INPUTS:
                    content = path.read_bytes()
                    hashes[relative] = sha(content)
                    data[relative] = content
                else:
                    with path.open("rb") as stream:
                        hashes[relative] = hashlib.file_digest(stream, "sha256").hexdigest()
        except OSError as error:
            errors.append({"path": relative, "error": str(error)})
    return hashes, data, errors


class Saved:
    def __init__(self, data):
        self.data = data
        self.warnings = []

    def text(self, path):
        content = self.data.get(path)
        return content.decode("utf-8", errors="replace") if content is not None else None

    def json(self, path):
        text = self.text(path)
        if text is None:
            return None
        try:
            return json.loads(text)
        except (ValueError, RecursionError) as error:
            self.warnings.append({"path": path, "error": str(error)})
            return None

    def events(self, path):
        text = self.text(path)
        if text is None:
            return [], {"present": False, "malformed_lines": 0}
        records, malformed = [], 0
        for line in text.splitlines():
            if not line.strip():
                continue
            try:
                record = json.loads(line)
                if not isinstance(record, dict):
                    raise ValueError("Expected object")
                records.append(record)
            except (ValueError, RecursionError):
                malformed += 1
        return records, {"present": True, "malformed_lines": malformed}


def command_kind(argv):
    """Offline equivalent of the runner's command classification; argv excludes executable."""
    if not isinstance(argv, list) or any(not isinstance(arg, str) for arg in argv):
        return "unknown"

    def information(arg):
        if arg in ("--help", "--version"):
            return "help" if arg == "--help" else "version"
        if arg.startswith("-") and not arg.startswith("--"):
            for letter in arg[1:]:
                if letter in "gek":
                    break
                if letter in "hV":
                    return "help" if letter == "h" else "version"
        return None

    i = 0
    while i < len(argv):
        arg = argv[i]
        if information(arg):
            return information(arg)
        if arg == "--":
            return argv[i + 1] if i + 1 < len(argv) and argv[i + 1] in COMMANDS else "unknown"
        if arg.split("=", 1)[0] in VALUE_OPTIONS:
            i += 1 if "=" in arg else 2
        elif arg in {"--no-reindex", "--force-reindex", "--rebuild-on-divergence",
                     "--yes-really-rebuild-the-index", "--ignore-errors", "--verbose"}:
            i += 1
        elif arg.startswith("-"):
            return "unknown"
        else:
            if arg not in COMMANDS:
                return "unknown"
            if arg == "help" and "models" in argv[i + 1:]:
                return "models"
            for trailing in argv[i + 1:]:
                if trailing == "--":
                    break
                if information(trailing):
                    return information(trailing)
            return arg
    return "help"


def journal(saved, prefix, result):
    records, info = saved.events(prefix + "slopdex-calls.jsonl")
    calls, pending, orphan = [], {}, 0
    for record in records:
        if record.get("event") == "finish":
            key = record.get("id")
            if isinstance(key, str) and key in pending:
                pending[key].update({name: record.get(name) for name in ("exit_code", "end_time")})
            else:
                orphan += 1
        else:
            call = dict(record)
            call["kind"] = command_kind(call.get("argv"))
            calls.append(call)
            key = record.get("id")
            if record.get("event") == "start" and isinstance(key, str):
                pending[key] = call
    info["orphan_finishes"] = orphan
    counts = Counter(call["kind"] for call in calls)
    exits = Counter("unfinished" if call.get("exit_code") is None else str(call["exit_code"]) for call in calls)
    # Missing journals are not evidence of zero invocations, except saved zero-count results.
    fallback = obj(result.get("slopdex_commands")) if not info["present"] else None
    return {**info, "calls": calls, "counts": dict(counts), "exit_statuses": dict(exits),
            "saved_result_counts_if_journal_missing": fallback,
            "saved_result_total": result.get("slopdex_calls")}


def journal_absence_check(info, forbidden):
    """An incomplete/unclassifiable journal cannot prove absence."""
    if any(call["kind"] in forbidden for call in info["calls"]):
        return False
    if info["present"]:
        return True if not info["malformed_lines"] and not info["orphan_finishes"] and not info["counts"].get("unknown") else None
    return True if info["saved_result_total"] == 0 else None


def map_options(argv):
    values = {"kinds": [], "globs": [], "expressions": []}
    aliases = {"-k": "kinds", "--kind": "kinds", "--kinds": "kinds",
               "-g": "globs", "--glob": "globs", "-e": "expressions", "--expression": "expressions"}
    if not isinstance(argv, list):
        argv = []
    i = 0
    while i < len(argv):
        arg = argv[i]
        if not isinstance(arg, str):
            i += 1
            continue
        key, sep, value = arg.partition("=")
        if key in aliases:
            if not sep and i + 1 < len(argv):
                i += 1
                value = argv[i]
            values[aliases[key]].append(value)
        elif arg[:2] in ("-k", "-g", "-e") and len(arg) > 2:
            values[aliases[arg[:2]]].append(arg[2:])
        i += 1
    return {**values, "private": "--private" in argv, "ignore_errors": "--ignore-errors" in argv,
            "test_exclusion_heuristic": any(isinstance(g, str) and g.startswith("!") and
                                            ("_test.go" in g or "/test" in g) for g in values["globs"]),
            "unrestricted_expression_heuristic": any(e in (".*", "^.*$", ".+") for e in values["expressions"])}


def shell_segments(command):
    """Tokenize for inspection only. Deliberately not a shell interpreter."""
    try:
        lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|()<>")
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        return []
    segments, current = [], []
    for token in tokens:
        if token and all(c in ";&|()< >" for c in token):
            if current:
                segments.append(current)
            current = []
        else:
            current.append(token)
    if current:
        segments.append(current)
    return segments


def shell_maps(segments):
    # Match executable position only: paths containing /slopdex/ are not commands.
    return [segment[1:] for segment in segments if segment and Path(segment[0]).name == "slopdex"]


def saved_lines(output):
    content = re.search(r"<content>\n?(.*?)</content>", output, re.S)
    text = content.group(1) if content else output
    return {int(match[1]): match[2] for match in re.finditer(r"^(\d+): (.*)$", text, re.M)}


def read_evidence(tool, state, segments):
    """Return only source text actually saved, never an inferred requested range."""
    inputs, metadata = obj(state.get("input")), obj(state.get("metadata"))
    output = state.get("output") if isinstance(state.get("output"), str) else ""
    if state.get("status") != "completed" or number(metadata.get("exit")) not in (None, 0):
        return None
    if tool == "read":
        path = relative_source(inputs.get("filePath"))
        if not path.endswith(".go") or "<type>directory</type>" in output:
            return None
        lines = saved_lines(output)
        method = "saved_read_output_line_numbers"
        if not lines:
            display = obj(metadata.get("display"))
            start = display.get("lineStart")
            if display.get("type") == "file" and type(start) is int and isinstance(display.get("text"), str):
                # Only use display text when it is actually the saved unnumbered output.
                text = display["text"]
                if output.strip() == text.strip():
                    lines = {start + i: line for i, line in enumerate(text.splitlines())}
                    method = "saved_read_display_range"
        if not lines:
            return None
        return {"path": path, "lines": lines, "method": method,
                "requested_offset": inputs.get("offset", 1), "requested_limit": inputs.get("limit"),
                "truncated": metadata.get("truncated")}
    if tool != "bash" or len(segments) != 1:
        return None
    argv = segments[0]
    if not argv or any("$" in arg or "`" in arg or "\n" in arg for arg in argv):
        return None
    name = Path(argv[0]).name
    path, start, maximum = None, 1, None
    # Exactly one literal file, no redirection/pipelines/compound output.
    if name == "sed" and len(argv) == 4 and argv[1] == "-n":
        match = re.fullmatch(r"(\d+)(?:,(\d+|\$))?p", argv[2])
        if match:
            start = int(match[1])
            end = match[2] or match[1]
            maximum = None if end == "$" else int(end) - start + 1
            path = argv[3]
    elif name == "cat" and len(argv) == 2:
        path = argv[1]
    elif name == "head" and len(argv) in (2, 4):
        if len(argv) == 2:
            path, maximum = argv[1], 10
        elif argv[1] == "-n" and argv[2].isdigit():
            path, maximum = argv[3], int(argv[2])
    if not path or not path.endswith(".go") or re.search(r"[*?\[\]]", path):
        return None
    if not output.strip() or re.search(r"(?:permission denied|no such file|<bash_metadata>|output truncated)", output, re.I):
        return None
    rows = output.splitlines()
    if maximum is not None:
        rows = rows[:max(0, maximum)]
    return {"path": relative_source(path), "lines": {start + i: line for i, line in enumerate(rows)},
            "method": "heuristic_simple_shell_source_read", "requested_offset": start,
            "requested_limit": maximum, "truncated": metadata.get("truncated")}


def parse_trace(saved, prefix):
    events, info = saved.events(prefix + "stdout.jsonl")
    steps, tools, messages, starts = {}, {}, {}, {}
    first_timestamp, finished, ordinal, malformed_parts = None, 0, 0, 0
    for event in events:
        timestamp = number(event.get("timestamp"))
        if timestamp is not None:
            first_timestamp = timestamp if first_timestamp is None else min(first_timestamp, timestamp)
        kind, part = event.get("type"), event.get("part")
        if kind in ("tool_use", "step_finish", "step_start", "text") and not isinstance(part, dict):
            malformed_parts += 1
            continue
        part = obj(part)
        message = part.get("messageID")
        message = message if isinstance(message, str) else None
        if kind == "step_start":
            if isinstance(message, str) and message not in starts:
                starts[message] = len(starts) + 1
        elif kind == "step_finish":
            key = part.get("id") or message or f"line-{ordinal}"
            if not isinstance(key, str):
                key = f"line-{ordinal}"
            steps[key] = part
            finished = len(steps)
        elif kind == "text" and isinstance(part.get("text"), str):
            key = message if isinstance(message, str) else "unknown"
            messages[key] = messages.get(key, "") + part["text"]
        elif kind == "tool_use":
            if not isinstance(part.get("state"), dict):
                malformed_parts += 1
                continue
            key = part.get("callID") or part.get("id") or f"line-{ordinal}"
            if not isinstance(key, str):
                key = f"line-{ordinal}"
            if key not in tools:
                tools[key] = {"ordinal": len(tools) + 1, "step": starts.get(message, finished + 1),
                              "completed_steps_before": finished}
            tools[key].update({"part": part, "timestamp": timestamp})
        ordinal += 1
    tokens = Counter()
    presentations, costs = [], []
    missing_presentation_steps = 0
    for part in steps.values():
        usage, cache = obj(part.get("tokens")), obj(obj(part.get("tokens")).get("cache"))
        per_step = {}
        for name in TOKEN_NAMES:
            value = cache.get(name.removeprefix("cache_")) if name.startswith("cache_") else usage.get(name)
            per_step[name] = number(value) or 0
            tokens[name] += per_step[name]
        if number(usage.get("input")) is not None and all(value is None or number(value) is not None for value in (cache.get("read"), cache.get("write"))):
            presentations.append(sum(per_step[name] for name in ("input", "cache_read", "cache_write")))
        else:
            missing_presentation_steps += 1
        if number(part.get("cost")) is not None:
            costs.append(part["cost"])
    outputs, bash_outputs, kinds, reads, queries = Counter(), Counter(), Counter(), [], Counter()
    probes, map_tools, grep_inventory = Counter(), [], 0
    mixed_calls = 0
    for call in tools.values():
        part, state = call["part"], obj(call["part"].get("state"))
        tool = part.get("tool") if isinstance(part.get("tool"), str) else "unknown"
        kinds[tool] += 1
        inputs = obj(state.get("input"))
        output = state.get("output") if isinstance(state.get("output"), str) else ""
        outputs[tool] += len(output)
        segments = shell_segments(inputs.get("command", "")) if tool == "bash" and isinstance(inputs.get("command", ""), str) else []
        maps = [argv for argv in shell_maps(segments) if command_kind(argv) == "map"]
        source = read_evidence(tool, state, segments)
        shell_source_mentioned = any(segment and Path(segment[0]).name in {"sed", "cat", "head", "tail", "nl", "awk", "python", "python3"}
                                     and any(arg.endswith(".go") for arg in segment[1:]) for segment in segments)
        if tool == "bash":
            category = "mixed_map_source" if maps and shell_source_mentioned else "mixed_map_shell" if maps and len(segments) > 1 else "map" if maps else "source_read" if source else "other_shell"
            bash_outputs[category] += len(output)
            if category.startswith("mixed_"):
                mixed_calls += 1
        if maps:
            map_tools.append({"tool_ordinal": call["ordinal"], "timestamp_ms": number(obj(state.get("time")).get("start")) or call["timestamp"],
                              "mixed": len(segments) > 1, "argv": maps})
            for argv in maps:
                queries[("map", json.dumps([relative_source(arg) for arg in argv]))] += 1
        if tool == "grep":
            pattern = inputs.get("pattern", "")
            queries[("grep", json.dumps({key: relative_source(value) if isinstance(value, str) else value for key, value in inputs.items()}, sort_keys=True))] += 1
            if isinstance(pattern, str) and re.search(r"\bfunc\b|\^func|\b(?:type|const|var)\b", pattern):
                grep_inventory += 1
        for segment in segments:
            name = Path(segment[0]).name
            if name == "git":
                probes["git"] += 1
            if name in {"ls", "pwd", "find", "tree"}:
                probes["layout"] += 1
            if name == "rg" and "--files" in segment:
                probes["layout"] += 1
            if "--help" in segment or "-h" in segment or name == "slopdex" and command_kind(segment[1:]) == "help":
                probes["help"] += 1
            if name in {"rg", "grep"}:
                queries[("shell_grep", json.dumps([relative_source(arg) for arg in segment]))] += 1
                if any(re.search(r"\bfunc\b|\^func", arg) for arg in segment[1:]):
                    grep_inventory += 1
        if tool == "glob" or tool == "read" and "<type>directory</type>" in output:
            probes["layout"] += 1
        elif tool == "read" and Path(relative_source(inputs.get("filePath"))).name in {"go.work", "go.mod", "package.json", "AGENTS.md", "README.md"}:
            probes["layout"] += 1
        if source:
            source.update({"tool": tool, "tool_ordinal": call["ordinal"], "step": call["step"],
                           "completed_steps_before": call["completed_steps_before"],
                           "timestamp_ms": number(obj(state.get("time")).get("start")) or call["timestamp"]})
            reads.append(source)
    reads.sort(key=lambda read: (read["timestamp_ms"] if read["timestamp_ms"] is not None else math.inf, read["tool_ordinal"]))
    first = reads[0] if reads else None
    first_map = min(map_tools, key=lambda call: call["tool_ordinal"]) if map_tools else None
    before = first_map["tool_ordinal"] < first["tool_ordinal"] if first_map and first and not first_map["mixed"] else None
    after = sum(call["tool_ordinal"] >= first["tool_ordinal"] for call in map_tools) if first else 0
    info["malformed_parts"] = malformed_parts
    info["steps_missing_input_accounting"] = missing_presentation_steps
    first_seconds = (first["timestamp_ms"] - first_timestamp) / 1000 if first and first["timestamp_ms"] is not None and first_timestamp is not None else None
    discovery = {"first_map_before_first_successful_source_read": before,
                 "map_seen_without_successful_source_read": bool(map_tools) and not reads,
                 "map_order_evidence": "saved_bash_tool_order_heuristic",
                 "first_successful_source_read": {key: first[key] for key in ("path", "tool_ordinal", "step", "completed_steps_before", "timestamp_ms")} if first else None,
                 "steps_to_first_source_read": first["step"] if first else None,
                 "tools_to_first_source_read": first["tool_ordinal"] if first else None,
                 "seconds_to_first_source_read_from_first_event": first_seconds,
                 "probes_heuristic": dict(probes), "maps_after_or_at_first_source_tool": after,
                 "late_maps_after_last_source_tool": sum(call["tool_ordinal"] > max(read["tool_ordinal"] for read in reads) for call in map_tools) if reads else 0,
                 "declaration_inventory_calls_heuristic": grep_inventory,
                 "repeated_queries_heuristic": [{"kind": key[0], "query": key[1], "count": count, "extra_calls": count - 1} for key, count in sorted(queries.items()) if count > 1]}
    return {"trace": info, "steps": len(steps) if info["present"] else None,
            "tools": dict(kinds), "tool_calls": sum(kinds.values()) if info["present"] else None,
            "tokens": {name: tokens[name] for name in TOKEN_NAMES},
            "cost_usd": sum(costs) if costs and len(costs) == len(steps) else None,
            "input_presentations": sum(presentations) if presentations and not missing_presentation_steps else None,
            "peak_step_input_presentations": max(presentations) if presentations and not missing_presentation_steps else None,
            "saved_tool_output_chars_by_kind": dict(outputs), "saved_bash_output_chars_by_kind": dict(bash_outputs),
            "mixed_shell_map_calls": mixed_calls, "source_reads": reads, "discovery": discovery,
            "answer_text": next(reversed(messages.values()), "")}


def journal_discovery(discovery, journal_info, reads):
    """Prefer wrapper timestamps: multiple maps in one bash call are distinct calls."""
    maps = [call for call in journal_info["calls"] if call["kind"] == "map"]
    timed = [call for call in maps if number(call.get("time")) is not None]
    successful = [call for call in timed if call.get("exit_code") == 0]
    first = min((read["timestamp_ms"] / 1000 for read in reads if read["timestamp_ms"] is not None), default=None)
    last = max((read["timestamp_ms"] / 1000 for read in reads if read["timestamp_ms"] is not None), default=None)
    discovery["first_successful_map_before_first_successful_source_read"] = None
    discovery["journal_map_calls_without_order_timestamps"] = len(maps) - len(timed)
    if first is not None and timed:
        discovery["first_map_before_first_successful_source_read"] = min(call["time"] for call in timed) < first
        discovery["first_successful_map_before_first_successful_source_read"] = min(call["time"] for call in successful) < first if successful else None
        discovery["maps_after_or_at_first_source_tool"] = sum(call["time"] >= first for call in timed)
        discovery["late_maps_after_last_source_tool"] = sum(call["time"] > last for call in timed)
        discovery["map_order_evidence"] = "wrapper_journal_start_times_vs_saved_tool_start_times"
    elif journal_info["present"] and not maps and first is not None:
        discovery["first_map_before_first_successful_source_read"] = False
        discovery["map_order_evidence"] = "wrapper_journal_no_maps"
    discovery["map_seen_without_successful_source_read"] = bool(maps) and not reads if journal_info["present"] else discovery["map_seen_without_successful_source_read"]


def structural_gold(manifest):
    gold, audit = {}, {"used": False, "method": "unavailable", "read_only": True}
    metadata = obj(manifest.get("map_index"))
    path = obj(metadata.get("status")).get("indexPath")
    if not isinstance(path, str) or not Path(path).is_file():
        audit["reason"] = "Saved structural cache path is absent; use saved grade declaration lines."
        return gold, audit
    path = Path(path).resolve()
    audit["path"] = str(path)
    try:
        # Immutable read-only prevents SQLite sidecars/writes. Reject a nonempty WAL,
        # because immutable mode would ignore pending pages and give stale evidence.
        wal = Path(str(path) + "-wal")
        audit["wal_bytes"] = wal.stat().st_size if wal.exists() else 0
        if audit["wal_bytes"]:
            raise ValueError("Nonempty WAL present; decline immutable cache read")
        with path.open("rb") as stream:
            audit["sha256_before"] = hashlib.file_digest(stream, "sha256").hexdigest()
        with closing(sqlite3.connect(path.as_uri() + "?mode=ro&immutable=1", uri=True)) as db:
            checkpoint = db.execute("SELECT value FROM metadata WHERE key='checkpoint'").fetchone()
            if checkpoint != (manifest.get("commit"),):
                raise ValueError("Structural checkpoint differs from manifest commit")
            audit["checkpoint"] = checkpoint[0]
            for task in array(manifest.get("tasks")):
                for target in array(obj(task).get("targets")):
                    rows = db.execute("SELECT start_line,end_line,signature FROM symbols WHERE path=? AND name=? AND kind IN ('function','method')",
                                      (target["path"], target["symbol"])).fetchall()
                    if len(rows) == 1:
                        start, end, signature = rows[0]
                        gold[(target["path"], target["symbol"])] = {"line": start, "end_line": end, "signature": signature, "method": "cached_structural_sqlite"}
            audit["embedding_rows"] = db.execute("SELECT count(*) FROM embeddings").fetchone()[0]
            audit["unit_embedding_rows"] = db.execute("SELECT count(*) FROM unit_embeddings").fetchone()[0]
        with path.open("rb") as stream:
            audit["sha256_after"] = hashlib.file_digest(stream, "sha256").hexdigest()
        unchanged = audit["sha256_before"] == audit["sha256_after"] and (not wal.exists() or wal.stat().st_size == 0)
        audit.update({"used": unchanged, "method": "immutable_read_only_sqlite", "unchanged": unchanged, "declarations": len(gold)})
        if not unchanged:
            gold.clear()
            audit["reason"] = "Cache changed during read; discard structural evidence"
    except (OSError, sqlite3.Error, ValueError, KeyError, TypeError) as error:
        gold.clear()
        audit["reason"] = str(error)
    return gold, audit


def citation_score(text, task, gold):
    """Runner's JSON/schema and citation rules, without status/protocol zeroing.

    Only the same optional outer Markdown fence is removed; no broken-JSON salvage.
    """
    cleaned = re.sub(r"^```(?:json)?\s*|\s*```$", "", text.strip()).strip()
    try:
        answer = json.loads(cleaned)
        findings = answer["findings"]
        if not isinstance(findings, list) or len(findings) != len(task["targets"]) or not isinstance(answer["flow"], str) or not answer["flow"].strip():
            raise ValueError("Expected target-count findings and nonempty flow")
        for finding in findings:
            if (not isinstance(finding, dict) or not isinstance(finding.get("path"), str)
                or not isinstance(finding.get("symbol"), str) or type(finding.get("line")) is not int
                or finding["line"] <= 0 or not isinstance(finding.get("explanation"), str) or not finding["explanation"].strip()):
                raise ValueError("Invalid finding schema")
    except (ValueError, KeyError, TypeError, RecursionError) as error:
        return {"valid_answer": False, "f1": 0.0, "passed": False, "error": str(error), "method": "rescored_saved_answer"}
    if any((target["path"], target["symbol"]) not in gold for target in task["targets"]):
        return {"valid_answer": True, "f1": None, "passed": None, "method": "gold_lines_unavailable"}
    cited, details = set(), []
    for finding in findings:
        key = (finding["path"].removeprefix("./"), finding["symbol"])
        expected = gold.get(key, {}).get("line")
        valid = expected is not None and expected <= finding["line"] <= expected + 2
        if valid:
            cited.add(key)
        details.append({"path": key[0], "symbol": key[1], "citation_valid": valid, "expected_line": expected})
    recall, precision = len(cited) / len(task["targets"]), len(cited) / len(findings)
    return {"valid_answer": True, "f1": 2 * recall * precision / (recall + precision) if recall + precision else 0.0,
            "precision": precision, "recall": recall, "passed": len(cited) == len(task["targets"]),
            "matches": details, "method": "rescored_saved_answer"}


def source_coverage(task, gold, reads):
    coverage = []
    for target in task.get("targets", []):
        record = gold.get((target["path"], target["symbol"]), {})
        start, end = record.get("line"), record.get("end_line")
        selected = [read for read in reads if read["path"] == target["path"]]
        all_lines = {line: text for read in selected for line, text in read["lines"].items()}
        signature_end, body_lines = None, []
        # Cached signatures are whitespace-normalized, so their newline count
        # cannot identify a multiline signature's end. Use saved contiguous lines.
        if start in all_lines:
            parens = brackets = 0
            for line in range(start, min(end or start + 40, start + 40) + 1):
                if line not in all_lines:
                    break
                text = all_lines[line].split("//", 1)[0]
                parens += text.count("(") - text.count(")")
                brackets += text.count("[") - text.count("]")
                if "{" in text and parens == brackets == 0:
                    signature_end = line
                    break
        if signature_end is not None:
            stop = end if end is not None else signature_end + 20
            for line, text in sorted(all_lines.items()):
                if signature_end < line <= stop:
                    stripped = text.strip()
                    if stripped.startswith("func "):
                        break
                    if stripped and not stripped.startswith(("//", "/*", "*", "}")) and stripped not in ("{", ")", ");"):
                        body_lines.append(line)
        evidence = []
        for read in selected:
            lines = read["lines"]
            overlap = [line for line in lines if start is not None and start <= line <= (end or start + 20)]
            if overlap:
                evidence.append({"tool": read["tool"], "tool_ordinal": read["tool_ordinal"], "method": read["method"],
                                 "output_line_start": min(lines), "output_line_end": max(lines),
                                 "requested_offset": read["requested_offset"], "requested_limit": read["requested_limit"],
                                 "overlap_lines": len(overlap), "body_lines_exposed_heuristic": sum(line in body_lines for line in overlap)})
        coverage.append({**target, "declaration_line": start, "end_line": end, "gold_method": record.get("method", "unavailable"),
                         "declaration_exposed": start in all_lines if start is not None else None,
                         "signature_end_line_heuristic": signature_end,
                         "implementation_beyond_signature_exposed_heuristic": bool(body_lines),
                         "body_lines_exposed_heuristic": len(body_lines), "read_evidence": evidence,
                         "review": "Review saved source and explanation; brace/comment heuristic is not a Go parser." if body_lines else
                         "Manually review: no conservative body evidence (missing declaration, signature-only, unsupported shell, or no read)."})
    return coverage


def trial_audit(saved, prefix, trial, result, manifest):
    checks, unknown = {}, []
    guidance = obj(manifest.get("arm_instructions")).get(trial["arm"])
    agents = saved.text(prefix + "AGENTS.md")
    checks["frozen_instructions_match"] = agents == BASE_INSTRUCTIONS + guidance if agents is not None and isinstance(guidance, str) else None
    task = next((task for task in array(manifest.get("tasks")) if obj(task).get("id") == trial["task"]), {})
    prompt = saved.text(prefix + "prompt.txt")
    checks["frozen_task_prompt_prefix_match"] = prompt.startswith(task["prompt"] + "\n") if prompt is not None and isinstance(task.get("prompt"), str) else None
    checks["result_identity_match"] = all(result.get(key) == trial.get(key) for key in ("id", "model", "variant", "arm", "task", "repeat")) if result else None
    checks["workspace_no_edits_reported"] = result.get("workspace_violations") == [] if result and "workspace_violations" in result else None
    config = saved.json(prefix + "opencode.json")
    verified = saved.json(prefix + "verified-config.json")
    if isinstance(config, dict):
        build = obj(obj(config.get("agent")).get("build"))
        permission = obj(config.get("permission"))
        controls = {"share": "disabled", "autoupdate": False, "snapshot": False, "lsp": False, "formatter": False, "plugin": []}
        checks["config_controls_match"] = all(config.get(key) == value for key, value in controls.items()) and all(permission.get(key) == "deny" for key in ("edit", "task", "question", "webfetch", "websearch", "skill", "external_directory")) and build.get("permission") == permission and build.get("steps") == obj(manifest.get("config")).get("max_steps")
        checks["config_model_match"] = config.get("model") == config.get("small_model") == trial["model"]
        provider, _, model = trial["model"].partition("/")
        options = obj(obj(obj(obj(config.get("provider")).get(provider)).get("models")).get(model)).get("variants")
        checks["model_options_match"] = obj(options).get(trial["variant"]) == obj(trial.get("model_config")).get("options")
        instructions = config.get("instructions")
        checks["single_saved_workspace_instruction"] = isinstance(instructions, list) and len(instructions) == 1 and isinstance(instructions[0], str) and instructions[0].endswith("/repo/AGENTS.md")
        if trial["arm"] == "off":
            checks["baseline_bash_deny_patterns_match"] = permission.get("bash") == {"*": "allow", "*slopdex *": "deny", "*slopdex": "deny"}
        expected_verified = {key: config.get(key) for key in ("model", "small_model", "share", "autoupdate", "snapshot", "lsp", "formatter", "permission", "instructions", "provider")}
        expected_verified["agent"] = {"build": build}
        checks["verified_config_matches_generated"] = contains(verified, expected_verified) if isinstance(verified, dict) and verified else None
    else:
        unknown.append("generated/verified OpenCode config unavailable")
    checks["trial_model_config_frozen"] = trial.get("model_config") in obj(manifest.get("config")).get("models", []) if "model_config" in trial else None
    command = saved.json(prefix + "command.json")
    if isinstance(command, list):
        checks["command_model_variant_match"] = all(flag in command and command.index(flag) + 1 < len(command) and command[command.index(flag) + 1] == trial[key] for flag, key in (("--model", "model"), ("--variant", "variant")))
    else:
        checks["command_model_variant_match"] = None
    return {"id": trial["id"], "checks": checks, "unknown": unknown,
            "agents_sha256": sha(agents.encode()) if agents is not None else None,
            "prompt_sha256": sha(prompt.encode()) if prompt is not None else None}


PAIR_METRICS = ("steps", "tool_calls", "input_presentations", "peak_step_input_presentations", "source_context_chars",
                "saved_tool_output_chars", "cost_usd", "wall_seconds", "strict_f1", "citation_f1")


def paired(rows):
    by_case, cases, groups = defaultdict(dict), [], defaultdict(list)
    for row in rows:
        if row["completed"]:
            by_case[(row["model"], row["variant"], row["task"], row["repeat"])][row["arm"]] = row
    for (model, variant, task, repeat), arms in sorted(by_case.items()):
        if "off" not in arms:
            continue
        baseline = arms["off"]
        for arm, comparison in sorted(arms.items()):
            if arm == "off":
                continue
            case = {"model": model, "variant": variant, "task": task, "repeat": repeat, "arm": arm,
                    "reference_id": baseline["id"], "comparison_id": comparison["id"],
                    "both_status_ok": baseline["status"] == comparison["status"] == "ok",
                    "both_valid_answer": baseline["valid_answer"] is True and comparison["valid_answer"] is True,
                    "reference_status": baseline["status"], "comparison_status": comparison["status"], "metrics": {}}
            for metric in PAIR_METRICS:
                reference, value = baseline.get(metric), comparison.get(metric)
                case["metrics"][metric] = {"off": reference, "comparison": value,
                                           "delta": value - reference if number(reference) is not None and number(value) is not None else None,
                                           "ratio": value / reference if number(reference) is not None and reference > 0 and number(value) is not None else None}
            cases.append(case)
            groups[(model, variant, arm)].append(case)
    summaries = []
    for (model, variant, arm), cases_in_group in sorted(groups.items()):
        cohorts = {}
        for name, selected in (("all_available", cases_in_group),
                               ("both_status_ok", [case for case in cases_in_group if case["both_status_ok"]]),
                               ("both_valid_answer", [case for case in cases_in_group if case["both_valid_answer"]]),
                               ("both_status_ok_and_valid_answer", [case for case in cases_in_group if case["both_status_ok"] and case["both_valid_answer"]])):
            metrics = {}
            for metric in PAIR_METRICS:
                values = [case["metrics"][metric] for case in selected]
                ratios = [value["ratio"] for value in values if value["ratio"] is not None]
                mean_ratio = average(ratios)
                metrics[metric] = {"n": sum(value["delta"] is not None for value in values),
                                   "mean_delta": average(value["delta"] for value in values), "ratio_n": len(ratios),
                                   "mean_case_ratio": mean_ratio,
                                   "mean_ratio_effect_pct": (mean_ratio - 1) * 100 if mean_ratio is not None else None}
            cohorts[name] = {"n": len(selected), "metrics": metrics}
        summaries.append({"model": model, "variant": variant, "arm": arm, "reference_arm": "off", "cohorts": cohorts})
    return cases, summaries


def aggregate(rows):
    groups = defaultdict(list)
    for row in rows:
        groups[(row["model"], row["variant"], row["arm"])].append(row)
    result = []
    for (model, variant, arm), planned in sorted(groups.items()):
        completed = [row for row in planned if row["completed"]]
        means = {metric: {"n": sum(number(row.get(metric)) is not None for row in completed), "mean": average(row.get(metric) for row in completed)} for metric in PAIR_METRICS}
        discovery, outputs, bash, commands, exits, source_calls, tokens, probes = Counter(), Counter(), Counter(), Counter(), Counter(), Counter(), Counter(), Counter()
        for row in completed:
            outputs.update(row["saved_tool_output_chars_by_kind"])
            bash.update(row["saved_bash_output_chars_by_kind"])
            commands.update(row["journal"]["counts"])
            exits.update(row["journal"]["exit_statuses"])
            source_calls.update(row["successful_source_reads_by_kind"])
            tokens.update({key: value for key, value in row["tokens"].items() if number(value) is not None})
            d = row["discovery"]
            probes.update(d["probes_heuristic"])
            discovery["map_before_source_true"] += d["first_map_before_first_successful_source_read"] is True
            discovery["map_before_source_evaluable"] += d["first_map_before_first_successful_source_read"] is not None
            discovery["successful_map_before_source_true"] += d["first_successful_map_before_first_successful_source_read"] is True
            discovery["successful_map_before_source_evaluable"] += d["first_successful_map_before_first_successful_source_read"] is not None
            for key in ("maps_after_or_at_first_source_tool", "late_maps_after_last_source_tool", "declaration_inventory_calls_heuristic"):
                discovery[key] += d[key]
            discovery["repeated_query_extra_calls_heuristic"] += sum(q["extra_calls"] for q in d["repeated_queries_heuristic"])
        coverage = [target for row in completed for target in row["source_verification"]]
        result.append({"model": model, "variant": variant, "arm": arm, "planned": len(planned), "n": len(completed),
                       "statuses": dict(Counter(row["status"] for row in completed)),
                       "strict_passes": sum(row["strict_passed"] is True for row in completed),
                       "strict_pass_known_n": sum(type(row["strict_passed"]) is bool for row in completed),
                       "valid_answers": sum(row["valid_answer"] is True for row in completed),
                       "valid_answer_known_n": sum(type(row["valid_answer"]) is bool for row in completed),
                       "citation_passes": sum(row["citation_passed"] is True for row in completed),
                       "citation_pass_known_n": sum(type(row["citation_passed"]) is bool for row in completed),
                       "citation_f1_among_valid_answers": {"n": sum(row["valid_answer"] is True and number(row["citation_f1"]) is not None for row in completed),
                                                            "mean": average(row["citation_f1"] for row in completed if row["valid_answer"] is True)},
                       "means": means, "raw_token_sums": {key: tokens[key] for key in TOKEN_NAMES},
                       "raw_token_means": {key: {"n": sum(number(row["tokens"].get(key)) is not None for row in completed), "mean": average(row["tokens"].get(key) for row in completed)} for key in TOKEN_NAMES},
                       "saved_tool_output_chars_by_kind": dict(outputs), "saved_bash_output_chars_by_kind": dict(bash),
                       "successful_source_reads_by_kind": dict(source_calls), "journal_commands": dict(commands),
                       "journal_present_n": sum(row["journal"]["present"] for row in completed),
                       "journal_missing_n": sum(not row["journal"]["present"] for row in completed),
                       "journal_missing_with_saved_zero_n": sum(not row["journal"]["present"] and row["journal"]["saved_result_total"] == 0 for row in completed),
                       "journal_malformed_lines": sum(row["journal"]["malformed_lines"] for row in completed),
                       "journal_exit_statuses": dict(exits), "discovery_counts": dict(discovery), "probes_heuristic": dict(probes),
                       "first_source_means": {key: {"n": sum(number(row["discovery"].get(key)) is not None for row in completed), "mean": average(row["discovery"].get(key) for row in completed)} for key in ("steps_to_first_source_read", "tools_to_first_source_read", "seconds_to_first_source_read_from_first_event")},
                       "source_verification": {"gold_declarations": len(coverage), "known_line_n": sum(target["declaration_line"] is not None for target in coverage),
                                               "declarations_exposed": sum(target["declaration_exposed"] is True for target in coverage),
                                               "implementation_exposed_heuristic": sum(target["implementation_beyond_signature_exposed_heuristic"] for target in coverage)}})
    return result


CAVEATS = [
    "Input-token presentations = input + cache_read + cache_write, summed across steps: repeated context presentations, not unique source/context tokens. Peak is the largest saved step presentation. Output and reasoning are separate provider-reported fields, not reconstructed from total.",
    "Strict F1/passes are original result.json grades, including failure penalties. Citation F1 is rescored using the runner's schema, optional outer fences and declaration-line +0..2 rule, without status penalties; invalid JSON is never salvaged. Valid JSON/schema is not proof of explanation correctness.",
    "Saved tool-state output characters count output once per callID (latest saved state); metadata.preview/display duplicates are excluded. Source-context characters are visible .go source lines including signatures, comments and surrounding code, not just function bodies, with one normalized newline per visible line; repeated reads count again.",
    "Successful source reads require completed read-tool output or conservative single-command sed -n/cat/head on one literal .go path. Grep/map declarations are not body reads. Compound/piped shell, tail/nl/awk/python reads, truncation markers and mixed map/source outputs are not attributed to source coverage; their saved output remains counted separately.",
    "Discovery order prefers wrapper journal start times against source-tool start times, falling back to saved bash tool order (mixed commands ambiguous). Steps/tools to first read are inclusive one-based positions; seconds start at first trace timestamp and use tool start when saved, excluding process startup. The first successful source-context read may be a declaration preview or an unrelated file.",
    "Help/git/layout probes, declaration inventories, exact repeated queries, map test exclusions and implementation exposure are heuristics. They do not classify semantic intent. Map argv kind/private/glob details and journal exits are retained for manual review; unfinished and missing journals are distinct from zero calls.",
    "Coverage uses checkpoint-matched structural SQLite end lines when available and saved grade lines otherwise. Signature/body brace and comment checks are conservative heuristics, not Go parsing. Missing declaration/signature fragments and unsupported shell reads can undercount; complex signatures/comments can misclassify. Exposure does not establish complete implementation verification or comprehension.",
    "Paired deltas are comparison minus off for the same model/variant/task/repeat. Mean case ratios exclude zero/missing baseline denominators and report their own n. All-available, both-status-ok, both-valid-answer and their intersection are separate cohorts; failure-driven savings and survivor bias must be considered together with quality.",
    "No confidence intervals or significance claims: a one-repeat, six-task pilot is for screening. Validate a finalist on the planned 20 tasks × 2 models × 2 arms × 2 repeats = 160 trials; review quality/coverage as well as savings.",
    "Audit checks saved controls and journals, not independent network/process telemetry. Absent semantic/index calls and zero cached embedding rows support a structural-only run, but cannot prove every provider/API action or deleted sandbox file. Baseline index-copy absence is not directly observable after sandbox cleanup; manifest/harness design and zero baseline navigation are supporting evidence only.",
]


def build(saved, manifest):
    tasks = {task["id"]: task for task in array(manifest.get("tasks")) if isinstance(task, dict) and "id" in task}
    gold, cache_audit = structural_gold(manifest)
    results = {}
    for trial in manifest["trials"]:
        prefix = f"trials/{trial['id']}/"
        value = saved.json(prefix + "result.json")
        results[trial["id"]] = value if isinstance(value, dict) and isinstance(value.get("grade"), dict) and isinstance(value.get("status"), str) else None
        for match in array(obj(obj(value).get("grade")).get("matches")):
            if isinstance(match, dict) and type(match.get("expected_line")) is int and isinstance(match.get("path"), str) and isinstance(match.get("symbol"), str):
                key = (match["path"], match["symbol"])
                if key not in gold:
                    gold[key] = {"line": match["expected_line"], "method": "saved_grade_expected_line"}
                elif gold[key]["line"] != match["expected_line"]:
                    saved.warnings.append({"path": prefix + "result.json", "error": "Conflicting gold line for " + str(key)})
    rows, audits = [], []
    for trial in manifest["trials"]:
        prefix = f"trials/{trial['id']}/"
        result = results[trial["id"]]
        trace = parse_trace(saved, prefix)
        grade = obj(obj(result).get("grade"))
        task = tasks.get(trial["task"], {"targets": []})
        task_gold = {(target["path"], target["symbol"]): gold[(target["path"], target["symbol"])] for target in task["targets"] if (target["path"], target["symbol"]) in gold}
        answer = saved.text(prefix + "answer.txt")
        score = citation_score(answer if answer is not None else trace["answer_text"], task, task_gold) if task["targets"] else {"valid_answer": None, "f1": None, "passed": None, "method": "task_unavailable"}
        p, r = number(grade.get("precision")), number(grade.get("recall"))
        if score["valid_answer"] is True and score["f1"] is None and grade.get("valid_answer") is True and p is not None and r is not None:
            score.update({"f1": 2 * p * r / (p + r) if p + r else 0.0, "passed": r == 1,
                          "method": "saved_valid_grade_precision_recall_gold_unavailable"})
        if answer is None and not trace["answer_text"]:
            # Recover from saved citation precision/recall only for already valid
            # answers. This never invents an answer or salvages invalid JSON.
            if grade.get("valid_answer") is True and p is not None and r is not None:
                score = {"valid_answer": True, "f1": 2 * p * r / (p + r) if p + r else 0.0,
                         "passed": r == 1, "method": "saved_valid_grade_precision_recall_answer_missing"}
            elif result is None:
                score = {"valid_answer": None, "f1": None, "passed": None, "method": "answer_missing"}
        journal_info = journal(saved, prefix, result or {})
        tokens = {key: number(obj(obj(result).get("tokens")).get(key)) if result else (trace["tokens"][key] if trace["trace"]["present"] and trace["steps"] else None) for key in TOKEN_NAMES}
        presentations = sum(tokens[key] for key in ("input", "cache_read", "cache_write")) if all(tokens[key] is not None for key in ("input", "cache_read", "cache_write")) else trace["input_presentations"]
        reads = trace["source_reads"]
        journal_discovery(trace["discovery"], journal_info, reads)
        source_counts = Counter(read["tool"] for read in reads)
        source_chars = sum(sum(len(text) + 1 for text in read["lines"].values()) for read in reads)
        row = {**{key: trial[key] for key in ("id", "model", "variant", "task", "repeat", "arm")},
               "completed": result is not None, "status": result["status"] if result else "incomplete",
               "strict_f1": number(grade.get("f1")), "strict_passed": grade.get("passed"),
               "valid_answer": score["valid_answer"], "citation_f1": score["f1"], "citation_passed": score["passed"], "citation_score": score,
               "steps": number(obj(result).get("steps")) if result else trace["steps"],
               "tools": obj(obj(result).get("tools")) if result and isinstance(result.get("tools"), dict) else trace["tools"],
               "tool_calls": sum(value for value in obj(result).get("tools", {}).values() if number(value) is not None) if result and isinstance(result.get("tools"), dict) else trace["tool_calls"],
               "cost_usd": number(result.get("cost_usd")) if result else trace["cost_usd"],
               "wall_seconds": number(obj(result).get("wall_seconds")), "tokens": tokens,
               "input_presentations": presentations, "peak_step_input_presentations": trace["peak_step_input_presentations"],
               "saved_tool_output_chars": sum(trace["saved_tool_output_chars_by_kind"].values()) if trace["trace"]["present"] else None,
               "saved_tool_output_chars_by_kind": trace["saved_tool_output_chars_by_kind"],
               "saved_bash_output_chars_by_kind": trace["saved_bash_output_chars_by_kind"],
               "successful_source_read_calls": len(reads) if trace["trace"]["present"] else None,
               "successful_source_reads_by_kind": dict(source_counts), "source_context_chars": source_chars if trace["trace"]["present"] else None,
               "source_context_chars_by_kind": {kind: sum(sum(len(text) + 1 for text in read["lines"].values()) for read in reads if read["tool"] == kind) for kind in source_counts},
               "source_read_ranges": [{**{key: value for key, value in read.items() if key != "lines"}, "output_line_start": min(read["lines"]), "output_line_end": max(read["lines"]), "visible_line_count": len(read["lines"])} for read in reads if read["lines"]],
               "trace": trace["trace"], "trace_accounting": {key: trace[key] for key in ("steps", "tool_calls", "tokens", "cost_usd", "input_presentations")},
               "journal": journal_info, "map_argv": [{"argv": call.get("argv"), "exit_code": call.get("exit_code"), **map_options(call.get("argv"))} for call in journal_info["calls"] if call["kind"] == "map"],
               "discovery": trace["discovery"], "source_verification": source_coverage(task, task_gold, reads)}
        rows.append(row)
        audit = trial_audit(saved, prefix, trial, result or {}, manifest)
        audit["checks"]["no_semantic_or_index_journal_calls"] = journal_absence_check(journal_info, SEMANTIC_OR_INDEX)
        audit["checks"]["journal_counts_match_saved_result"] = journal_info["counts"] == obj(result.get("slopdex_commands")) and len(journal_info["calls"]) == result.get("slopdex_calls") if result and journal_info["present"] else None
        audit["checks"]["answer_validity_matches_saved_grade"] = score["valid_answer"] == grade.get("valid_answer") if result and type(score["valid_answer"]) is bool and type(grade.get("valid_answer")) is bool else None
        if trial["arm"] == "off":
            audit["checks"]["baseline_no_navigation_calls"] = False if journal_info["calls"] else journal_absence_check(journal_info, COMMANDS | {"version"})
        audit["journal"] = {key: value for key, value in journal_info.items() if key != "calls"}
        audits.append(audit)
    cases, comparisons = paired(rows)
    identity_keys = ("config", "tasks", "commit", "trials", "arm_instructions", "harness_sha256", "opencode_version", "slopdex_version")
    identity = {key: manifest.get(key) for key in identity_keys}
    checks = {"manifest_identity_fingerprint_match": digest(identity) == manifest.get("fingerprint") if all(key in manifest for key in identity_keys) else None,
              "structural_only_manifest": "index" in manifest and manifest["index"] is None,
              "structural_commit_version_config_match": all(obj(obj(manifest.get("map_index")).get("identity")).get(key) == value for key, value in (("commit", manifest.get("commit")), ("slopdex_version", manifest.get("slopdex_version")), ("config", obj(manifest.get("config")).get("slopdex")))) if manifest.get("map_index") else None,
              "expected_slopdex_version": manifest.get("slopdex_version") == "slopdex 0.29.0",
              "expected_opencode_version": manifest.get("opencode_version") == "1.18.34"}
    completed = sum(row["completed"] for row in rows)
    metrics = {"schema_version": 1, "planned": len(rows), "completed": completed, "complete": completed == len(rows),
               "manifest_sha256": sha(saved.data["manifest.json"]), "commit": manifest.get("commit"), "seed": manifest.get("seed"),
               "versions": {key: manifest.get(key) for key in ("slopdex_version", "opencode_version")},
               "caveats": CAVEATS, "groups": aggregate(rows), "paired_cases": cases, "paired_comparisons": comparisons, "trials": rows}
    audit = {"schema_version": 1, "mode": "offline_artifact_reader", "runner_imported": False, "external_commands": 0, "provider_calls": 0,
             "manifest_checks": checks, "manifest_controls": {key: manifest.get(key) for key in ("fingerprint", "harness_sha256", "commit", "seed", "slopdex_version", "opencode_version", "config", "arm_instructions", "index")},
             "structural_cache": cache_audit, "trials": audits, "warnings": saved.warnings,
             "baseline_index_copy_absence": "not directly observable from persisted artifacts; no sandbox or cache copies are made by this report",
             "manual_review": ["Check explanation/flow correctness and ownership semantics in answer.txt.", "Review signature-only/missing body evidence and mixed shell/map output.", "Inspect failed control checks, missing journals and failure-driven apparent savings."]}
    return metrics, audit


def fmt(value, digits=1):
    return f"{value:.{digits}f}" if number(value) is not None else "n/a"


def markdown(metrics, audit):
    lines = ["# Offline map-prompt efficiency", "", f"Completed **{metrics['completed']}/{metrics['planned']}** trials in this snapshot. " + ("Complete." if metrics["complete"] else "**Interim: incomplete run; arm balance and pair counts may change.**"),
             f"Commit `{metrics['commit']}`; seed `{metrics['seed']}`; versions `{metrics['versions']['slopdex_version']}` / OpenCode `{metrics['versions']['opencode_version']}`.", "",
             "## Quality and efficiency by model / arm", "", "Means use completed results; missing values have metric-specific n in metrics.json. Citation quality removes status penalties, never invalid-JSON penalties.", "",
             "| Model / variant | Arm | N / planned | Strict pass | Strict F1 | Valid answer | Citation pass | Citation F1 | Steps | Tools | Input presentations | Peak input | USD | Seconds |",
             "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"]
    for group in metrics["groups"]:
        m = group["means"]
        lines.append(f"| {group['model']} / {group['variant']} | {group['arm']} | {group['n']}/{group['planned']} | {group['strict_passes']}/{group['strict_pass_known_n']} | {fmt(m['strict_f1']['mean'], 3)} | {group['valid_answers']}/{group['valid_answer_known_n']} | {group['citation_passes']}/{group['citation_pass_known_n']} | {fmt(m['citation_f1']['mean'], 3)} | {fmt(m['steps']['mean'])} | {fmt(m['tool_calls']['mean'])} | {fmt(m['input_presentations']['mean'], 0)} | {fmt(m['peak_step_input_presentations']['mean'], 0)} | {fmt(m['cost_usd']['mean'], 6)} | {fmt(m['wall_seconds']['mean'])} |")
    lines += ["", "### Raw reported tokens (mean per completed trial)", "", "Input presentations = input + cache read + cache write: **repeated context across steps**, not unique context. Reasoning and output remain separate reported fields.", "",
              "| Model / variant | Arm | Input | Cache read | Cache write | Output | Reasoning |", "|---|---|---:|---:|---:|---:|---:|"]
    for group in metrics["groups"]:
        lines.append(f"| {group['model']} / {group['variant']} | {group['arm']} | " + " | ".join(fmt(group["raw_token_means"][key]["mean"], 0) for key in TOKEN_NAMES[:-1]) + " |")
    lines += ["", "### Saved output and source exposure (totals)", "", "Output is counted once per saved tool call. Source characters include visible surrounding context, comments and signatures; mixed shell/map is not body evidence.", "",
              "| Model / variant | Arm | Tool output chars by kind | Bash output chars by kind | Source calls by kind | Source chars | Gold body exposure heuristic |", "|---|---|---|---|---|---:|---:|"]
    for group in metrics["groups"]:
        cover = group["source_verification"]
        rows = [row for row in metrics["trials"] if row["completed"] and (row["model"], row["variant"], row["arm"]) == (group["model"], group["variant"], group["arm"])]
        lines.append(f"| {group['model']} / {group['variant']} | {group['arm']} | `{json.dumps(group['saved_tool_output_chars_by_kind'])}` | `{json.dumps(group['saved_bash_output_chars_by_kind'])}` | `{json.dumps(group['successful_source_reads_by_kind'])}` | {sum(row['source_context_chars'] or 0 for row in rows)} | {cover['implementation_exposed_heuristic']}/{cover['gold_declarations']} |")
    lines += ["", "## Paired effects versus off", "", "Δ = comparison − off. Negative efficiency Δ/effect indicates savings; positive F1 Δ indicates higher citation quality. Effects are means of per-case ratios − 1, not ratios of means; zero denominators are excluded. Each metric shows its available-pair n. No significance or CI claims.", "",
              "| Model / variant | Arm | Cohort | Pairs | Steps Δ / effect (n) | Context Δ / effect (n) | USD Δ / effect (n) | Seconds Δ / effect (n) | Strict F1 Δ | Citation F1 Δ |", "|---|---|---|---:|---|---|---|---|---:|---:|"]
    for comparison in metrics["paired_comparisons"]:
        for cohort, summary in comparison["cohorts"].items():
            values = []
            for key, digits in (("steps", 1), ("input_presentations", 0), ("cost_usd", 6), ("wall_seconds", 1)):
                metric = summary["metrics"][key]
                values.append(f"{fmt(metric['mean_delta'], digits)} / {fmt(metric['mean_ratio_effect_pct'])}% (Δ n={metric['n']}, ratio n={metric['ratio_n']})")
            lines.append(f"| {comparison['model']} / {comparison['variant']} | {comparison['arm']} | {cohort} | {summary['n']} | " + " | ".join(values) + f" | {fmt(summary['metrics']['strict_f1']['mean_delta'], 3)} | {fmt(summary['metrics']['citation_f1']['mean_delta'], 3)} |")
    lines += ["", "### Per-case deltas", "", "| Model / variant | Arm | Task / rep | Off / comparison status | Both valid answer | Steps Δ | Context Δ | USD Δ | Seconds Δ | Strict / citation F1 Δ |", "|---|---|---|---|---|---:|---:|---:|---:|---|"]
    for case in metrics["paired_cases"]:
        m = case["metrics"]
        lines.append(f"| {case['model']} / {case['variant']} | {case['arm']} | {case['task']} / {case['repeat']} | {case['reference_status']} / {case['comparison_status']} | {case['both_valid_answer']} | {fmt(m['steps']['delta'])} | {fmt(m['input_presentations']['delta'], 0)} | {fmt(m['cost_usd']['delta'], 6)} | {fmt(m['wall_seconds']['delta'])} | {fmt(m['strict_f1']['delta'], 3)} / {fmt(m['citation_f1']['delta'], 3)} |")
    lines += ["", "## Discovery and journals", "", "Map-before-source and declaration inventories are heuristics. Detailed map argv/private/kinds/test exclusions, repeated queries, read ranges and exit statuses are in metrics.json.", "",
              "| Model / variant | Arm | Map before source | First read steps / tools / sec | Maps after source / late | Probes | Declaration inventories | Repeat extras | Journal commands / exits (present n) |", "|---|---|---:|---|---|---|---:|---:|---|"]
    for group in metrics["groups"]:
        d, first = group["discovery_counts"], group["first_source_means"]
        timing = " / ".join(fmt(first[key]["mean"]) + f" (n={first[key]['n']})" for key in first)
        lines.append(f"| {group['model']} / {group['variant']} | {group['arm']} | {d.get('map_before_source_true', 0)}/{d.get('map_before_source_evaluable', 0)} | {timing} | {d.get('maps_after_or_at_first_source_tool', 0)} / {d.get('late_maps_after_last_source_tool', 0)} | `{json.dumps(group['probes_heuristic'])}` | {d.get('declaration_inventory_calls_heuristic', 0)} | {d.get('repeated_query_extra_calls_heuristic', 0)} | `{json.dumps(group['journal_commands'])}` / `{json.dumps(group['journal_exit_statuses'])}` (n={group['journal_present_n']}/{group['n']}; missing saved-zero={group['journal_missing_with_saved_zero_n']}) |")
    failed = [("manifest", key) for key, value in audit["manifest_checks"].items() if value is False]
    failed += [(trial["id"], key) for trial in audit["trials"] for key, value in trial["checks"].items() if value is False]
    lines += ["", "## Audit and review", "", f"Saved-control checks reporting false: **{len(failed)}**. Unknown/missing evidence is retained as null, not treated as a pass. Structural cache method: `{audit['structural_cache']['method']}`.",
              "Original-file SHA-256 before/after and concurrent changes are recorded in efficiency-audit.json. This reader makes no external commands/provider calls and writes only the three report files."]
    for trial, check in failed:
        lines.append(f"- `{trial}`: `{check}`")
    lines += ["", "## Interpretation and limitations", ""] + [f"- {caveat}" for caveat in CAVEATS]
    return "\n".join(lines) + "\n"


def report(job):
    job = Path(job).resolve()
    if not job.is_dir():
        raise ValueError(f"Not a job directory: {job}")
    for name in OUTPUTS:
        if (job / name).is_symlink():
            raise ValueError(f"Refuse to overwrite symlink: {job / name}")
    before, data, errors = snapshot(job, capture=True)
    saved = Saved(data)
    manifest = saved.json("manifest.json")
    if not isinstance(manifest, dict) or not isinstance(manifest.get("trials"), list):
        raise ValueError("Missing/malformed manifest.json trials")
    ids, keys = set(), set()
    for trial in manifest["trials"]:
        if not isinstance(trial, dict) or any(key not in trial for key in ("id", "model", "variant", "task", "arm", "repeat")):
            raise ValueError("Malformed trial identity in manifest")
        trial_id = trial["id"]
        if not isinstance(trial_id, str) or not re.fullmatch(r"[A-Za-z0-9_-]+", trial_id):
            raise ValueError("Unsafe trial ID")
        key = tuple(trial[name] for name in ("model", "variant", "task", "arm", "repeat"))
        if trial_id in ids or key in keys:
            raise ValueError("Duplicate trial ID or model/task/repeat/arm identity")
        ids.add(trial_id)
        keys.add(key)
    metrics, audit = build(saved, manifest)
    metrics["job_dir"] = str(job)
    audit["job_dir"] = str(job)
    (job / "metrics.json").write_text(json.dumps(metrics, indent=2, sort_keys=True, allow_nan=False) + "\n")
    (job / "efficiency.md").write_text(markdown(metrics, audit))
    after, _, after_errors = snapshot(job)
    changed = [key for key in sorted(before.keys() & after.keys()) if before[key] != after[key]]
    added, removed = sorted(after.keys() - before.keys()), sorted(before.keys() - after.keys())
    audit["preservation"] = {"before_sha256": before, "after_sha256": after, "changed": changed, "added": added, "removed": removed,
                             "read_errors": errors + after_errors, "stable": not (changed or added or removed or errors or after_errors),
                             "interpretation": "A live runner may append traces/add results/update official reports during this snapshot; changes are observed, not attributed to the reader. Re-run when quiescent to verify stable originals."}
    (job / "efficiency-audit.json").write_text(json.dumps(audit, indent=2, sort_keys=True, allow_nan=False) + "\n")
    return metrics, audit


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("job_dir", type=Path)
    parser.add_argument("--require-complete", action="store_true", help="Write reports, then exit 2 unless every planned result is readable")
    args = parser.parse_args(argv)
    try:
        metrics, audit = report(args.job_dir)
    except (ValueError, OSError, TypeError, KeyError) as error:
        print(f"map-prompt-report: {error}", file=sys.stderr)
        return 1
    stable = audit["preservation"]["stable"]
    print(f"Reported {metrics['completed']}/{metrics['planned']} trials; original artifacts {'stable' if stable else 'changed during snapshot (see audit)'}.\n"
          f"Saved: {Path(args.job_dir) / 'metrics.json'}, {Path(args.job_dir) / 'efficiency.md'}, {Path(args.job_dir) / 'efficiency-audit.json'}")
    return 2 if args.require_complete and not metrics["complete"] else 0


if __name__ == "__main__":
    sys.exit(main())
