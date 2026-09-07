# slopdex

Callable-level embedding index, semantic search, and duplicate discovery for TypeScript and JavaScript repositories.

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

Search by meaning:

```bash
slopdex search "validate an authenticated session" --limit 10
```

Find similar callables grouped into connected clusters. Cross-search excludes one-line callables by default:

```bash
slopdex cross-search --threshold 0.85
```

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

Clusters list each function once:

```bash
slopdex cross-search --threshold 0.85 --format clusters
```

```text
Cluster 1 (3 functions, similarity 0.8732-0.9410)
  src/auth.ts:18:0 :: authenticate
  src/session.ts:42:0 :: validateSession
  src/users.ts:27:2 :: Users.authenticate
```

`--format summary` also produces compact file and function names for `search`. JSON is the default for `search`; clusters are the default for `cross-search`.

Use `--threshold` to omit weaker matches. The threshold is a raw cosine similarity and is applied before `--limit`:

```bash
slopdex cross-search --format summary --threshold 0.8 --limit 5
slopdex search "validate session" --format summary --threshold 0.8
```

Use an inclusive range to omit matches that are either weaker or stronger than the desired band:

```bash
slopdex cross-search --format summary --threshold 0.85-0.95 --limit 5
```

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

## Library

```ts
import {
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

Standalone functions `updateFiles`, `updateFromGit`, `updateFromWorkingTree`, `similaritySearch`, and `crossSearchFunctions` are also exported.

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
- Threshold ranges are inclusive and are applied before the result limit.
- Same-index cross-search excludes the source function itself and lists each unordered function pair once by default.
- Cross-file filtering is applied before the per-function result limit.
- Cross-search defaults to a minimum callable length of two lines; source and match length filtering is applied before the result limit.
- Cross-search name regexes match qualified callable names and filter both sources and matches before the result limit.

## Development

```bash
npm run check
```
