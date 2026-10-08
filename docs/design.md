# Architecture and design review

Slopdex is a native Rust CLI for mapping repository structure, searching indexed
code and documents, comparing similar callables, and explaining search results.
The workspace SQLite database is the source of truth; vector indexes and shared
provider caches accelerate work but do not define the live repository snapshot.

```text
                          user / automation
                                 |
                                 v
                  +-------------------------------+
                  | CLI + UI (cli.rs, ui.rs)       |
                  | config, commands, presentation|
                  +---------------+---------------+
                                  |
                                  v
                  +-------------------------------+
                  | Engine (engine.rs)            |
                  | refresh, map, search, compare, |
                  | describe; index locking       |
                  +---+-----------+-----------+---+
                       |           |           |
                       v           |           | semantic preparation / queries
               +---------------+   |           v
               | working tree +|   |   +-------------------------+
               | Git metadata  |   |   | artifacts (cache.rs)    |<--> optional S3
               +-------+-------+   |   | per-user SQLite         |
                       |           |   +------------+------------+
                       v           |                | cache miss
               +---------------+   |                v
               | parse/        |   |   +-------------------------+
               | symbols, chunks|   |   | provider traits/adapters|<--> HTTP APIs
               +-------+-------+   |   | embeddings, LLM, rerank |
                       |           |   +------------+------------+
                       |           |                | results via Engine
                       v           v                v
                  +-------------------------------------------+
                  | workspace SQLite (storage.rs)             |
                  | sources, structure, search units, vectors,|
                  | diagnostics, artifacts, result cache     |
                  +------------------+------------------------+
                                     | authoritative vectors
                                     v
                  +-------------------------------------------+
                  | USearch sidecars (vectors.rs)              |
                  | filtered approximate nearest neighbors   |
                  +------------------+------------------------+
                                     |
                                     v
                   Engine reads SQLite / USearch -> CLI output
```

## How the pieces fit

1. `cli.rs` resolves root/config/index paths and dispatches commands to `Engine`;
   `ui.rs` reports progress separately from result output. Search variants and
   cross-search require an existing source index (`slopdex update` creates it).
   Cross-search checks target existence before opening or refreshing the source.
   Existing indexes normally refresh before running. Read-oriented commands try
   a shared lock and reopen with an exclusive lock if refresh or a query needs a write.
2. Refresh walks eligible working-tree files; `git.rs` supplies HEAD and dirty-file
   provenance, not the source text. `parse/` extracts canonical declarations,
   callable and document/Markdown search units, attached source descriptions, and diagnostics. `storage.rs`
   transactionally reconciles those records with stable unit IDs. `map` reads this
   structure without requiring network providers or vector sidecars. When no
   index exists, map parses directly with the same discovery rules, selectors,
   call expansion, and rendering, creating no index/cache artifacts even with
   `--no-reindex`.
3. Semantic refresh uses `models.rs` traits and `providers/` adapters to generate
   missing embeddings, including available source or generated descriptions.
   Description text is optional; indexing it is always enabled. Source descriptions
   take precedence. Only explicit `generate descriptions` invokes indexed generation,
   excluding source-described files/callables and Markdown files. Completed work is
   cached before live publication: workspace SQLite first, then a reusable
   per-user cache and optional best-effort S3. Provider profiles select vector
   projections independently of parsed structure. Generated-artifact reuse includes
   source hashes, generation profile/settings, and system instruction.
4. Search uses independent code, document, and description indexes, plus a lazy
   symbol-name/heading-title index. Plain search includes all of them; hits for the
   same callable from code and descriptions merge by maximum similarity. File
   descriptions remain separate hits. `filter.rs` selects paths and declarations;
   `-q` matches names, available descriptions, and heading titles. USearch retrieves
   approximate neighbors, with optional query reranking. `cross-search` retrieves
   and ranks indexed callables by code, reporting optional description scores.
   `describe` sends search context to an LLM. `map.rs`
   renders declaration excerpts, and `callgraph.rs` expands conservatively
   resolved caller/callee context from saved structure.
