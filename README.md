# slopdex

Callable-level embedding index, semantic search, and duplicate discovery for TypeScript and JavaScript repositories.

The index extracts named functions with tree-sitter, stores metadata and float32 embeddings in SQLite, and uses `sqlite-vec` for exact cosine search. It supports explicit working-tree updates, transactional Git-delta updates, and function-to-function cross-search within one codebase or between compatible indexes.

## Requirements

- Node.js 24 or newer
- Git for Git-tracked updates and `added-since` searches
- An OpenAI or Jina AI API key

## Install

```bash
npm install
npm run build
```

Install the bundled Slopdex skill for OpenCode:

```bash
npm run install:skill:opencode
```

This copies the skill to `~/.config/opencode/skills/slopdex/SKILL.md`, creating the destination directories when needed.

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

When a command needs an index and none exists, Slopdex prints a notice to stderr and automatically indexes committed `HEAD`. Automatic initialization requires a clean Git worktree, just like `update-git`.

Index an exact committed snapshot and record its commit:

```bash
slopdex update-git --root /path/to/repository
```

Later Git updates read only files changed between the recorded commit and `HEAD`:

```bash
slopdex update-git --root /path/to/repository --target HEAD
```

Update or delete specific working-tree files without advancing the Git checkpoint:

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

`--min-similarity` remains available as an equivalent option; do not specify both.
Source functions with no matches at the selected threshold are omitted.
For same-index searches, each function pair is shown only in its first direction by default. Use
`--include-symmetric-duplicates` to include both `A -> B` and `B -> A` results.

Restrict source functions to a file or every indexed file recursively under a directory. Matches are still selected from the whole target index:

```bash
slopdex cross-search --source-path src/services --format summary
slopdex cross-search --source-path src/service.ts --format summary
```

Restrict source functions to functions currently present but absent at a historical commit:

```bash
slopdex cross-search --added-since origin/main --limit 5
```

Search one index against another. Both indexes must use exactly the same embedding profile:

```bash
slopdex cross-search \
  --target-root /path/to/other-repository \
  --target-index /path/to/other-repository/.slopdex/index.sqlite
```

## Library

```ts
import {
  JinaEmbeddingProvider,
  crossSearch,
  openCodeIndex,
} from "slopdex";

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
  sourceFilter: { type: "added-since", commit: "origin/main", path: "src/services" },
  limitPerFunction: 5,
})) {
  console.log(result);
}

index.close();
```

Standalone functions `updateFiles`, `updateFromGit`, `similaritySearch`, and `crossSearchFunctions` are also exported.

## Semantics

- Git updates read blobs from the target commit, not dirty working-tree contents.
- Git updates abort when the worktree contains staged, unstaged, or untracked changes; commit or stash them first.
- The Git checkpoint advances only after every changed file has parsed and embedded successfully.
- Explicit updates mark files as working-tree sourced and do not move the checkpoint.
- The next Git update after those changes are committed reconciles working-tree-sourced files to the commit.
- `added-since X` means a current function whose logical callable identity was absent at X. It does not rely only on `firstSeenCommit`.
- Search output is ordered by raw cosine similarity descending, then function ID ascending.
- Threshold ranges are inclusive and are applied before the result limit.
- Same-index cross-search excludes the source function itself and lists each unordered function pair once by default.

## Development

```bash
npm run check
```
