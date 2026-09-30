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
   `ui.rs` reports progress separately from result output. Index-using commands
   normally refresh before running. Read-oriented commands try a shared lock and
   reopen with an exclusive lock if refresh or a query needs a write.
2. Refresh walks eligible working-tree files; `git.rs` supplies HEAD and dirty-file
   provenance, not the source text. `parse/` extracts canonical declarations,
   callable and document/Markdown search units, and diagnostics. `storage.rs`
   transactionally reconciles those records with stable unit IDs. `map` reads this
   structure without requiring network providers or vector sidecars.
3. Semantic refresh uses `models.rs` traits and `providers/` adapters to generate
   missing embeddings and optional file/callable descriptions. Completed work is
   cached before live publication: workspace SQLite first, then a reusable
   per-user cache and optional best-effort S3. Provider profiles select vector
   projections independently of parsed structure.
4. Search loads SQLite vectors into disposable USearch indexes for code and
   document chunks, plus description/combined indexes when descriptions are
   complete. `filter.rs` selects eligible paths and names; USearch retrieves
   approximate neighbors, with optional query reranking. `cross-search` compares
   indexed callables, while `describe` sends search context to an LLM. `map.rs`
   renders declaration excerpts, and `callgraph.rs` expands conservatively
   resolved caller/callee context from saved structure.

## Design assessment

- **Good separation of authority and accelerators.** SQLite transactions publish
  live snapshots; sidecars can be rebuilt from stored vectors, and paid model
  artifacts survive failed refreshes. The structure-only path remains usable
  without credentials.
- **The engine is the coordination hotspot.** `engine.rs` owns scanning, semantic
  preparation, vector loading, and query behavior. This keeps snapshot decisions
  in one place, but changes to refresh or scoring need careful end-to-end review.
- **Explicit tradeoffs.** Filesystem/HEAD validation is optimistic rather than an
  atomic snapshot; HNSW retrieval is approximate even with exact cosine fusion.
  Description artifact keys favor reuse across renames and model changes, so a
  model-setting change alone does not guarantee regenerated prose. Shared remote
  artifacts can contain source-bearing prompts, so bucket access matters.

See the [command reference](reference.md) for behavior and configuration, and
[implementation and distribution](implementation.md) for schema, cache, and
release details.
