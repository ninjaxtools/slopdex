#!/usr/bin/env python3
"""Dump compact per-task evidence from a terminal-bench repo checkout.

Used to decide which tasks start with an existing codebase (relevant for
evaluating code search) versus from-scratch tasks. Outputs one JSON object
per line: task, category, keywords, dockerfile signals, instruction text,
solution overview and test imports.

Usage:
    python3 task_features.py /path/to/terminal-bench-2 > tb-features.jsonl
    python3 task_features.py /path/to/terminal-bench-2 --task gpt2-codegolf
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path

DOCKER_SIG = re.compile(
    r"^\s*(COPY|ADD)\b|git\s+clone|wget|curl\b.*\.(zip|tar|tgz|gz\b)|"
    r"pip\s+install\s+(\.|-[e]\s*\.)|npm\s+(ci|install)\b|unzip\b|\.tar\b",
    re.IGNORECASE,
)
IMPORT_RE = re.compile(r"^\s*(?:import\s+([\w\.]+)|from\s+([\w\.]+)\s+import\s+)")


def read_text(path: Path, limit: int = 4000) -> str:
    try:
        return path.read_text(errors="replace")[:limit]
    except OSError:
        return ""


def dockerfile_signals(task_dir: Path) -> list[str]:
    out = []
    for name in ("environment/Dockerfile", "Dockerfile"):
        dockerfile = task_dir / name
        if not dockerfile.is_file():
            continue
        for line in dockerfile.read_text(errors="replace").splitlines():
            stripped = line.strip()
            if not stripped or stripped.startswith("#"):
                continue
            if DOCKER_SIG.search(stripped):
                out.append(stripped[:200])
    return out


def test_imports(task_dir: Path) -> list[str]:
    found: list[str] = []
    tests = task_dir / "tests"
    if not tests.is_dir():
        return found
    for path in sorted(tests.rglob("*.py"))[:20]:
        for line in path.read_text(errors="replace").splitlines()[:60]:
            match = IMPORT_RE.match(line)
            if match:
                module = (match.group(1) or match.group(2) or "").split(".")[0]
                if module and module not in found:
                    found.append(module)
    return found[:30]


def solution_overview(task_dir: Path) -> dict[str, object]:
    solution = task_dir / "solution"
    files: list[str] = []
    head = ""
    if solution.is_dir():
        files = sorted(
            str(p.relative_to(solution)) for p in solution.rglob("*") if p.is_file()
        )[:20]
        solve = solution / "solve.sh"
        if solve.is_file():
            head = read_text(solve, 1500)
    return {"files": files, "solve_head": head}


def task_features(task_dir: Path) -> dict[str, object]:
    meta: dict[str, object] = {}
    toml_path = task_dir / "task.toml"
    if toml_path.is_file():
        try:
            data = tomllib.loads(toml_path.read_text())
            task = data.get("task", {})
            meta = {
                "category": (data.get("metadata", {}) or {}).get("category"),
                "keywords": task.get("keywords", []),
                "description": task.get("description", ""),
            }
        except (OSError, tomllib.TOMLDecodeError):
            pass
    return {
        "task": task_dir.name,
        **meta,
        "dockerfile": dockerfile_signals(task_dir),
        "instruction": read_text(task_dir / "instruction.md", 2500),
        "solution": solution_overview(task_dir),
        "test_imports": test_imports(task_dir),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("repo", help="terminal-bench repo checkout")
    parser.add_argument("--task", action="append", default=[], help="only this task")
    args = parser.parse_args()

    root = Path(args.repo)
    tasks = sorted(
        p for p in root.iterdir() if p.is_dir() and (p / "task.toml").is_file()
    )
    if args.task:
        wanted = set(args.task)
        tasks = [p for p in tasks if p.name in wanted]
    for task_dir in tasks:
        print(json.dumps(task_features(task_dir)))


if __name__ == "__main__":
    sys.exit(main())
