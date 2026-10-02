# TypeScript paired evaluation

Compare OpenCode with conventional search (`off`) against OpenCode with slopdex
(`slopdex`) on ten source-navigation and implementation-understanding tasks.
The default matrix is **10 tasks × 2 models × 2 arms = 40 trials**, all at medium
thinking. The harness is Python 3.11+ with no third-party Python dependencies.

## Quick start

From the slopdex repository root:

```sh
# Inspect tasks and the matrix without model calls or downloads.
python3 evals/typescript/run.py list
python3 evals/typescript/run.py run --dry-run

# Optional: download the source and validate the reference declarations.
python3 evals/typescript/run.py validate

# Optional: prepare the semantic index ahead of the timed trials.
python3 evals/typescript/run.py prepare

# Run all 40 trials; run performs source/index preparation automatically.
python3 evals/typescript/run.py run --output evals/typescript/jobs/first

# Repeating the same command resumes, skipping trials with saved results.
python3 evals/typescript/run.py run --output evals/typescript/jobs/first

# Regenerate the comparison from saved artifacts.
python3 evals/typescript/run.py report evals/typescript/jobs/first
```

Requirements: Linux/macOS, Python 3.11+, Git, `opencode` and `slopdex` on `PATH`.
Authenticate OpenCode to the `opencode-go` provider first, or supply its API key
through the usual environment variables. Existing OpenCode authentication in
the user's XDG data directory is copied for the selected provider into a temporary
data directory. Set `OPENAI_API_KEY` for the default
embedding configuration. Neither TypeScript build dependencies nor a Go toolchain
are needed: tasks inspect source rather than compiling or changing it.

The runner honors `TMPDIR`. Trial index snapshots can be large; on hosts with a
small RAM-backed `/tmp`, use disk-backed scratch space under the ignored jobs
directory:

```sh
mkdir -p evals/typescript/jobs/tmp
TMPDIR="$PWD/evals/typescript/jobs/tmp" python3 evals/typescript/run.py run \
  --task ambiguous-expression-type-arguments \
  --output evals/typescript/jobs/parser-smoke
```

For a smaller first experiment:

```sh
python3 evals/typescript/run.py run \
  --task ambiguous-expression-type-arguments \
  --model opencode-go/deepseek-v4.1-flash@medium \
  --output evals/typescript/jobs/parser-smoke

# More repetitions reduce sensitivity to a single model response.
python3 evals/typescript/run.py run --repeats 3 --seed 42 \
  --output evals/typescript/jobs/repeated

# Baseline alone needs no slopdex executable or embedding key.
python3 evals/typescript/run.py run --arms off \
  --output evals/typescript/jobs/baseline
```

## Pinned source and tasks

`repo/` is a real Git submodule of
<https://github.com/microsoft/TypeScript>, pinned to
`edf7da4e93ab65e1f7766deae156c35132c0cde3`
(the current default-branch HEAD when this eval was created).
Upstream now uses **`main`**, with no `master` ref. This checkout contains the
**Go-native compiler under `tsc/internal/`**, so the tasks reference its Go
implementation.

On the first source-dependent command the runner executes:

```sh
git submodule update --init --depth 1 -- evals/typescript/repo
```

Shallow submodules are supported. This fetches the recorded gitlink, including
the pinned commit if it is no longer the branch tip. The runner does not follow
the moving upstream branch on subsequent runs. It checks that the gitlink,
checkout HEAD and `tasks.json` agree, and refuses tracked local modifications.
Each trial uses an independent local clone of this commit, so agents never work
in the canonical submodule. Temporary trial workspaces are cleaned up afterward.

`tasks.json` defines the prompts and three verified reference symbols per task:

| Task | Area |
|---|---|
| `overloaded-generic-calls` | Overload selection, inference and contextual callbacks |
| `discriminant-branch-types` | Discriminated-union control-flow narrowing |
| `conditional-package-subpaths` | Conditional exports and wildcard subpath resolution |
| `inherited-config-file-selection` | Config inheritance and file selection |
| `unbuilt-referenced-project` | Declaration-to-source project-reference redirection |
| `single-file-program-reuse` | Incremental program reuse after an editor edit |
| `lazy-auto-import-completion` | Export candidates and lazy completion resolution |
| `rename-occurrence-consistency` | Shorthand/alias rename edits and deduplication |
| `async-lexical-runtime-semantics` | Async lowering and lexical bindings |
| `ambiguous-expression-type-arguments` | Speculative parsing and state rollback |

These are navigation tasks, not patch-generation tasks. Each asks for three
implementation roles and a flow explanation. Prompts do not disclose the target
paths or symbols. The answer key and harness artifacts are outside the agent's
workspace, and external-directory access is denied. Treat this as a cooperative
local benchmark rather than a security sandbox.

## Models, thinking, and controls

Edit `config.json` to change models, variants, provider reasoning options, step
limits, timeouts, or slopdex configuration. Use `--config PATH` and `--cache PATH`
**before** the subcommand for alternative configurations/cache directories.
`--task` and `--model provider/model@variant` can each be repeated to select a
subset of the configured matrix.

Defaults:

- `opencode-go/deepseek-v4.1-flash`, `medium`
- `opencode-go/muse-spark-1.3-contributor`, `medium`
- Explicit variant options: `{"reasoningEffort": "medium"}` for both.
- 600-second timeout and 40 agent iterations per trial.

The CLI command is `opencode run --model PROVIDER/MODEL --variant medium
--agent build --format json --title TRIAL_ID -- PROMPT`. The injected OpenCode
configuration defines the variant explicitly. This matters for DeepSeek, whose
current published OpenCode catalog lists `low`, `high`, and `max` but omits
`medium`. The requested setting is sent as `reasoningEffort: medium`; provider
rejection appears as an agent error, with no fallback to another effort level.
The provider ultimately determines how that setting maps to actual reasoning.

Trials are sequential. Model/task/repetition pairs are seeded-shuffled, with
alternating arm order to counterbalance first-run effects. Each arm gets the same
prompt, model, iteration budget and fresh checkout. The treatment replaces only
the code-navigation guidance and slopdex availability. Baseline guidance points
to ordinary glob/grep/reads. Slopdex guidance points to semantic search and map,
while retaining conventional tools. The semantic index is prebuilt before timing;
there is no requirement to call slopdex a particular number of times.

Global OpenCode configuration, global instruction files, external skills, project
config, and external plugins are isolated/disabled. Provider authentication and
API-key environment variables are retained. Account/organization configuration,
other providers' credentials and previous sessions are not copied. Delegation, web search/fetch, editing,
interactive questions and LSP are disabled for both arms. Source edits are also
checked after the trial. The baseline denies bash commands mentioning slopdex and
has a blocking PATH shim. These are protocol controls, not an OS-level sandbox.
Every new CLI process loads its generated config automatically. The runner sets
both `PWD` and `--dir`, loads the arm instructions explicitly, and validates the
effective benchmark configuration with `opencode debug config` before timing.

## Indexing and costs

The default index uses OpenAI `text-embedding-3-large`, 3072 dimensions, embeddings
only, and no reranker. It indexes `tsc/internal/**/*.go`, including compiler tests
but excluding the unrelated baseline corpus outside that subtree. `maxFileSize`
is 16 MiB so the large checker implementation is not skipped. Agents can still
read the entire checkout. The scope/configuration is saved in the run manifest.
To use another embedding provider, edit the `slopdex` object in `config.json` and
set that provider's key in the environment.

Index caches are keyed by repository commit, slopdex version, and full slopdex
configuration. Preparation fails on operational indexing failures, an empty
index, or missing searchable vectors for any selected task's target function.
Parser-recovery diagnostics elsewhere in the checkout are retained and reported;
they do not prevent running a fully covered task. A failed preparation can be
retried while preserving paid embedding artifacts. Cache builds and result-directory
runs use advisory locks. Each trial receives a separate SQLite backup, including
WAL pages. The schema-3 root identity is adjusted to the clone's root; paths and
reference commit remain unchanged. An unsupported schema fails explicitly.

Reports separate one-time index preparation time from trial time. OpenCode's
reported USD cost covers its model calls, **not** embedding preparation/query
costs or subscription billing. Token fields are recorded as reported by OpenCode,
including input, output, reasoning, and cache reads/writes; missing usage fields
are zero, while absent cost telemetry is `null`. Query embedding latency is
included in the trial's wall time. Cached indexing makes the default comparison
an embedding-preindexed workflow. Each fresh trial's SQLite snapshot does not
include native ANN sidecars: its first semantic query builds those locally, and
that initialization/load time is included in trial wall time. On the first parser
smoke run, the first search took approximately two minutes in each treatment arm;
interpret latency comparisons with that cold-search overhead in mind.
Provider-side prompt caches cannot be reset by
this harness; cache-token counters and counterbalanced order help interpret them.

## Scoring and artifacts

Each answer must be JSON with exactly three `findings` (`path`, `symbol`, `line`,
`explanation`) and a nonempty `flow` explanation. Grading compares repository-
relative paths and exact unqualified symbol names. A citation must point to the
function declaration's first line or the next two lines. Duplicate findings earn
credit only once; call-site citations do not earn citation credit.

- **Symbol recall:** reference symbols identified, ignoring citation quality.
- **Citation recall/precision/F1:** uniquely identified reference symbols with
  valid declaration citations. F1 is the primary comparison metric.
- **Passed:** all three required symbols have valid citations.
- Timeouts, model errors, malformed answers and protocol violations receive
  zero comparison F1 and do not pass. Their raw partial grading remains saved.

This scorer checks navigation evidence and answer shape. It does **not** certify
the explanation's semantic correctness. Review `answer.txt` and the trace for that;
there is no extra model judge or hidden grading model cost.

Each output directory contains:

- `manifest.json`: pinned commit, exact config/prompts, ordered trial matrix, tool
  versions, index metadata and resume fingerprint.
- `trials/ID/`: prompt, arm instructions, generated OpenCode config, CLI argv,
  raw JSONL events, stderr, final answer, slopdex invocation log if used, and graded
  `result.json` with time, cost, tokens, steps and tool counts.
- `summary.json` and `report.md`: per-model/variant/arm statistics and paired
  per-task differences (`slopdex − baseline`). Failures count as zero in accuracy;
  latency/token/cost deltas use pairs where both trials completed validly.

Resume requires the same selected tasks, models, seed, repetitions, arms,
configuration, harness source and executable versions. Fully completed runs can
regenerate reports without downloading source or rebuilding indexes. Saved failed trials are retained too;
to retry one, remove its generated `trials/ID/result.json` and resume, or start a
new output directory. An interrupted trial without a result is rerun from a fresh
checkout. `jobs/` and `cache/` are Git-ignored; no API-key environment values are
written into harness configuration or manifests.

## Verification and extension

```sh
python3 -m unittest discover -s evals/typescript -v
python3 evals/typescript/run.py validate
cargo verify

# Optional: check installed OpenCode config compatibility without model calls.
SLOPDEX_EVAL_CHECK_OPENCODE=1 python3 -m unittest discover -s evals/typescript -v
```

The offline tests cover duplicate/call-site scoring, invalid answers, event and
usage parsing, timeouts, index relocation, config isolation, deterministic paired
scheduling, resume behavior, and an end-to-end pair with fake CLIs (no model or
embedding calls). The optional installed-CLI check validates both models and arms
using OpenCode's resolved configuration, without running any model.

To add tasks, add prompts and three reference declarations to `tasks.json`, then
run `validate`. To update upstream, deliberately update the submodule gitlink and
`repository_commit`, review every task against the new source, and validate again.
Do not use `git submodule update --remote` for ordinary eval runs.
