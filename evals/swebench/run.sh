#!/usr/bin/env bash
# Run one SWE-bench arm: opencode (+ optional slopdex) inside the official
# per-instance containers, patches out, graded by the official harness.
#
#   MODE=off|grep|slopdex [INDEX_MODE=vector|descriptions] ./run.sh
#
# Requires: docker, python3, an OPENCODE_API_KEY for the agent, and for
# MODE=slopdex a JINA_API_KEY or OPENAI_API_KEY (plus OPENCODE_API_KEY for
# INDEX_MODE=descriptions). Results land in jobs/<job>/ with predictions.jsonl.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
MODE="${MODE:-off}"
INDEX_MODE="${INDEX_MODE:-vector}"
MODEL="${MODEL:-opencode-go/muse-spark-1.3-contributor}"
INSTANCES="${INSTANCES:-$ROOT/instances.txt}"
AGENT_TIMEOUT="${AGENT_TIMEOUT:-3600}"
THRESHOLD="${THRESHOLD:-0.5}"
CACHE_DIR="${CACHE_DIR:-$ROOT/cache}"
JOB_NAME="${JOB_NAME:-swe-$MODE-$INDEX_MODE}"
JOB_DIR="${JOB_DIR:-$ROOT/jobs/$JOB_NAME}"
mkdir -p "$JOB_DIR/problems" "$JOB_DIR/patches" "$JOB_DIR/logs"

export MODE INDEX_MODE MODEL JOB_DIR CACHE_DIR AGENT_TIMEOUT THRESHOLD
export OPENCODE_VERSION="${OPENCODE_VERSION:-}" SLOPDEX_VERSION="${SLOPDEX_VERSION:-}"
export DESCRIPTION_PROVIDER="${DESCRIPTION_PROVIDER:-opencode-go}"
export DESCRIPTION_MODEL="${DESCRIPTION_MODEL:-deepseek-v4.1-flash}"
export PREBUILD_INDEX="${PREBUILD_INDEX:-true}" PREBUILD_TIMEOUT="${PREBUILD_TIMEOUT:-0}"

if [[ "$MODE" == "slopdex" && -z "${JINA_API_KEY:-}${OPENAI_API_KEY:-}" ]]; then
  echo "MODE=slopdex needs JINA_API_KEY or OPENAI_API_KEY" >&2; exit 1
fi
if [[ -z "${OPENCODE_API_KEY:-}" ]]; then
  echo "warning: OPENCODE_API_KEY is not set; the agent cannot authenticate" >&2
fi

# --- fetch problem statements once (public datasets-server API, no auth) ---
mapfile -t IDS < <(grep -vE '^\s*(#|$)' "$INSTANCES")
printf '%s\n' "${IDS[@]}" > "$JOB_DIR/wanted.txt"
python3 - "$JOB_DIR/wanted.txt" "$JOB_DIR/rows.json" <<'PYEOF'
import json, sys, urllib.request
wanted = {l.strip() for l in open(sys.argv[1]) if l.strip()}
base = ("https://datasets-server.huggingface.co/rows?dataset=SWE-bench%2FSWE-bench_Verified"
        "&config=default&split=test&offset={}&length=100")
rows, offset, total = {}, 0, None
while total is None or len(rows) < total:
    with urllib.request.urlopen(base.format(offset), timeout=120) as r:
        d = json.load(r)
    total = d["num_rows_total"]
    for row in d["rows"]:
        if row["row"]["instance_id"] in wanted:
            rows[row["row"]["instance_id"]] = row["row"]
    offset += 100
missing = wanted - set(rows)
assert not missing, f"instances not in dataset: {missing}"
json.dump(rows, open(sys.argv[2], "w"))
print(f"fetched {len(rows)} problem statements")
PYEOF

: > "$JOB_DIR/predictions.jsonl"
for id in "${IDS[@]}"; do
  python3 -c "
import json
rows = json.load(open('$JOB_DIR/rows.json'))
open('$JOB_DIR/problems/$id.md', 'w').write(rows['$id']['problem_statement'])
"
  bash "$ROOT/run_instance.sh" "$id" || echo "FAILED: $id (continuing)" >&2
done

echo "=== $JOB_NAME: $(wc -l < "$JOB_DIR/predictions.jsonl") predictions in $JOB_DIR/predictions.jsonl ==="
echo "grade with: ./grade.sh \"$JOB_DIR\""
