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

Optionally enable a hosted second-stage reranker for `search` and `search-description`:

```bash
export COHERE_API_KEY="your-api-key"
slopdex config reranker cohere
# Or: export JINA_API_KEY="your-api-key" && slopdex config reranker jina
# Or use an LLM: export OPENAI_API_KEY="your-api-key" && slopdex config reranker openai
```

The config command is the only CLI switch for reranking. Use `slopdex config reranker disable` to return to embedding-only ordering. An optional final argument selects a model, for example `slopdex config reranker cohere rerank-v4.0-fast`. OpenAI LLM reranking defaults to `gpt-5.6-luna` with high reasoning and receives the top 10 embedding results; change the pool with `--reranker-candidates`, for example `slopdex config reranker openai gpt-5.6-luna --reranker-candidates 20`.

To search generated descriptions of each function's role instead:

```bash
slopdex descriptions enable
slopdex search-description "keep the repository index synchronized" --format summary --limit 10
```

The default description provider requires `OPENAI_API_KEY` even when Jina supplies embeddings. OpenCode Zen and Go use `OPENCODE_API_KEY`. Description generation can add provider costs; see [descriptions and scoring](#descriptions-and-scoring).

`descriptions disable` turns off description generation while retaining cached data.

To choose from OpenCode's current published models and persist description settings without creating an index:

```bash
slopdex models opencode-go
slopdex config model opencode-go/gpt-5.6-luna
slopdex config descriptions enable
```

The next index-using command creates or refreshes the index and applies the configured description state.
You can also pass the selection separately as `slopdex config model --description-provider opencode-go --description-model gpt-5.6-luna`.

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

These select source functions while keeping the full eligible index available for matches.

### Find related code stored far apart

```bash
slopdex cross-search --cohesion --threshold 0.8 --limit 20 --format summary
```

`--cohesion` keeps the semantic matches selected by cross-search, annotates them with physical distance, and orders each source's matches from farthest to nearest. Similarity breaks distance ties.

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
| `models [opencode\|opencode-go]` | Fetch valid models from the current published Zen and/or Go catalogs. | Qualified `provider/model` lines; optional JSON array |
| `config model <model\|provider/model>` | Validate a published OpenCode model and persist its provider/model selection without opening an index. Bare IDs auto-resolve only when unambiguous. | Updated setting summary; optional JSON |
| `config descriptions <enable\|disable>` | Persist whether the next index-using command should enable or disable descriptions. Does not open an index. | Updated setting summary; optional JSON |
| `config reranker <cohere\|jina\|openai\|disable> [model]` | Enable a hosted or OpenAI LLM query reranker, optionally selecting a model, or disable it. OpenAI accepts `--reranker-candidates <number>` from 1 to 100 and defaults to 10. Does not open an index. | Updated setting summary; optional JSON |
| `search <query>` | Search function code by meaning. Quote multiword queries. | Summary; optional JSON array |
| `descriptions <enable\|disable>` | Enable or disable automatic purpose descriptions. Re-enabling with unchanged inputs reuses cached descriptions. | JSON statistics |
| `search-description <query>` | Search purpose descriptions after enabling them. | Summary including description text; optional JSON array |
| `cross-search` | Find neighbors for each selected function in this or another index. | `clusters` by default; optional `summary` or JSONL |
| `status` | Refresh and show index metadata, counts, and profiles. | JSON object |
| `index-errors` | Read saved file/function indexing failures. | Summary; optional JSON array |
| `update-git` | Explicitly refresh a Git snapshot, with current working-tree changes when targeting HEAD. | JSON update statistics |
| `update-files <path...>` | After automatic refresh, explicitly reparse selected working-tree files. Paths are repository-relative or absolute within the root. | JSON update statistics |
| `reindex-files [--callables]` | Regenerate descriptions for files changed since their stored file description. By default stops after each file description; `--callables` also regenerates its callable descriptions. | JSON description statistics |
| `delete-files <path...>` | After automatic refresh, remove paths from the index; source files are not deleted. A later refresh can restore eligible files. | JSON update statistics |

Manual maintenance examples:

```bash
slopdex update-git
slopdex update-files src/service.ts src/model.ts
slopdex reindex-files
slopdex reindex-files --callables
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
| `--description-provider <openai\|opencode\|opencode-go>` | Description provider; OpenAI by default. OpenCode values use Zen or Go with `OPENCODE_API_KEY`. |
| `--description-model <name>` | Description model; `gpt-5.6-sol` for OpenAI/Zen and `gpt-5.6-luna` for Go, then the persisted model unless overridden. Published OpenCode models use their documented protocol. |
| `--ignore-errors` | Silence warnings about saved indexing errors; records remain available. |
| `-h`, `--help` | Show CLI usage without refreshing. |
| `--version` | Print the package version and exit. |

Explicit relative config and index paths resolve from the current directory, not `--root`.

### Search and analysis

| Argument | Applies to | Meaning / default |
| --- | --- | --- |
| `--limit <number>` | Both query searches, cross-search | Positive integer. Query matches: `10`; cross-search neighbors per source: `5`. |
| `--threshold <number\|min-max>` | Both query searches, cross-search | Minimum similarity, or range with inclusive minimum and exclusive maximum. Default `-1`. |
| `--format <json\|summary\|clusters>` | Both query searches, cross-search, index-errors | Output format; see the commands table. `clusters` is only for ordinary cross-search. |
| `-e <regex>`, `--regexp <regex>`, `--regex <regex>` | Both query searches, cross-search | Equivalent case-sensitive JavaScript regex options over qualified names. Query searches filter results before limiting; cross-search filters sources only. |
| `--min-lines <number>` | Cross-search | Minimum source and candidate callable length; positive integer, default `2`. Use `1` to include one-line wrappers. |
| `--source-path <path>` | Cross-search | Select sources in a file or recursive directory, relative to the repository root (or absolute within it). |
| `--changed-since <commit>` | Cross-search | Select added, modified, or moved functions relative to an ancestor of the indexed Git checkpoint, including working-tree changes. Requires Git. |
| `--uncommitted` | Cross-search | Select functions indexed from working-tree files: staged, unstaged, or untracked changes in Git; all working-tree functions without Git. |
| `--cross-file-only` | Cross-search | Exclude matches from the same physical file. |
| `--include-symmetric-duplicates` | Cross-search | Allow both directions of same-index matches; otherwise each unordered pair is emitted once. |
| `--cohesion` | Cross-search | Add `physicalDistance` and re-rank each source's matches by descending distance, with similarity as the tie-breaker. Defaults to summary output; incompatible with clusters. |
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

With reranking enabled, query summaries show `rerankScore rerank (similarity similarity)`. JSON retains `similarity` and adds `rerankScore`. The similarity threshold first filters embedding candidates. Cohere and Jina receive up to five times the requested limit. The OpenAI LLM receives the configured number of top embedding results, 10 by default, or the requested result limit when it is larger. Candidates include function descriptions when available and function metadata/source code. Cross-search and cohesion analysis are not sent to rerankers.

```text
Cluster 1 (3 functions, similarity 0.9124-0.9568)
  src/auth/session.ts:18:1 :: validateSession
  src/http/middleware.ts:42:1 :: authenticate
  src/users/user-service.ts:27:3 :: UserService.authenticate
```

- A cluster groups functions connected by matches. Its range covers observed links; not every pair necessarily matches directly.
- Clusters sort by member count, then name. Cluster number is not severity.
- Locations identify where to inspect behavior, callers, and architectural roles. Wrappers, adapters, tests, and separate interface implementations can legitimately resemble one another.

### Physical cohesion

```text
src/auth/session.ts :: validateSession
  0.9400  packages/http/middleware.ts :: authenticate  [distance 4]
  0.9300  src/auth/token.ts :: validateToken  [distance 1]
```

Run cross-search with `--cohesion` to put physically distant matches first. Distance is `0` within one file, `1` between files in one folder, and `1` plus directory-tree hops across folders. The option only changes the order of each source's selected semantic matches; it does not change similarity scores or establish that distant code belongs together.

For automation, pass `--format json`: query searches and diagnostics return JSON arrays; cross-search returns **JSONL**, one row per matched source. With `--cohesion`, each match includes `physicalDistance`. Results go to stdout; notices and warnings go to stderr.

## System behavior

### Descriptions and scoring

Descriptions are optional and disabled initially. `descriptions enable` persists the selected provider and model and keeps callable descriptions current on later updates. Use `slopdex descriptions enable --description-provider opencode-go` for OpenCode Go, or combine `--description-provider` and `--description-model` to change both. `slopdex descriptions disable` stops automatic updates and description-based searching/scoring while retaining cached descriptions. `status` exposes callable/file description counts, stale file-description count, enabled state, and profile.

Descriptions are generated in source order through one conversation per file. Instructions and complete file source form a stable prefix; Slopdex asks for the overall file description first, then each callable request and answer extends that conversation. This allows supported providers to reuse their prompt cache instead of receiving a separate duplicated file context for every callable.

Ordinary updates preserve an existing file description even when its source changes, while still refreshing callable descriptions. `slopdex reindex-files` explicitly regenerates stale file descriptions and their embeddings; add `--callables` to continue through and replace every callable description in those files.

Tree-sitter extraction, generated descriptions, and document/query vectors are content-addressed in the same SQLite database. Each validated result is committed immediately, independently of the final logical index update. If indexing is interrupted or a later provider call fails, rerunning reuses every completed result whose profile, operation, input, and source context hash still match.

With complete descriptions, `search` and cross-search average **one-third code similarity + one-third callable-description similarity + one-third file-description similarity**. `search-description` averages callable and file descriptions without code. Cross-repository analysis needs complete descriptions on both sides; otherwise the entire analysis uses code-only scores. Stale file descriptions remain searchable until explicitly reindexed. Thresholds and limits apply to the selected score.

Text output labels combined scores. JSON exposes `codeSimilarity`, `descriptionSimilarity`, `fileDescriptionSimilarity`, and cross-search scoring mode/weights. Compare runs only with matching scoring mode, weights, embedding and description-generator profiles, threshold, and source/candidate filters.

### Exclusions

Root and nested `.gitignore` rules apply even to tracked files and without Git. Working-tree refreshes use current rules; committed-only snapshots use the target commit's rules. Refresh removes newly excluded files and discovers newly eligible ones. Explicit `update-files` rejects ignored files.

Built-in exclusions: `.git`, `.slopdex`, `node_modules`, `dist`, `build`, `coverage`, `vendor`, `generated`, `.venv`, `venv`, `__pycache__`, `.tox`, `.mypy_cache`, `.pytest_cache`, and `target`. Config `include`/`exclude` globs narrow coverage; they cannot override built-in exclusions. Ignore exceptions cannot re-include files beneath an excluded parent directory. Files over 1 MiB are skipped unless `maxFileSize` is raised.

### Failures and recovery

Parse, extraction, read, and file-size failures are saved while healthy callables remain searchable. Inspect them with `slopdex index-errors --format summary`. JSON includes paths, locations, recoverable names, messages, available source, and snapshot provenance. `status` reports `indexingErrorCount` and `failedFileCount`; `functionCount` counts searchable callables.

Saved failures trigger stderr warnings, including on cached runs, help, and cross-search targets. `--ignore-errors` silences warnings without clearing records. Updates retry failed files; successful indexing, deletion, or exclusion clears their diagnostics. Version output bypasses diagnostics.

Use the recovery flag named in the error: `--rebuild-on-divergence` for Git history changes, `--force-reindex` for incompatible indexes. Schema versions 6 and 7 migrate in place; earlier schemas require `--force-reindex`, with compatible rebuilds preserving reusable artifact caches. For provider/authentication failures, fix the reported configuration and rerun. For source-change-during-indexing errors, rerun after edits settle. Exit status is `0` on success, `2` for argument/domain errors, and `1` for other failures (or invocation without a command).

## Configuration

Optional file: `<root>/.slopdex/config.json`. Example using Jina embeddings, OpenAI LLM reranking, and OpenCode Go descriptions (requires `JINA_API_KEY`, `OPENAI_API_KEY` for query searches, plus `OPENCODE_API_KEY` when descriptions are enabled):

```json
{
  "provider": "jina",
  "model": "jina-embeddings-v4",
  "dimensions": 1024,
  "rerankingEnabled": true,
  "rerankerProvider": "openai",
  "rerankerModel": "gpt-5.6-luna",
  "rerankerCandidates": 10,
  "descriptionProvider": "opencode-go",
  "exclude": ["**/fixtures/**"]
}
```

| Property | Purpose / default |
| --- | --- |
| `provider`, `model`, `dimensions` | Embedding settings; defaults are listed in the CLI table. |
| `descriptionProvider` | Description provider: `openai`, `opencode` (Zen), or `opencode-go`; defaults to `openai`. |
| `descriptionModel` | Description model; provider default unless explicitly set. |
| `descriptionsEnabled` | When true or false, the next index-using command applies that enabled state during its normal refresh. Unset leaves persisted index state unchanged. |
| `rerankingEnabled` | Enables second-stage ranking for `search` and `search-description`; disabled/unset by default. Prefer changing it through `config reranker`. |
| `rerankerProvider` | Reranker: `cohere`, `jina`, or `openai`. OpenAI uses an LLM rather than a dedicated reranking endpoint. |
| `rerankerModel` | Provider model; defaults to Cohere `rerank-v4.0-pro`, Jina `jina-reranker-v3.5`, or OpenAI `gpt-5.6-luna`. |
| `rerankerCandidates` | Embedding-ranked candidates sent to the OpenAI LLM; integer from `1` to `100`, default `10`. The requested result limit takes precedence when larger, up to 100. |
| `indexPath` | Index location; `<root>/.slopdex/index.sqlite`. |
| `include` | Repository-relative glob array; empty/unset includes all supported eligible files. |
| `exclude` | Additional repository-relative exclusion globs. |
| `maxFileSize` | Maximum source-file size in bytes; positive integer, default `1048576`. |
| `embeddingBatchSize` | Embedding inputs per batch; positive integer, default `32`. |

Keep keys in the environment (`OPENAI_API_KEY`, `JINA_API_KEY`, `COHERE_API_KEY`, `OPENCODE_API_KEY`). Reranker settings do not change the stored index and do not require a rebuild. Changing the embedding profile requires rebuilding with `--force-reindex`.

## Development

- [Implementation and library API](docs/implementation.md)
