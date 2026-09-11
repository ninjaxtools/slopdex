---
name: slopdex
description: Use when using the slopdex CLI to search code by meaning or purpose, find duplicate-function candidates, analyze physical code cohesion, or maintain indexes of Python, JavaScript/JSX, TypeScript/TSX, Rust, Go, Java, and C code.
---

# Slopdex operator guide for agents

Slopdex searches named functions by meaning, finds similar-code candidates, and identifies related functions stored far apart. Languages are detected automatically and can coexist in one index.

## Choose the command that answers the task

Run the requested operation directly. Do not precede it with status, help, version, executable lookup, or credential probes unless those are the user's task or needed to diagnose a reported failure. Missing indexes initialize automatically.

### Find code by meaning

```bash
slopdex search "validate an authenticated session" --format summary --limit 10
slopdex search "persist user data" -e 'save|persist' --format summary --limit 5
```

Describe behavior rather than guessing a symbol name. `-e` restricts result qualified names before limiting. Read the matched source to establish behavior and callers.

### Search function purpose

```bash
slopdex use-summaries
slopdex search-summary "keep the repository index synchronized" --format summary --limit 10
```

Use this workflow when purpose-summary generation/search is requested. `use-summaries` enables persistent automatic updates and adds API generation work. It needs `OPENAI_API_KEY` even with Jina embeddings. Repeating it with unchanged inputs reuses summaries. `search-summary` requires summaries to be enabled and includes summary text in results.

To select another model:

```bash
slopdex use-summaries --summary-model <model-id>
```

The model persists for future updates. Do not enable summaries as a routine prerequisite for ordinary code search or duplicate discovery.

### Find duplicate candidates

```bash
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9 --limit 5
```

The default output is connected clusters. This excludes same-file matches and short functions. `--limit` is neighbors **per source**, not a limit on total findings or clusters. For source-by-source matches, add `--format summary`.

Broaden discovery through adjacent score bands when needed:

```bash
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.85-0.9 --limit 5
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.8-0.85 --limit 5
```

Ranges include the lower bound and exclude the upper bound. Use `--min-lines 1` when one-line wrappers are relevant. Treat matches as review candidates; inspect source before suggesting consolidation.

### Review changes or a module

```bash
slopdex cross-search --uncommitted --cross-file-only --min-lines 4 --threshold 0.9
slopdex cross-search --changed-since origin/main --format summary --threshold 0.9
slopdex cross-search --source-path src/services -e '^UserService\.' --format summary --threshold 0.9
```

These restrict sources while searching the full eligible index. The same filters work with `cohesion`. All supplied restrictions intersect:

```bash
slopdex cross-search --source-path src -e 'validate' \
  --changed-since origin/main --uncommitted \
  --cross-file-only --min-lines 4 --threshold 0.9
```

Here a source must have changed since the commit and belong to an uncommitted file, within the selected path/name scope. `--regex` is different from `-e`: it restricts **both** sources and candidates.

### Review physical cohesion

```bash
slopdex cohesion --threshold 0.8 --neighbors 20 --limit 50 --format summary
slopdex cohesion --source-path src/services --threshold 0.8 --format summary
```

Ranks related functions by semantic affinity and file/folder separation. For structured processing use `--format json`; add `--include-source` only when full callable bodies are needed.

### Compare repositories

```bash
slopdex cross-search \
  --target-root /path/to/other/repo \
  --target-index /path/to/other/repo/.slopdex/index.sqlite \
  --threshold 0.9 --format summary
```

Both target options are required. Both indexes refresh and must have identical embedding profiles. The target refresh uses the source command's embedding provider and the target's file-selection/summary configuration. Use `--target-config <path>` for a custom target config.

### Inspect or maintain the index

```bash
slopdex status
slopdex index-errors --format summary
slopdex update-git
slopdex update-files src/service.ts src/model.ts
slopdex delete-files src/removed.ts
slopdex --version
```

- `status` refreshes, then reports coverage, checkpoint, profiles, and error counts; use when metadata is requested.
- `index-errors` reads saved failures without refreshing or needing API credentials.
- `update-git` explicitly refreshes HEAD and working-tree changes.
- `update-files` reparses specified working-tree files after automatic refresh, even when their contents are unchanged.
- `delete-files` removes index entries after automatic refresh, not source files. Eligible files can return on later refresh.
- `--version` prints the built package version. `--help` describes available commands/options.

## Command-line reference

Usage: `slopdex <command> [arguments] [options]`. Quote queries and regexes. Boolean flags default to off. Use options only with their applicable commands.

### General settings

| Argument | Meaning / default |
| --- | --- |
| `--root <path>` | Repository root; current directory by default. |
| `--config <path>` | Config; `<root>/.slopdex/config.json`. |
| `--index <path>` | Index; `<root>/.slopdex/index.sqlite`. Overrides config `indexPath`. |
| `--provider <openai\|jina>` | Embedding provider; `openai`. |
| `--model <name>` | Embedding model; OpenAI `text-embedding-3-large`, Jina `jina-embeddings-v4`. |
| `--dimensions <number>` | Positive dimensions supported by the model; OpenAI `3072`, Jina `1024`. |
| `--summary-model <name>` | OpenAI summary model; initially `gpt-5.6-sol`, then the persisted selection. |
| `--ignore-errors` | Silence saved-diagnostic warnings without deleting records. |
| `-h`, `--help` | Usage; no refresh. |
| `--version` | Package version; exits without refresh or saved-diagnostic warnings. |

Explicit relative config/index paths resolve from the current directory. Source paths and explicit file arguments resolve within `--root`. Prefer absolute paths when operating across repositories. CLI settings override config.

### Query and analysis options

| Argument | Applies to / behavior |
| --- | --- |
| `--limit <number>` | Positive integer. `search`/`search-summary`: matches, default `10`. Cross-search: neighbors per source, default `5`. Cohesion: reported pairs and file rows, default `50`. |
| `--threshold <number\|min-max>` | Both query searches and analyses. Inclusive minimum or half-open range. Default `-1` for query/cross-search; `0.8` for cohesion. Cohesion minimum must be in `[-1, 1)`. |
| `--format <json\|summary\|clusters>` | Both query searches, cross-search, cohesion, index-errors. `clusters` only supports cross-search; output defaults below. |
| `-e <regex>`, `--regexp <regex>` | Case-sensitive JavaScript regex on qualified names. Query searches filter results before limiting; cross-search/cohesion filter sources only. |
| `--regex <regex>` | Cross-search/cohesion: filter both source and candidate qualified names. |
| `--min-lines <number>` | Cross-search/cohesion: positive source/candidate length minimum, default `2`. |
| `--source-path <path>` | Cross-search/cohesion: source file or recursive directory within the root. |
| `--changed-since <commit>` | Cross-search/cohesion: added, modified, or moved functions since an ancestor of the indexed Git checkpoint, including working-tree changes. Requires Git. |
| `--uncommitted` | Cross-search/cohesion: functions indexed from working-tree files; in Git these are staged, unstaged, or untracked changes. Without Git this selects all working-tree functions. |
| `--cross-file-only` | Cross-search: exclude same-physical-file matches. |
| `--include-symmetric-duplicates` | Cross-search: allow both directions of same-index matches; otherwise each unordered pair appears once. |
| `--neighbors <number>` | Cohesion: positive neighbor count per source, default `20`; changes analysis scope. |
| `--include-source` | Cohesion JSON: include callable bodies; omitted by default. |
| `--target-root <path>` | Cross-search: second repository; requires `--target-index`. |
| `--target-index <path>` | Cross-search: second index file; requires `--target-root`. |
| `--target-config <path>` | Cross-search: target config, default `<target-root>/.slopdex/config.json`; requires both target options. |

### Refresh and recovery options

| Argument | Behavior |
| --- | --- |
| `--target <ref>` | `update-git` snapshot, default `HEAD`. Non-HEAD targets are committed-only; later commands normally return to HEAD. |
| `--rebuild-on-divergence` | Permit reconciliation after non-descendant history changes, such as a rebase/branch switch. |
| `--force-reindex` | Recreate an incompatible index. Compatible indexes still use normal refresh; this is not an unconditional reparse flag. |
| `--no-reindex` | With Git, reconcile the committed snapshot but omit working-tree overlays. Without Git, reuse a non-empty index; missing/empty indexes still populate. Not an offline mode. |

Use `--no-reindex` when the task calls for committed-only results or reuse of an existing non-Git index, rather than silently weakening freshness.

## Interpret and report results

### Output formats

| Command | Default | Alternatives |
| --- | --- | --- |
| `search`, `search-summary` | JSON array | `summary`; purpose search includes generated summary text |
| `cross-search` | `clusters` | `summary`, or `json` for JSONL with one row per matched source |
| `cohesion` | One JSON report | `summary` for ranked pairs |
| `index-errors` | JSON array | `summary` |
| `status`, update commands, `use-summaries` | JSON object | — |

Prefer summary output for compact source review, clusters for duplicate families, and JSON/JSONL for structured processing. Stdout carries results; stderr carries notices and warnings. Cross-search omits sources without emitted matches. Empty output means no findings under the chosen coverage/filters, not proof that no similar code exists.

### Similarity and clusters

```text
Cluster 1 (3 functions, similarity 0.9124-0.9568)
  src/auth/session.ts:18:1 :: validateSession
  src/http/middleware.ts:42:1 :: authenticate
  src/users/user-service.ts:27:3 :: UserService.authenticate
```

- Similarity is a model-dependent resemblance score, not a duplication probability.
- The range describes observed links. Members can be connected transitively; not all pairs necessarily match.
- Cluster numbers reflect ordering by member count and name, not severity.
- Inspect listed locations and callers. Tests, facades, adapters, and intentional layers can resemble each other without being redundant.

When reporting candidates, identify paths/symbols, summarize the shared behavior you verified, and explain whether consolidation is appropriate. Do not infer equivalence from the score alone.

### Purpose-aware scoring

`search` uses code only; `search-summary` uses purpose summaries only. Cross-search and cohesion automatically use **50% code + 50% summary similarity** when summaries are enabled and complete. Cross-repository analysis needs completeness on both sides; otherwise all scores are code-only. Summary-generator models may differ even though embedding profiles must match.

Thresholds and limits apply to the selected score. Text labels combined scoring; JSON includes component scores and mode/weights (`scoring` in cross-search, `parameters` in cohesion). Compare runs only with matching scoring mode, weights, embedding and summary-generator profiles, threshold, neighbor count, and source/candidate filters.

### Cohesion

```text
Cohesion: 184 functions analyzed, 37 semantic edges
  same file 35.1%  same folder 29.7%  remote 35.2%  mean distance 1.84

1. gap 0.6053  similarity 0.9400  distance 4  reciprocal
   src/auth/session.ts:18:1 :: validateSession
   packages/http/middleware.ts:42:1 :: authenticate
```

- **Functions analyzed / edges:** selected-source coverage and unique qualifying neighbor pairs before report limiting, not quality grades.
- **Same file / same folder / remote:** shares of weighted semantic affinity. More remote affinity means more related code crosses folder boundaries.
- **Mean distance:** weighted physical separation; `0` is same file, `1` is different files in one folder, larger means farther apart.
- **Gap / rank:** a `0–1` review score combining similarity above the threshold with separation. Higher gap ranks earlier; same-file pairs have zero gap.
- **Reciprocal:** both functions selected each other as neighbors. JSON `null` means an endpoint was not evaluated because of source filtering.
- **JSON details:** `semanticWeight` reflects similarity above threshold; `separationWeight` reflects distance; `sourceTestPair` flags a path-inferred source/test relationship; file `externalAffinityRatio` measures affinity outside that file's folder.

The example merits reviewing separated authentication responsibilities, while accounting for intentional layering. There is no universal pass/fail threshold. Use comparable runs to evaluate changes. Filtered reports describe selected sources, not the full repository. Summary metrics use all qualifying edges; pairs/files are limited, and groups use reported pairs.

## Operational properties

- Requires Node.js 24+. Install with `npm install -g @ninjaxtools/slopdex` if installation is the task.
- Run from the repository root or pass `--root`. The default local index is `.slopdex/index.sqlite`; add `.slopdex/` to `.gitignore`.
- Most commands, including `status`, refresh before operating. With Git, the default is HEAD plus current working-tree changes; the checkpoint records the committed base. Without Git, refresh scans the working tree and warns. `index-errors`, help, and version do not refresh.
- Source filters narrow analysis, not the preceding refresh. Indexing and summary generation can make many API calls; unchanged inputs reuse cached results.
- Embedding keys are `OPENAI_API_KEY` or `JINA_API_KEY`; other than diagnostics/help/version, CLI commands require the configured embedding key even for cached analyses. Summaries also require OpenAI credentials when generation is needed.
- Function source and queries go to the embedding provider. Enabled summary generation sends repository name, path, callable source, and file context to OpenAI, and summary text to the embedding provider. Results and diagnostics remain in the local index. Never expose key values in tool calls, output, or commits.
- Coverage: Python `.py/.pyw`; JavaScript `.js/.mjs/.cjs/.jsx`; TypeScript `.ts/.mts/.cts/.tsx`; Rust `.rs`; Go `.go`; Java `.java`; C `.c/.h`. Named callables with bodies, including supported bound closures and nested functions, are indexed. Anonymous callbacks, bodyless declarations, macro expansion, and runtime relationships are outside coverage.
- Root/nested `.gitignore` rules apply even to tracked files and without Git. Current overlays use current rules; committed-only snapshots use committed rules. Refresh removes newly ignored files; explicit updates reject ignored paths.
- Dependency/build directories are excluded: `.git`, `.slopdex`, `node_modules`, `dist`, `build`, `coverage`, `vendor`, `generated`, `.venv`, `venv`, `__pycache__`, `.tox`, `.mypy_cache`, `.pytest_cache`, `target`. Config/ignore exceptions cannot override built-in exclusions or an ignored parent directory.

### Optional configuration

`<root>/.slopdex/config.json`:

```json
{
  "provider": "jina",
  "model": "jina-embeddings-v4",
  "dimensions": 1024,
  "exclude": ["**/fixtures/**"]
}
```

Supported properties: `provider`, `model`, `dimensions`, `summaryModel`, `indexPath`, `include`, `exclude`, `maxFileSize`, `embeddingBatchSize`. Includes/excludes are repository-relative globs; an empty include list permits all eligible supported files. `maxFileSize` defaults to `1048576` bytes; `embeddingBatchSize` to `32`; both are positive integers. Keep credentials in the environment. Embedding-profile changes require `--force-reindex`.

## Failures and incomplete coverage

Parse, extraction, read, and file-size failures are saved while healthy functions remain searchable. Inspect `slopdex index-errors --format summary`; JSON adds locations, recovered names, source, and snapshot provenance. `status` reports `indexingErrorCount` and `failedFileCount`, and `functionCount` counts searchable callables.

Saved errors warn on stderr, including on cached runs and target indexes. `--ignore-errors` only silences the warning. Updates retry failed files; successful indexing, deletion, or exclusion clears records. Mention relevant incomplete coverage when interpreting results.

Preserve the exact command and error when an operation fails. Fix the reported cause instead of retrying equivalent initialization commands:

- Missing credentials or provider/configuration errors: report the actionable message without displaying keys.
- Divergent checkpoint: use `--rebuild-on-divergence` when proceeding with the requested snapshot.
- Incompatible index: `--force-reindex` recreates it with the requested profile.
- Source changed during indexing: rerun after edits settle.
- Bare runtime errors such as `Invalid argument`: report the failure and diagnose the runtime/tool rather than trying unrelated refresh commands.

Exit codes: `0` success, `2` argument/domain errors, `1` other failures or missing command. Use help to resolve capability questions and version to report the installed package version when needed.
