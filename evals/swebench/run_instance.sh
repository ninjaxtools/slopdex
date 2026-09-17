#!/usr/bin/env bash
# Per-instance SWE-bench runner. Called by run.sh, not directly.
# Env (all required unless noted): INSTANCE, MODE, INDEX_MODE, MODEL, JOB_DIR,
# CACHE_DIR, AGENT_TIMEOUT, THRESHOLD, SLOPDEX_VERSION (optional),
# OPENCODE_API_KEY, JINA_API_KEY/OPENAI_API_KEY (slopdex arm), OPENCODE_API_KEY (descriptions).
set -euo pipefail

INSTANCE="$1"
ROOT="$(cd "$(dirname "$0")" && pwd)"
IMAGE="ghcr.io/epoch-research/swe-bench.eval.x86_64.${INSTANCE}:latest"
CNAME="swe-$(echo "$INSTANCE" | tr '_' '-')-$$"
WORKDIR=/testbed
MODEL="${MODEL:-opencode-go/muse-spark-1.3-contributor}"
PATCHES="$JOB_DIR/patches"
LOGS="$JOB_DIR/logs"
mkdir -p "$PATCHES" "$LOGS"

cleanup() { docker rm -f "$CNAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT

dexec() { docker exec -i "$CNAME" "$@"; }

echo "=== $INSTANCE ($MODE/$INDEX_MODE) ==="
docker pull -q "$IMAGE"
docker run -d --name "$CNAME" "$IMAGE" sleep infinity >/dev/null

# --- 1. toolchain: pinned Node 24 system-wide (no nvm, no PATH games) ---
NODE_VER="24.15.0"
NODE_URL="https://nodejs.org/dist/v${NODE_VER}/node-v${NODE_VER}-linux-x64.tar.xz"
dexec python3 - "$NODE_URL" <<'PYEOF'
import sys, urllib.request
urllib.request.urlretrieve(sys.argv[1], "/tmp/node.tar.xz")
PYEOF
dexec bash -lc 'rm -rf /tmp/node-dist && mkdir -p /tmp/node-dist && (tar -xJf /tmp/node.tar.xz -C /tmp/node-dist || python3 -c "import tarfile; tarfile.open(\"/tmp/node.tar.xz\").extractall(\"/tmp/node-dist\")") && cp -r /tmp/node-dist/node-v*-linux-x64/bin /tmp/node-dist/node-v*-linux-x64/lib /usr/local/ && node --version && npm --version'
dexec bash -lc "npm install -g opencode-ai${OPENCODE_VERSION:+@$OPENCODE_VERSION} && opencode --version"
if [[ "$MODE" == "slopdex" ]]; then
  dexec bash -lc "npm install -g @ninjaxtools/slopdex${SLOPDEX_VERSION:+@$SLOPDEX_VERSION} && slopdex --version"
fi
if [[ "$MODE" == "grep" ]]; then
  dexec bash -lc 'command -v rg >/dev/null || (apt-get update -qq && apt-get install -y -qq ripgrep) || true'
fi

# --- 2. opencode provider config (mirrors Harbor: models only, built-in baseURL) ---
MODEL_ID="${MODEL#*/}"
dexec bash -lc 'mkdir -p /root/.config/opencode' 
printf '{"provider": {"%s": {"models": {"%s": {}}}}}' "${MODEL%%/*}" "$MODEL_ID" \
  | dexec bash -lc 'tee /root/.config/opencode/opencode.json >/dev/null'

# --- 3. problem statement + treatment files ---
PROBLEM_FILE="$JOB_DIR/problems/${INSTANCE}.md"
dexec bash -lc 'mkdir -p /tmp/swe && cat > /tmp/swe/problem.md' < "$PROBLEM_FILE"
bash "$ROOT/index_prep.sh" "$CNAME"

# --- 4. run the agent ---
cat > "$LOGS/${INSTANCE}.run.sh" <<EOF
#!/bin/bash
cd $WORKDIR
timeout ${AGENT_TIMEOUT}s opencode --model=${MODEL} run --format=json --thinking --dangerously-skip-permissions -- "\$(cat /tmp/swe/problem.md)" > /tmp/swe/opencode.log 2>&1
echo "AGENT_EXIT=\$?"
EOF
docker cp "$LOGS/${INSTANCE}.run.sh" "$CNAME:/tmp/swe/run-agent.sh"
dexec bash -lc 'chmod +x /tmp/swe/run-agent.sh && /tmp/swe/run-agent.sh' 2>&1 | tee "$LOGS/${INSTANCE}.agent.out" || true
docker cp "$CNAME:/tmp/swe/opencode.log" "$LOGS/${INSTANCE}.opencode.log" 2>/dev/null || true

# --- 5. workspace cleanup: remove harness traces before diffing ---
dexec bash -lc 'cd /testbed && if git ls-files --error-unmatch AGENTS.md >/dev/null 2>&1; then git checkout -- AGENTS.md; elif [ -f /tmp/swe/agents_backup.md ]; then cp /tmp/swe/agents_backup.md AGENTS.md; else rm -f AGENTS.md; fi; rm -f /tmp/swe/agents_backup.md; if ! git ls-files --error-unmatch .slopdex >/dev/null 2>&1; then rm -rf .slopdex; fi' || true

# --- 6. extract patch (intent-to-add covers new files) ---
if dexec bash -lc 'cd /testbed && git add -A -N && git diff HEAD' > "$PATCHES/${INSTANCE}.diff" 2>"$LOGS/${INSTANCE}.diff.err"; then
  echo "patch: $(wc -l < "$PATCHES/${INSTANCE}.diff") lines"
else
  echo "diff failed, see $LOGS/${INSTANCE}.diff.err"
  : > "$PATCHES/${INSTANCE}.diff"
fi
python3 - "$INSTANCE" "$MODEL" "$PATCHES/${INSTANCE}.diff" "$JOB_DIR/predictions.jsonl" <<'PYEOF'
import json, sys
iid, model, diff_path, out = sys.argv[1:5]
with open(diff_path) as f:
    patch = f.read()
with open(out, "a") as f:
    f.write(json.dumps({"instance_id": iid, "model_patch": patch, "model_name": model}) + "\n")
PYEOF
echo "done: $INSTANCE"
