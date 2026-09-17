#!/usr/bin/env bash
# Grade a predictions.jsonl with the official SWE-bench harness against the
# epoch.ai image mirror (same images the runner uses).
#
#   ./grade.sh <job-dir> [run-id] [max-workers]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
JOB_DIR="${1:?usage: grade.sh <job-dir> [run-id] [max-workers]}"
RUN_ID="${2:-$(basename "$JOB_DIR")}"
WORKERS="${3:-4}"
PRED="$JOB_DIR/predictions.jsonl"
[[ -f "$PRED" ]] || { echo "no $PRED" >&2; exit 1; }

UV="${UV:-$(command -v uv || echo "$HOME/.local/bin/uv")}"
if [[ ! -x "$ROOT/.venv/bin/python" ]]; then
  "$UV" venv "$ROOT/.venv" --python 3.11 2>/dev/null || "$UV" venv "$ROOT/.venv"
fi
"$UV" pip install --python "$ROOT/.venv/bin/python" -q swebench 2>&1 | tail -1 || true

# Retag epoch mirror images to the names the official harness expects:
# sweb.eval.x86_64/sweb.eval.x86_64.<id with __ -> _1776_>:latest
python3 - "$PRED" <<'PYEOF' | bash
import json, subprocess, sys
seen = set()
for line in open(sys.argv[1]):
    iid = json.loads(line)["instance_id"]
    if iid in seen:
        continue
    seen.add(iid)
    src = f"ghcr.io/epoch-research/swe-bench.eval.x86_64.{iid}:latest"
    dst = f"sweb.eval.x86_64/sweb.eval.x86_64.{iid.replace('__', '_1776_')}:latest"
    print(f"docker pull -q {src} >/dev/null")
    print(f"docker tag {src} {dst}")
PYEOF

"$ROOT/.venv/bin/python" -m swebench.harness.run_evaluation \
  --dataset_name SWE-bench/SWE-bench_Verified \
  --predictions_path "$PRED" \
  --max_workers "$WORKERS" \
  --run_id "$RUN_ID" \
  --namespace sweb.eval.x86_64 \
  --cache_level env
