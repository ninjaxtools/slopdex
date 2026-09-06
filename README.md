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

Use `JINA_API_KEY` for Jina AI or `OPENAI_API_KEY` for OpenAI. The OpenAI default is `text-embedding-3-small` with 1536 dimensions. Provider, model, dimensions, and embedding strategy form an immutable index profile; changing one requires a new or rebuilt index.

## CLI

Before running a command, Slopdex updates the index from committed `HEAD`, then overlays staged, unstaged, and untracked working-tree changes. If the index does not exist, it prints a notice and creates it automatically.

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

Find the nearest functions for each indexed function that has at least one match. Results are emitted as JSONL:

```bash
slopdex cross-search --limit 5 > similarities.jsonl
```

For a human-readable summary with each match indented beneath its source function:

```bash
slopdex cross-search --limit 5 --format summary
```

```text
src/users.ts :: Users.authenticate
  0.9321  src/session.ts :: validateSession
  0.8475  src/auth.ts :: authenticate
```

Group overlapping similarity pairs into connected clusters and list each function once:

```bash
slopdex cross-search --threshold 0.85 --format clusters
```

```text
Cluster 1 (3 functions, similarity 0.8732-0.9410)
  src/auth.ts:18:0 :: authenticate
  src/session.ts:42:0 :: validateSession
  src/users.ts:27:2 :: Users.authenticate
```

`--format summary` also produces compact file and function names for `search`. JSON remains the default format.

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
})) {
  console.log(result);
}

index.close();
```

Standalone functions `updateFiles`, `updateFromGit`, `similaritySearch`, and `crossSearchFunctions` are also exported.

## Semantics

- Git updates first reconcile the index to blobs from the target commit. When that target is the checked-out `HEAD`, they then overlay staged, unstaged, and untracked files from the working tree.
- The Git checkpoint always identifies the committed base. Working-tree files are marked separately and do not advance it.
- Commit reconciliation and the working-tree overlay are applied in one database transaction after every changed file has parsed and embedded successfully.
- Explicit updates mark files as working-tree sourced and do not move the checkpoint.
- Every Git update removes stale working-tree state before re-indexing the current overlay, so deleted transient files cannot remain in the index.
- `changed-since X` returns current functions that were added, modified, or moved relative to X, including uncommitted overlays. Deleted functions are not returned because they cannot be cross-search sources.
- `uncommitted` returns functions from files currently marked as working-tree sourced.
- Search output is ordered by raw cosine similarity descending, then function ID ascending.
- Threshold ranges are inclusive and are applied before the result limit.
- Same-index cross-search excludes the source function itself and lists each unordered function pair once by default.

## Development

```bash
npm run check
```
