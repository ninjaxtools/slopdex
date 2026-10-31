# Excalidraw: no-map versus README-guided map

Excalidraw commit: `a758e93d6a03e9f995973703cb886850a398a6d3`
Comparison mode: **saved baseline (not interleaved)**.
`off` = saved no-map baseline; `map` = README-guided map.

OpenCode: `1.18.34`; slopdex: `slopdex 0.46.0`.
Slopdex commit: `48d72f4f545863df400219eea2699daf4241f9e4` (dirty: `False`).
Source SHA-256: `cd70aab1f13490d34f2050de373647ed86956cdec388ea142bb7cda0b29dde8e`; binary SHA-256: `d27c743d625a945ef2d2d34bc93fce60dccb25fa85d56cc5ffa8d1d670b10d07`.
Limits: 40 steps, 600 seconds.

**Historical / saved comparison, not an interleaved experiment.** Baseline results are reused; timing, provider conditions and run order can differ.

## Saved sources

- baseline_sources: `['baselines/8d3d5fb541f07ef01e445b42dd81589b3139dd3803f6ad5cc230a7b1b4c2c85b.json']`
- map_source: `latest-map.json`

Completed **80/80** trials. Complete.
Failures contribute zero F1 / passes. Consumption includes failed trials and may be failure-shortened; it is not, by itself, evidence of efficiency.
Missing telemetry is excluded with metric-specific counts; observed totals with missing telemetry are incomplete spend, not estimated full spend.

## Side-by-side means and accuracy

Means per saved result (including failures).

| Model / variant | Off N/planned | Map N/planned | Off passes | Map passes | Off F1 | Map F1 | Off seconds | Map seconds | Off steps | Map steps | Off tokens | Map tokens | Off USD | Map USD |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | 20/20 | 20/20 | 19/20 | 20/20 | 0.950 | 1.000 | 92.6 | 148.1 | 17.4 | 14.6 | 440393 | 626490 | 0.013977 | 0.024032 |
| opencode-go/muse-spark-1.3-contributor / medium | 20/20 | 20/20 | 20/20 | 20/20 | 1.000 | 1.000 | 48.6 | 74.2 | 10.7 | 12.2 | 277313 | 325686 | 0.005654 | 0.009007 |

### All token categories (mean per saved trial)

Total tokens use a reported nonzero total without adding any categories again. Otherwise total = input + output + reasoning + cache_read + cache_write: reasoning is distinct from output in the pinned OpenCode V1 telemetry. A fallback requires all five categories; missing categories remain unknown, not zero. These are accumulated token presentations across steps, not unique context tokens.

| Model / variant | Arm | Input | Output | Reasoning | Cache read | Cache write | Total | Total source counts |
|---|---|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | off | 47378 | 3842 | 5692 | 383482 | 0 | 440393 | {'reported': 20} |
| opencode-go/deepseek-v4.1-flash / medium | map | 95708 | 5056 | 8484 | 517242 | 0 | 626490 | {'reported': 20} |
| opencode-go/muse-spark-1.3-contributor / medium | off | 42806 | 2394 | 2176 | 229938 | 0 | 277313 | {'reported': 20} |
| opencode-go/muse-spark-1.3-contributor / medium | map | 70508 | 3086 | 4215 | 247877 | 0 | 325686 | {'reported': 20} |

### Observed execution and spend totals

These arm totals are not balanced-pair effects when results or telemetry are missing.

| Model / variant | Arm | N | Seconds | Steps | Input | Output | Reasoning | Cache read | Cache write | Total tokens | USD |
|---|---|---|---|---|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | off | 20 | 1851.3 | 348.0 | 947560 | 76832 | 113839 | 7669632 | 0 | 8807863 | 0.279545 |
| opencode-go/deepseek-v4.1-flash / medium | map | 20 | 2961.8 | 292.0 | 1914151 | 101126 | 169685 | 10344832 | 0 | 12529794 | 0.480644 |
| opencode-go/muse-spark-1.3-contributor / medium | off | 20 | 972.3 | 213.0 | 856114 | 47879 | 43516 | 4598758 | 0 | 5546267 | 0.113088 |
| opencode-go/muse-spark-1.3-contributor / medium | map | 20 | 1484.6 | 243.0 | 1410170 | 61716 | 84300 | 4957537 | 0 | 6513723 | 0.180135 |

### Missing telemetry and status counts

| Model / variant | Arm | Missing metric counts | Statuses |
|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | off | none | {'invalid_answer': 1, 'ok': 19} |
| opencode-go/deepseek-v4.1-flash / medium | map | none | {'ok': 20} |
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
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 1 | wall_seconds | 92.6 | 148.1 | +55.5 | +1110.5 | +87.9 | +60.0 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 1 | steps | 17.4 | 14.6 | -2.8 | -56.0 | -7.5 | -16.1 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 1 | cost_usd | 0.013977 | 0.024032 | +0.010055 | +0.201098 | +89.1 | +71.9 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 1 | input_tokens | 47378.0 | 95707.6 | +48329.6 | +966591.0 | +119.0 | +102.0 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 1 | output_tokens | 3841.6 | 5056.3 | +1214.7 | +24294.0 | +37.8 | +31.6 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 1 | reasoning_tokens | 5691.9 | 8484.2 | +2792.3 | +55846.0 | +97.6 | +49.1 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 1 | cache_read_tokens | 383481.6 | 517241.6 | +133760.0 | +2675200.0 | +68.1 | +34.9 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 1 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 20 | 0 | 0 | 20 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 1 | total_tokens | 440393.2 | 626489.7 | +186096.5 | +3721931.0 | +73.3 | +42.3 | 20 | 20 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 1 | f1 | 0.950 | 1.000 | +0.050 | +1.000 | +0.0 | +5.3 | 20 | 19 | 0 | 1 |
| opencode-go/deepseek-v4.1-flash / medium | all_pairs | 20 | 1 | passed | 0.950 | 1.000 | +0.050 | +1.000 | +0.0 | +5.3 | 20 | 19 | 0 | 1 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 19 | 0 | wall_seconds | 91.1 | 143.7 | +52.6 | +1000.2 | +87.7 | +57.8 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 19 | 0 | steps | 17.0 | 14.0 | -3.0 | -57.0 | -8.1 | -17.6 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 19 | 0 | cost_usd | 0.013794 | 0.023012 | +0.009218 | +0.175135 | +86.0 | +66.8 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 19 | 0 | input_tokens | 47049.9 | 92082.7 | +45032.8 | +855624.0 | +114.4 | +95.7 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 19 | 0 | output_tokens | 3853.0 | 4909.5 | +1056.5 | +20074.0 | +33.7 | +27.4 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 19 | 0 | reasoning_tokens | 5496.8 | 8151.9 | +2655.1 | +50446.0 | +99.7 | +48.3 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 19 | 0 | cache_read_tokens | 375585.7 | 454177.7 | +78592.0 | +1493248.0 | +60.0 | +20.9 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 19 | 0 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 19 | 0 | 0 | 19 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 19 | 0 | total_tokens | 431985.4 | 559321.8 | +127336.4 | +2419392.0 | +65.7 | +29.5 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 19 | 0 | f1 | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_ok | 19 | 0 | passed | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 19 | 0 | wall_seconds | 91.1 | 143.7 | +52.6 | +1000.2 | +87.7 | +57.8 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 19 | 0 | steps | 17.0 | 14.0 | -3.0 | -57.0 | -8.1 | -17.6 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 19 | 0 | cost_usd | 0.013794 | 0.023012 | +0.009218 | +0.175135 | +86.0 | +66.8 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 19 | 0 | input_tokens | 47049.9 | 92082.7 | +45032.8 | +855624.0 | +114.4 | +95.7 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 19 | 0 | output_tokens | 3853.0 | 4909.5 | +1056.5 | +20074.0 | +33.7 | +27.4 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 19 | 0 | reasoning_tokens | 5496.8 | 8151.9 | +2655.1 | +50446.0 | +99.7 | +48.3 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 19 | 0 | cache_read_tokens | 375585.7 | 454177.7 | +78592.0 | +1493248.0 | +60.0 | +20.9 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 19 | 0 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 19 | 0 | 0 | 19 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 19 | 0 | total_tokens | 431985.4 | 559321.8 | +127336.4 | +2419392.0 | +65.7 | +29.5 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 19 | 0 | f1 | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 19 | 19 | 0 | 0 |
| opencode-go/deepseek-v4.1-flash / medium | both_passed | 19 | 0 | passed | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 19 | 19 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | wall_seconds | 48.6 | 74.2 | +25.6 | +512.4 | +63.1 | +52.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | steps | 10.7 | 12.2 | +1.5 | +30.0 | +21.0 | +14.1 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | cost_usd | 0.005654 | 0.009007 | +0.003352 | +0.067047 | +69.1 | +59.3 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | input_tokens | 42805.7 | 70508.5 | +27702.8 | +554056.0 | +77.7 | +64.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | output_tokens | 2393.9 | 3085.8 | +691.9 | +13837.0 | +32.4 | +28.9 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | reasoning_tokens | 2175.8 | 4215.0 | +2039.2 | +40784.0 | +105.8 | +93.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | cache_read_tokens | 229937.9 | 247876.9 | +17939.0 | +358779.0 | +29.6 | +7.8 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 20 | 0 | 0 | 20 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | total_tokens | 277313.3 | 325686.2 | +48372.8 | +967456.0 | +36.8 | +17.4 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | f1 | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | all_pairs | 20 | 0 | passed | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | wall_seconds | 48.6 | 74.2 | +25.6 | +512.4 | +63.1 | +52.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | steps | 10.7 | 12.2 | +1.5 | +30.0 | +21.0 | +14.1 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | cost_usd | 0.005654 | 0.009007 | +0.003352 | +0.067047 | +69.1 | +59.3 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | input_tokens | 42805.7 | 70508.5 | +27702.8 | +554056.0 | +77.7 | +64.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | output_tokens | 2393.9 | 3085.8 | +691.9 | +13837.0 | +32.4 | +28.9 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | reasoning_tokens | 2175.8 | 4215.0 | +2039.2 | +40784.0 | +105.8 | +93.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | cache_read_tokens | 229937.9 | 247876.9 | +17939.0 | +358779.0 | +29.6 | +7.8 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 20 | 0 | 0 | 20 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | total_tokens | 277313.3 | 325686.2 | +48372.8 | +967456.0 | +36.8 | +17.4 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | f1 | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_ok | 20 | 0 | passed | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | wall_seconds | 48.6 | 74.2 | +25.6 | +512.4 | +63.1 | +52.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | steps | 10.7 | 12.2 | +1.5 | +30.0 | +21.0 | +14.1 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | cost_usd | 0.005654 | 0.009007 | +0.003352 | +0.067047 | +69.1 | +59.3 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | input_tokens | 42805.7 | 70508.5 | +27702.8 | +554056.0 | +77.7 | +64.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | output_tokens | 2393.9 | 3085.8 | +691.9 | +13837.0 | +32.4 | +28.9 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | reasoning_tokens | 2175.8 | 4215.0 | +2039.2 | +40784.0 | +105.8 | +93.7 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | cache_read_tokens | 229937.9 | 247876.9 | +17939.0 | +358779.0 | +29.6 | +7.8 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | cache_write_tokens | 0.0 | 0.0 | +0.0 | +0.0 | n/a | n/a | 20 | 0 | 0 | 20 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | total_tokens | 277313.3 | 325686.2 | +48372.8 | +967456.0 | +36.8 | +17.4 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | f1 | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 20 | 20 | 0 | 0 |
| opencode-go/muse-spark-1.3-contributor / medium | both_passed | 20 | 0 | passed | 1.000 | 1.000 | +0.000 | +0.000 | +0.0 | +0.0 | 20 | 20 | 0 | 0 |

### Per-task pairs

| Model / variant | Task / repeat | Off / map status | Both ok | Both passed | F1 Δ | Seconds Δ | Steps Δ | Total tokens Δ | USD Δ |
|---|---|---|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash / medium | alt-drag-copy-pipeline / 1 | ok / ok | True | True | +0.000 | -72.0 | -17.0 | -568303 | -0.001105 |
| opencode-go/deepseek-v4.1-flash / medium | arrow-caption-slide-along-route / 1 | ok / ok | True | True | +0.000 | +54.9 | +3.0 | +321869 | +0.013144 |
| opencode-go/deepseek-v4.1-flash / medium | arrow-end-knob-attach-to-nested-shapes / 1 | ok / ok | True | True | +0.000 | +95.7 | -1.0 | +86419 | -0.002138 |
| opencode-go/deepseek-v4.1-flash / medium | caption-type-step-reflows-oval / 1 | ok / ok | True | True | +0.000 | +68.8 | +4.0 | +613077 | +0.020348 |
| opencode-go/deepseek-v4.1-flash / medium | click-pick-among-overlapping-shapes / 1 | ok / ok | True | True | +0.000 | +80.1 | +1.0 | +557813 | +0.018949 |
| opencode-go/deepseek-v4.1-flash / medium | cluster-command-availability-and-nesting / 1 | ok / ok | True | True | +0.000 | +11.4 | -7.0 | -1858 | +0.007943 |
| opencode-go/deepseek-v4.1-flash / medium | collab-picture-relay-between-peers / 1 | ok / ok | True | True | +0.000 | +31.5 | -4.0 | +59433 | +0.007991 |
| opencode-go/deepseek-v4.1-flash / medium | eraser-sweep-and-commit / 1 | ok / ok | True | True | +0.000 | +138.5 | +2.0 | +91200 | +0.011042 |
| opencode-go/deepseek-v4.1-flash / medium | frame-raster-save-with-embedded-drawing / 1 | ok / ok | True | True | +0.000 | +39.6 | -3.0 | +167588 | +0.014016 |
| opencode-go/deepseek-v4.1-flash / medium | freeform-loop-selection / 1 | ok / ok | True | True | +0.000 | -1.2 | -9.0 | -250106 | +0.000499 |
| opencode-go/deepseek-v4.1-flash / medium | group-turn-with-attached-connectors / 1 | ok / ok | True | True | +0.000 | +31.5 | -7.0 | -240176 | +0.003063 |
| opencode-go/deepseek-v4.1-flash / medium | install-shape-collection-from-link / 1 | ok / ok | True | True | +0.000 | +113.5 | +0.0 | +822998 | +0.024861 |
| opencode-go/deepseek-v4.1-flash / medium | legacy-label-and-polyline-migration / 1 | ok / ok | True | True | +0.000 | +37.5 | +8.0 | +195605 | +0.010875 |
| opencode-go/deepseek-v4.1-flash / medium | offline-drawing-survives-reload-and-tabs / 1 | ok / ok | True | True | +0.000 | +144.7 | +1.0 | +327610 | +0.020432 |
| opencode-go/deepseek-v4.1-flash / medium | peer-cursor-and-away-presence / 1 | ok / ok | True | True | +0.000 | +15.3 | -4.0 | -42296 | +0.005047 |
| opencode-go/deepseek-v4.1-flash / medium | picture-upload-sniff-shrink-decode / 1 | ok / ok | True | True | +0.000 | +108.8 | -4.0 | -122136 | +0.000384 |
| opencode-go/deepseek-v4.1-flash / medium | shadow-peer-viewport / 1 | ok / ok | True | True | +0.000 | -18.6 | -15.0 | -551335 | -0.000922 |
| opencode-go/deepseek-v4.1-flash / medium | startup-typeface-warmup-and-repaint / 1 | ok / ok | True | True | +0.000 | +104.1 | +0.0 | +1019669 | +0.019614 |
| opencode-go/deepseek-v4.1-flash / medium | undo-entry-enactment-internals / 1 | invalid_answer / ok | False | False | +1.000 | +110.3 | +1.0 | +1302539 | +0.025963 |
| opencode-go/deepseek-v4.1-flash / medium | vector-output-drawing-roundtrip / 1 | ok / ok | True | True | +0.000 | +16.1 | -5.0 | -67679 | +0.001093 |
| opencode-go/muse-spark-1.3-contributor / medium | alt-drag-copy-pipeline / 1 | ok / ok | True | True | +0.000 | +33.8 | -2.0 | -42678 | +0.004261 |
| opencode-go/muse-spark-1.3-contributor / medium | arrow-caption-slide-along-route / 1 | ok / ok | True | True | +0.000 | +14.4 | +8.0 | +209278 | +0.001037 |
| opencode-go/muse-spark-1.3-contributor / medium | arrow-end-knob-attach-to-nested-shapes / 1 | ok / ok | True | True | +0.000 | +48.7 | +1.0 | -70382 | +0.002031 |
| opencode-go/muse-spark-1.3-contributor / medium | caption-type-step-reflows-oval / 1 | ok / ok | True | True | +0.000 | -16.1 | +0.0 | -24452 | -0.001496 |
| opencode-go/muse-spark-1.3-contributor / medium | click-pick-among-overlapping-shapes / 1 | ok / ok | True | True | +0.000 | +28.2 | +7.0 | +372049 | +0.008496 |
| opencode-go/muse-spark-1.3-contributor / medium | cluster-command-availability-and-nesting / 1 | ok / ok | True | True | +0.000 | +14.1 | +6.0 | +156878 | +0.004531 |
| opencode-go/muse-spark-1.3-contributor / medium | collab-picture-relay-between-peers / 1 | ok / ok | True | True | +0.000 | +35.4 | -1.0 | +26308 | +0.005008 |
| opencode-go/muse-spark-1.3-contributor / medium | eraser-sweep-and-commit / 1 | ok / ok | True | True | +0.000 | +16.0 | +7.0 | +131563 | +0.000440 |
| opencode-go/muse-spark-1.3-contributor / medium | frame-raster-save-with-embedded-drawing / 1 | ok / ok | True | True | +0.000 | +79.2 | -1.0 | -15042 | +0.005023 |
| opencode-go/muse-spark-1.3-contributor / medium | freeform-loop-selection / 1 | ok / ok | True | True | +0.000 | +27.5 | +0.0 | -32664 | +0.003596 |
| opencode-go/muse-spark-1.3-contributor / medium | group-turn-with-attached-connectors / 1 | ok / ok | True | True | +0.000 | +58.0 | -2.0 | -99880 | +0.004334 |
| opencode-go/muse-spark-1.3-contributor / medium | install-shape-collection-from-link / 1 | ok / ok | True | True | +0.000 | +71.4 | +1.0 | +92411 | +0.005648 |
| opencode-go/muse-spark-1.3-contributor / medium | legacy-label-and-polyline-migration / 1 | ok / ok | True | True | +0.000 | +33.4 | +4.0 | +139926 | +0.005562 |
| opencode-go/muse-spark-1.3-contributor / medium | offline-drawing-survives-reload-and-tabs / 1 | ok / ok | True | True | +0.000 | +5.0 | +3.0 | +105422 | +0.000206 |
| opencode-go/muse-spark-1.3-contributor / medium | peer-cursor-and-away-presence / 1 | ok / ok | True | True | +0.000 | +5.1 | +0.0 | -24613 | +0.003731 |
| opencode-go/muse-spark-1.3-contributor / medium | picture-upload-sniff-shrink-decode / 1 | ok / ok | True | True | +0.000 | +42.9 | +3.0 | +110168 | +0.005356 |
| opencode-go/muse-spark-1.3-contributor / medium | shadow-peer-viewport / 1 | ok / ok | True | True | +0.000 | +48.7 | -3.0 | -28564 | +0.004301 |
| opencode-go/muse-spark-1.3-contributor / medium | startup-typeface-warmup-and-repaint / 1 | ok / ok | True | True | +0.000 | +4.2 | +0.0 | +59866 | +0.002983 |
| opencode-go/muse-spark-1.3-contributor / medium | undo-entry-enactment-internals / 1 | ok / ok | True | True | +0.000 | -41.3 | -1.0 | -61614 | -0.000134 |
| opencode-go/muse-spark-1.3-contributor / medium | vector-output-drawing-roundtrip / 1 | ok / ok | True | True | +0.000 | +3.9 | +0.0 | -36524 | +0.002135 |

## Slopdex command usage

Counts are agent invocation attempts recorded by the PATH wrapper. Help/version are separate; index preparation and ANN validation are excluded. Older count-only artifacts without logs are labeled `unknown`.

| Command | Total invocations |
|---|---|
| faq | 42 |
| help | 3 |
| map | 31 |

### By model and thinking mode

| Model | Thinking | Arm | Command | Invocations | Trials using command |
|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash | medium | map | faq | 28 | 20/20 |
| opencode-go/deepseek-v4.1-flash | medium | map | map | 16 | 16/20 |
| opencode-go/muse-spark-1.3-contributor | medium | map | faq | 14 | 14/20 |
| opencode-go/muse-spark-1.3-contributor | medium | map | help | 3 | 3/20 |
| opencode-go/muse-spark-1.3-contributor | medium | map | map | 15 | 14/20 |

### By task and trial

| Model | Thinking | Arm | Task | Repeat | Invocations | Commands |
|---|---|---|---|---|---|---|
| opencode-go/deepseek-v4.1-flash | medium | map | alt-drag-copy-pipeline | 1 | 1 | `faq` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | arrow-caption-slide-along-route | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | arrow-end-knob-attach-to-nested-shapes | 1 | 1 | `faq` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | caption-type-step-reflows-oval | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | click-pick-among-overlapping-shapes | 1 | 3 | `faq` × 2, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | cluster-command-availability-and-nesting | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | collab-picture-relay-between-peers | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | eraser-sweep-and-commit | 1 | 3 | `faq` × 2, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | frame-raster-save-with-embedded-drawing | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | freeform-loop-selection | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | group-turn-with-attached-connectors | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | install-shape-collection-from-link | 1 | 4 | `faq` × 3, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | legacy-label-and-polyline-migration | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | offline-drawing-survives-reload-and-tabs | 1 | 3 | `faq` × 2, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | peer-cursor-and-away-presence | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | picture-upload-sniff-shrink-decode | 1 | 2 | `faq` × 2 |
| opencode-go/deepseek-v4.1-flash | medium | map | shadow-peer-viewport | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | startup-typeface-warmup-and-repaint | 1 | 3 | `faq` × 2, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | undo-entry-enactment-internals | 1 | 3 | `faq` × 2, `map` × 1 |
| opencode-go/deepseek-v4.1-flash | medium | map | vector-output-drawing-roundtrip | 1 | 1 | `faq` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | alt-drag-copy-pipeline | 1 | 3 | `faq` × 1, `map` × 2 |
| opencode-go/muse-spark-1.3-contributor | medium | map | arrow-caption-slide-along-route | 1 | 0 | — |
| opencode-go/muse-spark-1.3-contributor | medium | map | arrow-end-knob-attach-to-nested-shapes | 1 | 3 | `faq` × 1, `help` × 1, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | caption-type-step-reflows-oval | 1 | 0 | — |
| opencode-go/muse-spark-1.3-contributor | medium | map | click-pick-among-overlapping-shapes | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | cluster-command-availability-and-nesting | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | collab-picture-relay-between-peers | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | eraser-sweep-and-commit | 1 | 0 | — |
| opencode-go/muse-spark-1.3-contributor | medium | map | frame-raster-save-with-embedded-drawing | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | freeform-loop-selection | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | group-turn-with-attached-connectors | 1 | 3 | `faq` × 1, `help` × 1, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | install-shape-collection-from-link | 1 | 3 | `faq` × 1, `help` × 1, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | legacy-label-and-polyline-migration | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | offline-drawing-survives-reload-and-tabs | 1 | 0 | — |
| opencode-go/muse-spark-1.3-contributor | medium | map | peer-cursor-and-away-presence | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | picture-upload-sniff-shrink-decode | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | shadow-peer-viewport | 1 | 2 | `faq` × 1, `map` × 1 |
| opencode-go/muse-spark-1.3-contributor | medium | map | startup-typeface-warmup-and-repaint | 1 | 0 | — |
| opencode-go/muse-spark-1.3-contributor | medium | map | undo-entry-enactment-internals | 1 | 0 | — |
| opencode-go/muse-spark-1.3-contributor | medium | map | vector-output-drawing-roundtrip | 1 | 2 | `faq` × 1, `map` × 1 |

Preparation, embedding/index charges and validation are excluded from agent execution seconds and OpenCode-reported USD. Reported model USD is not necessarily an invoice total; zero is kept as reported, missing is not zero.
Scoring measures symbol identification with valid declaration citations. Explanation/flow correctness requires manual review of saved answers; no LLM judge is used. No significance or confidence-interval claims.
