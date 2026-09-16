#!/usr/bin/env bash
# Run one arm of the slopdex Terminal-Bench pilot.
#
#   MODE=off|grep|slopdex MODEL=<provider/model> [INDEX_MODE=vector|descriptions] ./run.sh
#
# Requires: `harbor` on PATH (`uv tool install 'harbor[modal]'`), a running
# Docker daemon, an embedding key for MODE=slopdex (JINA_API_KEY or
# OPENAI_API_KEY), and for INDEX_MODE=descriptions an OPENCODE_API_KEY.
#
# The slopdex index is cached under ./cache/<key>/ (keyed by repo state,
# index mode, provider keys and slopdex version) so vectors and descriptions
# are generated once and reused across runs. Set USE_CACHE=false to disable,
# or CACHE_DIR=/path to relocate the cache.
set -euo pipefail

cd "$(dirname "$0")"
export PYTHONPATH="$PWD${PYTHONPATH:+:$PYTHONPATH}"

MODE="${MODE:-off}"
INDEX_MODE="${INDEX_MODE:-vector}"
MODEL="${MODEL:-opencode-go/muse-spark-1.3-contributor}"
DATASET="${DATASET:-terminal-bench@2.0}"
N_TASKS="${N_TASKS:-10}"
ATTEMPTS="${ATTEMPTS:-1}"
CONCURRENCY="${CONCURRENCY:-4}"
JOB_NAME="${JOB_NAME:-slopdex-$MODE-$INDEX_MODE}"
JOBS_DIR="${JOBS_DIR:-$PWD/jobs}"

AK=(--ak "mode=$MODE")
if [[ "$MODE" == "slopdex" ]]; then
  AK+=(--ak "slopdex_mode=$INDEX_MODE")
fi
if [[ -n "${SLOPDEX_VERSION:-}" ]]; then
  AK+=(--ak "slopdex_version=$SLOPDEX_VERSION")
fi
if [[ -n "${SLOPDEX_THRESHOLD:-}" ]]; then
  AK+=(--ak "slopdex_threshold=$SLOPDEX_THRESHOLD")
fi
if [[ -n "${DESCRIPTION_PROVIDER:-}" ]]; then
  AK+=(--ak "description_provider=$DESCRIPTION_PROVIDER")
fi
if [[ -n "${DESCRIPTION_MODEL:-}" ]]; then
  AK+=(--ak "description_model=$DESCRIPTION_MODEL")
fi
if [[ -n "${PREBUILD_INDEX:-}" ]]; then
  AK+=(--ak "prebuild_index=$PREBUILD_INDEX")
fi
if [[ -n "${PREBUILD_TIMEOUT_SEC:-}" ]]; then
  AK+=(--ak "prebuild_timeout_sec=$PREBUILD_TIMEOUT_SEC")
fi
if [[ -n "${USE_CACHE:-}" ]]; then
  AK+=(--ak "use_cache=$USE_CACHE")
fi
if [[ -n "${CACHE_DIR:-}" ]]; then
  AK+=(--ak "cache_dir=$CACHE_DIR")
fi

AE=()
if [[ -n "${JINA_API_KEY:-}" ]]; then
  AE+=(--ae "JINA_API_KEY=$JINA_API_KEY")
fi
if [[ -n "${OPENAI_API_KEY:-}" ]]; then
  AE+=(--ae "OPENAI_API_KEY=$OPENAI_API_KEY")
fi
if [[ -n "${OPENCODE_API_KEY:-}" ]]; then
  AE+=(--ae "OPENCODE_API_KEY=$OPENCODE_API_KEY")
fi
if [[ -n "${COHERE_API_KEY:-}" ]]; then
  AE+=(--ae "COHERE_API_KEY=$COHERE_API_KEY")
fi

harbor run \
  -d "$DATASET" \
  --agent slopdex_opencode:SlopdexOpenCode \
  --model "$MODEL" \
  "${AK[@]}" \
  "${AE[@]}" \
  --n-tasks "$N_TASKS" \
  -k "$ATTEMPTS" \
  -n "$CONCURRENCY" \
  --job-name "$JOB_NAME" \
  -o "$JOBS_DIR" \
  "$@"
