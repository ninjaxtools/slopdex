# TypeScript: baseline versus README map

Compare conventional local navigation (**`off`**, no slopdex) with **`map`** using
the exact agent instruction from the root [README.md](../../README.md). There are
no search, combined-tool, or experimental-prompt evaluations.

The corpus contains **20 tasks and 80 reference declarations** in the Go-native
TypeScript compiler. The default matrix is two models at medium thinking, one
repetition, and two arms: 80 results, but **only missing or changed trials run**.

## Run and compare

From the repository root:

```sh
# No downloads or model calls.
python3 evals/typescript/run.py list
python3 evals/typescript/run.py run --dry-run

# Read the checked-in comparison without any executable or model calls.
python3 evals/typescript/run.py report

# Rebuild slopdex after implementation changes, then evaluate.
cargo build --release
PATH="/path/to/opencode-v1:$PWD/target/release:$PATH" \
  python3 evals/typescript/run.py run
```

The runner automatically reuses matching checked-in baselines and the latest map
results. With unchanged controls/builds it makes **zero model calls**. Changing
only the map implementation or README instruction reruns map, not baseline.
Changing a model or its reasoning options requires a new baseline for that
setting; existing settings remain reusable, including when selecting a subset.

```sh
# Optional source/index validation and preparation.
python3 evals/typescript/run.py validate
PATH="$PWD/target/release:$PATH" python3 evals/typescript/run.py prepare

# Select a configured model/task, or just one of the two arms.
python3 evals/typescript/run.py run \
  --model opencode-go/deepseek-v4.1-flash@medium \
  --task ambiguous-expression-type-arguments
python3 evals/typescript/run.py run --arms off
python3 evals/typescript/run.py run --arms map

# Raw jobs are resumable, with identical controls and build identity.
python3 evals/typescript/run.py run --output evals/typescript/jobs/my-run
python3 evals/typescript/run.py run --output evals/typescript/jobs/my-run

# Import an already-completed run without rerunning it.
python3 evals/typescript/run.py save evals/typescript/jobs/map-prompt-validation
python3 evals/typescript/run.py save evals/typescript/jobs/map-readme-frontload-039
python3 evals/typescript/run.py report

# Regenerate a raw job report rather than the checked-in comparison.
python3 evals/typescript/run.py report evals/typescript/jobs/my-run
```

Use `--config PATH`, `--cache PATH`, and `--results PATH` **before** the subcommand
to choose alternative settings, caches, or a separate result store. For example:

```sh
python3 evals/typescript/run.py --results /tmp/opencode/eval-results run --arms off
```

Requirements: Linux/macOS, Python 3.11+, Git, OpenCode **V1**, and slopdex on `PATH`
when running map. The saved runs use **OpenCode 1.18.34**; use that version to reuse
their baselines. V2's configuration and usage events are not supported by this
harness. Authenticate to the configured provider (`opencode-go` by default).
Credentials are copied only into disposable trial directories and never checked
in. Supply embedding credentials through the environment, not `config.json`.

Map preparation and new `-q` queries need an embedding key (`OPENAI_API_KEY` for
the default OpenAI configuration). Baseline needs no slopdex or embedding key.
Neither TypeScript build dependencies nor a Go toolchain are required.

For limited RAM-backed temporary space, use disk-backed scratch:

```sh
mkdir -p evals/typescript/jobs/tmp
TMPDIR="$PWD/evals/typescript/jobs/tmp" python3 evals/typescript/run.py run
```

## Checked-in results and invalidation

[`results/report.md`](results/report.md) and [`results/summary.json`](results/summary.json)
compare the saved baseline with the latest complete map evaluation.

- `results/baselines/*.json`: immutable baseline datasets. Each trial stores its
  exact prompt, agent instructions, model/variant/options, OpenCode controls and
  version, TypeScript commit, answer, grade, time, tokens, steps, cost, and reuse
  fingerprint. Adding new settings does not discard old baselines.
- `results/latest-map.json`: the latest complete map dataset, with the same
  evidence plus the README guidance, slopdex settings/version, harness hash,
  slopdex source commit, source-content hash (including dirty source), and binary
  hash. A partial run never replaces the latest complete dataset.
- `jobs/`: ignored full traces, generated CLI configs, commands, validation logs,
  and resumable manifests. `cache/`: ignored prepared indexes. Neither is needed
  to read the checked-in comparison or reuse portable results.

Reuse is per model/task/repetition, not just per variant name. Baseline keys cover
the task and reference source, full prompt/instructions, model provider ID and
reasoning options, OpenCode version/config, timeout, and step limit. They
deliberately exclude slopdex configuration, implementation, and README map
guidance. Map adds those controls and the harness/build identity. Rebuild slopdex
before evaluating changed source: both the source hash and actual executable
hash are recorded. A Git commit change also invalidates map. Report-only
regeneration never changes the measured evidence.

The imported baseline has 80 completed trials (two repetitions) from the saved
`map-prompt-validation` job, with exactly matching prompts, settings, and command
permissions. The default comparison uses repetition 1 for its 40 pairs; both
repetitions are reusable. An older `baseline-limit-50` job used a broader bash
denial rule and is not silently treated as the current baseline.
The imported `map-readme-frontload-039` has 40 completed trials
using slopdex 0.39.0 and the current frontloaded README instruction. Its original
manifest did not record source/binary hashes: these remain unknown, rather than
being invented at import. It is available for comparison, but the first current
map run conservatively reruns it to establish a reusable build fingerprint.
The original harness hash and tool versions remain saved.

Changes to model-facing protocol or scoring semantics should bump
`saved.PROTOCOL_VERSION`; cosmetic reporting changes need not invalidate a
baseline. Check in newly generated `results/` along with relevant code changes.

## What is measured

Reports compare **map − baseline**, grouped by model and thinking setting:

- Execution wall time (agent run only) and `step_finish` count.
- Input, output, reasoning, cache-read, cache-write, and total tokens.
- OpenCode-reported USD cost, with missing cost telemetry kept as unknown.
- Citation F1, all-target passes, answer/protocol status, and map invocations.
- Per-task differences and aggregate absolute/percentage changes.

Total tokens use OpenCode's reported total when available; otherwise they sum
the distinct V1 input/output/reasoning/cache categories. Reports retain the
original categories and identify the total's source. USD is model-call spend,
not subscription billing or slopdex embedding costs.

All-trial totals include resources consumed by failures. Protocol-valid and
both-passed paired cohorts are reported separately: a failed or shortened answer
must not be presented as an efficiency improvement. Accuracy includes failures
as zero. Saved-baseline comparisons are explicitly **not interleaved**; elapsed
time and provider caching can vary between runs.

## Source, controls, and grading

`repo/` is a Git submodule pinned to TypeScript commit
`edf7da4e93ab65e1f7766deae156c35132c0cde3`. Source-dependent commands initialize
the recorded gitlink, never a moving branch, and verify it against `tasks.json`.
Every executed trial gets an independent clone. There are ten intermediate tasks
(three findings each) and ten advanced tasks (five findings across at least three
files). Agents see the role-based prompt, not reference symbols or paths.

Default models in `config.json`:

- `opencode-go/deepseek-v4.1-flash@medium`
- `opencode-go/muse-spark-1.3-contributor@medium`
- Explicit `{"reasoningEffort": "medium"}`, 600-second timeout, 40 steps.

Trials run sequentially in a seeded task/model order, alternating arm order when
both are fresh. Both arms allow glob, grep, and reads. Map uses the root README
snippet verbatim and must successfully invoke `slopdex map`; other slopdex
navigation and root/index/config overrides are blocked. Baseline denies all
slopdex calls. Missing required map use or disallowed commands is a protocol
violation. These are cooperative controls, not a security sandbox.

Global configs, instructions, plugins, and previous sessions are isolated.
Editing, delegation, network tools, interactive questions, and LSP are disabled.
Effective OpenCode controls are checked before timing. Source edits are checked
afterward.

The map index contains structure, normalized-name vectors, and attached source
description vectors, not full code-body embeddings. Preparation uses an isolated
schema-4 artifact store. Each trial gets its own workspace/store backup and
warmed shared ANN base. An offline `map -q` preflight clears selector caches and
removes provider credentials, checking graph hashes/mtimes and embedding counts.
Preparation, copying, and preflight are excluded from agent timing and USD;
ordinary map loading and query embedding latency remain timed.

Answers must be JSON with exactly the requested findings (`path`, `symbol`,
`line`, `explanation`) and a nonempty `flow`. Citations must identify the first
declaration line or either of the next two lines. Duplicate findings and
call-site citations do not earn extra credit. F1 measures declaration evidence,
not explanation correctness; review saved answers manually. No model judge is
used.

## Verification

```sh
python3 -m unittest discover -s evals/typescript -v
cargo verify

# Optional installed-CLI checks, without external model calls.
SLOPDEX_EVAL_CHECK_OPENCODE=1 python3 -m unittest discover -s evals/typescript -v
SLOPDEX_EVAL_CHECK_ANN=1 python3 -m unittest discover -s evals/typescript -p test_native_ann.py -v
```

To add tasks, update `tasks.json` and validate every reference declaration. To
update TypeScript, deliberately update its gitlink and `repository_commit`, then
review and validate all tasks; this invalidates the baseline as well as map.
