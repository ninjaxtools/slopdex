# Map discovery prompt study

The goal is to make map replace declaration-location work and file inventories,
then switch promptly to reading implementation code. The preferred experimental
profile is [`map-follow`](prompts/map-follow.md). The root README now advertises
a language-neutral adaptation; the exact tested prompt is the Go-specific file.

## Experiments

1. Six balanced cases × two models × off/README-map/map-first/map-follow:
   48 interleaved trials, one repetition, seed 42.
2. Selected map-follow, unchanged, against off across all 20 cases and both
   models: 160 interleaved trials, two repetitions, seed 43.
3. Added a source-verification reminder (`map-verify`) and screened it against
   off on three difficult cases: 24 interleaved trials, two repetitions, seed 44.

All used medium reasoning, OpenCode 1.18.34, slopdex 0.29.0, the pinned compiler
commit, 40 steps, 600-second timeouts and provider-free structural snapshots.
No semantic-search or embedding work was part of these prompt trials.

## Full-suite result

Ratio-of-total changes for map-follow versus the matched baseline:

| Model | Agent steps | Repeated input presentations | Peak step input | Model cost | Wall time |
|---|---:|---:|---:|---:|---:|
| DeepSeek | −11.3% | −25.8% | −16.7% | −10.8% | −18.2% |
| Muse | −2.4% | −34.3% | −32.8% | −29.2% | +1.7% |

Input presentations sum uncached input, cache-read and cache-write tokens across
agent steps; they are repeated context, not unique code tokens. Visible tool
output fell 19.7%/41.4%, and visible source context fell 18.8%/39.1%, so the
effect is not solely a cached-billing artifact. Task-cluster bootstrap intervals
for cost/context were below zero, while step intervals included zero.

The 14 cases outside the pilot retained savings: DeepSeek input presentations
−25.1%, model cost −5.2%; Muse −36.2%, −31.1%. The prompt was selected on the
six pilot cases, so report this held-out subset separately.

Strict citation/schema/protocol passes were 36/40 versus 32/40 for DeepSeek and
40/40 for both Muse arms. Citation-only scores retain invalid-answer penalties.
They do not establish explanation correctness.

## What the prompt does

- Uses task-derived OR-ed name terms, function-only output and test exclusions.
- Batches selectors and source reads.
- Stops inventory browsing once a plausible entry point is found.
- Follows concrete names from implementation, using map only for unknown locations.
- Leaves grep available for body text and usages.
- Avoids repeat declaration checks, whole-file previews and metadata/help detours.

`--ignore-errors` is appropriate here because the experiment's cached target
coverage is prevalidated. It is not a recommendation to suppress unexamined
indexing errors in arbitrary projects. Go caller/callee expansion was omitted
because the current resolver misses important receiver and cross-file links.

## Quality limit

Manual reviews found substantive explanation errors despite correct citations
and implementation-read exposure. Some were shared with baseline; others were
map-only regressions. The additional map-verify instruction retained context/cost
savings in its small screen, but did not reliably establish better factual
fidelity against concurrent off answers. It remains experimental, rather than
replacing the full-suite-tested map-follow profile.

Treat map-follow as a measured discovery/context aid. Critical conditions,
cleanup, ownership contracts and sentinel behavior still require source review.
The study demonstrates efficiency gains, not a correctness guarantee.

## Running and reporting

Use fresh output directories because instructions and harness settings are
fingerprinted. Exact prompt files are frozen in each manifest.

```sh
PATH="$PWD/target/release:$PATH" TMPDIR="$PWD/evals/typescript/jobs/tmp" \
  python3 evals/typescript/run.py run --arms off map-follow --repeats 2 --seed 43 \
  --output evals/typescript/jobs/map-prompt-validation-new

python3 evals/typescript/map_prompt_report.py \
  evals/typescript/jobs/map-prompt-validation-new --require-complete
```

Saved local results are under `jobs/map-prompt-pilot`, `jobs/map-prompt-validation`
and `jobs/map-prompt-verification`. `efficiency.md`, `metrics.json` and
`efficiency-audit.json` provide offline accounting without changing original
artifacts. The validation's `conclusion.md`, `validation-summary.json` and
`explanation-review.md` document paired aggregates, held-out results, bootstrap
scope and manual source-grounded findings.
