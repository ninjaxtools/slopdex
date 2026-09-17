#!/usr/bin/env bash
# Treatment + index cache for one SWE-bench container. Called by run_instance.sh.
# Env: MODE, INDEX_MODE, THRESHOLD, CACHE_DIR, PREBUILD_TIMEOUT (0 = per-mode
# default), DESCRIPTION_PROVIDER, DESCRIPTION_MODEL, *_API_KEY passthrough.
# Source of truth for the AGENTS.md text is evals/harbor/slopdex_opencode.py.
set -euo pipefail

CNAME="$1"
ROOT="$(cd "$(dirname "$0")" && pwd)"
WORKDIR=/testbed
MARKER="<!-- slopdex-eval -->"
THRESHOLD="${THRESHOLD:-0.5}"
CACHE_DIR="${CACHE_DIR:-$ROOT/cache}"
mkdir -p "$CACHE_DIR"

if [[ "$MODE" == "slopdex" ]]; then
  BLOCK=$(printf '%s\n## Code search\n\n- Use semantic code search to find code by meaning instead of guessing paths:\n  `slopdex search "<describe the code you need>" --threshold %s`\n- Run `slopdex --help` for all commands and options.\n' "$MARKER" "$THRESHOLD")
elif [[ "$MODE" == "grep" ]]; then
  BLOCK=$(printf '%s\n## Code search\n\n- Use keyword search to find code by meaning instead of guessing paths:\n  `rg -n "<describe the code you need>"` (or `grep -rn "<pattern>" .`)\n- Run `rg --help` for all options.\n' "$MARKER")
else
  BLOCK=""
fi

if [[ -n "$BLOCK" ]]; then
  printf '%s' "$BLOCK" | docker exec -i "$CNAME" bash -lc "cd $WORKDIR && cp AGENTS.md /tmp/swe/agents_backup.md 2>/dev/null || rm -f /tmp/swe/agents_backup.md; grep -qF '$MARKER' AGENTS.md 2>/dev/null || cat >> AGENTS.md"
fi

[[ "$MODE" == "slopdex" ]] || exit 0

# --- slopdex config (mirrors the Harbor plugin) ---
if [[ "$INDEX_MODE" == "descriptions" ]]; then
  CONFIG=$(printf '{"descriptionProvider": "%s", "descriptionModel": "%s", "descriptionsEnabled": true}' "${DESCRIPTION_PROVIDER:-opencode-go}" "${DESCRIPTION_MODEL:-deepseek-v4.1-flash}")
else
  CONFIG='{"descriptionsEnabled": false}'
fi
printf '%s' "$CONFIG" | docker exec -i "$CNAME" bash -lc "cd $WORKDIR && mkdir -p .slopdex && cat > .slopdex/config.json"

# --- cache key ---
HEAD=$(docker exec -i "$CNAME" bash -lc "cd $WORKDIR && git rev-parse HEAD")
TREE=$(docker exec -i "$CNAME" bash -lc "cd $WORKDIR && git status --porcelain | sha256sum" | cut -d' ' -f1)
SLOPV=$(docker exec -i "$CNAME" bash -lc 'slopdex --version' | tr -d '[:space:]')
ENVKEYS=""
[[ -n "${JINA_API_KEY:-}" ]] && ENVKEYS="${ENVKEYS}JINA_API_KEY,"
[[ -n "${OPENAI_API_KEY:-}" ]] && ENVKEYS="${ENVKEYS}OPENAI_API_KEY,"
export SWE_HEAD="$HEAD" SWE_TREE="$TREE" SWE_INDEX_MODE="$INDEX_MODE" SWE_SLOPV="$SLOPV" SWE_ENVKEYS="$ENVKEYS"
export SWE_DESC_PROVIDER="${DESCRIPTION_PROVIDER:-opencode-go}" SWE_DESC_MODEL="${DESCRIPTION_MODEL:-deepseek-v4.1-flash}"
KEY=$(python3 - <<'PYEOF'
import hashlib, json, os
identity = {
    "root": "/testbed",
    "workdir": "/testbed",
    "head": os.environ["SWE_HEAD"],
    "tree": os.environ["SWE_TREE"],
    "slopdex_mode": os.environ["SWE_INDEX_MODE"],
    "slopdex_version": os.environ["SWE_SLOPV"],
    "description_provider": os.environ.get("SWE_DESC_PROVIDER", "opencode-go"),
    "description_model": os.environ.get("SWE_DESC_MODEL", "deepseek-v4.1-flash"),
    "env_keys": sorted(k for k in os.environ.get("SWE_ENVKEYS", "").split(",") if k),
}
print(hashlib.sha256(json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()).hexdigest()[:32])
PYEOF
)

if [[ "$INDEX_MODE" == "descriptions" ]]; then DEFTIMEOUT=3600; else DEFTIMEOUT=1800; fi
PREBUILD_TIMEOUT="${PREBUILD_TIMEOUT:-$DEFTIMEOUT}"
LOCK="$CACHE_DIR/$KEY.lock"
ENTRY="$CACHE_DIR/$KEY"

sloxec() { docker exec -i "$CNAME" bash -lc "cd $WORKDIR && $1"; }
sloxec_env() {
  local envs=()
  [[ -n "${JINA_API_KEY:-}" ]] && envs+=(-e "JINA_API_KEY=$JINA_API_KEY")
  [[ -n "${OPENAI_API_KEY:-}" ]] && envs+=(-e "OPENAI_API_KEY=$OPENAI_API_KEY")
  [[ -n "${OPENCODE_API_KEY:-}" ]] && envs+=(-e "OPENCODE_API_KEY=$OPENCODE_API_KEY")
  [[ -n "${COHERE_API_KEY:-}" ]] && envs+=(-e "COHERE_API_KEY=$COHERE_API_KEY")
  docker exec -i "${envs[@]}" "$CNAME" bash -lc "cd $WORKDIR && $1"
}

restore() {
  [[ -f "$ENTRY/index.sqlite" ]] || return 1
  for f in index.sqlite index.sqlite-wal index.sqlite-shm; do
    [[ -f "$ENTRY/$f" ]] && docker cp "$ENTRY/$f" "$CNAME:$WORKDIR/.slopdex/$f"
  done
  echo "index restored from cache $KEY"
  return 0
}

prebuild_and_store() {
  [[ "${PREBUILD_INDEX:-true}" == "true" ]] || return 0
  if ! sloxec_env "slopdex update-git"; then
    echo "prebuild failed, agent will build lazily" >&2
    return 0
  fi
  local tmp="$CACHE_DIR/.$KEY.tmp"
  rm -rf "$tmp"; mkdir -p "$tmp"
  local stored=0
  for f in index.sqlite index.sqlite-wal index.sqlite-shm; do
    if docker cp "$CNAME:$WORKDIR/.slopdex/$f" "$tmp/$f" 2>/dev/null; then stored=1; fi
  done
  if [[ $stored -eq 0 ]]; then rm -rf "$tmp"; echo "nothing to cache" >&2; return 0; fi
  local status="null"
  if out=$(sloxec_env "slopdex status" 2>/dev/null); then
    status=$(printf '%s' "$out" | python3 -c "import json,sys; print(json.dumps(json.load(sys.stdin)))" 2>/dev/null || echo "null")
  fi
  python3 - "$KEY" "$SLOPV" "$status" <<'PYEOF' > "$tmp/meta.json"
import json, sys, time
print(json.dumps({
    "cache_key": sys.argv[1],
    "slopdex_version": sys.argv[2],
    "created_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "index_status": json.loads(sys.argv[3]) if sys.argv[3] != "null" else None,
}, indent=2))
PYEOF
  rm -rf "$ENTRY"; mv "$tmp" "$ENTRY"
  echo "index cached at $ENTRY"
}

(
  # shellcheck disable=SC2094
  flock -w $((PREBUILD_TIMEOUT + 900)) 9 || { echo "cache lock timeout, building without cache"; }
  if [[ -d "$ENTRY" ]] && restore; then exit 0; fi
  prebuild_and_store
) 9>"$LOCK"
