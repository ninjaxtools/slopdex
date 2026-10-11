# Map instruction review and 20-case rerun

This document records the **longer five-bullet prompt** experiment. The latest
baseline/report files now describe the subsequent original-plus-limit run; see
[map-limit-review.md](map-limit-review.md). The longer prompt's raw report is
`jobs/map-instruction-scoped-20/report.md`, and its recorded instruction remains
in [map-instruction-comparison.json](map-instruction-comparison.json).

## Evidence from the previous run

Reviewed the 34 completed map traces in
`jobs/baseline-vs-map-331e607` (18 DeepSeek, 16 Muse). This is a navigation review,
not evidence that every failed answer was caused by map.

- **31/34** substantive first maps were piped through `head` or `sed`. This kept
  the first files rather than bounding relevant declarations with map's selector.
- **34/34** substantive first maps used two-hop callers/callees. Expansion can
  include declarations outside the requested paths, globs, and direct-match limit.
- **0/34** substantive first maps used `--limit`.
- The 34 trials made **70 slopdex invocations**, including **16 help calls**.
  DeepSeek made 47 invocations over 18 cases; Muse made 23 over 16 cases.
- Broad regexes such as `config|base|inherit` and `transform|decorator|field|initializer`
  were combined with semantic queries. These selectors form a union, not an
  intersection, so the combination broadened discovery.
- Muse's reverse-mapped-inference case mapped `./...` before reading help and
  retrying `.`. Its decorator case mapped `packages/` twice despite the Go sources
  being elsewhere, then used help and a broad map. Both eventually passed, but
  the detours were avoidable.
- Map text ranges may include leading comments and adjacent declarations. They
  are useful read locations, not automatically the exact function citation line.

The previous report's lower token/cost figures included failed answers; those
figures alone do not demonstrate improved efficiency.

### Failure causes are not just navigation

DeepSeek had six invalid answers: three extra closing braces, one extra finding,
one prose prefix, and one step-budget-exhausted answer with fences/commentary.
Only that last case reached 40 steps. Its two protocol violations came from
invoking blocked index/status diagnostics. All 30 required declarations across
the eight failed cases had appeared at their cited lines in actual source reads.
One diagnostic-detour case also used receiver-qualified rather than requested
unqualified symbol names. Thus better navigation alone cannot be credited with
fixing final-answer validity.

Concrete trace references (paths relative to the previous job's `trials/`):

- `a01295349cb4934e/stdout.jsonl`: two identical maps of `packages/`, then a
  correct checker map; `answer.txt` has an extra closing brace.
- `b9b128b44efcecfe/stdout.jsonl`: repeated config map for another output slice;
  `answer.txt` contains four findings where three were requested.
- `e930704c9e3bbffd/slopdex-calls.jsonl`: blocked status/index commands after
  empty scopes; final citations were otherwise correct.
- `6239cacf4fb52f1b/stdout.jsonl`: all five declarations read by step 18, then
  continued investigation until the 40-step budget was exhausted.

## Instruction changes

The root `README.md` remains the sole map agent instruction used by the harness.
The revised snippet asks agents to:

1. Discover actual source paths with a quick glob/list; use a scoped function map.
2. Choose short semantic concepts **or** distinctive symbol regexes; avoid broad OR unions.
3. Use `--limit 60`, not shell truncation, and defer graph expansion until a
   specific function relationship is needed (one hop).
4. Read promising implementations immediately, verify declaration lines, and
   use targeted grep/read rather than repeated discovery/help or index-warning
   detours.
5. Stop when the requested source evidence is complete; check finding count,
   symbol form, citations, and exact final-answer format before responding.

No task prompts, reference answers, scoring, model options, timeout, step budget,
or harness behavior were changed. This is a general workflow instruction, not
task-specific hints.

## Fixed selection and controls

**20 fresh map cases = 10 tasks × 2 models.** The matching 20 baseline results
are reused, not rerun. Both models use medium reasoning, a 40-step budget, and a
600-second timeout with OpenCode 1.18.34.

Selection was fixed before execution: Python `random.Random(0)`, sampling five
sorted task IDs per difficulty. Outcomes were not a sampling criterion.

| Difficulty | Selected tasks |
|---|---|
| Intermediate | `overloaded-generic-calls`, `unbuilt-referenced-project`, `ambiguous-expression-type-arguments`, `conditional-package-subpaths`, `inherited-config-file-selection` |
| Advanced | `reverse-mapped-inference-expansion-cache`, `request-snapshot-autoimport-adoption-race`, `granular-autoimport-index-refresh-and-visibility`, `dynamic-contentmapper-refresh-and-bundle-cache`, `decorator-private-field-initializer-handoff` |

The slopdex executable is copied into an ignored, content-addressed cache before
execution, so another build cannot replace it during this run. Prompts,
instructions, controls, source/build hashes, answers, metrics, and grades are
recorded by the existing harness. The previous interrupted run is preserved.

The new baseline comparison is historical, not interleaved. Eighteen selected
cases have completed old-map results; any old/new comparison must use only those
matching cases, without assigning zeros to missing trials. The older and newer
slopdex binaries have different hashes even though their source-content hashes
match, so an old/new comparison is descriptive, not a controlled prompt-only
experiment.

## Results

**Complete: 20/20 fresh map cases, 20/20 saved baselines reused.** The run is
`jobs/map-instruction-scoped-20`. The latest portable map dataset and baseline
report have been updated. No baseline or extra map trials were run.

All 20 final answers were valid and identified all requested declarations. The
protocol-inclusive outcome is **19/20 passed**: DeepSeek 10/10, Muse 9/10.
Muse's auto-import registry case invoked blocked `slopdex status --ignore-errors`
after mapping the wrong `packages/` scope twice, despite eventually producing
correct source citations. The protocol penalty is retained, not regraded away.

### Revised map versus saved baseline

Each cell is a mean per case, **baseline → revised map**. Resource comparisons
include failed baseline/map trials; token totals include cached tokens and USD
excludes embedding charges. Both models have ten matched cases.

| Model | Seconds | Total tokens | Steps | USD | Passed |
|---|---:|---:|---:|---:|---:|
| DeepSeek | 102.0 → 116.9 | 627,607 → 542,341 | 18.5 → 17.6 | $0.015589 → $0.014311 | 7/10 → 10/10 |
| Muse | 35.0 → 57.4 | 296,183 → 238,813 | 11.6 → 11.2 | $0.006136 → $0.005094 | 10/10 → 9/10 |

| Model | Time change | Total-token change | Input-token change | Step change | Cost change |
|---|---:|---:|---:|---:|---:|
| DeepSeek | +14.6% | −13.6% | −18.9% | −4.9% | −8.2% |
| Muse | +63.9% | −19.4% | −23.2% | −3.4% | −17.0% |

For the seven DeepSeek cases where both baseline and map passed, map used 6.4%
less time, 27.6% fewer total tokens, 17.6% fewer steps, and 14.2% lower cost.
This is a selected cohort, not a substitute for the all-case comparison. Muse's
nine both-passing pairs still took 62.7% longer, despite 23.4% fewer total tokens
and 16.5% lower cost.

The baseline comparison and exact prompts, answers, settings, and build hashes
for this experiment are preserved in `jobs/map-instruction-scoped-20/`. Current
[report.md](report.md), [summary.json](summary.json), and
[latest-map.json](latest-map.json) refer to the subsequent run instead.

### Previous instruction versus revised instruction

Only 18 cases have both old and new map results (nine per model). The old
instruction passed **14/18**; the revised instruction passed **17/18** on these
same cases. All consumption includes failures, so a newly successful answer can
cost more than the previous failed attempt.

| Model | Passed: old → new | Time change | Total-token change | Step change | Cost change |
|---|---:|---:|---:|---:|---:|
| DeepSeek | 5/9 → 9/9 | +8.7% | +11.2% | −18.2% | +13.5% |
| Muse | 9/9 → 8/9 | +4.7% | −14.2% | −2.9% | −9.7% |

Saved old/new matched-case evidence:
[map-instruction-comparison.json](map-instruction-comparison.json). The two
missing old cases are excluded, not assigned failures. The instruction changed
as a bundle; this unreplicated historical comparison cannot isolate which line
helped or rule out provider/run variation.

### Did usage actually change?

Across all 20 new cases:

- **20/20** substantive first maps used `--limit` and `-k fns`.
- **0/20** substantive first maps requested graph expansion or combined `-e`
  with `-q` (previously 34/34 for both patterns).
- There were **38 slopdex invocations**, including **32 maps and 5 help calls**.
- On the nine matched DeepSeek cases, invocations fell **28 → 17**, map calls
  **20 → 15**, and help calls **7 → 2**.
- Compliance is incomplete: **15/20** first-map shell commands still sliced
  output with `head`/`sed`, and Muse's failed case still ignored source-directory
  verification and the prohibition on other navigation commands.

**Conclusion:** the revised instruction improved DeepSeek's answer validity and
reduced discovery detours, but did not deliver an overall speed improvement.
Muse showed lower token/cost usage but retained a protocol failure. The next
instruction experiment should focus on the persistent slicing/wrong-scope
behavior and make the permitted fallback after an empty map more explicit;
no additional trials were run in this experiment.
