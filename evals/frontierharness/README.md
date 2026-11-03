# FrontierHarness Eval: OpenCode versus slopdex map

Runs the published [FrontierHarness Eval](https://frontierharness.org) v1 tasks
twice with OpenCode. There are two modes:

- **`deepswe`** (default): the 9 DeepSWE tasks. These are real repositories,
  which is where slopdex actually gets used. In the full run the agent called
  slopdex in all 9 DeepSWE tasks but in only 2 of the 21 Terminal-Bench tasks,
  most of which have little or no code to index.
- **`full`**: all 30 tasks (21 Terminal-Bench 2.1 + 9 DeepSWE), as published.

The two arms:

- **`off`**: plain OpenCode, as in the published FrontierHarness OpenCode row.
- **`map`**: slopdex installed and indexed. It gets the root README map snippet as
  OpenCode instructions, the same guidance as the [Excalidraw eval](../excalidraw/README.md)
  map arm (`run.py` reads it through the Excalidraw harness).

Scoring is the tasks' own verifiers (pass/fail). No model judge is used.

## How this relates to maki's run

maki's published result (`frontierharness-report-2026-09-21.zip` on its v0.5.5
release) came from the official FrontierHarness skill. That skill runs on Runta
cloud runtimes: one golden checkpoint per task and a fresh restore per trial.
Inside each runtime:

- Terminal-Bench ran as `harbor run -p <frozen task> --allow-agent-host <provider host>`.
- DeepSWE ran as `pier run -p <deep-swe task>`.
- An "agent-phase overlay" gave Terminal-Bench tasks `[agent] network_mode = "no-network"`,
  so only the model host was reachable while the agent worked.

maki also ran a matched Codex control under the same policy.

This harness keeps the task bytes, runner semantics and network policy, but swaps
Runta for **local Docker**:

| | FrontierHarness/maki | here |
| --- | --- | --- |
| Tasks | 30 published ids | same ids, frozen from the pinned sources below |
| Terminal-Bench | `harbor run -p`, agent-phase overlay | same, Harbor 0.23 |
| DeepSWE | Pier 0.3.1 | Harbor 0.23, which now parses schema 1.3 tasks, separate verifiers and `[agent] network_mode` |
| Runtime | Runta golden checkpoint, fresh restore | local Docker, fresh container per trial |
| Model | Kimi K3 (Fireworks) | `opencode-go/deepseek-v4.1-flash`, medium reasoning |
| Control | matched Codex cohort | the `off` arm |
| Cost | frozen Kimi K3 price table | OpenCode-reported USD |

Numbers are therefore **comparable between `off` and `map`**, not to the
leaderboard. The published OpenCode/Kimi K3 row (50.0% pass, $3.24 effective cost
per pass) is printed only for context.

## Run

```sh
# Freeze tasks (git clones + `harbor download`, no model calls) and list the
# DeepSWE tasks; add `--mode full` to list all 30.
python3 evals/frontierharness/run.py list

# Build the portable slopdex binary (Ubuntu 22.04 / glibc 2.35 container, like the
# release runner) and show the plan, without trials.
python3 evals/frontierharness/run.py prepare
python3 evals/frontierharness/run.py run --dry-run

# Run every missing or changed DeepSWE trial, then print results/report-deepswe.md.
python3 evals/frontierharness/run.py run

# The same for all 30 tasks, printing results/report-full.md.
python3 evals/frontierharness/run.py run --mode full

# Subsets.
python3 evals/frontierharness/run.py run --arms off
python3 evals/frontierharness/run.py run --task terminal-bench/regex-log --task datacurve/httpx-multipart-response-parsing

# Regenerate a mode's report from saved trials.
python3 evals/frontierharness/run.py report
python3 evals/frontierharness/run.py report --mode full
```

Requirements:

- Linux, Docker, Git, Python 3.11+, Node (only for the tests), and `harbor` 0.23 on
  `PATH` (`uv tool install harbor`).
- An opencode-go key in `OPENCODE_GO_EVALS_KEY` (preferred, like the Excalidraw
  eval) or `OPENCODE_API_KEY`.
- For the map arm, `OPENAI_API_KEY` for embeddings.

Keys reach Harbor as `${VAR}` templates, so they never appear in argv or job
configs. DeepSWE tasks want 2 CPUs / 8 GB each, so the default concurrency is 2.
Use `--concurrency` or `config.json` to change it.

## What is controlled

- **Sources.** All three are pinned in `config.json`:
  - FrontierHarness eval repo: commit `e837a70`, for the task list and public Terminal-Bench metadata.
  - DeepSWE: `435ee89`, the reproduction ref FrontierHarness pins.
  - Terminal-Bench 2.1 from the Harbor registry.

  Terminal-Bench tasks take the published FrontierHarness `task.toml` and
  `instruction.md` over the registry files, plus maki's agent-phase overlay.
  Setup and verifier keep the published baseline because they install packages.
  Each frozen task's content digest is recorded.
- **Network.** During the agent phase only `opencode.ai` (model) and
  `api.openai.com` (embeddings) are reachable. Both arms get the same allowlist.
  Harbor's egress sidecar rejects all non-TCP traffic in that phase, including
  the upstream lookups of Docker's embedded DNS. So each trial resolves the
  allowlisted hosts on the host and pins them in the sidecar's `/etc/hosts`
  (`agent-hosts.yaml` overlay, addresses recorded per trial). TLS still has to
  pass the sidecar's SNI allowlist; other hosts stay unreachable.
- **Workspace isolation.** slopdex, its index, config, instructions and shims live
  under `/installed-agent/slopdex`, never in the task repository. DeepSWE verifiers
  diff the repository, so this keeps the arms' patches comparable.
- **Timing.** Indexing (`slopdex update` plus ANN warm-up) runs during agent setup
  and is not counted (`setup_seconds` is recorded). Agent time is Harbor's agent
  execution phase. The index updates incrementally as the agent edits files, as
  in normal use.
- **Usage.** Each map call goes through a journaling wrapper (`slopdex-calls.jsonl`).
  Nested `slopdex faq` model calls go through an `opencode` shim
  (`faq-usage.jsonl`), and their USD is **added to the trial cost**. A faq call
  without cost telemetry makes the trial's cost unknown rather than zero. The
  wrapper refuses `--root`/`--config`/`--index` overrides.
- **Reuse.** A trial is reused only when its controls digest matches. The digest
  covers task digest, arm, model and options, OpenCode and Harbor versions,
  allowlist, harness source, and for map also the slopdex binary hash, settings
  and instruction text. Rebuilding slopdex from changed source reruns map only.
- **Infrastructure failures** count as `infra_invalid` and are retried on the
  next run, not scored. These are image or setup errors, a missing verifier
  result, and harness exceptions other than an agent timeout or a nonzero agent
  exit.

## Results

`results/trials/<arm>/*.json` hold one record per trial: status, reward, agent and
setup time, tokens, agent and faq cost, slopdex command counts, the exact
controls, and the path of the full Harbor job under the ignored `jobs/`.
Trial records are shared between modes, so a full run also fills the DeepSWE
report. `results/report-<mode>.md` and `results/summary-<mode>.json` give, per arm:

- pass rate;
- effective cost per pass (all valid trials' cost / passes, as FrontierHarness defines it);
- median cost per pass;
- median agent time;
- cache hit rate;
- tokens;
- faq calls and slopdex command counts;
- paired pass/fail counts and a per-task table.

## Verification

```sh
python3 -m unittest discover -s evals/frontierharness -v
cargo verify
```
