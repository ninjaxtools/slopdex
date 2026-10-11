"""Portable, secret-free evaluation records and per-trial reuse keys."""

from datetime import datetime, timezone
from pathlib import Path

PROTOCOL_VERSION = 1


def reject_credentials(value):
    """Result settings are committed; API credentials must stay in the environment."""
    if isinstance(value, dict):
        for key, item in value.items():
            if key.lower().endswith(("apikey", "api_key", "password", "secret", "token", "authorization")) and item:
                raise ValueError("Evaluation config contains credentials; supply them through the environment")
            reject_credentials(item)
    elif isinstance(value, list):
        for item in value:
            reject_credentials(item)


def trial_controls(manifest, trial, *, prompt, instructions):
    import run as runner

    config = manifest["config"]
    task = next(task for task in manifest["tasks"] if task["id"] == trial["task"])
    controls = {
        "protocol_version": PROTOCOL_VERSION,
        "repository_commit": manifest["commit"],
        "task": task,
        "repeat": trial["repeat"],
        "prompt": prompt,
        "instructions": instructions,
        "model": trial["model_config"],
        "timeout_seconds": config["timeout_seconds"],
        "opencode_version": manifest["opencode_version"],
        "opencode_config": runner.opencode_config(trial["model_config"], trial["arm"], config["max_steps"]),
    }
    if trial["arm"] == "map":
        controls.update({
            "slopdex_config": config["slopdex"],
            "slopdex_version": manifest["slopdex_version"],
            "implementation": manifest.get("implementation"),
            "harness_sha256": manifest["harness_sha256"],
        })
        if manifest.get("map_private") is not None:
            controls["map_private"] = manifest["map_private"]
    return controls


def make_record(manifest, trial, directory):
    import run as runner

    result = runner.read_json(directory / "result.json")
    for key in ("id", "arm", "model", "variant", "task", "repeat"):
        if result[key] != trial[key]:
            raise ValueError(f"Saved result does not match its trial: {directory}")
    result["slopdex_commands"] = runner.saved_slopdex_commands(directory, result)
    prompt, instructions = (directory / "prompt.txt").read_text(), (directory / "AGENTS.md").read_text()
    controls = trial_controls(manifest, trial, prompt=prompt, instructions=instructions)
    config_path = directory / "opencode.json"
    if config_path.exists():
        actual = runner.read_json(config_path)
        actual.pop("instructions", None)  # The disposable workspace path is not a control.
        if actual != controls["opencode_config"]:
            raise ValueError(f"Saved OpenCode config differs from the reusable protocol: {directory}")
    return {"trial": trial, "result": result, "prompt": prompt, "instructions": instructions,
            "answer": (directory / "answer.txt").read_text(), "controls": controls,
            "fingerprint": runner.digest(controls)}


def load_records(results: Path):
    import run as runner

    found = {}
    paths = sorted((results / "baselines").glob("*.json"))
    if (results / "latest-map.json").exists():
        paths.append(results / "latest-map.json")
    for path in paths:
        bundle = runner.read_json(path)
        if bundle.get("schema_version") != 1 or bundle.get("arm") not in {"off", "map"}:
            raise ValueError(f"Unsupported saved evaluation: {path}")
        for record in bundle["records"]:
            trial = record["trial"]
            reject_credentials(trial["model_config"])
            if trial["arm"] != bundle["arm"]:
                raise ValueError(f"Saved evaluation has a mismatched arm: {path}")
            actual = trial_controls(bundle["manifest"], trial, prompt=record["prompt"], instructions=record["instructions"])
            # Validate frozen controls, not today's protocol/permissions. A protocol
            # upgrade makes old keys miss; it must not make old evidence unreadable.
            actual["protocol_version"] = record["controls"]["protocol_version"]
            actual["opencode_config"] = record["controls"]["opencode_config"]
            if actual != record["controls"] or runner.digest(actual) != record["fingerprint"]:
                raise ValueError(f"Saved evaluation controls are corrupt: {path}")
            if any(record["result"][key] != trial[key] for key in ("id", "arm", "model", "variant", "task", "repeat")):
                raise ValueError(f"Saved evaluation result is mismatched: {path}")
            found[(trial["arm"], record["fingerprint"])] = (record, path)
    return found


def restore_record(record, directory):
    import run as runner

    directory.mkdir(parents=True, exist_ok=True)
    runner.write_json(directory / "result.json", record["result"])
    for name, key in (("prompt.txt", "prompt"), ("AGENTS.md", "instructions"), ("answer.txt", "answer")):
        (directory / name).write_text(record[key])


def save_bundle(output: Path, results: Path, arm: str):
    import run as runner

    manifest = runner.read_json(output / "manifest.json")
    reject_credentials(manifest)
    trials = [trial for trial in manifest["trials"] if trial["arm"] == arm]
    if not trials or any(not (output / "trials" / trial["id"] / "result.json").exists() for trial in trials):
        return None  # Never replace the latest complete map run with a partial run.
    records = [make_record(manifest, trial, output / "trials" / trial["id"]) for trial in trials]
    if arm == "off":
        existing = load_records(results)
        matches = [existing.get((arm, record["fingerprint"])) for record in records]
        if all(matches):
            return matches[0][1]  # Selecting a subset must not duplicate a baseline dataset.
    # Large host-specific index caches and absolute trial paths stay in ignored jobs/.
    portable = {key: value for key, value in manifest.items()
                if key not in {"index", "map_index", "map_symbol_index", "reused_trials"}}
    portable["trials"] = trials
    portable["arm_instructions"] = {arm: manifest["arm_instructions"][arm]}
    if arm == "map" and not manifest.get("implementation"):
        portable["implementation"] = None
        portable["provenance_note"] = "Historical job did not record slopdex source/binary identity; not eligible for automatic map reuse."
    bundle = {"schema_version": 1, "arm": arm, "manifest": portable, "records": records,
              "source_job": output.name,
              "recorded_at": datetime.now(timezone.utc).isoformat()}
    name = runner.digest(sorted(record["fingerprint"] for record in records))
    path = results / "baselines" / f"{name}.json" if arm == "off" else results / "latest-map.json"
    if arm == "off" and path.exists():
        # Baselines are immutable. Reusing a result must not rewrite its provenance.
        return path
    if arm == "map" and path.exists():
        saved = runner.read_json(path)
        if saved["records"] == records:
            return path
    runner.write_json(path, bundle)
    return path


def compare_saved(results: Path):
    import run as runner
    from report import build_summary, markdown

    records = load_records(results)
    latest = results / "latest-map.json"
    if not latest.exists():
        raise ValueError("No saved map evaluation; run the map arm first")
    bundle = runner.read_json(latest)
    map_manifest = bundle["manifest"]
    baselines, maps, sources = [], [], set()
    for record in bundle["records"]:
        map_trial = record["trial"]
        trial = {**map_trial, "arm": "off"}
        trial["id"] = runner.digest(f"{trial['model']}@{trial['variant']}:{trial['task']}:{trial['repeat']}:off")[:16]
        task = next(task for task in map_manifest["tasks"] if task["id"] == trial["task"])
        controls = trial_controls(map_manifest, trial, prompt=runner.task_prompt(task),
                                  instructions=runner.BASE_INSTRUCTIONS + runner.arm_instructions("off"))
        match = records.get(("off", runner.digest(controls)))
        if match is None:
            raise ValueError(f"No matching saved baseline for {trial['model']}@{trial['variant']} / {trial['task']}")
        baseline, source = match
        baselines.append(baseline)
        maps.append(record)
        sources.add(str(source.relative_to(results)))
    manifest = {**map_manifest, "trials": [record["trial"] for record in baselines + maps],
                "comparison_mode": "saved baseline (not interleaved)",
                "baseline_sources": sorted(sources), "map_source": "latest-map.json"}
    summary = build_summary(manifest, [record["result"] for record in baselines + maps])
    runner.write_json(results / "summary.json", summary)
    (results / "report.md").write_text(markdown(manifest, summary))
    return summary
