# Original map prompt plus one limit sentence: 20-case evaluation

**Complete: 20/20 fresh map trials passed, 10/10 per model.** All 20 matching
baseline results were reused; no baseline trials were rerun. There were no
answer-format errors or protocol violations. Every map trial had citation F1 1.0.

## Instruction and controls

The original committed README prompt was kept verbatim. Only this sentence was
added:

> Bound direct matches with `--limit 60` rather than piping to `head`, which can
> silently discard relevant output and errors.

Run: `jobs/map-instruction-limit-20`. It uses exactly the same ten selected tasks
as `map-instruction-scoped-20` (five intermediate, five advanced), with both
models at medium reasoning, seed 0, one repetition, 40 steps and a 600-second
timeout. The source, frozen executable, harness, and settings match that prior
20-case run. OpenCode remains 1.18.34 and slopdex remains 0.39.0.

The terse three-bullet variant was not evaluated; this experiment measures the
original prompt plus the one additional sentence.

## Versus saved baseline

Means per trial, **baseline → map**, for ten cases per model. Total tokens
include cached tokens; USD is OpenCode-reported model spend, excluding embeddings.
Resource totals include failed baseline trials, rather than dropping their cost.

| Model | Seconds | Total tokens | Steps | USD | Passed |
|---|---:|---:|---:|---:|---:|
| DeepSeek | 102.0 → 105.1 | 627,607 → 476,132 | 18.5 → 18.1 | $0.015589 → $0.013108 | 7/10 → 10/10 |
| Muse | 35.0 → 63.7 | 296,183 → 252,343 | 11.6 → 10.4 | $0.006136 → $0.005430 | 10/10 → 10/10 |

| Model | Time change | Total-token change | Input-token change | Step change | Cost change |
|---|---:|---:|---:|---:|---:|
| DeepSeek | +3.1% | −24.1% | −18.7% | −2.2% | −15.9% |
| Muse | +81.8% | −14.8% | −15.9% | −10.3% | −11.5% |

For the seven DeepSeek pairs where both baseline and map passed, map took 0.7%
less time and used 27.2% fewer total tokens, 4.4% fewer steps, and 17.7% lower
cost. Muse's ten pairs all passed, so its full and passing-cohort figures match.

## Versus the longer prompt's 20-case run

All twenty cases match; both runs use the same frozen slopdex executable.
The previous longer prompt passed 19/20; this original-plus-limit prompt passed
20/20. Its only prior failure was Muse invoking a blocked diagnostic command.

| Model | Passed: longer → original+limit | Time change | Total-token change | Step change | Cost change |
|---|---:|---:|---:|---:|---:|
| DeepSeek | 10/10 → 10/10 | −10.1% | −12.2% | +2.8% | −8.4% |
| Muse | 9/10 → 10/10 | +10.9% | +5.7% | −7.1% | +6.6% |

This is not an interleaved or replicated experiment. Run/provider variation and
the prior failed trial affect resource comparisons; the result does not prove
that the additional sentence caused the accuracy improvement.

## Observed map usage

Across all twenty cases, agents made **29 slopdex invocations**: 24 maps and four
help calls, plus one allowed version call, with no blocked invocations. This is
down from 38 invocations in the longer-prompt run.

- First maps used `--limit` in **15/20** cases (all ten Muse, five DeepSeek).
- **12/20** first-map shell commands still sliced output with `head`/`sed`.
- All twenty first maps retained the original two-hop graph expansion and
  combined regex/semantic selectors, as instructed. None added `-k fns`.

The sentence's intended behavior was only partially followed. The perfect
accuracy result should not be interpreted as perfect instruction compliance or
proof of a wall-time improvement.

## Saved evidence

- [report.md](report.md) and [summary.json](summary.json): current baseline/map
  comparison, including valid/passing cohorts and per-task differences.
- [latest-map.json](latest-map.json): all twenty answers, grades, exact prompts,
  instructions, settings, and build identity.
- [map-limit-comparison.json](map-limit-comparison.json): all twenty matched
  longer/original-plus-limit results, both instructions, settings and result hashes.

Previous job data and the immutable baseline files are unchanged. The earlier
[instruction review](map-instruction-review.md) describes the longer prompt's
experiment, not this latest run.
