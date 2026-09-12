# Implementation and library API

For installation, command examples, configuration, and result interpretation, see the [operator README](../README.md). This document covers the implementation and programmatic interface.

## Code map

| Module | Responsibility |
| --- | --- |
| `src/cli.ts` | Argument parsing, configuration, automatic refresh/recovery, diagnostics, and output selection. |
| `src/code-index.ts` | Index lifecycle, file preparation, Git/working-tree reconciliation, embedding/description caching, and search facade. |
| `src/parser/` | Language dispatch, native Tree-sitter extraction, callable identity, and recoverable diagnostics. |
| `src/source-policy.ts`, `src/gitignore.ts` | Supported paths, built-in/config exclusions, nested ignore rules. |
| `src/git/repository.ts` | Git commits, trees, blobs, diffs, ancestry, and working-tree changes. |
| `src/embeddings/`, `src/descriptions/`, `src/rerankers/` | Provider requests and provider profiles. |
| `src/storage/database.ts` | SQLite schema, durable artifact caches, transactions, metadata, and vector queries. |
| `src/search/` | Analysis scoring selection and cross-index neighbor discovery. |
| `src/analysis/cohesion.ts` | Physical distance, gap scores, aggregate affinity, and groups. |
| `src/format.ts` | Human-readable search and cluster output, plus library cohesion-report formatting. |
| `src/index.ts`, `src/types.ts` | Public exports and data contracts. |

## Callable extraction

Language selection uses file extensions. Native Tree-sitter grammars ship as dependencies; extraction does not require a language server, type checker, or project compiler configuration.

| Language | Extracted callables |
| --- | --- |
| Python | Functions, async functions, class methods, constructors, generators, and bound lambdas; decorators are included in source. |
| JavaScript / JSX | Functions, generators, methods, constructors, named function expressions/arrows, and components returning JSX. |
| TypeScript / TSX | Typed functions, methods, constructors, named function expressions/arrows, and generic JSX components. |
| Rust | Functions, `impl` methods/associated functions, trait default methods, and `let`-bound closures. |
| Go | Functions, receiver methods, and function literals bound to variables or assignments. |
| Java | Methods, constructors (including compact record constructors), and variable-bound lambdas. |
| C | Function definitions, including static/inline functions and functions returning pointers. |

Qualified names include enclosing classes, functions, and explicit modules. Go methods include receiver types (`Store[T].Get`); Rust trait implementations include type and trait (`<Store<T> as Read>.read`). Records retain source, signature, line/column locations, source hash, and identity used during incremental reconciliation.

Bodyless declarations and anonymous callbacks are omitted. Extraction is syntactic: Rust and C macros are not expanded, C preprocessor branches are indexed as written, and `.h` files use the C grammar. Recoverable callables survive syntax errors; malformed regions produce file or function diagnostics.

Source policy combines extension detection, built-in excluded path segments, and Node glob matching for configured includes/excludes. The `ignore` package implements root and nested `.gitignore` semantics, including anchoring, escaping, negation, and excluded-parent behavior. Working-tree updates read current rules; committed-only updates read rules from Git blobs. Rule changes are checked before applying prepared updates.

## Index lifecycle and storage

The CLI reconciles an index before most commands. Library callers choose when to update explicitly.

Git updates resolve the target commit, validate ancestry against the checkpoint, reconcile committed blobs, and optionally overlay current working-tree files when the target equals HEAD. Overlays account for staged, unstaged, untracked, renamed, and deleted paths. The saved checkpoint remains the committed base. Historical/other-branch targets are committed-only. Explicit `updateFiles` operations do not advance the checkpoint.

Tree-sitter results and each valid provider result are committed to content-addressed cache tables immediately. Generation checks detect concurrent logical index changes; working-tree updates also verify source and ignore-rule stability. A separate SQLite transaction atomically applies related file, function, diagnostic, description-reference, and checkpoint changes. Failed or aborted initialization retains the valid database and its cache rows so the next invocation resumes without repeating completed work.

Storage uses Node's `node:sqlite` and `sqlite-vec`. Writable connections enable WAL, foreign keys, and full synchronization. The schema contains:

- `metadata`: repository root, embedding and description profiles, generation, checkpoint, schema version, and feature/scan state.
- `files`: paths, content hashes, blob IDs, source mode, previous paths, language, and size.
- `functions`: identity, names, signatures, locations, source, provenance, and embedding/description references.
- `embeddings`: code, description, and query vectors keyed by embedding profile, operation, and exact input.
- `function_vectors`: synchronized `vec0` storage for filtered code-vector nearest-neighbor queries.
- `description_cache`: generated description text keyed independently by description profile and complete source context.
- `parse_cache`: successful Tree-sitter extraction results keyed by parser strategy, path, and file-content hash.
- `callable_provenance`: first-seen committed source identity.
- `indexing_errors`: diagnostics associated with files.

Current schema version is `8`. Schemas 6 and 7 are migrated in place by adding file-description state when necessary and building the nearest-neighbor vector table; earlier schemas require `--force-reindex`. Metadata validation rejects incompatible roots, embedding profiles, and unsupported schemas. Forced rebuilds clear logical index state while retaining content-addressed caches when possible; incompatible older databases are recreated. Enabled OpenAI description settings are preserved for the same repository where possible. Git divergence reconciliation is a separate operation controlled by `--rebuild-on-divergence`.

### Diagnostics

Diagnostics cover parse errors, parser exceptions, extraction failures, read failures, and file-size limits. Records include scope, code, message, language, path, recoverable qualified name, location, available source, and Git/working-tree provenance. Healthy callables remain searchable.

Diagnostics commit with their corresponding file update. Updates retry failed files even when their Git blobs are unchanged; successful replacement, deletion, and exclusion clear failures. Standalone readers inspect saved diagnostics without constructing an embedding provider. The CLI's exit handler reports remaining failures for source and target indexes; version exits before registering that handler.

## Embeddings and descriptions

Embedding profiles consist of provider, model, dimensions, and strategy version. Cross-index analysis requires matching profiles. Vectors are validated and normalized before storage/search.

- OpenAI defaults to `text-embedding-3-large`, 3072 dimensions, strategy `callable-v2`. Inputs are truncated to 8192 `cl100k_base` tokens.
- Jina defaults to `jina-embeddings-v4`, 1024 dimensions, strategy `callable-v2:code-query-passage`. Requests distinguish `code.passage` documents from `code.query` queries and enable truncation.

Embedding inputs identify language, callable kind, qualified symbol, signature, documentation, and source. For Python, a function's first-statement docstring is included in a separate `documentation` section in addition to remaining part of the callable source.

Purpose generation uses the AI SDK and strategy `callable-purpose-v2`. The default is OpenAI's Responses API with `gpt-5.6-sol`; OpenCode Zen and Go are also selectable, defaulting to `gpt-5.6-sol` and `gpt-5.6-luna`. OpenCode catalog models are routed through their published protocol using the OpenAI Responses, Anthropic Messages, Google Generative AI, or OpenAI-compatible adapter. Generation opens one conversation per file: stable instructions and complete file context form the prefix, the first request describes the file overall, then callable prompts and generated answers are appended sequentially in source order. This avoids repeating the file within a request and gives provider prompt caches an increasingly large reusable prefix. Responses requests use `store: false`. File and callable text are embedded with the configured embedding provider.

Description inputs include contextual and profile information so file-context, path, model, or generation-strategy changes invalidate relevant cached results. Description text identity is independent of the embedding profile, allowing an embedding-model change to reuse generation output while producing the required new vector. Every validated description and vector is cached before indexing continues. If generation resumes partway through a file, completed file/callable prompts and cached answers are replayed locally before the next request so the conversation prefix remains equivalent. Ordinary source updates retain the previous file description and its source hash, making staleness explicit without incurring automatic regeneration; `reindex-files` replaces stale file descriptions, optionally continuing through callable regeneration. `useDescriptions` persists the profile and enabled state; `disableDescriptions` turns automatic updates and description search/scoring off without deleting cached artifacts. Function references are attached only by the final logical transaction, so provider failure cannot expose partially updated callable records. Deleting a function removes it from description search but retains reusable cache rows.

## Similarity and analysis

`search` embeds a query and, when descriptions are complete, scores code, callable-description, and containing-file-description vectors. `search-description` scores callable and file descriptions. Filter-compatible code-only searches up to sqlite-vec's 8,192-dimension limit use the synchronized `vec0` nearest-neighbor table; larger custom profiles plus fused, regex, upper-bound, and multi-path searches retain the exact scalar scoring path. Analysis uses code-only cosine similarity unless all indexed callables and files have enabled description embeddings. Cross-index analysis requires completeness on both sides. Description-generator models may differ across indexes even though embedding profiles must match.

An optional `Reranker` performs a second-stage pass for the two natural-language query methods. After applying name and similarity filters, Cohere and Jina retrieve five times the requested result limit. `OpenAILLMReranker` retrieves its configured candidate count (10 by default), or the result limit when larger. Every reranker receives the query plus candidate path, code metadata/source, and purpose description when available, then returns the requested number in relevance order. Results preserve the embedding/fused `similarity` and add `rerankScore`.

Cohere defaults to `rerank-v4.0-pro` with `COHERE_API_KEY`; Jina defaults to `jina-reranker-v3.5` with `JINA_API_KEY`. The OpenAI LLM path defaults to `gpt-5.6-luna`, sends a strict JSON schema through the Responses API with high reasoning, no reasoning summary, and `store: false`, and validates result cardinality, indexes, uniqueness, and 0-1 scores. Its prompt preserves descriptions before source and caps source-bearing candidate text at 12,000 tokens each and 80,000 tokens in aggregate. Reranking does not participate in index metadata or cross-search because it creates no persisted artifacts and cross-search is callable-to-callable analysis rather than natural-language retrieval.

When descriptions are complete:

```text
similarity = (codeSimilarity + descriptionSimilarity + fileDescriptionSimilarity) / 3
```

All component scores and the average are computed in one SQLite query. Name, line-count, and path exclusions plus similarity bounds apply before ranking/limiting. Range upper bounds are exclusive. Combined JSON includes component scores; analysis metadata records mode, weights, and description profiles. Cohesion JSON uses schema version 3 for the three-component scoring contract.

Cross-search selects sources, queries neighbors per source, and deduplicates unordered same-index pairs unless symmetric results are requested. Self-matches are excluded in same-index queries. Same-file exclusion uses canonical roots and file identity to handle aliases. With the `cohesion` option, each selected match receives its physical path distance and matches are re-ranked by descending distance, then similarity. Cluster formatting builds connected components from emitted matches and sorts by member count, then name; transitive connectivity does not imply all-to-all similarity.

### Cohesion metrics

Cohesion builds a graph from selected-source top neighbors, retaining unique unordered edges. Reciprocity is true when both endpoints selected each other, false when both were evaluated but only one selected the other, and null when an endpoint was not evaluated.

```text
physicalDistance = 0                         # same file
physicalDistance = 1 + directory-tree hops  # different files
semanticWeight = clamp((similarity - threshold) / (1 - threshold), 0, 1)
separationWeight = 1 - exp(-physicalDistance / 2)
cohesionGap = semanticWeight * separationWeight
```

Affinity ratios and mean distance are weighted by `semanticWeight`. Cohesion metrics use all qualifying edges before output limiting. File reports aggregate internal, same-folder, and external affinity for selected-source files. Pairs rank by gap and tie-breakers; groups are connected components of reported pairs. Source/test classification is a path heuristic, not a dependency or call-graph analysis.

## Library API

```ts
import { OpenAIEmbeddingProvider, OpenAILLMReranker, openCodeIndex } from "@ninjaxtools/slopdex";

const index = openCodeIndex({
  rootDir: "/path/to/repository",
  provider: new OpenAIEmbeddingProvider(),
  reranker: new OpenAILLMReranker({ candidateCount: 10 }),
});

try {
  await index.updateFromGit();
  const results = await index.similaritySearch({
    query: "validate an authenticated session",
    limit: 10,
  });
  console.log(results);
} finally {
  index.close();
}
```

Exports include `CodeIndex`, `crossSearch`, `analyzeCohesion`, `cohesionLocation`, embedding/description providers, `CohereReranker`, `JinaReranker`, `OpenAILLMReranker`, error types, and the contracts in `src/types.ts`. Standalone update/search helpers wrap the corresponding index methods.

- Use `updateFromWorkingTree()` when Git is unavailable. Unlike the CLI, the library does not automatically refresh before queries or fall back from Git.
- Set `sourceFilter.nameRegex` for source-only cross-search/cohesion filtering; combine it with `path` and a filter type (`all`, `changed-since`, or `uncommitted`). `changed-since` also accepts `uncommitted: true`.
- Top-level analysis `nameRegex` filters both sources and candidates. Query `SimilaritySearchOptions.nameRegex` filters result names before limiting.
- Call `await index.useDescriptions()`, then `await index.searchDescription({ query: "maintain the repository index" })`. Select a model via `descriptionProvider: new OpenAIDescriptionProvider({ model: "gpt-5.6-sol" })` in index options. Custom description providers implement both stateless file/callable methods and may add `startFile()` for contextual sessions.
- Inspect failures through `index.indexErrors()` or exported `readIndexErrors(indexPath)` without a provider. Records use `IndexingError`.
- `analyzeCohesion` remains a programmatic report API. The CLI exposes physical-distance review through `cross-search --cohesion` instead of a separate command.

## Build and development

```bash
npm install
npm run check
```

`check` runs TypeScript checking, Vitest, the tsup build, and smoke tests. The smoke script verifies built exports, CLI help/version, language parsing, ignore behavior, and saved diagnostics without network calls. `npm run dev -- <arguments>` runs the source CLI through tsx.

The build produces ESM library and CLI files with declarations and source maps in `dist/`. `tsup.config.ts` reads `package.json` and injects `__SLOPDEX_VERSION__`; source-mode version output falls back to reading package metadata. `prepack` runs build and smoke checks.

The repository skill lives at `.agents/skills/slopdex/SKILL.md` and is included in the package. `npm run install:skill:opencode` copies it into the OpenCode skill directory.

### Parser parity

```bash
npm run check:parser-parity
# Or supply another built reference executable:
npm run check:parser-parity -- /path/to/treesitter-index
```

The default reference is `../treesitter-index/target/debug/treesitter-index`. The check covers eight shared languages; the reference has no C grammar. Callable regression tests also run in `npm run check` without a sibling checkout.

The tools index different information: `treesitter-index` includes declarations, types, imports, and `.pyi` stubs; Slopdex extracts callable implementations, nested callables, and bound closures, and also supports `.pyw` and C. Native Node grammar versions are pinned for compatibility with `tree-sitter@0.21`; the reference uses newer Python and Rust grammars. Rust `unsafe extern` blocks and async closures currently produce syntax-recovery warnings in Slopdex and are excluded from passing parity fixtures.

After parser behavior changes, explicitly run `slopdex update-files <path...>` to reparse unchanged files and refresh their symbols, signatures, and embeddings.
