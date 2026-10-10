# TypeScript: no-map versus README-guided map

TypeScript commit: `edf7da4e93ab65e1f7766deae156c35132c0cde3`
Comparison mode: **saved baseline (not interleaved)**.
`off` = saved no-map baseline; `map` = README-guided map.

OpenCode: `1.18.34`; slopdex: `slopdex 0.39.0`.
Slopdex source commit / binary hashes: **unknown in this historical run**; it is not eligible for automatic map reuse.
Limits: 40 steps, 600 seconds.

**Historical / saved comparison, not an interleaved experiment.** Baseline results are reused; timing, provider conditions and run order can differ.

## Saved sources

- baseline_sources: `['baselines/df5d2a55672cdebca5238c95416812a577d677151e10889733cbdf7ea8f72d5e.json']`
- map_source: `latest-map.json`

Completed **80/80** trials. Complete.
Failures contribute zero F1 / passes. Consumption includes failed trials and may be failure-shortened; it is not, by itself, evidence of efficiency.
Missing telemetry is excluded with metric-specific counts; observed totals with missing telemetry are incomplete spend, not estimated full spend.

## Side-by-side means and accuracy

Means per saved result (including failures).

| Model / variant | Off N/planned | Map N/planned | Off passes | Map passes | Off F1 | Map F1 | Off seconds | Map seconds | Off steps | Map steps | Off tokens | Map tokens | Off USD | Map USD |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | 20/20 | 20/20 | 16/20 | 15/20 | 0.833 | 0.750 | 105.4 | 115.4 | 19.3 | 17.7 | 665750 | 440242 | 0.015676 | 0.012690 |
| opencode-go/muse-spark-1.3-contributor / medium | 20/20 | 20/20 | 20/20 | 20/20 | 1.000 | 1.000 | 35.9 | 50.5 | 10.8 | 10.1 | 315403 | 252416 | 0.006279 | 0.005358 |

### All token categories (mean per saved trial)

Total tokens use a reported nonzero total without adding any categories again. Otherwise total = input + output + reasoning + cache_read + cache_write: reasoning is distinct from output in the pinned OpenCode V1 telemetry. A fallback requires all five categories; missing categories remain unknown, not zero. These are accumulated token presentations across steps, not unique context tokens.

| Model / variant | Arm | Input | Output | Reasoning | Cache read | Cache write | Total | Total source counts |
|---|---|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | off | 50346 | 4263 | 6252 | 604890 | 0 | 665750 | {'reported': 20} |
| opencode-go/deepseek-v4.1-flash / medium | map | 40111 | 3878 | 5289 | 390963 | 0 | 440242 | {'reported': 20} |
| opencode-go/muse-spark-1.3-contributor / medium | off | 49125 | 2406 | 1807 | 262065 | 0 | 315403 | {'reported': 20} |
| opencode-go/muse-spark-1.3-contributor / medium | map | 39880 | 2429 | 2342 | 207765 | 0 | 252416 | {'reported': 20} |

### Observed execution and spend totals

These arm totals are not balanced-pair effects when results or telemetry are missing.

| Model / variant | Arm | N | Seconds | Steps | Input | Output | Reasoning | Cache read | Cache write | Total tokens | USD |
|---|---|---|---|---|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | off | 20 | 2107.3 | 386.0 | 1006915 | 85257 | 125044 | 12097792 | 0 | 13315008 | 0.313511 |
| opencode-go/deepseek-v4.1-flash / medium | map | 20 | 2308.6 | 354.0 | 802227 | 77560 | 105789 | 7819264 | 0 | 8804840 | 0.253801 |
| opencode-go/muse-spark-1.3-contributor / medium | off | 20 | 718.3 | 217.0 | 982508 | 48129 | 36136 | 5241296 | 0 | 6308069 | 0.125586 |
| opencode-go/muse-spark-1.3-contributor / medium | map | 20 | 1010.7 | 201.0 | 797601 | 48587 | 46837 | 4155305 | 0 | 5048330 | 0.107156 |

### Missing telemetry and status counts

| Model / variant | Arm | Missing metric counts | Statuses |
|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | off | none | {'invalid_answer': 3, 'ok': 17} |
| opencode-go/deepseek-v4.1-flash / medium | map | none | {'invalid_answer': 3, 'ok': 16, 'protocol_violation': 1} |
| opencode-go/muse-spark-1.3-contributor / medium | off | none | {'ok': 20} |
| opencode-go/muse-spark-1.3-contributor / medium | map | none | {'ok': 20} |

## Paired effects: map − off

All-pairs consumption includes failures: **failure-shortened runs can look cheaper/faster without being useful**. `both_ok` requires both statuses ok; `both_passed` additionally requires both citation grades to pass. These selected cohorts can have survivor bias; read them with all-pairs accuracy and failure counts.
Δ is map − off. Negative consumption Δ means lower observed use; positive F1/pass Δ means better accuracy. Percentages are (map/off − 1) × 100. Mean case percentages and aggregate percentages (ratio of matched sums) are separate; zero baseline denominators have no case ratio. Every metric uses only pairs with both values, with its own n.

| Model / variant | Available pairs | Planned pairs | Missing pairs | Unmatched results |
|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | 20 | 20 | 0 | {'off': 0, 'map': 0} |
| opencode-go/muse-spark-1.3-contributor / medium | 20 | 20 | 0 | {'off': 0, 'map': 0} |

| Model / variant | Cohort | Pairs | Possible failure-shortened | Metric | Off mean | Map mean | Mean Δ | Total Δ | Mean case Δ % | Aggregate Δ % | Metric n | Ratio n | Missing n | Zero off n |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 5 | wall_seconds | 105.4 | 115.4 | +10.1 | +201.3 | +22.1 | +9.6 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 5 | steps | 19.3 | 17.7 | -1.6 | -32.0 | +2.0 | -8.3 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 5 | cost_usd | 0.015676 | 0.012690 | -0.002985 | -0.059710 | -3.0 | -19.0 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 5 | input_tokens | 50345.8 | 40111.3 | -10234.4 | -204688.0 | -5.9 | -20.3 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 5 | output_tokens | 4262.9 | 3878.0 | -384.9 | -7697.0 | -2.8 | -9.0 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 5 | reasoning_tokens | 6252.2 | 5289.4 | -962.8 | -19255.0 | +16.1 | -15.4 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 5 | cache_read_tokens | 604889.6 | 390963.2 | -213926.4 | -4278528.0 | -6.9 | -35.4 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 5 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 20 | 0 | 0 | 20 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 5 | total_tokens | 665750.4 | 440242.0 | -225508.4 | -4510168.0 | -7.4 | -33.9 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 5 | f1 | 0.833 | 0.750 | -0.083 | -1.667 | -14.7 | -10.0 | 20 | 17 | 0 | 3 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 5 | passed | 0.800 | 0.750 | -0.050 | -1.000 | -18.8 | -6.2 | 20 | 16 | 0 | 4 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 15 | 0 | wall_seconds | 99.4 | 109.0 | +9.6 | +144.1 | +28.2 | +9.7 | 15 | 15 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 15 | 0 | steps | 19.1 | 15.7 | -3.4 | -51.0 | -1.5 | -17.8 | 15 | 15 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 15 | 0 | cost_usd | 0.014772 | 0.011949 | -0.002823 | -0.042352 | +0.7 | -19.1 | 15 | 15 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 15 | 0 | input_tokens | 46907.6 | 38793.1 | -8114.5 | -121717.0 | -0.5 | -17.3 | 15 | 15 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 15 | 0 | output_tokens | 3966.9 | 3652.7 | -314.2 | -4713.0 | +0.4 | -7.9 | 15 | 15 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 15 | 0 | reasoning_tokens | 6011.3 | 5088.7 | -922.5 | -13838.0 | +14.4 | -15.3 | 15 | 15 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 15 | 0 | cache_read_tokens | 583022.9 | 294929.1 | -288093.9 | -4321408.0 | -6.3 | -49.4 | 15 | 15 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 15 | 0 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 15 | 0 | 0 | 15 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 15 | 0 | total_tokens | 639908.7 | 342463.6 | -297445.1 | -4461676.0 | -6.9 | -46.5 | 15 | 15 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 15 | 0 | f1 | 0.978 | 0.933 | -0.044 | -0.667 | -3.3 | -4.5 | 15 | 15 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 15 | 0 | passed | 0.933 | 0.933 | +0.000 | +0.000 | -7.1 | +0.0 | 15 | 14 | 0 | 1 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 13 | 0 | wall_seconds | 95.4 | 107.3 | +11.9 | +155.2 | +33.2 | +12.5 | 13 | 13 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 13 | 0 | steps | 19.2 | 15.4 | -3.8 | -50.0 | -1.0 | -20.0 | 13 | 13 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 13 | 0 | cost_usd | 0.014138 | 0.012111 | -0.002027 | -0.026352 | +6.9 | -14.3 | 13 | 13 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 13 | 0 | input_tokens | 45130.4 | 39238.4 | -5892.0 | -76596.0 | +5.4 | -13.1 | 13 | 13 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 13 | 0 | output_tokens | 3900.3 | 3733.5 | -166.8 | -2168.0 | +5.0 | -4.3 | 13 | 13 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 13 | 0 | reasoning_tokens | 5410.1 | 5145.3 | -264.8 | -3442.0 | +23.1 | -4.9 | 13 | 13 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 13 | 0 | cache_read_tokens | 594018.5 | 299234.5 | -294784.0 | -3832192.0 | +0.2 | -49.6 | 13 | 13 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 13 | 0 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 13 | 0 | 0 | 13 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 13 | 0 | total_tokens | 648459.2 | 347351.7 | -301107.5 | -3914398.0 | -0.7 | -46.4 | 13 | 13 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 13 | 0 | f1 | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 13 | 13 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 13 | 0 | passed | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 13 | 13 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | wall_seconds | 35.9 | 50.5 | +14.6 | +292.4 | +46.0 | +40.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | steps | 10.8 | 10.1 | -0.8 | -16.0 | -2.8 | -7.4 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | cost_usd | 0.006279 | 0.005358 | -0.000922 | -0.018431 | -2.7 | -14.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | input_tokens | 49125.4 | 39880.1 | -9245.4 | -184907.0 | -5.0 | -18.8 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | output_tokens | 2406.4 | 2429.3 | +22.9 | +458.0 | +3.1 | +1.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | reasoning_tokens | 1806.8 | 2341.8 | +535.0 | +10701.0 | +38.6 | +29.6 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | cache_read_tokens | 262064.8 | 207765.2 | -54299.6 | -1085991.0 | -6.1 | -20.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 20 | 0 | 0 | 20 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | total_tokens | 315403.5 | 252416.5 | -62986.9 | -1259739.0 | -5.8 | -20.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | f1 | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | passed | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | wall_seconds | 35.9 | 50.5 | +14.6 | +292.4 | +46.0 | +40.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | steps | 10.8 | 10.1 | -0.8 | -16.0 | -2.8 | -7.4 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | cost_usd | 0.006279 | 0.005358 | -0.000922 | -0.018431 | -2.7 | -14.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | input_tokens | 49125.4 | 39880.1 | -9245.4 | -184907.0 | -5.0 | -18.8 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | output_tokens | 2406.4 | 2429.3 | +22.9 | +458.0 | +3.1 | +1.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | reasoning_tokens | 1806.8 | 2341.8 | +535.0 | +10701.0 | +38.6 | +29.6 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | cache_read_tokens | 262064.8 | 207765.2 | -54299.6 | -1085991.0 | -6.1 | -20.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 20 | 0 | 0 | 20 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | total_tokens | 315403.5 | 252416.5 | -62986.9 | -1259739.0 | -5.8 | -20.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | f1 | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | passed | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | wall_seconds | 35.9 | 50.5 | +14.6 | +292.4 | +46.0 | +40.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | steps | 10.8 | 10.1 | -0.8 | -16.0 | -2.8 | -7.4 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | cost_usd | 0.006279 | 0.005358 | -0.000922 | -0.018431 | -2.7 | -14.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | input_tokens | 49125.4 | 39880.1 | -9245.4 | -184907.0 | -5.0 | -18.8 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | output_tokens | 2406.4 | 2429.3 | +22.9 | +458.0 | +3.1 | +1.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | reasoning_tokens | 1806.8 | 2341.8 | +535.0 | +10701.0 | +38.6 | +29.6 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | cache_read_tokens | 262064.8 | 207765.2 | -54299.6 | -1085991.0 | -6.1 | -20.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 20 | 0 | 0 | 20 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | total_tokens | 315403.5 | 252416.5 | -62986.9 | -1259739.0 | -5.8 | -20.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | f1 | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | passed | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 20 | 20 | 0 | 0 |

### Per-task pairs

| Model / variant | Task / repeat | Off / map status | Both ok | Both passed | F1 Δ | Seconds Δ | Steps Δ | Total tokens Δ | USD Δ |
|---|---|---|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | ambiguous-expression-type-arguments / 1 | invalid_answer / protocol_violation | False | False | +0.000 | -9.4 | -4.0 | -34779 | +0.001641 |
| opencode-go/deepseek-v4.1-flash / medium | async-generator-iterator-close-helper-flow / 1 | ok / ok | True | True | +0.000 | -56.7 | -19.0 | -1302934 | -0.010056 |
| opencode-go/deepseek-v4.1-flash / medium | async-lexical-runtime-semantics / 1 | ok / ok | True | True | +0.000 | +30.9 | +5.0 | +53840 | +0.001988 |
| opencode-go/deepseek-v4.1-flash / medium | conditional-package-subpaths / 1 | ok / ok | True | True | +0.000 | +2.7 | +1.0 | -21104 | -0.000886 |
| opencode-go/deepseek-v4.1-flash / medium | declaration-alias-visibility-fixed-point / 1 | ok / ok | True | True | +0.000 | -14.2 | -1.0 | -121832 | -0.002673 |
| opencode-go/deepseek-v4.1-flash / medium | decorator-private-field-initializer-handoff / 1 | ok / invalid_answer | False | False | -1.000 | +76.9 | +10.0 | +123853 | -0.005095 |
| opencode-go/deepseek-v4.1-flash / medium | discriminant-branch-types / 1 | ok / ok | True | True | +0.000 | +43.4 | +9.0 | +209341 | +0.007839 |
| opencode-go/deepseek-v4.1-flash / medium | dynamic-contentmapper-refresh-and-bundle-cache / 1 | invalid_answer / invalid_answer | False | False | +0.000 | +42.2 | -2.0 | -346495 | -0.006910 |
| opencode-go/deepseek-v4.1-flash / medium | granular-autoimport-index-refresh-and-visibility / 1 | ok / ok | True | True | +0.000 | +13.4 | -15.0 | -996978 | -0.011998 |
| opencode-go/deepseek-v4.1-flash / medium | inherited-config-file-selection / 1 | ok / ok | True | True | +0.000 | +100.5 | -1.0 | -149728 | -0.001717 |
| opencode-go/deepseek-v4.1-flash / medium | lazy-auto-import-completion / 1 | ok / ok | True | False | -1.000 | -4.0 | +5.0 | -146481 | -0.004525 |
| opencode-go/deepseek-v4.1-flash / medium | mapped-diagnostic-provenance-and-synthesized-output / 1 | ok / ok | True | True | +0.000 | -57.8 | -19.0 | -1423040 | -0.011445 |
| opencode-go/deepseek-v4.1-flash / medium | overloaded-generic-calls / 1 | ok / ok | True | True | +0.000 | -1.4 | -6.0 | -206432 | -0.003128 |
| opencode-go/deepseek-v4.1-flash / medium | rename-occurrence-consistency / 1 | ok / ok | True | True | +0.000 | +8.0 | -1.0 | -15955 | +0.002780 |
| opencode-go/deepseek-v4.1-flash / medium | request-snapshot-autoimport-adoption-race / 1 | ok / invalid_answer | False | False | -1.000 | -16.2 | +19.0 | +382865 | -0.000421 |
| opencode-go/deepseek-v4.1-flash / medium | reverse-mapped-inference-expansion-cache / 1 | ok / ok | True | True | +0.000 | +65.5 | +6.0 | +177236 | +0.004516 |
| opencode-go/deepseek-v4.1-flash / medium | single-file-program-reuse / 1 | ok / ok | True | False | +0.333 | -7.0 | -6.0 | -400797 | -0.011476 |
| opencode-go/deepseek-v4.1-flash / medium | source-map-pending-segments-round-trip / 1 | ok / ok | True | True | +0.000 | +32.3 | +0.0 | -30238 | -0.001710 |
| opencode-go/deepseek-v4.1-flash / medium | unbuilt-referenced-project / 1 | invalid_answer / ok | False | False | +1.000 | -36.4 | -4.0 | -173936 | -0.006574 |
| opencode-go/deepseek-v4.1-flash / medium | watcher-missing-lookup-and-project-dirty-state / 1 | ok / ok | True | True | +0.000 | -11.2 | -9.0 | -86574 | +0.000138 |
| opencode-go/muse-spark-1.3-contributor / medium | ambiguous-expression-type-arguments / 1 | ok / ok | True | True | +0.000 | -14.1 | -11.0 | -324448 | -0.004448 |
| opencode-go/muse-spark-1.3-contributor / medium | async-generator-iterator-close-helper-flow / 1 | ok / ok | True | True | +0.000 | +34.6 | +9.0 | +61675 | +0.000468 |
| opencode-go/muse-spark-1.3-contributor / medium | async-lexical-runtime-semantics / 1 | ok / ok | True | True | +0.000 | +6.4 | -1.0 | -27189 | -0.000408 |
| opencode-go/muse-spark-1.3-contributor / medium | conditional-package-subpaths / 1 | ok / ok | True | True | +0.000 | +9.0 | +0.0 | +14508 | +0.000005 |
| opencode-go/muse-spark-1.3-contributor / medium | declaration-alias-visibility-fixed-point / 1 | ok / ok | True | True | +0.000 | +19.5 | +0.0 | -604355 | -0.005327 |
| opencode-go/muse-spark-1.3-contributor / medium | decorator-private-field-initializer-handoff / 1 | ok / ok | True | True | +0.000 | +38.7 | -2.0 | +19239 | -0.001021 |
| opencode-go/muse-spark-1.3-contributor / medium | discriminant-branch-types / 1 | ok / ok | True | True | +0.000 | +15.3 | +2.0 | +39014 | +0.000936 |
| opencode-go/muse-spark-1.3-contributor / medium | dynamic-contentmapper-refresh-and-bundle-cache / 1 | ok / ok | True | True | +0.000 | +22.5 | -2.0 | -44046 | -0.000600 |
| opencode-go/muse-spark-1.3-contributor / medium | granular-autoimport-index-refresh-and-visibility / 1 | ok / ok | True | True | +0.000 | +0.0 | -5.0 | -212150 | -0.003248 |
| opencode-go/muse-spark-1.3-contributor / medium | inherited-config-file-selection / 1 | ok / ok | True | True | +0.000 | +9.2 | -2.0 | +21222 | +0.001302 |
| opencode-go/muse-spark-1.3-contributor / medium | lazy-auto-import-completion / 1 | ok / ok | True | True | +0.000 | -0.5 | -4.0 | +4257 | -0.001885 |
| opencode-go/muse-spark-1.3-contributor / medium | mapped-diagnostic-provenance-and-synthesized-output / 1 | ok / ok | True | True | +0.000 | -5.0 | -1.0 | -217970 | -0.004888 |
| opencode-go/muse-spark-1.3-contributor / medium | overloaded-generic-calls / 1 | ok / ok | True | True | +0.000 | +55.9 | -7.0 | -164841 | -0.001275 |
| opencode-go/muse-spark-1.3-contributor / medium | rename-occurrence-consistency / 1 | ok / ok | True | True | +0.000 | +14.6 | -2.0 | -12983 | +0.001270 |
| opencode-go/muse-spark-1.3-contributor / medium | request-snapshot-autoimport-adoption-race / 1 | ok / ok | True | True | +0.000 | +20.1 | +12.0 | +402809 | +0.001335 |
| opencode-go/muse-spark-1.3-contributor / medium | reverse-mapped-inference-expansion-cache / 1 | ok / ok | True | True | +0.000 | +19.9 | +2.0 | +57441 | +0.001809 |
| opencode-go/muse-spark-1.3-contributor / medium | single-file-program-reuse / 1 | ok / ok | True | True | +0.000 | +5.7 | -1.0 | -34004 | -0.000090 |
| opencode-go/muse-spark-1.3-contributor / medium | source-map-pending-segments-round-trip / 1 | ok / ok | True | True | +0.000 | +14.2 | +0.0 | -44388 | -0.000248 |
| opencode-go/muse-spark-1.3-contributor / medium | unbuilt-referenced-project / 1 | ok / ok | True | True | +0.000 | +23.7 | +2.0 | +130257 | +0.000867 |
| opencode-go/muse-spark-1.3-contributor / medium | watcher-missing-lookup-and-project-dirty-state / 1 | ok / ok | True | True | +0.000 | +2.6 | -5.0 | -323787 | -0.002984 |

## Slopdex command usage

Counts are agent invocation attempts recorded by the PATH wrapper. Help/version are separate; index preparation and ANN validation are excluded. Older count-only artifacts without logs are labeled `unknown`.

| Command | Total invocations |
|---|---|
| help | 12 |
| map | 59 |

### By model and thinking mode

| Model | Thinking | Arm | Command | Invocations | Trials using command |
|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash | medium | map | help | 5 | 5/20 |
| opencode-go/deepseek-v4.1-flash | medium | map | map | 35 | 20/20 |
| opencode-go/muse-spark-1.3-contributor | medium | map | help | 7 | 4/20 |
| opencode-go/muse-spark-1.3-contributor | medium | map | map | 24 | 20/20 |

### By task and trial

| Model | Thinking | Arm | Task | Repeat | Invocations | Commands |
|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash | medium | map | ambiguous-expression-type-arguments | 1 | 5 | `help` × 1, `map` × 4 |
| opencode-go/deepseek-v4.1-flash | medium | map | async-generator-iterator-close-helper-flow | 1 | 3 | `help` × 1, `map` × 2 |
| opencode-go/deepseek-v4.1-flash | medium | map | async-lexical-runtime-semantics | 1 | 1 | `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | conditional-package-subpaths | 1 | 2 | `help` × 1, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | declaration-alias-visibility-fixed-point | 1 | 3 | `map` × 3 |
| opencode-go/deepseek-v4.1-flash | medium | map | decorator-private-field-initializer-handoff | 1 | 6 | `help` × 1, `map` × 5 |
| opencode-go/deepseek-v4.1-flash | medium | map | discriminant-branch-types | 1 | 1 | `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | dynamic-contentmapper-refresh-and-bundle-cache | 1 | 1 | `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | granular-autoimport-index-refresh-and-visibility | 1 | 1 | `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | inherited-config-file-selection | 1 | 2 | `map` × 2 |
| opencode-go/deepseek-v4.1-flash | medium | map | lazy-auto-import-completion | 1 | 2 | `map` × 2 |
| opencode-go/deepseek-v4.1-flash | medium | map | mapped-diagnostic-provenance-and-synthesized-output | 1 | 1 | `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | overloaded-generic-calls | 1 | 1 | `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | rename-occurrence-consistency | 1 | 1 | `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | request-snapshot-autoimport-adoption-race | 1 | 2 | `help` × 1, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | reverse-mapped-inference-expansion-cache | 1 | 1 | `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | single-file-program-reuse | 1 | 2 | `map` × 2 |
| opencode-go/deepseek-v4.1-flash | medium | map | source-map-pending-segments-round-trip | 1 | 1 | `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | unbuilt-referenced-project | 1 | 1 | `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | watcher-missing-lookup-and-project-dirty-state | 1 | 3 | `map` × 3 |
| opencode-go/muse-spark-1.3-contributor | medium | map | ambiguous-expression-type-arguments | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | async-generator-iterator-close-helper-flow | 1 | 6 | `help` × 2, `map` × 4 |
| opencode-go/muse-spark-1.3-contributor | medium | map | async-lexical-runtime-semantics | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | conditional-package-subpaths | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | declaration-alias-visibility-fixed-point | 1 | 3 | `help` × 2, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | decorator-private-field-initializer-handoff | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | discriminant-branch-types | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | dynamic-contentmapper-refresh-and-bundle-cache | 1 | 2 | `help` × 1, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | granular-autoimport-index-refresh-and-visibility | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | inherited-config-file-selection | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | lazy-auto-import-completion | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | mapped-diagnostic-provenance-and-synthesized-output | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | overloaded-generic-calls | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | rename-occurrence-consistency | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | request-snapshot-autoimport-adoption-race | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | reverse-mapped-inference-expansion-cache | 1 | 3 | `help` × 2, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | single-file-program-reuse | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | source-map-pending-segments-round-trip | 1 | 2 | `map` × 2 |
| opencode-go/muse-spark-1.3-contributor | medium | map | unbuilt-referenced-project | 1 | 1 | `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | watcher-missing-lookup-and-project-dirty-state | 1 | 1 | `map` × 1 |

Preparation, embedding/index charges and validation are excluded from agent execution seconds and OpenCode-reported USD. Reported model USD is not necessarily an invoice total; zero is kept as reported, missing is not zero.
Scoring measures symbol identification with valid declaration citations. Explanation/flow correctness requires manual review of saved answers; no LLM judge is used. No significance or confidence-interval claims.
