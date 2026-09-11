# slopdex

Slopdex uses Tree-sitter to index named functions in Python, JavaScript, JSX, TypeScript, TSX, Rust, Go, Java, and C repositories. It uses embeddings to search code by meaning, find similar implementations, and measure whether related functions are stored near each other.

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

Filter source symbols by qualified name with `-e <regex>` (or `--regexp <regex>`):

```bash
slopdex cross-search -e '^UserService\.' --format summary
slopdex cohesion -e 'validate|authenticate' --source-path src/auth
```

The regex uses case-sensitive JavaScript syntax and matches qualified names such as `UserService.authenticate`. It filters only the source symbols for cross-search and cohesion; matching candidates are still drawn from the whole eligible index, including symbols that do not match the regex. It works with code-only and combined code/summary scoring.

Source restrictions combine by intersection:

```bash
slopdex cross-search -e 'validate' \
  --source-path src \
  --changed-since origin/main \
  --uncommitted \
  --min-lines 4 \
  --cross-file-only
```

With both Git filters, a source must have changed since the specified commit and belong to an uncommitted file. Path, name, and minimum-line filters narrow the selection further. Existing target restrictions such as `--min-lines` and `--cross-file-only` continue to apply. The separate `--regex` option filters **both** analysis sources and matching candidates.

## Output And Filters

`search` supports `json` and `summary` output. `cross-search` supports `json`, `summary`, and `clusters` output. Use `--format` to select one.

For `search` and `search-summary`, `-e` restricts result symbols before the result limit is applied:

```bash
slopdex search "validate session" -e '^Session\.' --limit 10
slopdex search-summary "persist user data" -e 'save|persist' --limit 5
```

Use `--threshold` with a minimum similarity or a range:

```bash
slopdex search "validate session" --threshold 0.8 --format summary
slopdex cross-search --threshold 0.85-0.9 --format summary
```

For a range, the lower bound is included and the upper bound is excluded. Similarity values depend on the embedding model, so use them to rank results from the same index profile.

Run `slopdex --help` for all commands and options.

## Indexing And Data

### Supported Languages

Languages are detected automatically from file extensions. A single index can contain multiple languages; search, purpose summaries, cross-search, and cohesion work across all of them.

| Language | Extensions | Extracted callables |
| --- | --- | --- |
| Python | `.py`, `.pyw` | Functions, async functions, class methods, constructors, generators, and bound lambdas; includes decorators in source |
| JavaScript | `.js`, `.mjs`, `.cjs` | Functions, generators, methods, constructors, and named function expressions/arrows |
| JSX | `.jsx` | JavaScript callables, including components returning JSX |
| TypeScript | `.ts`, `.mts`, `.cts` | Typed functions, methods, constructors, and named function expressions/arrows |
| TSX | `.tsx` | TypeScript callables, including generic components returning JSX |
| Rust | `.rs` | Functions, `impl` methods/associated functions, trait default methods, and `let`-bound closures |
| Go | `.go` | Functions, receiver methods, and function literals bound to variables or assignments |
| Java | `.java` | Methods, constructors (including compact record constructors), and variable-bound lambdas |
| C | `.c`, `.h` | Function definitions, including static/inline functions and functions returning pointers |

Qualified names include enclosing classes, functions, and explicit modules. Go methods include their receiver type (`Store[T].Get`); Rust trait implementations include the type and trait (`<Store<T> as Read>.read`). Definitions retain their source, signature, line/column locations, and stable identity for incremental updates. Declarations without bodies and anonymous callbacks are omitted. Extraction is syntactic: Rust macros and C macros are not expanded, and C preprocessor branches are indexed as written. `.h` files use the C grammar.

Tree-sitter grammars ship as package dependencies; no language server or project compiler configuration is required. Syntax errors produce warnings while recoverable callables are still indexed. Existing indexes pick up newly supported files during the next normal refresh.

Default exclusions cover dependency and build directories: `.git`, `.slopdex`, `node_modules`, `dist`, `build`, `coverage`, `vendor`, `generated`, `.venv`, `venv`, `__pycache__`, `.tox`, `.mypy_cache`, `.pytest_cache`, and `target`. Optional `include` and `exclude` glob arrays in `.slopdex/config.json` further restrict the indexed files.

Slopdex also respects root and nested `.gitignore` files using the [`ignore`](https://www.npmjs.com/package/ignore) package. Patterns, directory rules, anchoring, escapes, and `!` exceptions follow Git ignore semantics. A nested rule cannot re-include files beneath an excluded parent directory. These exclusions apply even to tracked files and when Git is unavailable; explicit `update-files` requests for ignored files are rejected.

Working-tree indexing and HEAD overlays use the current `.gitignore` files. Historical snapshots and committed-only Git indexing use the ignore files stored in the selected commit. Normal refreshes remove previously indexed files that become ignored and discover files that become eligible again, including when only ignore rules change. Configuration includes and negated ignore rules cannot override the built-in exclusions.

### Index Updates

Before each analysis command, Slopdex updates its index from the current Git commit and the staged, unstaged, and untracked files in the working tree. The index is created at `.slopdex/index.sqlite` by default.

Add `.slopdex/` to the repository's `.gitignore` so the local index is not committed.

Slopdex sends extracted function source to the configured embedding provider. The function metadata and embeddings are stored in the local SQLite index.

Whole-repository cohesion analysis compares neighbors for every selected function. Use `--source-path`, `--changed-since`, or `--uncommitted` to reduce the scope in large repositories.

### Inspecting Indexing Errors

Slopdex persists file and function indexing diagnostics in the SQLite `indexing_errors` table. Parse errors, parser exceptions, callable-extraction failures, file-read failures, and files exceeding `maxFileSize` retain references instead of silently disappearing. Healthy functions in partially parsed files and other healthy files remain searchable; malformed callables are omitted from semantic search and listed in the diagnostics.

```bash
slopdex index-errors
slopdex index-errors --format summary
slopdex index-errors --index /path/to/another/index.sqlite
```

This command reads the saved diagnostics without refreshing the index or calling an embedding/summary provider, so it works without API credentials. JSON includes the path, language, error code and message, file/function scope, recovered qualified name when available, line/column ranges, available source text, and Git/working-tree provenance. If Tree-sitter cannot identify a function, the unparsed region is still retained as a file diagnostic.

Every invocation warns on stderr while saved errors remain, including cached runs and cross-search target indexes. Suppress this warning with `--ignore-errors`:

```bash
slopdex cross-search --ignore-errors
slopdex index-errors --format summary --ignore-errors
```

Silencing warnings does not delete diagnostics. `status` reports `indexingErrorCount` and `failedFileCount`; `functionCount` counts searchable callables. Normal updates retry failed files, including unchanged Git blobs. Errors clear when a file is successfully indexed, deleted, or excluded. Existing indexes migrate automatically and rescan once on their next full update to detect previously unreported parse failures. Diagnostics are committed atomically with the corresponding file update.

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

Library callers can set `sourceFilter.nameRegex` on cross-search/cohesion options for source-only filtering, together with `path` and the Git filter type. A `changed-since` filter also accepts `uncommitted: true`. Query searches accept `nameRegex` in `SimilaritySearchOptions` to filter result symbols. Cohesion reports record source restrictions in `parameters.sourceFilter` and label filtered analyses as `selected-sources`.

Use `index.indexErrors()` to inspect diagnostics through an open index, or the exported `readIndexErrors(indexPath)` to inspect a saved database without a provider. Error records use the exported `IndexingError` type.

For summary search, call `await index.useSummaries()` after updating the index, then `await index.searchSummary({ query: "maintain the repository index" })`. Optionally pass `summaryProvider: new OpenAISummaryProvider({ model: "gpt-5.6-sol" })` when opening an index. Custom providers implement the exported `SummaryProvider` interface. The package also exports standalone `useSummaries` and `searchSummary` helpers.

## Development

```bash
npm run check
```

To cross-check the shared-language fixtures against a built sibling checkout of `treesitter-index`, run:

```bash
npm run check:parser-parity
# Or provide another reference executable:
npm run check:parser-parity -- /path/to/treesitter-index
```

The default reference is `../treesitter-index/target/debug/treesitter-index`. The check covers eight shared languages; that project has no C grammar. Callable regression tests also run as part of `npm run check` without requiring the sibling checkout.

The projects index different information: `treesitter-index` includes declarations, types, imports, and `.pyi` stubs, while Slopdex indexes callable implementations (including nested callables and bound closures). Slopdex also supports `.pyw` and C. Its native Node grammar versions are pinned for compatibility with `tree-sitter@0.21`; the reference uses newer Python and Rust grammars. In particular, Rust `unsafe extern` blocks and async closures currently produce syntax-recovery warnings in Slopdex, so those cases are not covered by the passing parity fixtures.

After upgrading parser behavior, explicitly reparse previously indexed files with `slopdex update-files <path...>` to refresh their symbols, signatures, and embeddings even when the file contents are unchanged.

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
