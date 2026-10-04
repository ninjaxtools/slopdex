# TypeScript paired evaluation

Compare OpenCode using **slopdex map only** (`map`) against OpenCode using
**slopdex search only** (`search`) on twenty source-navigation and
implementation-understanding tasks. Each arm uses the corresponding agent
instruction block from the project's root [README.md](../../README.md).
The default matrix is **20 tasks × 2 models × 2 arms = 80 trials**, all at medium
thinking. The harness is Python 3.11+ with no third-party Python dependencies.
Add `map-search` to evaluate both tools together: **120 trials** across three arms.
For map-only discovery prompting and measured context/cost savings, see the
[map prompt study](MAP_PROMPTS.md) and the `map-first`/`map-follow` experimental arms.

## Quick start

From the slopdex repository root:

```sh
# Inspect tasks and the matrix without model calls or downloads.
python3 evals/typescript/run.py list
python3 evals/typescript/run.py run --dry-run

# Optional: download the source and validate the reference declarations.
python3 evals/typescript/run.py validate

# Optional: prepare local structure and the warmed semantic index ahead of trials.
python3 evals/typescript/run.py prepare

# Run all 80 trials; run performs source/index preparation automatically.
python3 evals/typescript/run.py run --arms map search --output evals/typescript/jobs/map-vs-search

# Repeating the same command resumes, skipping trials with saved results.
python3 evals/typescript/run.py run --arms map search --output evals/typescript/jobs/map-vs-search

# Regenerate the comparison from saved artifacts.
python3 evals/typescript/run.py report evals/typescript/jobs/map-vs-search

# Compare map, bounded search, and combined navigation with the current guidance.
TMPDIR="$PWD/evals/typescript/jobs/tmp" python3 evals/typescript/run.py run \
  --arms map search map-search --output evals/typescript/jobs/map-search-limit-50

# Compare conventional navigation, README map guidance and two discovery prompts.
TMPDIR="$PWD/evals/typescript/jobs/tmp" python3 evals/typescript/run.py run \
  --arms off map map-first map-follow --seed 42 \
  --output evals/typescript/jobs/map-prompt-comparison
```

Requirements: Linux/macOS, Python 3.11+, Git, `opencode` and `slopdex` on `PATH`.
Authenticate OpenCode to the `opencode-go` provider first, or supply its API key
through the usual environment variables. Existing OpenCode authentication in
the user's XDG data directory is copied for the selected provider into a temporary
data directory. Set `OPENAI_API_KEY` for the default
embedding configuration for the search arm. Map-only runs need no embedding API
key or provider calls. Neither TypeScript build dependencies nor a Go toolchain
are needed: tasks inspect source rather than compiling or changing it.

The runner honors `TMPDIR`. Trial index snapshots can be large; on hosts with a
small RAM-backed `/tmp`, use disk-backed scratch space under the ignored jobs
directory:

```sh
mkdir -p evals/typescript/jobs/tmp
TMPDIR="$PWD/evals/typescript/jobs/tmp" python3 evals/typescript/run.py run \
  --task ambiguous-expression-type-arguments \
  --output evals/typescript/jobs/map-search-smoke
```

For a smaller first experiment:

```sh
python3 evals/typescript/run.py run \
  --task ambiguous-expression-type-arguments \
  --model opencode-go/deepseek-v4.1-flash@medium \
  --output evals/typescript/jobs/map-search-smoke

# More repetitions reduce sensitivity to a single model response.
python3 evals/typescript/run.py run --repeats 3 --seed 42 \
  --output evals/typescript/jobs/repeated

# Baseline alone needs no slopdex executable or embedding key.
python3 evals/typescript/run.py run --arms off \
  --output evals/typescript/jobs/baseline

# Map alone builds only local structure and needs no embedding key.
python3 evals/typescript/run.py prepare --arms map
python3 evals/typescript/run.py run --arms map \
  --output evals/typescript/jobs/map-only

# Retain the previous conventional-vs-combined comparison when explicitly selected.
python3 evals/typescript/run.py run --arms off slopdex \
  --output evals/typescript/jobs/conventional-vs-combined
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

`tasks.json` defines ten intermediate cases with three reference functions each
and ten advanced cases with five reference functions each, for **80 verified
reference functions**. Every advanced case spans at least three implementation
files and requires causal reasoning about state, ownership, caching or emit
behavior across subsystem boundaries.

### Intermediate cases (three required findings)

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

### Advanced cases (five required findings)

| Task | Area |
|---|---|
| `request-snapshot-autoimport-adoption-race` | Deferred requests, snapshot adoption races, cancellation and shared-program ownership |
| `mapped-diagnostic-provenance-and-synthesized-output` | Canonical/supplemental virtual files, synthesized diagnostics, mapping fidelity and UTF-16 |
| `watcher-missing-lookup-and-project-dirty-state` | Watch batches, missing-ancestor lookups, overlays, cache refresh and rebuild eligibility |
| `granular-autoimport-index-refresh-and-visibility` | Shared realpath extraction, granular export-index replacement, cancellation and package shadowing |
| `dynamic-contentmapper-refresh-and-bundle-cache` | Remote mapper reopening, configuration identity, locale-sensitive caches and concurrent bundle ownership |
| `declaration-alias-visibility-fixed-point` | Late-visible aliases, accessibility diagnostics, portable module spelling and declaration worklists |
| `async-generator-iterator-close-helper-flow` | Async delegation, iterator cleanup/error precedence and runtime-helper dependencies |
| `decorator-private-field-initializer-handoff` | Decorator queues, computed accessors, private brands and class-field transform ordering |
| `reverse-mapped-inference-expansion-cache` | Recursive expansion guards, cached inference failures and array/tuple/object reconstruction |
| `source-map-pending-segments-round-trip` | Segment accumulation, malformed decoding, UTF-16 positions and asymmetric mapping lookups |

These are navigation tasks, not patch-generation tasks. Each asks for specific
implementation roles and a flow explanation. Advanced prompts require tracing
intervening adapters, contrasting success/failure or fresh/stale paths, and
explaining invariants rather than just locating declarations. The five findings
identify mandatory stages; supporting helpers can be cited in the explanations.
Prompts explicitly target the active Go-native implementation, avoiding bundled
legacy TypeScript compiler fixtures. Prompts do not disclose the target
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
- Arms: `map`, `search`.

The CLI command is `opencode run --model PROVIDER/MODEL --variant medium
--agent build --format json --title TRIAL_ID -- PROMPT`. The injected OpenCode
configuration defines the variant explicitly. This matters for DeepSeek, whose
current published OpenCode catalog lists `low`, `high`, and `max` but omits
`medium`. The requested setting is sent as `reasoningEffort: medium`; provider
rejection appears as an agent error, with no fallback to another effort level.
The provider ultimately determines how that setting maps to actual reasoning.

Trials are sequential. Model/task/repetition pairs are seeded-shuffled, with
alternating arm order for two arms; larger matrices rotate and reverse order to
balance every arm position. Each arm gets the same
task prompt, model, iteration budget and fresh checkout. Navigation guidance and
available slopdex commands differ between arms:

| Arm | Agent navigation guidance | Allowed slopdex navigation |
|---|---|---|
| `map` (default) | Root README's exact structural-map agent snippet with `--private`, symbol-name regexes, narrow paths and targeted reads | `map` only |
| `search` (default) | Root README's exact semantic-search agent snippet with threshold 0.3, limit 50 and query variation | `search` only |
| `map-search` (optional) | Both exact README snippets, with targeted reads and bounded output | `map` and `search` only; both must succeed at least once |
| `map-first` (experimental) | [Map-first discovery prompt](prompts/map-first.md), immediately followed by implementation reads | `map` only |
| `map-follow` (experimental) | [One discovery pass, then follow source](prompts/map-follow.md), with an explicit stop rule | `map` only |
| `map-verify` (experimental) | [Source-following with predicate/cleanup verification](prompts/map-verify.md) | `map` only |
| `off` (optional legacy baseline) | Conventional glob/grep/reads | None |
| `slopdex` (optional legacy combined arm) | Original combined map/search guidance | All slopdex commands |

README instruction blocks are read when planning the run and frozen in its
manifest's `arm_instructions` and resume fingerprint. Each trial's `AGENTS.md`
contains its selected block plus common evaluation controls. Map guidance also
reminds agents to include `--private` for unexported Go implementation symbols.
The experimental map prompts use the same provider-free structural cache and
map-only controls. Their exact text is frozen in each manifest. They suppress
prevalidated parser warnings, exclude test files during declaration discovery,
batch terms/paths and avoid metadata/help inventories. Source verification is
still required. Compare steps, model tokens/cost, tool-output characters and
discovery timing alongside citation and answer-format validity.
Map's `-e` filters symbol names and qualified names rather than raw source text;
the README example explicitly distinguishes those patterns. The earlier
`jobs/map-vs-search` run used the prior map guidance and search threshold 0.5;
use a fresh output directory, such as `jobs/map-vs-search-03`, for the updated
instructions. Existing run manifests preserve the instructions used at the time.
The subsequent `jobs/map-vs-search-03` run used threshold 0.3 without a result
limit; `jobs/map-search-limit-50` uses limit 50, includes the combined arm, and
uses the stdout broken-pipe fix. Closed stdout is quiet success; other output
errors remain failures. These changes are evaluated together in that run.
Conventional local reads, globbing and grep remain available in both default
arms for path discovery and source verification.

The PATH wrapper blocks other slopdex navigation commands before execution.
Help/version requests remain available. `search-code`, `search-md`, `describe`,
`cross-search`, indexing/configuration commands and `map` are blocked in the
search-only arm; analogous non-map commands are blocked in the map-only arm.
The catalog-fetching `help models` operation is also blocked. Agents cannot
override the pinned root, index, provider/configuration, or rebuild controls.
Each exclusive arm must successfully invoke its assigned navigation command at
least once; help-only/no-command runs and disallowed-command attempts are
recorded as protocol violations, receiving zero comparison F1. This verifies
actual use of the advertised approach. These are cooperative protocol controls,
not an OS-level sandbox.
The combined `map-search` arm applies the same protected-configuration controls,
allows only map/search navigation, and requires an exit-zero invocation of each.
It shares one semantic index snapshot containing both structure and embeddings.

Reports compare **search − map** for the default pair. If additional arms are
selected with `--arms`, all available paired arm combinations are reported with
explicit reference/comparison labels; historical off/slopdex reports remain
readable.

Global OpenCode configuration, global instruction files, external skills, project
config, and external plugins are isolated/disabled. Provider authentication and
API-key environment variables are retained. Account/organization configuration,
other providers' credentials and previous sessions are not copied. Delegation, web search/fetch, editing,
interactive questions and LSP are disabled for both arms. Source edits are also
checked after the trial. The baseline denies bash commands matching the slopdex
executable name and has a blocking PATH shim; ordinary workspace paths remain
allowed. These are protocol controls, not an OS-level sandbox.
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

The map arm has a separate provider-free structural cache, built with `slopdex
map` rather than semantic preparation. It contains local symbols and source
metadata, without embedding vectors or ANN files. Structural target coverage is
verified before trials. `prepare --arms map` and `run --arms map` therefore work
without an embedding API key, even on a cold cache. The search and optional
combined arms use the semantic/ANN cache described below.

Index caches are keyed by repository commit, slopdex version, and full slopdex
configuration. Preparation fails on operational indexing failures, an empty
index, or missing searchable vectors for any selected task's target function.
Parser-recovery diagnostics elsewhere in the checkout are retained and reported;
they do not prevent running a fully covered task. A failed preparation can be
retried while preserving paid embedding artifacts. Cache builds and result-directory
runs use advisory locks. Each search/combined trial receives a separate SQLite backup, including
WAL pages, plus the warmed native ANN binaries and their manifests. The schema-3
root identity is adjusted to the clone's root; generation, vector IDs, paths and
reference commit remain unchanged. An unsupported schema fails explicitly.

Preparation runs an offline, no-reindex `cross-search` with an impossible source
regex to load and persist the native indexes without querying a provider. Cache
metadata records the ANN generation, embedding profile, artifact sizes and SHA-256
hashes. Older embedding-only caches are warmed in place; missing, stale or corrupt
sidecars are repaired during preparation. Code and Markdown indexes are always
included, and description/combined indexes are included when descriptions are
enabled.

Before timing each search/combined trial, the runner copies all matching binaries and
manifests and opens them through the writable native loader using the same offline
probe. Their hashes and modification times must remain unchanged. This verifies
the relocated snapshot is reusable; an unexpected rebuild fails preparation.
The check is recorded in `ann-validation.json` and its CLI logs.

Reports separate one-time index preparation time from trial time. OpenCode's
reported USD cost covers its model calls, **not** embedding preparation/query
costs or subscription billing. Token fields are recorded as reported by OpenCode,
including input, output, reasoning, and cache reads/writes; missing usage fields
are zero, while absent cost telemetry is `null`. Query embedding latency is
included in the trial's wall time. Cached indexing makes the default comparison
an embedding-preindexed and ANN-prebuilt workflow. Native graph construction,
snapshot copying and the pre-trial reuse check are excluded from the timed model
run. Ordinary per-command index loading and query embedding time remain included.
Reports record cache-level ANN warm-up separately from embedding preparation;
per-trial results record reuse-validation time separately from agent wall time.
The earlier mixed-tool experiments have different arm definitions; use a new
output directory such as `jobs/map-vs-search` for this default comparison.
Provider-side prompt caches cannot be reset by
this harness; cache-token counters and counterbalanced order help interpret them.

## Scoring and artifacts

Each answer must be JSON with exactly the task's required number of `findings`
(`path`, `symbol`, `line`, `explanation`) and a nonempty `flow` explanation:
**three for intermediate cases, five for advanced cases**. The runner injects the
required count into each task prompt and rejects answers with fewer or more
findings. Grading compares repository-
relative paths and exact unqualified symbol names. A citation must point to the
function declaration's first line or the next two lines. Duplicate findings earn
credit only once; call-site citations do not earn citation credit.

- **Symbol recall:** reference symbols identified, ignoring citation quality.
- **Citation recall/precision/F1:** uniquely identified reference symbols with
  valid declaration citations. F1 is the primary comparison metric.
- **Passed:** all required symbols have valid citations.
- Timeouts, model errors, malformed answers and protocol violations receive
  zero comparison F1 and do not pass. Their raw partial grading remains saved.

This scorer checks navigation evidence and answer shape. It does **not** certify
the explanation's semantic correctness. Review `answer.txt` and the trace for that;
there is no extra model judge or hidden grading model cost.

Each output directory contains:

- `manifest.json`: pinned commit, exact config/prompts, ordered trial matrix, tool
  versions, frozen README instructions, structural/semantic index metadata and
  resume fingerprint.
- `trials/ID/`: prompt, arm instructions, generated OpenCode config, CLI argv,
  raw JSONL events, stderr, final answer, slopdex invocation log if used, and graded
  `result.json` with time, cost, tokens, steps, tool counts and command-protocol
  violations. Search/combined trials also
  include `ann-validation.json` and `ann-validation/` logs from the untimed reuse
  check.
- `summary.json` and `report.md`: per-model/variant/arm statistics and paired
  per-task differences (default: `search − map`). Failures count as zero in accuracy;
  latency/token/cost deltas use pairs where both trials completed validly.

Slopdex usage is recorded both as a total invocation count (`slopdex_calls`) and
per-command counts (`slopdex_commands`, for example `{"search": 2, "map": 1}`).
Reports show totals across the run, counts by model/thinking mode/arm, the number
of trials using each command, and the command breakdown for each treatment trial.
`summary.json` includes these counters and a compact per-trial usage list.
Counts are invocation attempts captured by the PATH wrapper, including failed
commands. `--help`, `-h`, and command help requests count as `help`; version
requests count as `version`. Other counts use the top-level command name, with
full arguments retained in `slopdex-calls.jsonl`. New logs journal invocation
start/finish events, whether the command was allowed, and its exit status;
reports collapse those events into one count per attempt. Automatic cache preparation and
ANN reuse-validation commands are excluded from agent usage counters.

The `report` subcommand backfills command counts into existing `result.json`
files from their saved invocation logs, preserving their grading and timings.
Legacy artifacts with a positive total but no log or command breakdown retain
that count under `unknown`. Regenerating an existing report requires no model runs:

```sh
python3 evals/typescript/run.py report evals/typescript/jobs/warmed-full
```

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

# Optional: real native-index build/copy/reuse with a local embedding fixture.
# Uses installed slopdex; no external model or embedding calls.
SLOPDEX_EVAL_CHECK_ANN=1 python3 -m unittest discover -s evals/typescript -p test_native_ann.py -v
```

The offline tests cover duplicate/call-site scoring, invalid answers, event and
usage parsing, per-command invocation counts and backfilling, timeouts, index relocation, config isolation, deterministic paired
scheduling, resume behavior, and an end-to-end pair with fake CLIs (no model or
embedding calls). The optional installed-CLI check validates both models and arms
using OpenCode's resolved configuration, without running any model.
Exclusive-arm tests exercise the generated wrapper, successful-use requirements,
denied commands, journal merging and README guidance. Paired-report tests cover
map/search, legacy off/slopdex, partial runs, and all multi-arm comparisons.
Native ANN tests verify sidecar integrity and optional description indexes. The
installed-slopdex test builds a tiny real index using a localhost embedding
fixture, then verifies relocated native binaries are reused unchanged offline.
It also builds and uses a local map-only index with the provider offline and no
embedding credentials.

To add tasks, add prompts and at least three distinct reference declarations to
`tasks.json`, then run `validate`. Cases marked `difficulty: "advanced"` must have
at least five targets spanning at least three files. The scorer uses each task's
own reference count. The earlier ten-case reports describe the earlier prompt
version; use a fresh output directory for the expanded corpus.
To update upstream, deliberately update the submodule gitlink and
`repository_commit`, review every task against the new source, and validate again.
Do not use `git submodule update --remote` for ordinary eval runs.
