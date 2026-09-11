# slopdex

Slopdex indexes named functions in TypeScript and JavaScript repositories. It uses embeddings to search code by meaning, find similar implementations, and measure whether related functions are stored near each other.

## Install

```bash
npm install -g @ninjaxtools/slopdex
```

## Getting Started

Set your OpenAI API key:

```bash
export OPENAI_API_KEY="your-api-key"
```

Run a semantic search from the repository you want to analyze:

```bash
slopdex search "validate an authenticated session" --format summary --limit 10
```

## Analysis Examples

### Search By Function Purpose

Enable optional purpose summaries for every indexed callable:

```bash
slopdex use-summaries
slopdex search-summary "keep the repository index synchronized" --limit 10 --format summary
```

Summaries describe a function's responsibility and role in its codebase, using its repository name, path, source, and surrounding file context. Generation uses the OpenAI Responses API with **`gpt-5.6-sol`** by default and requires `OPENAI_API_KEY`, including when Jina supplies the embeddings.

`use-summaries` stores each summary and its embedding in SQLite and enables a persistent repository-index setting. Subsequent indexing operations automatically generate summaries for new and changed files, including changes to surrounding context and file paths. Unchanged inputs reuse cached summaries and vectors; deleted functions disappear from summary search. Running `use-summaries` again is a no-op when summaries are already complete. Summary generation and embeddings are saved atomically, so a failed request does not leave partially updated callables.

`search-summary` uses a separate summary embedding store with the configured embedding provider and supports the same query, limit, threshold, and output options as `search`. JSON results include the summary; `--format summary` prints it alongside each match. `status` reports `summariesEnabled`, `summaryCount`, and `summaryProfile`.

To choose a different summary model, run `slopdex use-summaries --summary-model <model-id>` or set `summaryModel` in `.slopdex/config.json`. The chosen model is persisted for future updates. Existing indexes migrate automatically; summaries remain disabled until enabled explicitly.

### Combined Code And Purpose Analysis

Once summaries are enabled and every indexed callable has a summary embedding, `cross-search` and `cohesion` automatically combine implementation and purpose similarity:

```text
similarity = 0.5 * codeSimilarity + 0.5 * summarySimilarity
```

```bash
slopdex use-summaries
slopdex cross-search --threshold 0.8 --format json
slopdex cohesion --threshold 0.8 --format summary
```

Both cosine scores and their average are calculated in one SQLite query. Thresholds (including half-open ranges), ranking, and neighbor/result limits apply to the combined score. Cohesion uses these combined neighbors for reciprocity, gap scores, file affinities, and groups.

For cross-repository search, both indexes must have complete, enabled summaries. If either index lacks them, the entire analysis uses code-only similarity. Cohesion likewise uses code-only scoring if its summary index is incomplete or disabled. The existing embedding profiles must match across repositories; summary-generator models may differ.

Combined JSON matches and cohesion pairs expose `codeSimilarity` and `summarySimilarity` alongside `similarity`. Cross-search rows include a `scoring` object with `similarityMode`, `similarityWeights`, and both summary-generator profiles. Cohesion records the mode and weights in `parameters` and the summary-generator profile in `repository.summaryProfile`. The mode is `"code-summary-average"` with weights `{ "code": 0.5, "summary": 0.5 }`, or `"code"` with weights `{ "code": 1, "summary": 0 }`. Text output labels combined scores.

Compare analysis results only when the similarity mode and weights match, as well as the embedding profile, summary-generator profiles, thresholds, and analysis scope. Enabling summaries can change rankings and cohesion metrics. `search` continues to use only code vectors, and `search-summary` uses only summary vectors.

### Duplicate Analysis

Compare functions in different files and include functions that span at least four lines:

```bash
slopdex cross-search \
  --cross-file-only \
  --min-lines 4 \
  --threshold 0.9 \
  --limit 5
```

The default output groups related functions into clusters:

```text
Cluster 1 (3 functions, similarity 0.9124-0.9568)
  src/auth/session.ts:18:1 :: validateSession
  src/http/middleware.ts:42:1 :: authenticate
  src/users/user-service.ts:27:3 :: UserService.authenticate
```

A cluster contains functions connected by similarity matches. The similarity range covers the observed links in the cluster. Connected functions may be linked through another function, so review the source before deciding that code is duplicated.

### Cohesion Analysis

Find related functions that are separated across files and directories:

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

The summary divides semantic relationships into the same file, the same folder, and different folders. Mean distance increases when related functions are stored farther apart. The gap score combines semantic similarity with path distance and ranks pairs for review. `reciprocal` means both functions are among each other's nearest semantic matches.

Cohesion does not have a universal pass threshold. Compare results only when the similarity mode and weights, embedding and summary-generator profiles, threshold, neighbor count, and source scope are the same.

## Limit Analysis Scope

Restrict source functions to a file or directory:

```bash
slopdex cross-search --source-path src/services --format summary
slopdex cohesion --source-path src/services --format summary
```

Analyze functions added, changed, or moved since a commit:

```bash
slopdex cross-search --changed-since origin/main --format summary
slopdex cohesion --changed-since origin/main --format summary
```

Analyze functions in files with uncommitted changes:

```bash
slopdex cross-search --uncommitted --format summary
slopdex cohesion --uncommitted --format summary
```

These options restrict the source functions. Slopdex still compares them with the full index.

## Output And Filters

`search` supports `json` and `summary` output. `cross-search` supports `json`, `summary`, and `clusters` output. Use `--format` to select one.

Use `--threshold` with a minimum similarity or a range:

```bash
slopdex search "validate session" --threshold 0.8 --format summary
slopdex cross-search --threshold 0.85-0.9 --format summary
```

For a range, the lower bound is included and the upper bound is excluded. Similarity values depend on the embedding model, so use them to rank results from the same index profile.

Run `slopdex --help` for all commands and options.

## Indexing And Data

Before each analysis command, Slopdex updates its index from the current Git commit and the staged, unstaged, and untracked files in the working tree. The index is created at `.slopdex/index.sqlite` by default.

Add `.slopdex/` to the repository's `.gitignore` so the local index is not committed.

Slopdex sends extracted function source to the configured embedding provider. The function metadata and embeddings are stored in the local SQLite index.

Whole-repository cohesion analysis compares neighbors for every selected function. Use `--source-path`, `--changed-since`, or `--uncommitted` to reduce the scope in large repositories.

## Library

The package also exports the index and analysis APIs:

```ts
import {
  OpenAIEmbeddingProvider,
  openCodeIndex,
} from "@ninjaxtools/slopdex";

const index = openCodeIndex({
  rootDir: "/path/to/repository",
  provider: new OpenAIEmbeddingProvider(),
});

await index.updateFromGit();

const results = await index.similaritySearch({
  query: "validate an authenticated session",
  limit: 10,
});

index.close();
```

Exports also include `crossSearch`, `analyzeCohesion`, `JinaEmbeddingProvider`, and standalone functions for index updates and searches.

For summary search, call `await index.useSummaries()` after updating the index, then `await index.searchSummary({ query: "maintain the repository index" })`. Optionally pass `summaryProvider: new OpenAISummaryProvider({ model: "gpt-5.6-sol" })` when opening an index. Custom providers implement the exported `SummaryProvider` interface. The package also exports standalone `useSummaries` and `searchSummary` helpers.

## Development

```bash
npm run check
```

## Configuration

Configuration is optional. Without `.slopdex/config.json`, Slopdex uses OpenAI's `text-embedding-3-large` model with 3072 dimensions and the built-in source exclusions.

Create `.slopdex/config.json` to change these settings:

```json
{
  "provider": "jina",
  "model": "jina-embeddings-v4",
  "dimensions": 1024,
  "exclude": ["**/fixtures/**"]
}
```

Use `OPENAI_API_KEY` for OpenAI and `JINA_API_KEY` for Jina AI. The provider, model, dimensions, and embedding strategy define the index profile. Rebuild the index with `--force-reindex` after changing the profile.
