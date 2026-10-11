# TypeScript: no-map versus README-guided map

TypeScript commit: `edf7da4e93ab65e1f7766deae156c35132c0cde3`
Comparison mode: **saved baseline (not interleaved)**.
`off` = saved no-map baseline; `map` = README-guided map.

OpenCode: `1.18.34`; slopdex: `slopdex 0.39.0`.
Slopdex commit: `e765b6258e66a968544deeb8affc8ffb6fbff382` (dirty: `False`).
Source SHA-256: `d96cffb9e74b10b0296bdf0175d07951241d827045e88a89ea4fb303d48cde80`; binary SHA-256: `c7c352eb31f3c223ab73891f71d85ca54de4acd0926117516fbfb5037de6d03e`.
Limits: 40 steps, 600 seconds.

**Historical / saved comparison, not an interleaved experiment.** Baseline results are reused; timing, provider conditions and run order can differ.

## Saved sources

- baseline_sources: `['baselines/df5d2a55672cdebca5238c95416812a577d677151e10889733cbdf7ea8f72d5e.json']`
- map_source: `latest-map.json`

Completed **40/40** trials. Complete.
Failures contribute zero F1 / passes. Consumption includes failed trials and may be failure-shortened; it is not, by itself, evidence of efficiency.
Missing telemetry is excluded with metric-specific counts; observed totals with missing telemetry are incomplete spend, not estimated full spend.

## Side-by-side means and accuracy

Means per saved result (including failures).

| Model / variant | Off N/planned | Map N/planned | Off passes | Map passes | Off F1 | Map F1 | Off seconds | Map seconds | Off steps | Map steps | Off tokens | Map tokens | Off USD | Map USD |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | 10/10 | 10/10 | 7/10 | 10/10 | 0.700 | 1.000 | 102.0 | 105.1 | 18.5 | 18.1 | 627607 | 476132 | 0.015589 | 0.013108 |
| opencode-go/muse-spark-1.3-contributor / medium | 10/10 | 10/10 | 10/10 | 10/10 | 1.000 | 1.000 | 35.0 | 63.7 | 11.6 | 10.4 | 296183 | 252343 | 0.006136 | 0.005430 |

### All token categories (mean per saved trial)

Total tokens use a reported nonzero total without adding any categories again. Otherwise total = input + output + reasoning + cache_read + cache_write: reasoning is distinct from output in the pinned OpenCode V1 telemetry. A fallback requires all five categories; missing categories remain unknown, not zero. These are accumulated token presentations across steps, not unique context tokens.

| Model / variant | Arm | Input | Output | Reasoning | Cache read | Cache write | Total | Total source counts |
|---|---|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | off | 51287 | 4464 | 5865 | 565990 | 0 | 627607 | {'reported': 10} |
| opencode-go/deepseek-v4.1-flash / medium | map | 41696 | 4103 | 5194 | 425139 | 0 | 476132 | {'reported': 10} |
| opencode-go/muse-spark-1.3-contributor / medium | off | 47722 | 2438 | 1940 | 244083 | 0 | 296183 | {'reported': 10} |
| opencode-go/muse-spark-1.3-contributor / medium | map | 40112 | 2457 | 2567 | 207208 | 0 | 252343 | {'reported': 10} |

### Observed execution and spend totals

These arm totals are not balanced-pair effects when results or telemetry are missing.

| Model / variant | Arm | N | Seconds | Steps | Input | Output | Reasoning | Cache read | Cache write | Total tokens | USD |
|---|---|---|---|---|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | off | 10 | 1019.7 | 185.0 | 512873 | 44645 | 58648 | 5659904 | 0 | 6276070 | 0.155886 |
| opencode-go/deepseek-v4.1-flash / medium | map | 10 | 1050.8 | 181.0 | 416958 | 41028 | 51945 | 4251392 | 0 | 4761323 | 0.131082 |
| opencode-go/muse-spark-1.3-contributor / medium | off | 10 | 350.2 | 116.0 | 477222 | 24380 | 19400 | 2440831 | 0 | 2961833 | 0.061360 |
| opencode-go/muse-spark-1.3-contributor / medium | map | 10 | 636.6 | 104.0 | 401116 | 24571 | 25668 | 2072077 | 0 | 2523432 | 0.054304 |

### Missing telemetry and status counts

| Model / variant | Arm | Missing metric counts | Statuses |
|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | off | none | {'invalid_answer': 3, 'ok': 7} |
| opencode-go/deepseek-v4.1-flash / medium | map | none | {'ok': 10} |
| opencode-go/muse-spark-1.3-contributor / medium | off | none | {'ok': 10} |
| opencode-go/muse-spark-1.3-contributor / medium | map | none | {'ok': 10} |

## Paired effects: map − off

All-pairs consumption includes failures: **failure-shortened runs can look cheaper/faster without being useful**. `both_ok` requires both statuses ok; `both_passed` additionally requires both citation grades to pass. These selected cohorts can have survivor bias; read them with all-pairs accuracy and failure counts.
Δ is map − off. Negative consumption Δ means lower observed use; positive F1/pass Δ means better accuracy. Percentages are (map/off − 1) × 100. Mean case percentages and aggregate percentages (ratio of matched sums) are separate; zero baseline denominators have no case ratio. Every metric uses only pairs with both values, with its own n.

| Model / variant | Available pairs | Planned pairs | Missing pairs | Unmatched results |
|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | 10 | 10 | 0 | {'off': 0, 'map': 0} |
| opencode-go/muse-spark-1.3-contributor / medium | 10 | 10 | 0 | {'off': 0, 'map': 0} |

| Model / variant | Cohort | Pairs | Possible failure-shortened | Metric | Off mean | Map mean | Mean Δ | Total Δ | Mean case Δ % | Aggregate Δ % | Metric n | Ratio n | Missing n | Zero off n |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 10 | 3 | wall_seconds | 102.0 | 105.1 | +3.1 | +31.2 | +10.7 | +3.1 | 10 | 10 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 10 | 3 | steps | 18.5 | 18.1 | -0.4 | -4.0 | +1.3 | -2.2 | 10 | 10 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 10 | 3 | cost_usd | 0.015589 | 0.013108 | -0.002480 | -0.024805 | -5.8 | -15.9 | 10 | 10 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 10 | 3 | input_tokens | 51287.3 | 41695.8 | -9591.5 | -95915.0 | -12.6 | -18.7 | 10 | 10 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 10 | 3 | output_tokens | 4464.5 | 4102.8 | -361.7 | -3617.0 | -5.7 | -8.1 | 10 | 10 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 10 | 3 | reasoning_tokens | 5864.8 | 5194.5 | -670.3 | -6703.0 | +33.8 | -11.4 | 10 | 10 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 10 | 3 | cache_read_tokens | 565990.4 | 425139.2 | -140851.2 | -1408512.0 | -8.0 | -24.9 | 10 | 10 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 10 | 3 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 10 | 0 | 0 | 10 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 10 | 3 | total_tokens | 627607.0 | 476132.3 | -151474.7 | -1514747.0 | -8.5 | -24.1 | 10 | 10 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 10 | 3 | f1 | 0.700 | 1.000 | +0.300 | +3.000 | +0.0 | +42.9 | 10 | 7 | 0 | 3 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 10 | 3 | passed | 0.700 | 1.000 | +0.300 | +3.000 | +0.0 | +42.9 | 10 | 7 | 0 | 3 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 7 | 0 | wall_seconds | 104.2 | 103.4 | -0.8 | -5.4 | +10.3 | -0.7 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 7 | 0 | steps | 19.4 | 18.6 | -0.9 | -6.0 | +0.1 | -4.4 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 7 | 0 | cost_usd | 0.015955 | 0.013128 | -0.002827 | -0.019787 | -9.6 | -17.7 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 7 | 0 | input_tokens | 50340.1 | 40444.3 | -9895.9 | -69271.0 | -15.5 | -19.7 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 7 | 0 | output_tokens | 4648.6 | 4368.7 | -279.9 | -1959.0 | -2.5 | -6.0 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 7 | 0 | reasoning_tokens | 6245.6 | 5161.9 | -1083.7 | -7586.0 | +7.1 | -17.4 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 7 | 0 | cache_read_tokens | 622354.3 | 447634.3 | -174720.0 | -1223040.0 | -10.8 | -28.1 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 7 | 0 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 7 | 0 | 0 | 7 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 7 | 0 | total_tokens | 683588.6 | 497609.1 | -185979.4 | -1301856.0 | -11.5 | -27.2 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 7 | 0 | f1 | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 7 | 0 | passed | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 7 | 0 | wall_seconds | 104.2 | 103.4 | -0.8 | -5.4 | +10.3 | -0.7 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 7 | 0 | steps | 19.4 | 18.6 | -0.9 | -6.0 | +0.1 | -4.4 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 7 | 0 | cost_usd | 0.015955 | 0.013128 | -0.002827 | -0.019787 | -9.6 | -17.7 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 7 | 0 | input_tokens | 50340.1 | 40444.3 | -9895.9 | -69271.0 | -15.5 | -19.7 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 7 | 0 | output_tokens | 4648.6 | 4368.7 | -279.9 | -1959.0 | -2.5 | -6.0 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 7 | 0 | reasoning_tokens | 6245.6 | 5161.9 | -1083.7 | -7586.0 | +7.1 | -17.4 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 7 | 0 | cache_read_tokens | 622354.3 | 447634.3 | -174720.0 | -1223040.0 | -10.8 | -28.1 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 7 | 0 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 7 | 0 | 0 | 7 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 7 | 0 | total_tokens | 683588.6 | 497609.1 | -185979.4 | -1301856.0 | -11.5 | -27.2 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 7 | 0 | f1 | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 7 | 7 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 7 | 0 | passed | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 7 | 7 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 10 | 0 | wall_seconds | 35.0 | 63.7 | +28.6 | +286.4 | +90.3 | +81.8 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 10 | 0 | steps | 11.6 | 10.4 | -1.2 | -12.0 | -4.7 | -10.3 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 10 | 0 | cost_usd | 0.006136 | 0.005430 | -0.000706 | -0.007056 | -5.9 | -11.5 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 10 | 0 | input_tokens | 47722.2 | 40111.6 | -7610.6 | -76106.0 | -11.0 | -15.9 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 10 | 0 | output_tokens | 2438.0 | 2457.1 | +19.1 | +191.0 | +3.3 | +0.8 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 10 | 0 | reasoning_tokens | 1940.0 | 2566.8 | +626.8 | +6268.0 | +45.2 | +32.3 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 10 | 0 | cache_read_tokens | 244083.1 | 207207.7 | -36875.4 | -368754.0 | -1.7 | -15.1 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 10 | 0 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 10 | 0 | 0 | 10 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 10 | 0 | total_tokens | 296183.3 | 252343.2 | -43840.1 | -438401.0 | -4.0 | -14.8 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 10 | 0 | f1 | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 10 | 0 | passed | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 10 | 0 | wall_seconds | 35.0 | 63.7 | +28.6 | +286.4 | +90.3 | +81.8 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 10 | 0 | steps | 11.6 | 10.4 | -1.2 | -12.0 | -4.7 | -10.3 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 10 | 0 | cost_usd | 0.006136 | 0.005430 | -0.000706 | -0.007056 | -5.9 | -11.5 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 10 | 0 | input_tokens | 47722.2 | 40111.6 | -7610.6 | -76106.0 | -11.0 | -15.9 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 10 | 0 | output_tokens | 2438.0 | 2457.1 | +19.1 | +191.0 | +3.3 | +0.8 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 10 | 0 | reasoning_tokens | 1940.0 | 2566.8 | +626.8 | +6268.0 | +45.2 | +32.3 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 10 | 0 | cache_read_tokens | 244083.1 | 207207.7 | -36875.4 | -368754.0 | -1.7 | -15.1 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 10 | 0 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 10 | 0 | 0 | 10 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 10 | 0 | total_tokens | 296183.3 | 252343.2 | -43840.1 | -438401.0 | -4.0 | -14.8 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 10 | 0 | f1 | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 10 | 0 | passed | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 10 | 0 | wall_seconds | 35.0 | 63.7 | +28.6 | +286.4 | +90.3 | +81.8 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 10 | 0 | steps | 11.6 | 10.4 | -1.2 | -12.0 | -4.7 | -10.3 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 10 | 0 | cost_usd | 0.006136 | 0.005430 | -0.000706 | -0.007056 | -5.9 | -11.5 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 10 | 0 | input_tokens | 47722.2 | 40111.6 | -7610.6 | -76106.0 | -11.0 | -15.9 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 10 | 0 | output_tokens | 2438.0 | 2457.1 | +19.1 | +191.0 | +3.3 | +0.8 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 10 | 0 | reasoning_tokens | 1940.0 | 2566.8 | +626.8 | +6268.0 | +45.2 | +32.3 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 10 | 0 | cache_read_tokens | 244083.1 | 207207.7 | -36875.4 | -368754.0 | -1.7 | -15.1 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 10 | 0 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 10 | 0 | 0 | 10 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 10 | 0 | total_tokens | 296183.3 | 252343.2 | -43840.1 | -438401.0 | -4.0 | -14.8 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 10 | 0 | f1 | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 10 | 10 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 10 | 0 | passed | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 10 | 10 | 0 | 0 |

### Per-task pairs

| Model / variant | Task / repeat | Off / map status | Both ok | Both passed | F1 Δ | Seconds Δ | Steps Δ | Total tokens Δ | USD Δ |
|---|---|---|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | ambiguous-expression-type-arguments / 1 | invalid_answer / ok | False | False | +1.000 | +27.4 | +4.0 | +104397 | +0.004120 |
| opencode-go/deepseek-v4.1-flash / medium | conditional-package-subpaths / 1 | ok / ok | True | True | +0.000 | +16.2 | +4.0 | +77133 | +0.001866 |
| opencode-go/deepseek-v4.1-flash / medium | decorator-private-field-initializer-handoff / 1 | ok / ok | True | True | +0.000 | -27.4 | -3.0 | -446223 | -0.007695 |
| opencode-go/deepseek-v4.1-flash / medium | dynamic-contentmapper-refresh-and-bundle-cache / 1 | invalid_answer / ok | False | False | +1.000 | +22.4 | -2.0 | -238267 | -0.003845 |
| opencode-go/deepseek-v4.1-flash / medium | granular-autoimport-index-refresh-and-visibility / 1 | ok / ok | True | True | +0.000 | +4.7 | -8.0 | -432211 | -0.005012 |
| opencode-go/deepseek-v4.1-flash / medium | inherited-config-file-selection / 1 | ok / ok | True | True | +0.000 | -5.8 | +0.0 | -128952 | -0.004063 |
| opencode-go/deepseek-v4.1-flash / medium | overloaded-generic-calls / 1 | ok / ok | True | True | +0.000 | -3.3 | -5.0 | -188833 | -0.003132 |
| opencode-go/deepseek-v4.1-flash / medium | request-snapshot-autoimport-adoption-race / 1 | ok / ok | True | True | +0.000 | -80.6 | -6.0 | -510543 | -0.008111 |
| opencode-go/deepseek-v4.1-flash / medium | reverse-mapped-inference-expansion-cache / 1 | ok / ok | True | True | +0.000 | +90.8 | +12.0 | +327773 | +0.006360 |
| opencode-go/deepseek-v4.1-flash / medium | unbuilt-referenced-project / 1 | invalid_answer / ok | False | False | +1.000 | -13.3 | +0.0 | -79021 | -0.005293 |
| opencode-go/muse-spark-1.3-contributor / medium | ambiguous-expression-type-arguments / 1 | ok / ok | True | True | +0.000 | -1.9 | -10.0 | -288360 | -0.003756 |
| opencode-go/muse-spark-1.3-contributor / medium | conditional-package-subpaths / 1 | ok / ok | True | True | +0.000 | +27.2 | +4.0 | +82555 | +0.000534 |
| opencode-go/muse-spark-1.3-contributor / medium | decorator-private-field-initializer-handoff / 1 | ok / ok | True | True | +0.000 | +27.7 | -2.0 | +83638 | +0.000549 |
| opencode-go/muse-spark-1.3-contributor / medium | dynamic-contentmapper-refresh-and-bundle-cache / 1 | ok / ok | True | True | +0.000 | +25.0 | -1.0 | +64446 | +0.000896 |
| opencode-go/muse-spark-1.3-contributor / medium | granular-autoimport-index-refresh-and-visibility / 1 | ok / ok | True | True | +0.000 | +15.1 | -3.0 | -179994 | -0.003742 |
| opencode-go/muse-spark-1.3-contributor / medium | inherited-config-file-selection / 1 | ok / ok | True | True | +0.000 | +87.2 | +2.0 | +59796 | +0.001143 |
| opencode-go/muse-spark-1.3-contributor / medium | overloaded-generic-calls / 1 | ok / ok | True | True | +0.000 | +12.4 | -5.0 | -125656 | -0.000593 |
| opencode-go/muse-spark-1.3-contributor / medium | request-snapshot-autoimport-adoption-race / 1 | ok / ok | True | True | +0.000 | +46.4 | +7.0 | +134139 | +0.000208 |
| opencode-go/muse-spark-1.3-contributor / medium | reverse-mapped-inference-expansion-cache / 1 | ok / ok | True | True | +0.000 | +30.9 | -1.0 | -19182 | +0.000339 |
| opencode-go/muse-spark-1.3-contributor / medium | unbuilt-referenced-project / 1 | ok / ok | True | True | +0.000 | +16.4 | -3.0 | -249783 | -0.002633 |

## Slopdex command usage

Counts are agent invocation attempts recorded by the PATH wrapper. Help/version are separate; index preparation and ANN validation are excluded. Older count-only artifacts without logs are labeled `unknown`.

| Command | Total invocations |
|---|---|
| help | 4 |
| map | 24 |
| version | 1 |

### By model and thinking mode

| Model | Thinking | Arm | Command | Invocations | Trials using command |
|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash | medium | map | help | 2 | 2/10 |
| opencode-go/deepseek-v4.1-flash | medium | map | map | 13 | 10/10 |
| opencode-go/deepseek-v4.1-flash | medium | map | version | 1 | 1/10 |
| opencode-go/muse-spark-1.3-contributor | medium | map | help | 2 | 2/10 |
| opencode-go/muse-spark-1.3-contributor | medium | map | map | 11 | 10/10 |

### By task and trial

| Model | Thinking | Arm | Task | Repeat | Invocations | Commands |
|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash | medium | map | ambiguous-expression-type-arguments | 1 | 2 | `help` × 1, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | conditional-package-subpaths | 1 | 1 | `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | decorator-private-field-initializer-handoff | 1 | 3 | `help` × 1, `map` × 2 |
| opencode-go/deepseek-v4.1-flash | medium | map | dynamic-contentmapper-refresh-and-bundle-cache | 1 | 1 | `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | granular-autoimport-index-refresh-and-visibility | 1 | 2 | `map` × 2 |
| opencode-go/deepseek-v4.1-flash | medium | map | inherited-config-file-selection | 1 | 2 | `map` × 2 |
| opencode-go/deepseek-v4.1-flash | medium | map | overloaded-generic-calls | 1 | 1 | `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | request-snapshot-autoimport-adoption-race | 1 | 2 | `map` × 1, `version` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | reverse-mapped-inference-expansion-cache | 1 | 1 | `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | unbuilt-referenced-project | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | ambiguous-expression-type-arguments | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | conditional-package-subpaths | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | decorator-private-field-initializer-handoff | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | dynamic-contentmapper-refresh-and-bundle-cache | 1 | 2 | `map` × 2 |
| opencode-go/muse-spark-1.3-contributor | medium | map | granular-autoimport-index-refresh-and-visibility | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | inherited-config-file-selection | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | overloaded-generic-calls | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | request-snapshot-autoimport-adoption-race | 1 | 2 | `help` × 1, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | reverse-mapped-inference-expansion-cache | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | unbuilt-referenced-project | 1 | 2 | `help` × 1, `map` × 1 |

Preparation, embedding/index charges and validation are excluded from agent execution seconds and OpenCode-reported USD. Reported model USD is not necessarily an invoice total; zero is kept as reported, missing is not zero.
Scoring measures symbol identification with valid declaration citations. Explanation/flow correctness requires manual review of saved answers; no LLM judge is used. No significance or confidence-interval claims.
