"""Harbor agent plugin: OpenCode with optional slopdex semantic code search.

The only difference between arms is whether slopdex is installed and advertised
in AGENTS.md, so a run compares the tool and not the scaffold:

    mode=off                no extra instructions (baseline OpenCode)
    mode=grep               neutral AGENTS.md block pointing at rg/grep
    mode=slopdex            slopdex installed, index prebuilt, AGENTS.md block

The slopdex arm has two index modes, selected with ``slopdex_mode``:

    vector          embeddings only (descriptionsEnabled: false)
    descriptions    callable descriptions from opencode-go/muse-spark-1.3-contributor

Both modes share a host-side index cache (``cache_dir``, default
``<plugin dir>/cache``) keyed by the repo state, index mode, provider keys and
slopdex version, so vectors and descriptions are generated once and reused
deterministically across invocations.

Usage (from evals/harbor):
    PYTHONPATH=. harbor run -d terminal-bench@2.0 \
        --agent slopdex_opencode:SlopdexOpenCode \
        --model anthropic/claude-opus-4-1 \
        --ak mode=slopdex --ak slopdex_mode=descriptions
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import os
import shlex
import shutil
import time
from pathlib import Path
from typing import Literal, override

from filelock import FileLock, Timeout
from pydantic import Field

from harbor.agents.installed.opencode import OpenCode, OpenCodeOptions
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

MARKER = "<!-- slopdex-eval -->"

SLOPDEX_INSTALL_TIMEOUT_SEC = 1200
SLOPDEX_PREBUILD_TIMEOUT_SEC = 1800
SLOPDEX_PREBUILD_TIMEOUT_DESCRIPTIONS_SEC = 3600
SLOPDEX_INDEX_ARTIFACTS = ("index.sqlite", "index.sqlite-wal", "index.sqlite-shm")

NODE_24_GUARD = (
    "node -e 'const major = Number(process.versions.node.split(\".\")[0]); "
    "process.exit(major < 24 ? 1 : 0)'"
)

SLOPDEX_BLOCK = """{marker}
## Code search

- Use semantic code search to find code by meaning instead of guessing paths:
  `slopdex search "<describe the code you need>" --threshold {threshold}`
- Run `slopdex --help` for all commands and options.
"""

GREP_BLOCK = """{marker}
## Code search

- Use keyword search to find code by meaning instead of guessing paths:
  `rg -n "<describe the code you need>"` (or `grep -rn "<pattern>" .`)
- Run `rg --help` for all options.
"""


class SlopdexOpenCodeOptions(OpenCodeOptions):
    mode: Literal["off", "grep", "slopdex"] = Field(
        default="off",
        description="Eval arm: off (baseline), grep (instruction control), slopdex.",
    )
    slopdex_mode: Literal["vector", "descriptions"] = Field(
        default="vector",
        description="Slopdex index mode: embeddings only, or + LLM descriptions.",
    )
    slopdex_version: str | None = Field(
        default=None,
        description="npm version of @ninjaxtools/slopdex to install. Omit for latest.",
    )
    slopdex_threshold: float = Field(
        default=0.5,
        description="Similarity threshold advertised in the slopdex AGENTS.md block.",
    )
    description_provider: Literal["openai", "opencode", "opencode-go"] = Field(
        default="opencode-go",
        description="Description provider written to .slopdex/config.json.",
    )
    description_model: str = Field(
        default="deepseek-v4.1-flash",
        description="Description model written to .slopdex/config.json.",
    )
    prebuild_index: bool = Field(
        default=True,
        description="Run `slopdex update-git` before the agent starts.",
    )
    prebuild_timeout_sec: int = Field(
        default=0,
        description="Index prebuild timeout. 0 uses the per-mode default.",
    )
    use_cache: bool = Field(
        default=True,
        description="Reuse a host-side slopdex index cache across invocations.",
    )
    cache_dir: str | None = Field(
        default=None,
        description="Index cache directory. Default: <plugin dir>/cache.",
    )


class SlopdexOpenCode(OpenCode):
    """OpenCode plus a slopdex arm for tool-ablation evals."""

    options_model = SlopdexOpenCodeOptions

    @staticmethod
    @override
    def name() -> str:
        return "opencode-slopdex"

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        options = self.options
        if not isinstance(options, SlopdexOpenCodeOptions):
            raise TypeError("SlopdexOpenCode requires SlopdexOpenCodeOptions")
        self._mode = options.mode
        self._slopdex_mode = options.slopdex_mode
        self._slopdex_version = options.slopdex_version
        self._slopdex_threshold = options.slopdex_threshold
        self._description_provider = options.description_provider
        self._description_model = options.description_model
        self._prebuild_index = options.prebuild_index
        self._prebuild_timeout_sec = options.prebuild_timeout_sec or (
            SLOPDEX_PREBUILD_TIMEOUT_DESCRIPTIONS_SEC
            if options.slopdex_mode == "descriptions"
            else SLOPDEX_PREBUILD_TIMEOUT_SEC
        )
        self._use_cache = options.use_cache
        self._cache_dir = options.cache_dir
        self._slopdex_installed_version: str | None = None

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        await super().install(environment)
        await self._install_node_24(environment, required=self._mode == "slopdex")
        if self._mode == "grep":
            await self.ensure_system_dependencies(environment, ("ripgrep",))
        elif self._mode == "slopdex":
            await self._install_slopdex(environment)

    async def _install_node_24(
        self, environment: BaseEnvironment, *, required: bool
    ) -> None:
        """Make Node 24 available without changing the nvm default.

        OpenCode installs under nvm's default Node (22) and its launcher is
        only on PATH when the default still resolves to that version, so the
        default must not move. Slopdex gets Node 24 through the wrapper
        written in _install_slopdex instead.
        """
        failure = (
            'echo "slopdex requires Node >= 24" >&2; exit 1'
            if required
            else "exit 0"
        )
        await self.exec_as_agent(
            environment,
            command=(
                f"{NODE_24_GUARD} && exit 0; "
                'if [ -f "$HOME/.nvm/nvm.sh" ]; then '
                '. "$HOME/.nvm/nvm.sh"; '
                "nvm install 24 >/dev/null 2>&1 || true; "
                f"{NODE_24_GUARD} && exit 0; "
                "fi; "
                f"{failure}"
            ),
            timeout_sec=SLOPDEX_INSTALL_TIMEOUT_SEC,
        )

    async def _install_slopdex(self, environment: BaseEnvironment) -> None:
        version_spec = f"@{self._slopdex_version}" if self._slopdex_version else ""
        result = await self.exec_as_agent(
            environment,
            command=(
                "set -e; "
                # Fresh shell on the nvm default (22): switch to 24 for this
                # shell only so npm installs slopdex under the v24 prefix.
                'if [ -f "$HOME/.nvm/nvm.sh" ]; then '
                '. "$HOME/.nvm/nvm.sh"; '
                "nvm use 24 >/dev/null 2>&1 || true; "
                "fi; "
                f"{NODE_24_GUARD}; "
                f"npm install -g @ninjaxtools/slopdex{version_spec} >/dev/null; "
                "printf 'SLOPDEX_PREFIX=%s\\n' \"$(npm prefix -g)\""
            ),
            timeout_sec=SLOPDEX_INSTALL_TIMEOUT_SEC,
        )
        prefix = next(
            (
                line.removeprefix("SLOPDEX_PREFIX=")
                for line in (result.stdout or "").splitlines()
                if line.startswith("SLOPDEX_PREFIX=")
            ),
            None,
        )
        if not prefix:
            raise RuntimeError("slopdex install did not report an npm prefix")
        bin_dir = f"{prefix}/bin"
        wrapper = (
            "#!/bin/sh\n"
            f'export PATH="{bin_dir}:$PATH"\n'
            f'exec "{bin_dir}/slopdex" "$@"\n'
        )
        await self.exec_as_root(
            environment,
            command=(
                "cat > /usr/local/bin/slopdex <<'SLOPDEX_WRAPPER'\n"
                f"{wrapper}"
                "SLOPDEX_WRAPPER\n"
                "chmod 755 /usr/local/bin/slopdex"
            ),
        )
        version_result = await self.exec_as_agent(environment, command="slopdex --version")
        self._slopdex_installed_version = (version_result.stdout or "").strip() or None

    def _slopdex_env(self) -> dict[str, str]:
        keys = ("JINA_API_KEY", "OPENAI_API_KEY", "OPENCODE_API_KEY", "COHERE_API_KEY")
        return {key: value for key in keys if (value := self._get_env(key))}

    def _slopdex_config(self) -> dict[str, object]:
        if self._slopdex_mode == "descriptions":
            return {
                "descriptionProvider": self._description_provider,
                "descriptionModel": self._description_model,
                "descriptionsEnabled": True,
            }
        return {"descriptionsEnabled": False}

    def _agents_md_block(self) -> str | None:
        if self._mode == "slopdex":
            return SLOPDEX_BLOCK.format(
                marker=MARKER, threshold=self._slopdex_threshold
            )
        if self._mode == "grep":
            return GREP_BLOCK.format(marker=MARKER)
        return None

    async def _prepare_workspace(
        self, environment: BaseEnvironment, instruction: str
    ) -> None:
        block = self._agents_md_block()
        if block is None:
            return
        await self.exec_as_agent(
            environment,
            command=(
                'ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"; '
                'cd "$ROOT"; '
                "cp AGENTS.md /tmp/slopdex_agents_backup.md 2>/dev/null "
                "|| rm -f /tmp/slopdex_agents_backup.md; "
                f"grep -qF {shlex.quote(MARKER)} AGENTS.md 2>/dev/null || "
                f"printf '%s' {shlex.quote(block)} >> AGENTS.md"
            ),
        )
        if self._mode != "slopdex":
            return
        await self._write_slopdex_config(environment)
        identity = await self._index_identity(environment, instruction)
        await self._prepare_index(environment, identity)

    async def _cleanup_workspace(self, environment: BaseEnvironment) -> None:
        """Remove every trace the harness left in the task repo.

        Verifiers diff the repo against the base commit, so our AGENTS.md
        block and .slopdex/ must not leak into grading.
        """
        if self._agents_md_block() is None:
            return
        try:
            await self.exec_as_agent(
                environment,
                command=(
                    'ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"; '
                    'cd "$ROOT"; '
                    "if git ls-files --error-unmatch AGENTS.md >/dev/null 2>&1; then "
                    "git checkout -- AGENTS.md; "
                    "elif [ -f /tmp/slopdex_agents_backup.md ]; then "
                    "cp /tmp/slopdex_agents_backup.md AGENTS.md; "
                    "else rm -f AGENTS.md; fi; "
                    "rm -f /tmp/slopdex_agents_backup.md; "
                    "if ! git ls-files --error-unmatch .slopdex >/dev/null 2>&1; then "
                    "rm -rf .slopdex; fi"
                ),
            )
        except Exception:
            self.logger.exception("slopdex eval workspace cleanup failed")

    async def _write_slopdex_config(self, environment: BaseEnvironment) -> None:
        config = json.dumps(self._slopdex_config(), indent=2) + "\n"
        await self.exec_as_agent(
            environment,
            command=(
                'ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"; '
                'cd "$ROOT"; mkdir -p .slopdex; '
                f"printf '%s' {shlex.quote(config)} > .slopdex/config.json"
            ),
        )

    def _cache_root(self) -> Path:
        if self._cache_dir:
            return Path(self._cache_dir).expanduser().resolve()
        return Path(__file__).resolve().parent / "cache"

    async def _index_identity(
        self, environment: BaseEnvironment, instruction: str
    ) -> dict[str, object]:
        """Fingerprint everything that determines the index contents.

        The sqlite file stores the embedding profile and the description
        profile, and slopdex rejects an incompatible index, so the cache key
        must pin the same inputs: repo state, index mode, provider keys and
        the slopdex version that produced the defaults.
        """
        result = await self.exec_as_agent(
            environment,
            command=(
                "set -e; "
                "ROOT=\"$(git rev-parse --show-toplevel 2>/dev/null || pwd -P)\"; "
                'printf "root=%s\\n" "$ROOT"; '
                'printf "workdir=%s\\n" "$(pwd -P)"; '
                "if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then "
                'printf "head=%s\\n" "$(git rev-parse HEAD)"; '
                "printf \"tree=%s\\n\" \"$(git status --porcelain | sha256sum | cut -d' ' -f1)\"; "
                "else "
                'printf "head=none\\n"; '
                "printf \"tree=%s\\n\" \"$(find . -type f -not -path './.slopdex/*' "
                "-printf '%p %s\\n' | sort | sha256sum | cut -d' ' -f1)\"; "
                "fi"
            ),
        )
        fields = dict(
            line.split("=", 1)
            for line in (result.stdout or "").splitlines()
            if "=" in line
        )
        identity: dict[str, object] = {
            "root": fields.get("root", ""),
            "workdir": fields.get("workdir", ""),
            "head": fields.get("head", ""),
            "tree": fields.get("tree", ""),
            "slopdex_mode": self._slopdex_mode,
            "slopdex_version": self._slopdex_installed_version,
            "description_provider": self._description_provider,
            "description_model": self._description_model,
            "env_keys": sorted(
                key
                for key in self._slopdex_env()
                if key in ("JINA_API_KEY", "OPENAI_API_KEY")
            ),
        }
        if fields.get("head") == "none":
            # Non-git task dirs: the file listing has no content hashes, so
            # fold in the instruction to separate tasks over similar trees.
            identity["instruction"] = hashlib.sha256(instruction.encode()).hexdigest()
        return identity

    @staticmethod
    def _cache_key(identity: dict[str, object]) -> str:
        payload = json.dumps(identity, sort_keys=True, separators=(",", ":"))
        return hashlib.sha256(payload.encode()).hexdigest()[:32]

    async def _restore_index(
        self, environment: BaseEnvironment, identity: dict[str, object], entry: Path
    ) -> bool:
        if not (entry / "index.sqlite").exists():
            return False
        remote_dir = f"{identity['root']}/.slopdex"
        restored: list[str] = []
        for name in SLOPDEX_INDEX_ARTIFACTS:
            source = entry / name
            if not source.exists():
                continue
            await self._upload_agent_owned_file(
                environment, source, f"{remote_dir}/{name}"
            )
            restored.append(name)
        self.logger.info("Restored slopdex index from %s (%s)", entry, ", ".join(restored))
        return True

    async def _store_index(
        self,
        environment: BaseEnvironment,
        identity: dict[str, object],
        key: str,
    ) -> None:
        root = self._cache_root()
        tmp = root / f".{key}.tmp"
        shutil.rmtree(tmp, ignore_errors=True)
        tmp.mkdir(parents=True, exist_ok=True)
        remote_dir = f"{identity['root']}/.slopdex"
        stored: list[str] = []
        for name in SLOPDEX_INDEX_ARTIFACTS:
            try:
                await environment.download_file(f"{remote_dir}/{name}", tmp / name)
                stored.append(name)
            except Exception:
                self.logger.debug("No %s to download from the container", name)
        if not stored:
            shutil.rmtree(tmp, ignore_errors=True)
            raise RuntimeError("no slopdex index artifacts were downloaded")
        status: object | None = None
        try:
            result = await self.exec_as_agent(
                environment, command="slopdex status", env=self._slopdex_env()
            )
            status = json.loads(result.stdout)
        except Exception:
            self.logger.debug("Could not capture slopdex status for cache metadata")
        meta = {
            "identity": identity,
            "cache_key": key,
            "slopdex_version": self._slopdex_installed_version,
            "created_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "artifacts": stored,
            "index_status": status,
        }
        (tmp / "meta.json").write_text(json.dumps(meta, indent=2) + "\n")
        entry = root / key
        shutil.rmtree(entry, ignore_errors=True)
        os.replace(tmp, entry)
        self.logger.info("Cached slopdex index at %s (%s)", entry, ", ".join(stored))

    async def _prepare_index(
        self, environment: BaseEnvironment, identity: dict[str, object]
    ) -> None:
        if not self._use_cache:
            if self._prebuild_index:
                await self._prebuild(environment)
            return
        root = self._cache_root()
        root.mkdir(parents=True, exist_ok=True)
        key = self._cache_key(identity)
        entry = root / key
        lock = FileLock(
            str(root / f"{key}.lock"),
            timeout=self._prebuild_timeout_sec + 900,
            thread_local=False,
        )
        try:
            # Never block the event loop: Harbor may run concurrent trials in
            # one process, and this lock can be held across a long prebuild.
            # thread_local=False because acquire/release may run on different
            # threads when offloaded with asyncio.to_thread.
            await asyncio.to_thread(lock.acquire)
        except Timeout:
            self.logger.warning(
                "Timed out waiting for the slopdex index cache lock; "
                "building without caching"
            )
            if self._prebuild_index:
                await self._prebuild(environment)
            return
        try:
            if await self._restore_index(environment, identity, entry):
                return
            if not self._prebuild_index:
                return
            if await self._prebuild(environment):
                try:
                    await self._store_index(environment, identity, key)
                except Exception:
                    self.logger.exception("Failed to cache the slopdex index")
        finally:
            await asyncio.to_thread(lock.release)

    async def _prebuild(self, environment: BaseEnvironment) -> bool:
        env = self._slopdex_env()
        has_embedding_key = "JINA_API_KEY" in env or "OPENAI_API_KEY" in env
        if not has_embedding_key:
            self.logger.warning(
                "No JINA_API_KEY/OPENAI_API_KEY in the agent env; slopdex "
                "commands will fail until one is passed via --ae"
            )
        if self._slopdex_mode == "descriptions" and "OPENCODE_API_KEY" not in env:
            self.logger.warning(
                "No OPENCODE_API_KEY in the agent env; description generation "
                "will fail until one is passed via --ae"
            )
        try:
            await self.exec_as_agent(
                environment,
                command=(
                    'ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"; '
                    'cd "$ROOT"; slopdex update-git'
                ),
                env=env,
                timeout_sec=self._prebuild_timeout_sec,
            )
            return True
        except Exception:
            self.logger.exception(
                "slopdex index prebuild failed; the agent will build the "
                "index lazily on first search"
            )
            return False

    @override
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        await self._prepare_workspace(environment, instruction)
        try:
            await super().run(instruction, environment, context)
        finally:
            await self._cleanup_workspace(environment)
