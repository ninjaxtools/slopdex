# slopdex

Callable-level embedding index, semantic search, duplicate discovery, and physical cohesion analysis for TypeScript and JavaScript repositories.

The index extracts named functions with tree-sitter, stores metadata and float32 embeddings in SQLite, and uses `sqlite-vec` for exact cosine search. It supports explicit working-tree updates, transactional Git-delta updates, and function-to-function cross-search within one codebase or between compatible indexes.

## Requirements

- Node.js 24
- An OpenAI or Jina AI API key

## Install

```bash
npm install -g @ninjaxtools/slopdex
```

Install the skill for OpenCode (`~/.config/opencode/skills/slopdex/SKILL.md`):

```bash
npm run install:skill:opencode
```

## Configuration

Create `.slopdex/config.json` in the repository being indexed:

```json
{
  "provider": "jina",
  "model": "jina-embeddings-v4",
  "dimensions": 1024,
  "exclude": ["**/fixtures/**"]
}
```

Use `JINA_API_KEY` for Jina AI or `OPENAI_API_KEY` for OpenAI. The OpenAI default is `text-embedding-3-large` with 3072 dimensions. Provider, model, dimensions, and embedding strategy form an immutable index profile; changing one requires a new or rebuilt index.

Pass `--force-reindex` to remove and recreate an existing index automatically when its stored profile or other index metadata is incompatible with the current settings. Slopdex prints a warning whenever it performs this rebuild.

## CLI

Before running a command, Slopdex updates the index from committed `HEAD`, then overlays staged, unstaged, and untracked working-tree changes. If the index does not exist, it prints a notice and creates it automatically.

If Git is unavailable or the root is not a Git repository, Slopdex prints a warning to stderr and re-indexes every supported working-tree file on every command. Pass `--no-reindex` to reuse an existing non-empty index instead; missing or empty indexes are still populated. In a Git repository, `--no-reindex` still updates committed files but skips staged, unstaged, and untracked overlays.

Index a committed snapshot, record its commit, and overlay current working-tree changes:

```bash
slopdex update-git --root /path/to/repository
```

The overlay is applied when the target resolves to the checked-out `HEAD`. An explicit historical or other-branch target is indexed as an exact committed snapshot without mixing in files from the current checkout.

Later Git updates read only files changed between the recorded commit and `HEAD`:

```bash
slopdex update-git --root /path/to/repository --target HEAD
```

After the automatic full refresh, explicitly update or delete specific working-tree files. These explicit operations do not themselves advance the Git checkpoint:

```bash
slopdex update-files src/service.ts src/model.ts
slopdex delete-files src/removed.ts
```

## Analysis Examples

### Duplicate Analysis

Compare substantial callables across files at a high similarity threshold:

```bash
slopdex cross-search \
  --cross-file-only \
  --min-lines 4 \
  --threshold 0.9 \
  --limit 5
```

```text
Cluster 1 (3 functions, similarity 0.9124-0.9568)
  src/auth/session.ts:18:1 :: validateSession
  src/http/middleware.ts:42:1 :: authenticate
  src/users/user-service.ts:27:3 :: UserService.authenticate
```

This result found three substantial authentication functions in three files with very high similarity. They are a strong candidate for reviewing repeated validation or session-handling logic and possibly extracting one shared implementation. The middleware and service locations may instead represent intentional architectural layers, so the cluster is evidence to inspect rather than proof that the functions should be merged. The similarity range covers observed links in the connected component; transitive clustering means every function is not necessarily directly similar to every other function.

### Cohesion Analysis

Rank semantically related functions that are separated across files and directory subtrees:

```bash
slopdex cohesion \
  --threshold 0.8 \
  --neighbors 20 \
  --limit 50 \
  --format summary
```

```text
Cohesion: 184 functions analyzed, 37 semantic edges
  same file 35.1%  same folder 29.7%  remote 35.2%  mean distance 1.84

1. gap 0.6053  similarity 0.9400  distance 4  reciprocal
   src/auth/session.ts:18:1 :: validateSession
   packages/http/middleware.ts:42:1 :: authenticate
```

There is no universal pass/fail cutoff for cohesion, but this example has several warning signs. More than a third of weighted semantic affinity crosses folder boundaries, and the mean distance of 1.84 is above the same-folder distance of one. The top pair is also strongly related at 0.94 similarity yet four distance units apart, producing a relatively high gap of 0.6053; the reciprocal match strengthens that signal. A more cohesive result under the same settings would concentrate affinity in the same-file and same-folder percentages, have a lower mean distance, and contain few high-gap remote pairs. An LLM or reviewer should inspect whether the shared authentication behavior belongs in one module, while accounting for the possibility that session and middleware responsibilities are intentionally separated. Compare these metrics between modules or over time rather than treating any single percentage as a fixed quality threshold.

## Command Details

### Semantic Search

Search by meaning:

```bash
slopdex search "validate an authenticated session" --limit 10
```

### Duplicate Discovery

Treat these clusters as review candidates rather than proof of duplication. Lower `--threshold` to broaden discovery, or lower `--min-lines` when short wrappers are relevant.

Use `--format json` for JSONL with one object per source function, or `--format summary` for a compact pair listing:

```bash
slopdex cross-search --limit 5 --format json > similarities.jsonl
slopdex cross-search --limit 5 --format summary
```

```text
src/users.ts :: Users.authenticate
  0.9321  src/session.ts :: validateSession
  0.8475  src/auth.ts :: authenticate
```

`--format summary` also produces compact file and function names for `search`. JSON is the default for `search`; clusters are the default for `cross-search`.

Use `--threshold` to omit weaker matches. The threshold is a raw cosine similarity and is applied before `--limit`:

```bash
slopdex cross-search --format summary --threshold 0.8 --limit 5
slopdex search "validate session" --format summary --threshold 0.8
```

Use a half-open range to select a similarity band. The left bound is inclusive and the right bound is exclusive, so `0.85-0.9` means `0.85 <= similarity < 0.9`:

```bash
slopdex cross-search --format summary --threshold 0.85-0.95 --limit 5
```

For iterative duplicate review, start with the strongest matches and then inspect progressively weaker bands. This keeps each pass focused while avoiding one large, noisy result set:

```bash
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9 --limit 5
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.85-0.9 --limit 5
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.8-0.85 --limit 5
```

Because range upper bounds are exclusive, these adjacent passes do not repeat boundary matches.

Source functions with no matches at the selected threshold are omitted.
For same-index searches, each function pair is shown only in its first direction by default. Use
`--include-symmetric-duplicates` to include both `A -> B` and `B -> A` results.

Cross-search includes only callables spanning at least two lines by default, for both sources and matches. Use `--min-lines` to change the minimum, including `--min-lines 1` to include one-line closures:

```bash
slopdex cross-search --min-lines 4 --threshold 0.8
```

Use `--regex` to require both source and matching candidates to have qualified names matching a JavaScript regular expression. Name filtering happens before the result limit:

```bash
slopdex cross-search --regex '^(User|Session)\.' --threshold 0.8
```

Use `--cross-file-only` to exclude matches from the source function's file. Same relative paths in different repository roots remain eligible:

```bash
slopdex cross-search --cross-file-only --format summary --threshold 0.8
```

Restrict source functions to a file or every indexed file recursively under a directory. Matches are still selected from the whole target index:

```bash
slopdex cross-search --source-path src/services --format summary
slopdex cross-search --source-path src/service.ts --format summary
```

Restrict source functions to functions added, modified, or moved since a historical commit. This also compares the current working-tree overlay against that commit:

```bash
slopdex cross-search --changed-since origin/main --limit 5
```

Restrict source functions to files with uncommitted working-tree content:

```bash
slopdex cross-search --uncommitted --format clusters --threshold 0.85
```

Search one index against another. Both indexes must use exactly the same embedding profile:

```bash
slopdex cross-search \
  --target-root /path/to/other-repository \
  --target-index /path/to/other-repository/.slopdex/index.sqlite
```

If the target uses a non-default configuration path, pass it with `--target-config`. The target repository's indexing policy is kept separate from the source policy.

### Cohesion Analysis

Cohesion analysis builds a same-index semantic neighbor graph and combines each edge's cosine similarity with its repository path distance. The default JSON report is designed for downstream analysis and contains repository-wide metrics, ranked pairs, file-level external-affinity metrics, and connected groups for navigation. Function source is omitted by default; use `--include-source` when a downstream consumer needs it.

Path distance is zero within a file. Crossing to another file costs one unit, plus one unit for each directory-tree hop after the paths' common ancestor. Every pair also reports `same-file`, `same-folder`, or `different-folder`, the common ancestor, and whether it crosses between source and test paths.

The ranking score is a review heuristic, not an instruction to move code:

```text
semanticWeight = clamp((similarity - threshold) / (1 - threshold), 0, 1)
separationWeight = 1 - exp(-physicalDistance / 2)
cohesionGap = semanticWeight * separationWeight
```

Raw similarity, path distance, both weights, and reciprocal-neighbor status remain in the output so an LLM or reviewer can assess the result. Reciprocity is `null` when a source filter means the other endpoint's neighborhood was not searched. Facades, adapters, source/test mirrors, and intentionally layered implementations may be correctly separated.

Restrict analysis sources while continuing to compare them with the whole index:

```bash
slopdex cohesion --source-path src/services --format summary
slopdex cohesion --changed-since origin/main --format json
slopdex cohesion --uncommitted --format json
```

## Library

```ts
import {
  analyzeCohesion,
  JinaEmbeddingProvider,
  crossSearch,
  openCodeIndex,
} from "@ninjaxtools/slopdex";

const index = openCodeIndex({
  rootDir: "/path/to/repository",
  provider: new JinaEmbeddingProvider(),
});

await index.updateFromGit();

const results = await index.similaritySearch({
  query: "validate an authenticated session",
  limit: 10,
});

const cohesion = await analyzeCohesion({
  source: index,
  minSimilarity: 0.8,
  neighbors: 20,
  limit: 50,
});

for await (const result of crossSearch({
  source: index,
  sourceFilter: { type: "changed-since", commit: "origin/main", path: "src/services" },
  limitPerFunction: 5,
  crossFileOnly: true,
  minLines: 4,
  nameRegex: "^(User|Session)\\.",
})) {
  console.log(result);
}

index.close();
```

Standalone functions `updateFiles`, `updateFromGit`, `updateFromWorkingTree`, `similaritySearch`, `crossSearchFunctions`, and `analyzeCodeCohesion` are also exported.

## Semantics

- Git updates first reconcile the index to blobs from the target commit. When that target is the checked-out `HEAD`, they then overlay staged, unstaged, and untracked files from the working tree.
- The Git checkpoint always identifies the committed base. Working-tree files are marked separately and do not advance it.
- Committed files are re-indexed only when their Git blob changes or their stored row is missing or inconsistent. Working-tree files are read and re-indexed on every refresh.
- Function embeddings are cached by their hashed embedding input, including versions that are not currently referenced, so reverting or recommitting known code does not call the embedding provider again.
- Commit reconciliation and the working-tree overlay are applied in one database transaction after every changed file has parsed and embedded successfully.
- Explicit updates mark files as working-tree sourced and do not move the checkpoint.
- Every Git update removes stale working-tree state before re-indexing the current overlay, so deleted transient files cannot remain in the index.
- `changed-since X` returns current functions that were added, modified, or moved relative to X, including uncommitted overlays. Deleted functions are not returned because they cannot be cross-search sources.
- `uncommitted` returns functions from files currently marked as working-tree sourced.
- Search output is ordered by raw cosine similarity descending, then function ID ascending.
- Threshold ranges include the left bound, exclude the right bound, and are applied before the result limit.
- Same-index cross-search excludes the source function itself and lists each unordered function pair once by default.
- Cross-file filtering is applied before the per-function result limit.
- Cross-search defaults to a minimum callable length of two lines; source and match length filtering is applied before the result limit.
- Cross-search name regexes match qualified callable names and filter both sources and matches before the result limit.
- Cohesion analyzes the strongest `--neighbors` semantic matches per source, ranks unique pairs globally, and applies `--limit` after scoring.
- Cohesion summary metrics use every discovered semantic edge. Pair, file, and group output is bounded by `--limit`.
- Cohesion's location ratios are exclusive: same file, different files in the same folder, and edges crossing a directory boundary.
- With `--source-path`, `--changed-since`, or `--uncommitted`, summary and file metrics are scoped to the selected source functions rather than presented as repository-wide measurements.
- Cohesion runs one exact vector-neighbor query per selected source function. Whole-repository analysis therefore has quadratic compute cost with the current exact search backend; use source filters on very large indexes.

## Development

```bash
npm run check
```
