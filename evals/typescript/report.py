"""Offline, reusable reports for no-map versus README-guided map trials."""

import collections
import math
from pathlib import Path
import statistics


ARMS = ("off", "map")
IDENTITY = ("model", "variant", "task", "repeat", "arm")
TOKEN_COMPONENTS = ("input", "output", "reasoning", "cache_read", "cache_write")
TOKEN_NAMES = (*TOKEN_COMPONENTS, "total")
METRICS = ("wall_seconds", "steps", "cost_usd", *(f"{name}_tokens" for name in TOKEN_NAMES), "f1", "passed")
TOKEN_DEFINITION = (
    "Total tokens use a reported nonzero total without adding any categories again. "
    "Otherwise total = input + output + reasoning + cache_read + cache_write: "
    "reasoning is distinct from output in the pinned OpenCode V1 telemetry. "
    "A fallback requires all five categories; missing categories remain unknown, not zero. "
    "These are accumulated token presentations across steps, not unique context tokens."
)


def number(value):
    return value if type(value) in (int, float) and math.isfinite(value) else None


def mean(values):
    known = [value for value in values if value is not None]
    return statistics.mean(known) if known else None


def total(values):
    known = [value for value in values if value is not None]
    return sum(known) if known else None


def identity(row):
    return tuple(row[key] for key in IDENTITY)


def token_usage(result):
    saved = result.get("tokens") or {}
    usage = {name: number(saved.get(name)) for name in TOKEN_COMPONENTS}
    reported = number(saved.get("total"))
    if reported is not None and reported > 0:
        usage["total"], source = reported, "reported"
    elif all(value is not None for value in usage.values()):
        usage["total"], source = sum(usage.values()), "category_sum"
    else:
        usage["total"], source = None, "missing"
    return usage, source, reported


def normalize(result):
    usage, source, reported = token_usage(result)
    grade = result.get("grade") or {}
    ok = result.get("status") == "ok"
    f1 = number(grade.get("f1")) if ok else 0.0
    passed = grade.get("passed") if ok and type(grade.get("passed")) is bool else (False if not ok else None)
    commands = dict(result.get("slopdex_commands") or {})
    if not commands and "slopdex_commands" not in result and result.get("slopdex_calls"):
        commands = {"unknown": result["slopdex_calls"]}
    return {
        **{key: result[key] for key in ("id", *IDENTITY)},
        "status": result.get("status", "unknown"), "f1": f1, "passed": passed,
        "valid_answer": grade.get("valid_answer"),
        "wall_seconds": number(result.get("wall_seconds")),
        "steps": number(result.get("steps")), "cost_usd": number(result.get("cost_usd")),
        "tokens": usage, "reported_total_tokens": reported, "total_tokens_source": source,
        "slopdex_calls": sum(commands.values()),
        "saved_slopdex_calls": result.get("slopdex_calls"),
        "slopdex_commands": dict(sorted(commands.items())),
    }


def metric_values(row):
    return {
        **{name: row[name] for name in ("wall_seconds", "steps", "cost_usd", "f1")},
        "passed": int(row["passed"]) if row["passed"] is not None else None,
        **{f"{name}_tokens": row["tokens"][name] for name in TOKEN_NAMES},
    }


def group_summary(model, variant, arm, planned, rows):
    values = [metric_values(row) for row in rows]
    counts = {name: sum(value[name] is not None for value in values) for name in METRICS}
    commands, command_trials = collections.Counter(), collections.Counter()
    for row in rows:
        commands.update(row["slopdex_commands"])
        command_trials.update(name for name, count in row["slopdex_commands"].items() if count)
    return {
        "model": model, "variant": variant, "arm": arm, "planned": planned,
        "n": len(rows), "missing_results": planned - len(rows),
        "ok": sum(row["status"] == "ok" for row in rows),
        "passed": sum(row["passed"] is True for row in rows),
        "pass_rate": mean(value["passed"] for value in values),
        "mean_f1": mean(row["f1"] for row in rows),
        "mean_wall_seconds": mean(row["wall_seconds"] for row in rows),
        "total_wall_seconds": total(row["wall_seconds"] for row in rows),
        "mean_steps": mean(row["steps"] for row in rows),
        "total_steps": total(row["steps"] for row in rows),
        "mean_cost_usd": mean(row["cost_usd"] for row in rows),
        "total_cost_usd": total(row["cost_usd"] for row in rows),
        "mean_tokens": {name: mean(row["tokens"][name] for row in rows) for name in TOKEN_NAMES},
        "sum_tokens": {name: total(row["tokens"][name] for row in rows) for name in TOKEN_NAMES},
        "telemetry_n": counts,
        "missing_telemetry": {name: len(rows) - count for name, count in counts.items()},
        "statuses": dict(sorted(collections.Counter(row["status"] for row in rows).items())),
        "total_tokens_sources": dict(sorted(collections.Counter(row["total_tokens_source"] for row in rows).items())),
        "slopdex_calls": sum(row["slopdex_calls"] for row in rows),
        "slopdex_commands": dict(sorted(commands.items())),
        "slopdex_command_trials": dict(sorted(command_trials.items())),
    }


def paired_case(reference, comparison):
    case = {
        **{key: reference[key] for key in ("model", "variant", "task", "repeat")},
        "reference_arm": "off", "comparison_arm": "map",
        "reference_id": reference["id"], "comparison_id": comparison["id"],
        "reference_status": reference["status"], "comparison_status": comparison["status"],
        "both_ok": reference["status"] == comparison["status"] == "ok",
        "both_passed": reference["passed"] is True and comparison["passed"] is True,
        "total_tokens_sources": {"off": reference["total_tokens_source"], "map": comparison["total_tokens_source"]},
        "metrics": {},
    }
    case["failure_shortened_possible"] = not case["both_ok"]
    baseline, treatment = metric_values(reference), metric_values(comparison)
    for name in METRICS:
        off, mapped = baseline[name], treatment[name]
        delta = mapped - off if off is not None and mapped is not None else None
        ratio = mapped / off if delta is not None and off > 0 else None
        case["metrics"][name] = {
            "off": off, "map": mapped, "delta": delta, "ratio": ratio,
            "delta_pct": (ratio - 1) * 100 if ratio is not None else None,
        }
    # Keep the original report's convenient names, now for every observed pair.
    for alias, metric in (("f1_delta", "f1"), ("seconds_delta", "wall_seconds"),
                          ("steps_delta", "steps"), ("input_tokens_delta", "input_tokens"),
                          ("total_tokens_delta", "total_tokens"), ("cost_delta_usd", "cost_usd")):
        case[alias] = case["metrics"][metric]["delta"]
    return case


def cohort_summary(cases):
    metrics = {}
    for name in METRICS:
        values = [case["metrics"][name] for case in cases]
        matched = [value for value in values if value["delta"] is not None]
        off_total, map_total = total(value["off"] for value in matched), total(value["map"] for value in matched)
        aggregate_ratio = map_total / off_total if off_total is not None and off_total > 0 else None
        metrics[name] = {
            "n": len(matched), "missing_n": len(cases) - len(matched),
            "off_missing_n": sum(value["off"] is None for value in values),
            "map_missing_n": sum(value["map"] is None for value in values),
            "mean_off": mean(value["off"] for value in matched),
            "mean_map": mean(value["map"] for value in matched),
            "sum_off": off_total, "sum_map": map_total,
            "mean_delta": mean(value["delta"] for value in matched),
            "sum_delta": total(value["delta"] for value in matched),
            "ratio_n": sum(value["ratio"] is not None for value in matched),
            "zero_reference_n": sum(value["off"] == 0 for value in matched),
            "mean_case_ratio": mean(value["ratio"] for value in matched),
            "mean_delta_pct": mean(value["delta_pct"] for value in matched),
            "aggregate_ratio": aggregate_ratio,
            "aggregate_delta_pct": (aggregate_ratio - 1) * 100 if aggregate_ratio is not None else None,
        }
    return {
        "n": len(cases),
        "failure_shortened_possible_n": sum(case["failure_shortened_possible"] for case in cases),
        "metrics": metrics,
    }


def build_summary(manifest, results):
    """Build a deterministic comparison from a manifest and flat saved results.

    Only off/map participate. Duplicate identities are rejected rather than paired
    by arrival order. This function never imports the runner or changes inputs.
    """
    planned = [trial for trial in manifest["trials"] if trial["arm"] in ARMS]
    by_identity, ids = {}, set()
    for trial in planned:
        key = identity(trial)
        if key in by_identity or trial["id"] in ids:
            raise ValueError("Duplicate planned trial identity or ID")
        by_identity[key] = trial
        ids.add(trial["id"])
    rows, seen = [], set()
    for result in results:
        if result["arm"] not in ARMS:
            continue
        key = identity(result)
        if key in seen:
            raise ValueError("Duplicate result identity")
        if key not in by_identity or result["id"] != by_identity[key]["id"]:
            raise ValueError("Result identity does not match a planned trial")
        seen.add(key)
        rows.append(normalize(result))
    rows.sort(key=lambda row: (*identity(row)[:-1], ARMS.index(row["arm"])))
    groups, group_plans = collections.defaultdict(list), collections.Counter()
    cases, planned_cases = collections.defaultdict(dict), collections.defaultdict(dict)
    for trial in planned:
        group_plans[(trial["model"], trial["variant"], trial["arm"])] += 1
        planned_cases[identity(trial)[:-1]][trial["arm"]] = trial
    for row in rows:
        groups[(row["model"], row["variant"], row["arm"])].append(row)
        cases[identity(row)[:-1]][row["arm"]] = row
    paired = [paired_case(arms["off"], arms["map"]) for _, arms in sorted(cases.items()) if all(arm in arms for arm in ARMS)]
    comparisons = []
    for model, variant in sorted({key[:2] for key in planned_cases}):
        selected = [case for case in paired if (case["model"], case["variant"]) == (model, variant)]
        planned_pairs = sum(key[:2] == (model, variant) and all(arm in arms for arm in ARMS) for key, arms in planned_cases.items())
        unmatched = {arm: sum(key[:2] == (model, variant) and arm in arms and not all(other in arms for other in ARMS) for key, arms in cases.items()) for arm in ARMS}
        comparisons.append({
            "model": model, "variant": variant, "reference_arm": "off", "comparison_arm": "map",
            "planned_pairs": planned_pairs, "available_pairs": len(selected),
            "missing_pairs": planned_pairs - len(selected), "unmatched_results": unmatched,
            "cohorts": {
                "all_pairs": cohort_summary(selected),
                "both_ok": cohort_summary([case for case in selected if case["both_ok"]]),
                "both_passed": cohort_summary([case for case in selected if case["both_passed"]]),
            },
        })
    commands = collections.Counter()
    for row in rows:
        commands.update(row["slopdex_commands"])
    return {
        "schema_version": 1, "comparison_mode": manifest.get("comparison_mode", "interleaved"),
        "sources": {key: value for key, value in manifest.items() if "source" in key or key.endswith("_ref")},
        "completed": len(rows), "planned": len(planned), "complete": len(rows) == len(planned),
        "ignored_planned_trials": len(manifest["trials"]) - len(planned),
        "token_total_definition": TOKEN_DEFINITION,
        "groups": [group_summary(*key, group_plans[key], groups[key]) for key in sorted(group_plans, key=lambda key: (*key[:2], ARMS.index(key[2])))],
        "paired": paired, "paired_summary": comparisons, "trials": rows,
        "slopdex_commands": dict(sorted(commands.items())),
    }


def fmt(value, digits=1, signed=False):
    if value is None:
        return "n/a"
    return f"{value:+.{digits}f}" if signed else f"{value:.{digits}f}"


def table(lines, headers, rows):
    lines += ["| " + " | ".join(headers) + " |", "|" + "|".join("---" for _ in headers) + "|"]
    lines += ["| " + " | ".join(str(value).replace("|", "\\|").replace("\n", " ") for value in row) + " |" for row in rows]


def markdown(manifest, summary):
    """Render the same summary for job reports or durable saved comparisons."""
    mode = summary["comparison_mode"]
    lines = ["# TypeScript: no-map versus README-guided map", "", f"TypeScript commit: `{manifest.get('commit', 'unknown')}`",
              f"Comparison mode: **{mode}**.",
              "`off` = saved no-map baseline; `map` = README-guided map.", ""]
    if manifest.get("opencode_version"):
        lines += [f"OpenCode: `{manifest['opencode_version']}`; slopdex: `{manifest.get('slopdex_version') or 'not used'}`."]
        implementation = manifest.get("implementation") or {}
        if implementation:
            lines += [f"Slopdex commit: `{implementation.get('commit', 'unknown')}` (dirty: `{implementation.get('dirty', False)}`).",
                      f"Source SHA-256: `{implementation.get('source_sha256', 'unknown')}`; binary SHA-256: `{implementation.get('binary_sha256', 'unknown')}`."]
        elif manifest.get("slopdex_version"):
            lines += ["Slopdex source commit / binary hashes: **unknown in this historical run**; it is not eligible for automatic map reuse."]
        config = manifest.get("config", {})
        lines += [f"Limits: {config.get('max_steps', 'unknown')} steps, {config.get('timeout_seconds', 'unknown')} seconds.", ""]
    if mode != "interleaved":
        lines += ["**Historical / saved comparison, not an interleaved experiment.** Baseline results are reused; timing, provider conditions and run order can differ.", ""]
    else:
        lines += ["Interleaved trial schedule; matching uses model, variant, task and repeat, never file or execution order.", ""]
    if summary["sources"]:
        lines += ["## Saved sources", ""]
        for name, value in sorted(summary["sources"].items()):
            lines.append(f"- {name}: `{value}`")
        lines.append("")
    if summary["ignored_planned_trials"]:
        lines += [f"Only off/map are compared; {summary['ignored_planned_trials']} historical trials from other arms are excluded.", ""]
    lines += [f"Completed **{summary['completed']}/{summary['planned']}** trials. " + ("Complete." if summary["complete"] else "**Partial run: missing results are not zero-cost trials or failures.**"),
              "Failures contribute zero F1 / passes. Consumption includes failed trials and may be failure-shortened; it is not, by itself, evidence of efficiency.",
              "Missing telemetry is excluded with metric-specific counts; observed totals with missing telemetry are incomplete spend, not estimated full spend.", "",
              "## Side-by-side means and accuracy", "", "Means per saved result (including failures).", ""]
    side_rows, token_rows, total_rows, missing_rows = [], [], [], []
    for comparison in summary["paired_summary"]:
        model, variant = comparison["model"], comparison["variant"]
        groups = {group["arm"]: group for group in summary["groups"] if (group["model"], group["variant"]) == (model, variant)}
        baseline, mapped = groups.get("off"), groups.get("map")
        label = f"{model} / {variant}"

        def side(field, digits=1):
            return [fmt(group[field], digits) if group else "n/a" for group in (baseline, mapped)]

        side_rows.append([label, *[f"{group['n']}/{group['planned']}" if group else "0/0" for group in (baseline, mapped)],
                          *[f"{group['passed']}/{group['telemetry_n']['passed']}" if group else "n/a" for group in (baseline, mapped)],
                          *side("mean_f1", 3), *side("mean_wall_seconds"), *side("mean_steps"),
                          *[fmt(group["mean_tokens"]["total"], 0) if group else "n/a" for group in (baseline, mapped)],
                          *side("mean_cost_usd", 6)])
    table(lines, ["Model / variant", "Off N/planned", "Map N/planned", "Off passes", "Map passes", "Off F1", "Map F1", "Off seconds", "Map seconds", "Off steps", "Map steps", "Off tokens", "Map tokens", "Off USD", "Map USD"], side_rows)
    for group in summary["groups"]:
        label = [f"{group['model']} / {group['variant']}", group["arm"]]
        token_rows.append([*label, *[fmt(group["mean_tokens"][name], 0) for name in TOKEN_NAMES], str(group["total_tokens_sources"])])
        total_rows.append([*label, group["n"], fmt(group["total_wall_seconds"]), fmt(group["total_steps"]),
                           *[fmt(group["sum_tokens"][name], 0) for name in TOKEN_NAMES], fmt(group["total_cost_usd"], 6)])
        missing = ", ".join(f"{name}={count}" for name, count in group["missing_telemetry"].items() if count) or "none"
        missing_rows.append([*label, missing, str(group["statuses"])])
    lines += ["", "### All token categories (mean per saved trial)", "", TOKEN_DEFINITION, ""]
    table(lines, ["Model / variant", "Arm", "Input", "Output", "Reasoning", "Cache read", "Cache write", "Total", "Total source counts"], token_rows)
    lines += ["", "### Observed execution and spend totals", "", "These arm totals are not balanced-pair effects when results or telemetry are missing.", ""]
    table(lines, ["Model / variant", "Arm", "N", "Seconds", "Steps", "Input", "Output", "Reasoning", "Cache read", "Cache write", "Total tokens", "USD"], total_rows)
    lines += ["", "### Missing telemetry and status counts", ""]
    table(lines, ["Model / variant", "Arm", "Missing metric counts", "Statuses"], missing_rows)
    lines += ["", "## Paired effects: map − off", "",
              "All-pairs consumption includes failures: **failure-shortened runs can look cheaper/faster without being useful**. `both_ok` requires both statuses ok; `both_passed` additionally requires both citation grades to pass. These selected cohorts can have survivor bias; read them with all-pairs accuracy and failure counts.",
              "Δ is map − off. Negative consumption Δ means lower observed use; positive F1/pass Δ means better accuracy. Percentages are (map/off − 1) × 100. Mean case percentages and aggregate percentages (ratio of matched sums) are separate; zero baseline denominators have no case ratio. Every metric uses only pairs with both values, with its own n.", ""]
    count_rows, effect_rows = [], []
    for comparison in summary["paired_summary"]:
        label = f"{comparison['model']} / {comparison['variant']}"
        count_rows.append([label, comparison["available_pairs"], comparison["planned_pairs"], comparison["missing_pairs"], str(comparison["unmatched_results"])])
        for name, cohort in comparison["cohorts"].items():
            for metric, values in cohort["metrics"].items():
                digits = 6 if metric == "cost_usd" else (3 if metric in ("f1", "passed") else 1)
                effect_rows.append([label, name, cohort["n"], cohort["failure_shortened_possible_n"], metric,
                                    fmt(values["mean_off"], digits), fmt(values["mean_map"], digits),
                                    fmt(values["mean_delta"], digits, True), fmt(values["sum_delta"], digits, True),
                                    fmt(values["mean_delta_pct"], 1, True), fmt(values["aggregate_delta_pct"], 1, True),
                                    values["n"], values["ratio_n"], values["missing_n"], values["zero_reference_n"]])
    table(lines, ["Model / variant", "Available pairs", "Planned pairs", "Missing pairs", "Unmatched results"], count_rows)
    lines.append("")
    table(lines, ["Model / variant", "Cohort", "Pairs", "Possible failure-shortened", "Metric", "Off mean", "Map mean", "Mean Δ", "Total Δ", "Mean case Δ %", "Aggregate Δ %", "Metric n", "Ratio n", "Missing n", "Zero off n"], effect_rows)
    lines += ["", "### Per-task pairs", ""]
    table(lines, ["Model / variant", "Task / repeat", "Off / map status", "Both ok", "Both passed", "F1 Δ", "Seconds Δ", "Steps Δ", "Total tokens Δ", "USD Δ"],
          [[f"{case['model']} / {case['variant']}", f"{case['task']} / {case['repeat']}", f"{case['reference_status']} / {case['comparison_status']}", case["both_ok"], case["both_passed"], fmt(case["f1_delta"], 3, True), fmt(case["seconds_delta"], 1, True), fmt(case["steps_delta"], 1, True), fmt(case["total_tokens_delta"], 0, True), fmt(case["cost_delta_usd"], 6, True)] for case in summary["paired"]])
    lines += ["", "## Slopdex command usage", "",
              "Counts are agent invocation attempts recorded by the PATH wrapper. Help/version are separate; index preparation and ANN validation are excluded. Older count-only artifacts without logs are labeled `unknown`.", ""]
    table(lines, ["Command", "Total invocations"], summary["slopdex_commands"].items())
    lines += ["", "### By model and thinking mode", ""]
    table(lines, ["Model", "Thinking", "Arm", "Command", "Invocations", "Trials using command"],
          [[group["model"], group["variant"], group["arm"], name, count, f"{group['slopdex_command_trials'][name]}/{group['n']}"] for group in summary["groups"] for name, count in group["slopdex_commands"].items()])
    lines += ["", "### By task and trial", ""]
    table(lines, ["Model", "Thinking", "Arm", "Task", "Repeat", "Invocations", "Commands"],
          [[row["model"], row["variant"], row["arm"], row["task"], row["repeat"], row["slopdex_calls"], ", ".join(f"`{name}` × {count}" for name, count in row["slopdex_commands"].items()) or "—"] for row in summary["trials"] if row["arm"] != "off" or row["slopdex_calls"]])
    if manifest.get("map_index"):
        lines += ["", "## Structural-index preparation", "", f"One-time structural-index preparation: {fmt(manifest['map_index'].get('wall_seconds'))}s (cached)."]
    if manifest.get("map_symbol_index"):
        metadata = manifest["map_symbol_index"]
        lines += ["", "## Map symbol-index preparation", "", f"One-time structural preparation: {fmt(metadata.get('wall_seconds'))}s; symbol embedding/ANN preparation: {fmt((metadata.get('native_ann') or {}).get('wall_seconds'))}s (cached).",
                  "Map trials receive normalized-name vectors and the symbol ANN sidecar, with reuse validated before timing."]
    lines += ["", "Preparation, embedding/index charges and validation are excluded from agent execution seconds and OpenCode-reported USD. Reported model USD is not necessarily an invoice total; zero is kept as reported, missing is not zero.",
              "Scoring measures symbol identification with valid declaration citations. Explanation/flow correctness requires manual review of saved answers; no LLM judge is used. No significance or confidence-interval claims.", ""]
    return "\n".join(lines)


def report(output: Path):
    # A lazy import permits run.py to re-export report without an import cycle.
    import run as runner

    output = Path(output)
    manifest = runner.read_json(output / "manifest.json")
    results = []
    for trial in manifest["trials"]:
        if trial["arm"] not in ARMS:
            continue
        directory = output / "trials" / trial["id"]
        path = directory / "result.json"
        if not path.exists():
            continue
        result = runner.read_json(path)
        counts = runner.saved_slopdex_commands(directory, result)
        if result.get("slopdex_commands") != counts:
            result["slopdex_commands"] = counts
            runner.write_json(path, result)
        results.append(result)
    summary = build_summary(manifest, results)
    runner.write_json(output / "summary.json", summary)
    (output / "report.md").write_text(markdown(manifest, summary))
    return summary
