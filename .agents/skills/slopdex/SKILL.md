---
name: slopdex
description: Semantic code search, find duplicate-function candidates, analyze physical code cohesion
---

# Slopdex operator guide for agents

Slopdex does semantic code search, finds similar-code candidates, and identifies related functions stored far apart.

### Find code by meaning

```bash
slopdex search "validate an authenticated session" --format summary --limit 10
slopdex search "persist user data" -e 'save|persist' --format summary --limit 5
```

Describe behavior rather than guessing a symbol name. `-e` is a regex that restricts which symbols (functions) are searched.

### Search function purpose

Code purpose-description generation needs to be enabled once:

```bash
slopdex descriptions enable # only needed once
```

Then purpose descriptions can be searched:

```bash
slopdex search-description "keep the repository index synchronized" --format summary --limit 10
```

Enabling needs `OPENAI_API_KEY` by default. Select OpenCode Zen or Go with
`--description-provider opencode` or `--description-provider opencode-go` and set `OPENCODE_API_KEY`.

To select another model:

```bash
slopdex descriptions enable --description-model <model-id>
```

List and persist a published OpenCode model without creating an index:

```bash
slopdex models opencode-go
slopdex config model opencode-go/gpt-5.6-luna
slopdex config descriptions enable
```

The next index-using command applies the configured description state. A bare model ID passed to
`config model` resolves automatically only when it belongs to one of Zen or Go; qualify shared IDs.

### Find duplicate candidates

```bash
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9 --limit 5
```

The default output is connected clusters. This excludes same-file matches and short functions. For source-by-source matches, add `--format summary`.

Broaden discovery through adjacent score bands when needed:

```bash
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.85-0.9 --limit 5
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.8-0.85 --limit 5
```

Ranges include the lower bound and exclude the upper bound. Use `--min-lines 1` when one-line wrappers are relevant.

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

Here a source must have changed since the commit and belong to an uncommitted file, within the selected path/name scope. `--regex` is an alias for `-e/--regexp`.

### Review physical cohesion

```bash
slopdex cohesion --threshold 0.8 --neighbors 20 --limit 50 --format summary
slopdex cohesion --source-path src/services --threshold 0.8 --format summary
```

Ranks related functions by semantic affinity and file/folder separation. add `--include-source` only when full callable bodies are needed.

### Compare repositories

```bash
slopdex cross-search \
  --target-root /path/to/other/repo \
  --target-index /path/to/other/repo/.slopdex/index.sqlite \
  --threshold 0.9 --format summary
```

Both target options are required. Both indexes refresh and must have identical embedding profiles. The target refresh uses the source command's embedding provider and the target's file-selection/description configuration. Use `--target-config <path>` for a custom target config.

### Inspect or maintain the index

```bash
slopdex status
slopdex index-errors --format summary
slopdex update-git
slopdex update-files src/service.ts src/model.ts
slopdex reindex-files
slopdex reindex-files --callables
slopdex delete-files src/removed.ts
slopdex --version
```

- `status` refreshes, then reports coverage, checkpoint, profiles, and error counts; use when metadata is requested.
- `index-errors` reads saved failures without refreshing or needing API credentials.
- `update-git` explicitly refreshes HEAD and working-tree changes.
- `update-files` reparses specified working-tree files after automatic refresh, even when their contents are unchanged.
- `reindex-files` regenerates stale file descriptions and embeddings. Add `--callables` to also replace callable descriptions in those files.
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
| `--description-provider <openai\|opencode\|opencode-go>` | Description provider; OpenAI by default. OpenCode values require `OPENCODE_API_KEY`. |
| `--description-model <name>` | Description model; `gpt-5.6-sol` for OpenAI/Zen and `gpt-5.6-luna` for Go. |
| `--ignore-errors` | Silence saved-diagnostic warnings without deleting records. |
| `-h`, `--help` | Usage; no refresh. |
| `--version` | Package version; exits without refresh or saved-diagnostic warnings. |

Explicit relative config/index paths resolve from the current directory. Source paths and explicit file arguments resolve within `--root`. Prefer absolute paths when operating across repositories. CLI settings override config.

### Query and analysis options

| Argument | Applies to / behavior |
| --- | --- |
| `--limit <number>` | Positive integer. `search`/`search-description`: matches, default `10`. Cross-search: neighbors per source, default `5`. Cohesion: reported pairs and file rows, default `50`. |
| `--threshold <number\|min-max>` | Both query searches and analyses. Inclusive minimum or half-open range. Default `-1` for query/cross-search; `0.8` for cohesion. Cohesion minimum must be in `[-1, 1)`. |
| `--format <json\|summary\|clusters>` | Both query searches, cross-search, cohesion, index-errors. `clusters` only supports cross-search; output defaults below. |
| `-e <regex>`, `--regexp <regex>`, `--regex <regex>` | Equivalent case-sensitive JavaScript regex options on qualified names. Query searches filter results before limiting; cross-search/cohesion filter sources only. |
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
| `--callables` | With `reindex-files`, continue after the file description and regenerate every callable description in each stale file. |

Use `--no-reindex` when the task calls for committed-only results or reuse of an existing non-Git index, rather than silently weakening freshness.

## Interpret and report results

### Output formats

| Command | Default | Alternatives |
| --- | --- | --- |
| `search`, `search-description` | `summary` | JSON array; purpose search includes generated description text |
| `cross-search` | `clusters` | `summary`, or `json` for JSONL with one row per matched source |
| `cohesion` | `summary` | One JSON report |
| `index-errors` | `summary` | JSON array |
| `status`, update commands, `descriptions` | JSON object | — |

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

When descriptions are complete, `search`, cross-search, and cohesion average code, callable-description, and file-description similarity with equal one-third weights. `search-description` averages callable and file descriptions. Cross-repository analysis needs completeness on both sides; otherwise all scores are code-only. Stale file descriptions remain in scoring until `reindex-files` refreshes them. Description-generator models may differ even though embedding profiles must match.

Thresholds and limits apply to the selected score. Text labels combined scoring; JSON includes component scores and mode/weights (`scoring` in cross-search, `parameters` in cohesion). Compare runs only with matching scoring mode, weights, embedding and description-generator profiles, threshold, neighbor count, and source/candidate filters.

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

The example merits reviewing separated authentication responsibilities, while accounting for intentional layering. There is no universal pass/fail threshold. Use comparable runs to evaluate changes. Filtered reports describe selected sources, not the full repository. Cohesion metrics use all qualifying edges; pairs/files are limited, and groups use reported pairs.
