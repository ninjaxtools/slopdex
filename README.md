# slopdex

Search functions by meaning, find duplicate-code candidates, and locate related functions spread across a codebase.

## Start here

Requires an embedding-provider API key. Install, set your key, and run commands from the repository you want to analyze (or pass `--root /path/to/repo`):

Coding agents should start with the bundled [Slopdex agent skill](.agents/skills/slopdex/SKILL.md).

```bash
npm install -g @ninjaxtools/slopdex
export OPENAI_API_KEY="your-api-key"
slopdex search "validate an authenticated session" --format summary --limit 10
```

The first command creates the index automatically. Later commands refresh it before searching.

Add `.slopdex/` to your repository's `.gitignore`.

### Search code

```bash
slopdex search "keep the repository index synchronized" --format summary --limit 10
```

To search generated descriptions of each function's role instead:

```bash
slopdex descriptions enable
slopdex search-description "keep the repository index synchronized" --format summary --limit 10
```

This requires `OPENAI_API_KEY` even when Jina supplies embeddings, and adds generation costs. See [descriptions and scoring](#descriptions-and-scoring).

`descriptions disable` turns off description generation while retaining cached data.

### Find duplicate-code candidates

```bash
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9 --limit 5
```

Compare functions across files, exclude short wrappers, and group strong matches into clusters. `--limit 5` selects up to five neighbors **per source function**, not five clusters.

### Review changed code or one module

```bash
slopdex cross-search --uncommitted --cross-file-only --min-lines 4 --threshold 0.9
slopdex cross-search --changed-since origin/main --format summary --threshold 0.9
slopdex cross-search --source-path src/services -e '^UserService\.' --format summary --threshold 0.9
```

These select source functions while keeping the full eligible index available for matches. The same source filters work with `cohesion`.

### Find related code stored far apart

```bash
slopdex cohesion --threshold 0.8 --neighbors 20 --limit 50 --format summary
```

Ranks semantically related pairs by their physical separation.

### Compare repositories

```bash
slopdex cross-search \
  --target-root /path/to/other/repo \
  --target-index /path/to/other/repo/.slopdex/index.sqlite \
  --threshold 0.9 --format summary
```

Both indexes refresh automatically and must use identical embedding profiles. The target is refreshed with the source command's embedding provider; target configuration supplies file-selection and description settings. Use `--target-config` for a non-default target config.

### Inspect index health

```bash
slopdex status
slopdex index-errors --format summary
slopdex --version
```

`status` refreshes the index and reports coverage, profiles, checkpoint, and error counts. `index-errors` reads saved failures without refreshing or requiring credentials. `--version` prints the built package version.

## Commands

Usage: `slopdex <command> [arguments] [options]`.

| Command | Purpose | Output |
| --- | --- | --- |
| `search <query>` | Search function code by meaning. Quote multiword queries. | Summary; optional JSON array |
| `descriptions <enable\|disable>` | Enable or disable automatic purpose descriptions. Re-enabling with unchanged inputs reuses cached descriptions. | JSON statistics |
| `search-description <query>` | Search purpose descriptions after enabling them. | Summary including description text; optional JSON array |
| `cross-search` | Find neighbors for each selected function in this or another index. | `clusters` by default; optional `summary` or JSONL |
| `cohesion` | Analyze semantic relationships versus file/folder separation. | Summary; optional JSON report |
| `status` | Refresh and show index metadata, counts, and profiles. | JSON object |
| `index-errors` | Read saved file/function indexing failures. | Summary; optional JSON array |
| `update-git` | Explicitly refresh a Git snapshot, with current working-tree changes when targeting HEAD. | JSON update statistics |
| `update-files <path...>` | After automatic refresh, explicitly reparse selected working-tree files. Paths are repository-relative or absolute within the root. | JSON update statistics |
| `delete-files <path...>` | After automatic refresh, remove paths from the index; source files are not deleted. A later refresh can restore eligible files. | JSON update statistics |

Manual maintenance examples:

```bash
slopdex update-git
slopdex update-files src/service.ts src/model.ts
slopdex delete-files src/removed.ts
slopdex update-git --target HEAD --rebuild-on-divergence
slopdex update-git --force-reindex
```

### Location, providers, and diagnostics

| Argument | Meaning / default |
| --- | --- |
| `--root <path>` | Repository root; current directory by default. |
| `--config <path>` | Config file; `<root>/.slopdex/config.json` by default. |
| `--index <path>` | Index file; `<root>/.slopdex/index.sqlite` by default. Overrides `indexPath` in config. |
| `--provider <openai\|jina>` | Embedding provider; `openai` by default. |
| `--model <name>` | Embedding model; `text-embedding-3-large` for OpenAI, `jina-embeddings-v4` for Jina. |
| `--dimensions <number>` | Positive embedding dimension count; OpenAI `3072`, Jina `1024`. Must be supported by the model. |
| `--description-model <name>` | OpenAI description model; `gpt-5.6-sol` initially, then the persisted model unless overridden. |
| `--ignore-errors` | Silence warnings about saved indexing errors; records remain available. |
| `-h`, `--help` | Show CLI usage without refreshing. |
| `--version` | Print the package version and exit. |

Explicit relative config and index paths resolve from the current directory, not `--root`.

### Search and analysis

| Argument | Applies to | Meaning / default |
| --- | --- | --- |
| `--limit <number>` | Both query searches, cross-search, cohesion | Positive integer. Query matches: `10`; cross-search neighbors per source: `5`; cohesion reported pairs and file rows: `50`. |
| `--threshold <number\|min-max>` | Both query searches, cross-search, cohesion | Minimum similarity, or range with inclusive minimum and exclusive maximum. Default `-1` for query/cross-search, `0.8` for cohesion. Cohesion minimum must be at least `-1` and below `1`. |
| `--format <json\|summary\|clusters>` | Both query searches, cross-search, cohesion, index-errors | Output format; see the commands table. `clusters` is only for cross-search. |
| `-e <regex>`, `--regexp <regex>`, `--regex <regex>` | Both query searches, cross-search, cohesion | Equivalent case-sensitive JavaScript regex options over qualified names. Query searches: filter results before limiting. Analysis: filter sources only. |
| `--min-lines <number>` | Cross-search, cohesion | Minimum source and candidate callable length; positive integer, default `2`. Use `1` to include one-line wrappers. |
| `--source-path <path>` | Cross-search, cohesion | Select sources in a file or recursive directory, relative to the repository root (or absolute within it). |
| `--changed-since <commit>` | Cross-search, cohesion | Select added, modified, or moved functions relative to an ancestor of the indexed Git checkpoint, including working-tree changes. Requires Git. |
| `--uncommitted` | Cross-search, cohesion | Select functions indexed from working-tree files: staged, unstaged, or untracked changes in Git; all working-tree functions without Git. |
| `--cross-file-only` | Cross-search | Exclude matches from the same physical file. |
| `--include-symmetric-duplicates` | Cross-search | Allow both directions of same-index matches; otherwise each unordered pair is emitted once. |
| `--neighbors <number>` | Cohesion | Neighbors considered per source; positive integer, default `20`. Changes the analysis graph. |
| `--include-source` | Cohesion JSON | Include callable bodies; omitted by default. |
| `--target-root <path>` | Cross-search | Second repository root; requires `--target-index`. |
| `--target-index <path>` | Cross-search | Second index file; requires `--target-root`. |
| `--target-config <path>` | Cross-search | Target config; defaults to `<target-root>/.slopdex/config.json`. Requires both target options. |

Review adjacent similarity bands without repeating boundary matches:

```bash
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9 --limit 5
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.85-0.9 --limit 5
```

### Refresh and recovery

| Argument | Meaning |
| --- | --- |
| `--target <ref>` | Git snapshot for `update-git`; default `HEAD`. Non-HEAD targets exclude working-tree changes. Later commands normally refresh back to HEAD. |
| `--rebuild-on-divergence` | Allow reconciliation when the saved checkpoint is not an ancestor of the target, such as after a rebase or branch switch. |
| `--force-reindex` | Recreate an **incompatible** index (repository, provider, model, dimensions, strategy, or schema mismatch). A compatible index still follows normal refresh behavior. |
| `--no-reindex` | With Git, still reconcile the committed snapshot but skip working-tree overlays. Without Git, reuse a non-empty index; missing/empty indexes are still populated. Not a general offline switch. |

## Reading results

### Similarity and duplicate clusters

Similarity is a model-dependent score, not a probability of duplication. Higher scores mean greater semantic resemblance. Query summaries show `score  path :: qualifiedName`; cross-search summaries group those lines beneath each source. Functions without matches are omitted from cross-search output.

```text
Cluster 1 (3 functions, similarity 0.9124-0.9568)
  src/auth/session.ts:18:1 :: validateSession
  src/http/middleware.ts:42:1 :: authenticate
  src/users/user-service.ts:27:3 :: UserService.authenticate
```

- A cluster groups functions connected by matches. Its range covers observed links; not every pair necessarily matches directly.
- Clusters sort by member count, then name. Cluster number is not severity.
- Locations identify where to inspect behavior, callers, and architectural roles. Wrappers, adapters, tests, and separate interface implementations can legitimately resemble one another.

### Cohesion

```text
Cohesion: 184 functions analyzed, 37 semantic edges
  same file 35.1%  same folder 29.7%  remote 35.2%  mean distance 1.84

1. gap 0.6053  similarity 0.9400  distance 4  reciprocal
   src/auth/session.ts:18:1 :: validateSession
   packages/http/middleware.ts:42:1 :: authenticate
```

| Field | Interpretation |
| --- | --- |
| Functions analyzed / semantic edges | Selected-source coverage / unique qualifying neighbor pairs, before report limiting. Not quality scores. |
| Same file / same folder / remote | Shares of weighted semantic affinity. Higher remote affinity means more related code crosses folder boundaries. |
| Mean distance | Weighted physical separation: `0` for the same file, `1` for different files in one folder, larger across folders. |
| Gap / rank | A `0–1` review score combining similarity above the threshold and separation; higher gap ranks first. Same-file pairs have zero gap. |
| Reciprocal | Both functions selected each other as neighbors. JSON `null` means the other endpoint was not evaluated under source filtering. |
| `sourceTestPair` | A source/test relationship inferred from paths; separation may be intentional. |
| `externalAffinityRatio` | In JSON file reports, the share of observed affinity outside that file's folder. |

The example suggests reviewing separated authentication responsibilities. It does not establish that they belong in one module. There is no universal cohesion pass threshold. Filtered reports describe selected sources, not the entire repository. Cohesion metrics cover all qualifying edges; reported pairs/files are limited, and groups are built from reported pairs.

For automation, pass `--format json`: query searches and diagnostics return JSON arrays; cross-search returns **JSONL**, one row per matched source; cohesion returns one JSON object containing `repository`, `parameters`, `metrics`, `pairs`, `files`, and `groups`. Results go to stdout; notices and warnings go to stderr.

## System behavior

### Descriptions and scoring

Descriptions are optional and disabled initially. `descriptions enable` persists the selected description model and keeps descriptions current on later updates, including file-context and path changes. Use `slopdex descriptions enable --description-model <model-id>` to change it, or `slopdex descriptions disable` to stop automatic updates and description-based searching/scoring while retaining cached descriptions. `status` exposes `descriptionsEnabled`, `descriptionCount`, and `descriptionProfile`.

Tree-sitter extraction, generated descriptions, and document/query vectors are content-addressed in the same SQLite database. Each validated result is committed immediately, independently of the final logical index update. If indexing is interrupted or a later provider call fails, rerunning reuses every completed result whose profile, operation, input, and source context hash still match.

`search` always searches code; `search-description` always searches purpose descriptions. When all callables have enabled descriptions, cross-search and cohesion automatically use **50% code similarity + 50% description similarity**. Cross-repository analysis needs complete descriptions on both sides; otherwise the entire analysis uses code-only scores. Thresholds and neighbor limits apply to the selected score.

Text output labels combined scores. JSON exposes `codeSimilarity`, `descriptionSimilarity`, and scoring mode/weights (`scoring` for cross-search, `parameters` for cohesion). Compare runs only with matching scoring mode, weights, embedding and description-generator profiles, threshold, neighbor count, and source/candidate filters.

### Exclusions

Root and nested `.gitignore` rules apply even to tracked files and without Git. Working-tree refreshes use current rules; committed-only snapshots use the target commit's rules. Refresh removes newly excluded files and discovers newly eligible ones. Explicit `update-files` rejects ignored files.

Built-in exclusions: `.git`, `.slopdex`, `node_modules`, `dist`, `build`, `coverage`, `vendor`, `generated`, `.venv`, `venv`, `__pycache__`, `.tox`, `.mypy_cache`, `.pytest_cache`, and `target`. Config `include`/`exclude` globs narrow coverage; they cannot override built-in exclusions. Ignore exceptions cannot re-include files beneath an excluded parent directory. Files over 1 MiB are skipped unless `maxFileSize` is raised.

### Failures and recovery

Parse, extraction, read, and file-size failures are saved while healthy callables remain searchable. Inspect them with `slopdex index-errors --format summary`. JSON includes paths, locations, recoverable names, messages, available source, and snapshot provenance. `status` reports `indexingErrorCount` and `failedFileCount`; `functionCount` counts searchable callables.

Saved failures trigger stderr warnings, including on cached runs, help, and cross-search targets. `--ignore-errors` silences warnings without clearing records. Updates retry failed files; successful indexing, deletion, or exclusion clears their diagnostics. Version output bypasses diagnostics.

Use the recovery flag named in the error: `--rebuild-on-divergence` for Git history changes, `--force-reindex` for incompatible indexes. Schema versions before 5 require `--force-reindex`; schema-5 rebuilds preserve reusable artifact caches. For provider/authentication failures, fix the reported configuration and rerun. For source-change-during-indexing errors, rerun after edits settle. Exit status is `0` on success, `2` for argument/domain errors, and `1` for other failures (or invocation without a command).

## Configuration

Optional file: `<root>/.slopdex/config.json`. Example using Jina (requires `JINA_API_KEY`):

```json
{
  "provider": "jina",
  "model": "jina-embeddings-v4",
  "dimensions": 1024,
  "exclude": ["**/fixtures/**"]
}
```

| Property | Purpose / default |
| --- | --- |
| `provider`, `model`, `dimensions` | Embedding settings; defaults are listed in the CLI table. |
| `descriptionModel` | Description model; initially `gpt-5.6-sol`. |
| `indexPath` | Index location; `<root>/.slopdex/index.sqlite`. |
| `include` | Repository-relative glob array; empty/unset includes all supported eligible files. |
| `exclude` | Additional repository-relative exclusion globs. |
| `maxFileSize` | Maximum source-file size in bytes; positive integer, default `1048576`. |
| `embeddingBatchSize` | Embedding inputs per batch; positive integer, default `32`. |

Keep keys in the environment (`OPENAI_API_KEY`, `JINA_API_KEY`). Changing the embedding profile requires rebuilding with `--force-reindex`.

## Development

- [Implementation and library API](docs/implementation.md)
