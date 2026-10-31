# Excalidraw: baseline versus README map

Compare conventional local navigation (**`off`**, no slopdex) with **`map`** using
the exact agent instruction from the root [README.md](../../README.md) by default.
Optional visibility switches compare map with and without private symbols. There
are no search, combined-tool, or experimental-prompt evaluations.

The corpus contains **20 tasks and 80 reference declarations** in the
[Excalidraw](https://github.com/excalidraw/excalidraw) TypeScript monorepo
(~140k non-test lines). The default matrix is two models at medium thinking, one
repetition, and two arms: 80 results, but **only missing or changed trials run**.

## Run and compare

From the repository root:

```sh
# No downloads or model calls.
python3 evals/excalidraw/run.py list
python3 evals/excalidraw/run.py run --dry-run

# Read the checked-in comparison without any executable or model calls.
python3 evals/excalidraw/run.py report

# Rebuild slopdex after implementation changes, then evaluate.
cargo build --release
PATH="$PWD/evals/excalidraw/cache/tools/opencode-v1-1.18.34:$PWD/target/release:$PATH" \
  python3 evals/excalidraw/run.py run
```

The runner automatically reuses matching checked-in baselines and the latest map
results. With unchanged controls/builds it makes **zero model calls**. Changing
only the map implementation or README instruction reruns map, not baseline.
Changing a model or its reasoning options requires a new baseline for that
setting; existing settings remain reusable, including when selecting a subset.

```sh
# Optional source/index validation and preparation.
python3 evals/excalidraw/run.py validate
PATH="$PWD/target/release:$PATH" python3 evals/excalidraw/run.py prepare

# Select a configured model/task, or just one of the two arms.
python3 evals/excalidraw/run.py run \
  --model opencode-go/deepseek-v4.1-flash@medium \
  --task collab-room-cloud-persistence
python3 evals/excalidraw/run.py run --arms off
python3 evals/excalidraw/run.py run --arms map

# Raw jobs are resumable, with identical controls and build identity.
python3 evals/excalidraw/run.py run --output evals/excalidraw/jobs/my-run
python3 evals/excalidraw/run.py run --output evals/excalidraw/jobs/my-run

# Import an already-completed run without rerunning it.
python3 evals/excalidraw/run.py save evals/excalidraw/jobs/my-run
python3 evals/excalidraw/run.py report

# Regenerate a raw job report rather than the checked-in comparison.
python3 evals/excalidraw/run.py report evals/excalidraw/jobs/my-run
```

Use `--config PATH`, `--cache PATH`, and `--results PATH` **before** the subcommand
to choose alternative settings, caches, or a separate result store. For example:

```sh
python3 evals/excalidraw/run.py --results /tmp/opencode/eval-results run --arms off
```

### Compare map with and without private symbols

`run --map-private` requires `--private` on every map navigation call;
`run --no-map-private` forbids it there. Help/version requests remain allowed.
Each override adjusts the README example and adds an explicit visibility
instruction. Omit both switches to preserve the exact README guidance and existing
policy. Overrides require the map arm; baseline instructions and reuse controls
stay unchanged.

Use **different `--results` stores and `--output` directories** for the two runs:
each result store retains only one `latest-map.json`, while an output directory
can resume only the same settings.

```sh
python3 evals/excalidraw/run.py --results /tmp/opencode/excalidraw-private run \
  --map-private --output evals/excalidraw/jobs/map-private
python3 evals/excalidraw/run.py --results /tmp/opencode/excalidraw-public run \
  --no-map-private --output evals/excalidraw/jobs/map-public

python3 evals/excalidraw/run.py --results /tmp/opencode/excalidraw-private report
python3 evals/excalidraw/run.py --results /tmp/opencode/excalidraw-public report
```

To reuse one baseline in both comparisons, import its completed baseline-only job
with `--results STORE save JOB` into each store before running. Private and public
map results have distinct reuse keys; neither silently reuses the other.

Requirements: Linux/macOS, Python 3.11+, Git, OpenCode **V1**, and slopdex on `PATH`
when running map. The saved runs use **OpenCode 1.18.34**; use that version to reuse
their baselines. V2's configuration and usage events are not supported by this
harness. Authenticate to the configured provider (`opencode-go` by default).
Credentials are copied only into disposable trial directories and never checked
in. Supply embedding credentials through the environment, not `config.json`.

Index preparation and new `-q`/`faq` queries need an embedding key (`OPENAI_API_KEY`
for the default OpenAI configuration). Baseline needs no slopdex or embedding key.
No Node/Yarn toolchain is required. The `opencode` first on `PATH` also answers
`slopdex faq` inside map trials, so put the V1 binary first.

For limited RAM-backed temporary space, use disk-backed scratch:

```sh
mkdir -p evals/excalidraw/jobs/tmp
TMPDIR="$PWD/evals/excalidraw/jobs/tmp" python3 evals/excalidraw/run.py run
```

## Checked-in results and invalidation

[`results/report.md`](results/report.md) and [`results/summary.json`](results/summary.json)
compare the saved baseline with the latest complete map evaluation.

- `results/baselines/*.json`: immutable baseline datasets. Each trial stores its
  exact prompt, agent instructions, model/variant/options, OpenCode controls and
  version, Excalidraw commit, answer, grade, time, tokens, steps, cost, and reuse
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

No results have been recorded for the Excalidraw corpus yet; the first run
establishes both the baseline and the map dataset.

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

`repo/` is a Git submodule pinned to Excalidraw commit
`a758e93d6a03e9f995973703cb886850a398a6d3`. Source-dependent commands initialize
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
snippet verbatim unless visibility is explicitly overridden, and must
successfully invoke `slopdex map` with the selected visibility. `slopdex faq` and
`slopdex refactor` are also allowed (refactoring edits files and is therefore a
workspace violation in these read-only tasks); other slopdex commands and
root/index/config/agent overrides are blocked. Baseline denies all
slopdex calls. Missing required map use or disallowed commands is a protocol
violation. These are cooperative controls, not a security sandbox.

Global configs, instructions, plugins, and previous sessions are isolated.
Editing, delegation, network tools, interactive questions, and LSP are disabled.
Effective OpenCode controls are checked before timing. Source edits are checked
afterward.

Preparation runs `slopdex update` on an isolated clone, producing the full
semantic index: structure, code-body and Markdown embeddings, and symbol-name
vectors, restricted by `config.json` `include`/`exclude` (tests, locales,
fixtures, and generated WOFF2 bindings are excluded). Preparation uses an
isolated schema-4 artifact store, then warms the shared ANN bases with an offline
content search and `map -q`. Each trial gets its own workspace/store backup and
copied ANN bases, and an offline search preflight (provider credentials removed)
checks that the copies are reused without rebuilding.

`faq` runs with `agentCommand: ["opencode", "run"]` and the trial's own model,
inside the trial's isolated OpenCode environment. Its answers are cached only in
the trial's index copy. **Nested faq model spend is not included in the reported
tokens or USD**; its wall time is included in the agent's run time.
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
python3 -m unittest discover -s evals/excalidraw -v
cargo verify

# Optional installed-CLI checks, without external model calls.
SLOPDEX_EVAL_CHECK_OPENCODE=1 python3 -m unittest discover -s evals/excalidraw -v
SLOPDEX_EVAL_CHECK_ANN=1 python3 -m unittest discover -s evals/excalidraw -p test_native_ann.py -v
```

To add tasks, update `tasks.json` and validate every reference declaration
(`run.py validate`). Gold lines come from a TypeScript declaration matcher
covering `function` declarations, arrow/function constants, and class members; a
target must match exactly one declaration in its file. To update Excalidraw,
deliberately update its gitlink and `repository_commit`, then review and validate
all tasks; this invalidates the baseline as well as map.
