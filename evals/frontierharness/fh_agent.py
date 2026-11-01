"""Harbor agent for the FrontierHarness eval: OpenCode, optionally with slopdex.

The two arms differ only in slopdex:

    arm=off   plain OpenCode, as in the published FrontierHarness OpenCode row
    arm=map   slopdex installed and indexed, plus the README map snippet as
              OpenCode instructions (identical to the Excalidraw map arm)

Nothing is written into the task repository: the binary, index, config,
instructions and shims live under /installed-agent/slopdex, so verifiers that
diff the repository (DeepSWE collects `git diff`) see only the agent's work.
Indexing happens during agent setup, outside the timed agent phase. Every
slopdex call and every nested `slopdex faq` model call is journaled under
/logs/agent so the host-side report can count commands and add faq usage.
"""

from __future__ import annotations

import json
import shlex
from pathlib import Path
from typing import Any, Literal, override

from pydantic import Field

from harbor.agents.installed.opencode import OpenCode, OpenCodeOptions
from harbor.environments.base import BaseEnvironment

from fh_shims import FAQ_SHIM_JS, WRAPPER_JS, render_js

SLOPDEX_HOME = "/installed-agent/slopdex"
SLOPDEX_BIN = f"{SLOPDEX_HOME}/bin/slopdex-real"
SLOPDEX_CONFIG = f"{SLOPDEX_HOME}/config.json"
SLOPDEX_INDEX = f"{SLOPDEX_HOME}/index.sqlite"
SLOPDEX_ARTIFACTS = f"{SLOPDEX_HOME}/artifacts.sqlite"
SLOPDEX_INSTRUCTIONS = f"{SLOPDEX_HOME}/AGENTS.md"
FAQ_SHIM = f"{SLOPDEX_HOME}/faq/opencode"
CALL_LOG = "/logs/agent/slopdex-calls.jsonl"
FAQ_LOG = "/logs/agent/faq-usage.jsonl"
SETUP_LOG_DIR = "/logs/agent/slopdex-setup"
WRAPPER = "/usr/local/bin/slopdex"
NVM = '[ -f "$HOME/.nvm/nvm.sh" ] && . "$HOME/.nvm/nvm.sh"; '
# Index preparation and ANN warm-up queries run during setup, not agent time.
INDEX_TIMEOUT_SEC = 3000
WARMUP_QUERY = "slopdex eval warmup"

def deep_merge(base: dict[str, Any], override: dict[str, Any]) -> dict[str, Any]:
    merged = dict(base)
    for key, value in override.items():
        if isinstance(merged.get(key), dict) and isinstance(value, dict):
            merged[key] = deep_merge(merged[key], value)
        else:
            merged[key] = value
    return merged


def arm_opencode_config(arm: str, base: dict[str, Any] | None) -> dict[str, Any]:
    config = dict(base or {})
    if arm == "map":
        config = deep_merge(config, {"instructions": [SLOPDEX_INSTRUCTIONS]})
    return config


class FrontierOpenCodeOptions(OpenCodeOptions):
    arm: Literal["off", "map"] = Field(default="off", description="Eval arm.")
    slopdex_binary: str | None = Field(default=None, description="Host path of a portable slopdex binary.")
    slopdex_settings: dict[str, Any] | None = Field(default=None, description="slopdex config (embeddings).")
    instructions: str | None = Field(default=None, description="Map-arm OpenCode instructions.")


class FrontierOpenCode(OpenCode):
    options_model = FrontierOpenCodeOptions

    @staticmethod
    @override
    def name() -> str:
        return "opencode-frontierharness"

    def __init__(self, *args, opencode_config: dict[str, Any] | None = None, **kwargs):
        arm = kwargs.get("arm", "off")
        super().__init__(*args, opencode_config=arm_opencode_config(arm, opencode_config), **kwargs)
        options = self.options
        if not isinstance(options, FrontierOpenCodeOptions):
            raise TypeError("FrontierOpenCode requires FrontierOpenCodeOptions")
        self._arm = options.arm
        self._slopdex_binary = options.slopdex_binary
        self._slopdex_settings = options.slopdex_settings or {}
        self._instructions = options.instructions
        if self._arm == "map" and not (self._slopdex_binary and self._instructions):
            raise ValueError("arm=map requires slopdex_binary and instructions")

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        await super().install(environment)
        if self._arm == "map":
            await self._install_slopdex(environment)

    async def _probe(self, environment: BaseEnvironment, command: str) -> str:
        result = await self.exec_as_agent(environment, command=NVM + command)
        lines = (result.stdout or "").strip().splitlines()
        return lines[-1].strip() if lines else ""

    async def _write(self, environment: BaseEnvironment, path: str, text: str, mode: str = "644") -> None:
        await self.exec_as_root(
            environment,
            command=f"mkdir -p {shlex.quote(str(Path(path).parent))} && "
            f"printf '%s' {shlex.quote(text)} > {shlex.quote(path)} && chmod {mode} {shlex.quote(path)}",
        )

    async def _install_slopdex(self, environment: BaseEnvironment) -> None:
        node = await self._probe(environment, "command -v node")
        opencode = await self._probe(environment, "command -v opencode")
        root = await self._probe(environment, "pwd -P")
        if not node or not opencode:
            raise RuntimeError("OpenCode install did not provide node and opencode on PATH")
        if root in ("", "/"):
            raise RuntimeError(f"refusing to index task working directory {root!r}")
        await self.exec_as_root(
            environment, command=f"mkdir -p {SLOPDEX_HOME}/bin {SLOPDEX_HOME}/faq {SETUP_LOG_DIR}"
        )
        await self._upload_agent_owned_file(environment, Path(self._slopdex_binary), SLOPDEX_BIN)
        await self.exec_as_root(environment, command=f"chmod 755 {SLOPDEX_BIN}")
        await self._write(environment, SLOPDEX_INSTRUCTIONS, self._instructions)
        await self._write(
            environment, FAQ_SHIM,
            render_js(FAQ_SHIM_JS, NODE=node, OPENCODE=json.dumps(opencode), LOG=json.dumps(FAQ_LOG)), "755",
        )
        settings = {
            **self._slopdex_settings,
            "artifactCachePath": SLOPDEX_ARTIFACTS,
            "agentCommand": [FAQ_SHIM, "run"],
            "agentModel": self.model_name,
        }
        await self._write(environment, SLOPDEX_CONFIG, json.dumps(settings, indent=2) + "\n")
        prefix = ["--root", root, "--config", SLOPDEX_CONFIG, "--index", SLOPDEX_INDEX]
        await self._write(
            environment, WRAPPER,
            render_js(WRAPPER_JS, NODE=node, BIN=json.dumps(SLOPDEX_BIN), PREFIX=json.dumps(prefix),
                      LOG=json.dumps(CALL_LOG)),
            "755",
        )
        # The agent user owns the slopdex home so the index can be updated as files change.
        await self.exec_as_root(
            environment,
            command=f'chown -R "$(stat -c %u:%g {shlex.quote(root)})" {SLOPDEX_HOME} {SETUP_LOG_DIR} 2>/dev/null || true',
        )
        base = " ".join(shlex.quote(part) for part in [SLOPDEX_BIN, *prefix])
        steps = {
            "version": f"{SLOPDEX_BIN} --version",
            "update": f"{base} update",
            "warm-search": f"{base} --no-reindex search {shlex.quote(WARMUP_QUERY)} --code --docs --descriptions --threshold 1 --limit 1",
            "warm-map": f"{base} --no-reindex map --private -q {shlex.quote(WARMUP_QUERY)} --symbol-threshold 0 -g '*' -g '!*'",
            "status": f"{base} --no-reindex status",
        }
        # exec_as_agent raises on a nonzero exit; the stderr tail lands in the trial exception.
        for name, command in steps.items():
            log = f"{SETUP_LOG_DIR}/{name}"
            await self.exec_as_agent(
                environment,
                command=f"cd {shlex.quote(root)} && {command} >{log}.stdout 2>{log}.stderr; "
                f"code=$?; tail -c 2000 {log}.stderr >&2; exit $code",
                timeout_sec=INDEX_TIMEOUT_SEC,
            )
