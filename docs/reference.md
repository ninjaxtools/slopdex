# Command reference

This reference describes the native Rust CLI and engine. **The installed
executable's `slopdex --help` and `slopdex <command> --help` govern accepted
commands, options, defaults, and argument combinations.** The workflows in the
[README](../README.md) use this CLI.

## Installation and command surface

`npm install -g @ninjaxtools/slopdex` installs a small Node launcher and downloads
the matching native Rust executable from its GitHub Release. Release binaries
cover Linux x64/ARM64, macOS Intel/Apple Silicon, and Windows x64. cargo-dist
generates the npm wrapper and installer; the release workflow publishes its
generated tarball through npm trusted publishing.

To install from a checkout, use `cargo install --path . --locked` at the repository
root. Source builds require Rust/Cargo, a C/C++ compiler, and platform build tools;
the native application runs without Node. For development, run:

```bash
cargo run --locked -- search "keep the repository index synchronized"
cargo verify
```

`cargo verify` runs workspace checks and release CLI smoke checks.
`cargo release-check` also checks the generated release workflow and release plan.
The retained `npm/package.json` is release metadata for verification against the
cargo-dist-generated package; cargo-dist adds the binary/launcher entries and
installer files. Root `npm install` is no longer a local entry point. See
[implementation and distribution](implementation.md) for tooling and release setup.

Usage: `slopdex [global options] <command> [arguments] [options]`. Global options
can also follow the command. Quote multiword queries.

| Command | Actual operation | Default output |
| --- | --- | --- |
| `search <query>` | Search callable code, complete enabled descriptions, and Markdown together. | Summary |
| `search-code <query>` | Search callable code. | Summary |
| `search-descriptions <query>` | Search callable/file description fusion; descriptions must be enabled and complete. Alias: `search-description`. | Summary with callable descriptions |
| `search-md <query>` | Search heading-aware `.md`/`.markdown` chunks. | Summary with chunk text |
| `describe <query>` | Search, then ask the configured description model to explain the existing code/docs relevant to the query. | Explanation text |
| `cross-search` | Find similar callable neighbors in this index or a second repository. | Clusters; summary with `--cohesion` |
| `status` | Refresh and report counts, generation, checkpoint, profiles, and backends. | JSON object |
| `index-errors` | Refresh and report saved read/parse/extraction diagnostics. | Summary |
| `update-git` | Explicitly refresh the current working tree and Git HEAD, when available. Alias: `refresh`; `--target` accepts only `HEAD`. | JSON refresh statistics |
| `descriptions <enable\|disable>` | Apply description state to the index and save it in config; retain reusable caches. | JSON status |
| `reindex-files [--callables]` | Regenerate stale file descriptions from indexed source; optionally regenerate their callable descriptions too. Requires enabled descriptions. | JSON statistics |
| `models [opencode\|opencode-go]` | Fetch one or both live public OpenCode catalogs without opening an index or requiring credentials. | Qualified `provider/model` lines |
| `config [action]` | Edit root-selected configuration without opening an index; no action starts interactive setup. | Updated-setting summary |

Configuration actions:

```bash
slopdex config
slopdex models opencode-go
slopdex config model opencode-go/gpt-5.6-luna
slopdex config fallback-model opencode-go/muse-spark-1.3-contributor
slopdex config descriptions enable
slopdex config reranker cohere
slopdex config reranker jina
slopdex config reranker openai --reranker-candidates 10
slopdex config reranker disable
slopdex config parallelism 10
```

`config model` and `config fallback-model` validate against published OpenCode
catalogs. Bare IDs must resolve unambiguously; the fallback must use the configured
description provider. `config model` changes description settings, whereas the
global `--model` selects the embedding model. `config reranker <provider> [model]`
accepts an optional model. `config descriptions` defers index work until the next
index command. Interactive configuration requires terminal stdin/stderr; it asks
about descriptions, reranking, embeddings, paths, filters, and common settings.
Use arrow keys and Enter to select providers and models; typing in an OpenCode
model menu filters the published catalog. Saved values are preselected, numeric
inputs validate inline, and Esc/Ctrl-C cancels without saving partial changes.
The wizard and its saved-settings summary render on stderr, so `--format json`
can write the resulting configuration to redirected stdout.
Advanced endpoint/HTTP settings are edited in JSON.

Help, version, configuration, and model-catalog commands do not refresh the index.
Other commands open it and normally refresh before doing their work.

On terminal stderr, cliclack displays progress for catalog loading, opening and
refreshing indexes, searches, explanations, and description regeneration.
Replaceable status text waits until its task or message has lasted at least
200 ms; faster tasks produce only permanent notices and completion summaries.
Known-size tasks show completed/target counts and, when measured progress
predicts more than one second of work, a 0–100% bar on its own line. With no
completed items to estimate from, the bar waits until one second has elapsed.
These tasks include files indexed,
embeddings and descriptions generated, vector snapshots processed, indexes
searched, candidates rescored/reranked, and source functions compared. Embedding
counts measure individual inputs, including partial batches. Parent progress
stays visible during nested work; completed stages retain their final counts.
Unknown-size operations, such as repository discovery or a model's text response,
use a spinner. Provider notices appear above the progress display. Warnings and
runtime errors use the same terminal styling. Redirected stderr and `TERM=dumb`
use plain diagnostics without animations; result data on stdout retains its
summary/JSON/JSONL format.

## Search and analysis options

Common query/cross-search filters:

- `--threshold <number|min-max>`: default `0.8` for cross-search and `0.3` for
  query commands (including `describe`); finite endpoints in `[-1,1]`,
  inclusive minimum and exclusive maximum. A range requires minimum < maximum.
- `--limit <positive integer>`: default unlimited. Caps query results (including
  Markdown), or the context matches for `describe`. For cross-search it caps
  emitted clusters or matched-source rows **after all selected sources are
  searched**.
- `-e`, `--regexp`, or `--regex`: case-sensitive **Rust regex** over qualified
  callable names. Applied before query result limits; Markdown is unaffected.
  For cross-search it filters sources only. Look-around and backreferences are
  unsupported by this regex engine.

`search` accepts `--code`, `--descriptions`, and `--md`. With no selector, it
searches all available kinds; any selector makes selection explicit. Explicit
description search requires complete enabled descriptions.

Cross-search options:

| Option | Behavior |
| --- | --- |
| `--matches <number>` | Maximum neighbors retrieved per source, default `5`. Symmetric-pair suppression can reduce the emitted count. |
| `--min-lines <number>` | Minimum line count for both sources and candidates, default `2`; use `1` for one-line callables. |
| `--source-path <path>` | Source file or recursive directory, root-relative or absolute within the root. |
| `--changed-since <commit>` | Source callables differing from this ancestor of the indexed Git checkpoint; details below. |
| `--uncommitted` | Source callables whose indexed file has working-tree provenance. |
| `--cross-file-only` | Exclude candidates with the same root-qualified file path as the source. |
| `--include-symmetric-duplicates` | Keep both directions of same-index pairs. By default each unordered observed pair is emitted once. |
| `--cohesion` | Sort each source's selected matches by descending filesystem distance, then similarity. Defaults to summary; incompatible with clusters. |
| `--target-root <path> --target-index <path>` | Compare to another index; both are required together. Embedding profiles must match. |
| `--target-config <path>` | Config for the second root; defaults to `<target-root>/.slopdex/config.json`. Requires both target options. |

Examples:

```bash
slopdex search "keep the repository index synchronized"
slopdex search-code "configure the embedding provider"
slopdex search-md "configure the embedding provider"
slopdex descriptions enable --description-provider opencode-go
slopdex search-descriptions "keep the repository index synchronized"
slopdex describe "I want to implement a new rpc endpoint"
slopdex reindex-files --callables
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.85-0.9
slopdex cross-search --uncommitted --cross-file-only --threshold 0.9
slopdex cross-search --changed-since origin/main --threshold 0.9
slopdex cross-search --source-path src/services -e '^UserService\.' --threshold 0.9
slopdex cross-search --cross-file-only --cohesion --threshold 0.8
slopdex cross-search --target-root /path/to/other/repo \
  --target-index /path/to/other/repo/.slopdex/index.sqlite --threshold 0.9
```

### Git source selection

Refresh reads the **current filesystem**, including staged, unstaged, and eligible
untracked files. Git supplies HEAD as the checkpoint and dirty-file provenance;
there is no separate committed-tree overlay or arbitrary-ref indexing mode.
Files reported by `git diff HEAD` or untracked-file discovery are marked
`sourceMode: "working-tree"`; clean files are marked `"git"`. Without a resolvable
HEAD, every indexed file uses working-tree provenance.

`--uncommitted` selects **all callables in those working-tree files**, including
unchanged siblings. `--changed-since` (engine option `changedSince`) resolves the
reference to a commit and requires it to be an ancestor of the saved checkpoint.
It parses each current callable's path at that commit and compares the pair
`(qualifiedName, sourceHash)`. New paths, renamed symbols, and edited callable
source are selected; line-number-only shifts are not. Missing/unreadable base
paths count as having no matching symbols. Removed callables cannot be sources
because they are absent from the current index. Combined source filters intersect.
With `--no-reindex`, these filters use saved provenance/content; changed-since
still needs the local Git history.

### Scores, fusion, and ANN recall

SQLite stores the authoritative embeddings; USearch performs filtered F32 cosine
HNSW search. Code and Markdown have separate indexes. Complete descriptions add
two callable indexes: description fusion and code/description fusion.

Each component vector is normalized independently before concatenation. For unit
vectors `c` (code), `d` (callable description), and `f` (file description):

```text
cos([c₁, d₁, f₁], [c₂, d₂, f₂])
  = (cos(c₁, c₂) + cos(d₁, d₂) + cos(f₁, f₂)) / 3
```

This is **exact averaged-cosine fusion mathematically**, subject to F32 rounding,
rather than merging independent top-k lists. Query searches repeat the unit query
vector in each component slot. `search-code` uses code alone;
`search-descriptions` averages callable and file description similarities;
`search` with code and descriptions averages all three. Markdown keeps its own
chunk score and joins the final ranked result list.

Descriptions are complete only when every indexed callable has both callable and
file-description embeddings and descriptions are enabled. Cross-search uses the
three-way average only when both indexes are complete and their configured
description profiles match; otherwise the entire comparison uses code alone.
JSON exposes the applicable `codeSimilarity`, `descriptionSimilarity`, and
`fileDescriptionSimilarity`; cross-search rows include scoring mode and weights.

**Neighbor retrieval remains approximate.** Filters run inside USearch graph
traversal, not on an unfiltered top-k list. Threshold ranges can trigger wider
retrieval, but neither exact fusion nor an unlimited output limit guarantees
exhaustive recall or exact top-k membership. There is no exhaustive scan fallback.
Similarity is model-dependent, not a probability of duplication.

### Reranking, clusters, and output

Reranking applies to query commands, including the search inside `describe`, not
cross-search. Embedding thresholds are applied first. Cohere/Jina receive up to
five times an explicit result limit, or all retrieved threshold-passing candidates
without a limit. OpenAI receives up to
`min(100, max(limit or 100, rerankerCandidates))`; the configured candidate count
defaults to `10`. Without an explicit limit the OpenAI retrieval cap is **100**.
Query JSON retains `similarity` and adds `rerankScore`.

Clusters are connected components of observed callable matches, sorted by member
count and then name. The displayed similarity range covers observed links;
members need not all match one another directly. Cross-search compares callables,
not Markdown chunks. Sources with no surviving matches are omitted.

Cross-search defaults to `--threshold 0.8` to avoid joining otherwise distinct
groups through weak matches. Lower thresholds can produce one large transitive
cluster; use `--threshold 0.3` to restore the pre-rewrite CLI's cutoff, or `0.9`
for more selective duplicate detection. Explicit ranges retain their inclusive
minimum and exclusive maximum.

Cohesion changes the order of each source's selected semantic matches. Distance
is `0` in one file, `1` between files in one directory, and `1` plus directory-tree
hops otherwise. It does not alter similarity or the neighbor selection score.

`--format json` produces query/diagnostic/model arrays, configuration/status
objects, and **JSONL for cross-search** (one object per matched source).
`--format clusters` is valid only for cross-search without cohesion. Results go
to stdout and warnings/errors to stderr. Clap argument errors exit with `2`;
runtime/configuration/domain failures exit with `1`; success/help/version exit
with `0`.

### Task explanations

`describe` sends its query, ranked search matches (including callable source and
Markdown content), and per-file descriptions to the configured description
provider. Any match strictly above `--describe-full-file-threshold` (default `0.8`)
also includes that file's complete **indexed** source from SQLite. The instruction
asks for an explanation of existing code/docs with paths and symbols, not an
implementation proposal.

The engine does not reread live files for this context or retry with whole-file
content removed. Provider retries/failover still apply. JSON output contains
`query`, `description`, `files`, and `functions`; full source and embedding input
are stripped from returned function metadata. Reranker order informs the prompt,
but `rerankScore` is not copied to the returned `functions` array. The generated
task explanation itself is not cached.

## Index lifecycle and persistence

### SQLite authority and USearch sidecars

The default database is `<root>/.slopdex/index.sqlite`. SQLite is authoritative
for file snapshots, callable/chunk records, provenance, diagnostics, descriptions,
document/query vectors, reusable artifacts, metadata, and cached search results.
Native schema **2** has six tables: `metadata`, `files`, `items`, `embeddings`,
`cache`, and `search_cache`, plus the `items_path` index on `items(path)`.
Parse results and successful description/embedding artifacts are committed before
the final live-record transaction, so interrupted indexing can reuse completed
work. Live changes update the generation and invalidate cached search results.

Persistent derived indexes sit beside the database:
`<index>.code.usearch`, `<index>.markdown.usearch`, and, when descriptions are
complete, `<index>.descriptions.usearch` and `<index>.combined.usearch`. Each has
a `.manifest.json` sidecar. Valid caches are reconciled incrementally by stable
item ID and vector hash; unchanged vectors retain their graph entries. Missing,
corrupt, or incompatible sidecars are rebuilt from SQLite without model calls.
F32 vectors are loaded into owned memory, not memory-mapped.

An exclusive `<index>.lock` is held for the engine's lifetime. A competing command
fails promptly with an index-in-use error; retry after it finishes. SQLite uses
WAL and a 30-second busy timeout. See [architecture](implementation.md#architecture-and-code-map)
for transaction and sidecar publication details.

### Descriptions

Descriptions are disabled initially. `descriptions enable` generates missing
file/callable descriptions and saves the enabled setting; disabling stops their
automatic generation and use in scoring while preserving reusable artifacts.
Unchanged callable descriptions are reused. Ordinary edits refresh changed
callables but retain existing file descriptions, which can become stale.
`reindex-files` refreshes stale file descriptions; `--callables` also regenerates
callable descriptions in those files. Matching cached regeneration artifacts can
still be reused. Merely changing the configured description model does not
regenerate all existing descriptions.

File descriptions use the complete file source. Each callable request is a
separate request containing its source, symbol, path, and file-description
context. There is no continuing per-file chat conversation. `status` reports
enabled state, profiles, description counts, and stale-file-description count.

### Native offline reuse and recovery

`--no-reindex` (engine config `noReindex`) skips refresh completely, with or
without Git, even for an empty index. It supports offline inspection and
cross-search of an existing native index; missing derived USearch files can still
be reconstructed locally. It does not make the database read-only or disable all
network operations: uncached query vectors/reranking, `describe`, and explicit
description regeneration still require their providers. Cached queries can run
offline when the matching artifacts/results exist. Catalog commands still fetch
their catalogs. Use the CLI flag: the CLI overwrites a JSON `noReindex` value with
the flag's value on every invocation.

Native index identity includes canonical root, native schema, and embedding
profile (provider/model/dimensions/strategy). Within the current table layout,
identity changes such as a different root or embedding profile require
`--force-reindex --yes-really-rebuild-the-index`. This clears native live records,
metadata, and search results while retaining artifact/vector caches. A compatible
index follows normal refresh even with that flag. It conflicts with
`--no-reindex`. `--rebuild-on-divergence` is accepted with the same confirmation,
but the current engine refreshes the working tree without a divergence gate;
the flag adds no engine behavior.

### Rebuilding an old index

Native schema **2** is a clean break from the old `rust_`-prefixed native tables
and TypeScript database layouts, including schema 11. Those databases are
rejected with instructions to remove the existing SQLite index and rebuild, or
choose a new index path. There is no legacy import or migration, and neither
`--force-reindex` nor `--no-reindex` bypasses old-layout rejection.

For the default database, stop any Slopdex process using the index, then run from
the repository root:

```bash
rm -f .slopdex/index.sqlite .slopdex/index.sqlite-wal .slopdex/index.sqlite-shm
slopdex update-git
```

For a custom database, remove that SQLite file and its `-wal`/`-shm` companions,
then run `slopdex --index /path/to/index.sqlite update-git`. To rebuild at a new,
unused path instead:

```bash
slopdex --index /path/to/new-index.sqlite update-git
```

Use that same `--index` path on subsequent commands, or save it as `indexPath` in
config. Rebuilding scans current files and regenerates embeddings and enabled
descriptions using the configured providers; old database artifacts and saved
description settings are not imported. Derived USearch sidecars are reconciled
or rebuilt from the new SQLite snapshot.

### Coverage and failures

Supported extensions: TS/TSX (`ts`, `mts`, `cts`, `tsx`), JS/JSX (`js`, `mjs`,
`cjs`, `jsx`), Python (`py`, `pyw`), Rust, Go, Java, C (`c`, `h`), and Markdown
(`md`, `markdown`). Only recognized callables are code-search entries; Markdown
uses bounded heading-aware chunks.

The ignore walker uses current root/nested `.gitignore`, Git exclude/global
rules, and ignore-file rules, including without a Git repository. Config
`include`/`exclude` globs further narrow repository-relative paths. Built-in
excluded directories are `.git`, `.slopdex`, `node_modules`, `dist`, `build`,
`coverage`, `vendor`, `generated`, `.venv`, `venv`, `__pycache__`, `.tox`,
`.mypy_cache`, `.pytest_cache`, and `target`. Inclusion globs cannot reopen these
pruned directories. Refresh removes deleted/newly excluded entries.

Read/UTF-8/size failures and parser diagnostics are saved; healthy callable
siblings remain searchable where parsing permits. `maxFileSize` defaults to
1 MiB. `index-errors` reports path, message, source mode, and available line
locations. `status` reports `indexingErrorCount` and `failedFileCount`.
Index-using commands warn about saved errors, including with `--no-reindex` and
for separate cross-search targets. `--ignore-errors` silences warnings without
clearing records. Help/version do not inspect diagnostics.

Provider failures abort refresh before live publication; completed artifacts
remain reusable. Refresh checks that HEAD and prepared file hashes have not
changed before committing. Rerun after edits settle or provider problems are
resolved. This check is not an atomic filesystem snapshot.

## Root configuration and provider overrides

The root defaults to the current directory; select another with `--root`. There
is no automatic climb to a Git/project root. The default config is
`<root>/.slopdex/config.json`, a JSON object; a missing file means defaults.
`--config` selects another file. `--index` overrides `indexPath`, otherwise the
index defaults to `<root>/.slopdex/index.sqlite`. Explicit relative config/index
paths and relative JSON `indexPath` resolve from **the process working directory**,
not the root or config's directory.

Command-line provider/model/dimension settings override JSON. Target repositories
load their own root-selected config, then receive the same global overrides.
When neither description provider nor model is specified, the index's saved
description profile supplies both. `descriptionsEnabled` in JSON overrides the
saved index state; omission retains it. Config writes preserve unknown properties
and replace the JSON file through a synced temporary file.

Example `.slopdex/config.json`:

```json
{
  "provider": "jina",
  "model": "jina-embeddings-v4",
  "dimensions": 1024,
  "descriptionsEnabled": true,
  "descriptionProvider": "opencode-go",
  "descriptionModel": "gpt-5.6-luna",
  "descriptionFallbackModel": "muse-spark-1.3-contributor",
  "rerankingEnabled": true,
  "rerankerProvider": "openai",
  "rerankerModel": "gpt-5.6-luna",
  "rerankerCandidates": 10,
  "exclude": ["**/fixtures/**"]
}
```

| JSON settings | Native behavior/default |
| --- | --- |
| `provider`, `model`, `dimensions` | OpenAI / `text-embedding-3-large` / `3072`; Jina defaults to `jina-embeddings-v4` / `1024`. OpenAI small/ada models default to `1536`. CLI: `--provider`, `--model`, `--dimensions`. |
| `descriptionProvider`, `descriptionModel` | OpenAI / `gpt-5.6-luna`; OpenCode Zen (`opencode`) and Go (`opencode-go`) default to `muse-spark-1.3-contributor`. Corresponding `--description-*` options override them. |
| `descriptionFallbackModel` | Optional same-provider fallback; CLI `--description-fallback-model`. Successful fallback stays active within that provider instance until it fails. |
| `descriptionsEnabled` | Explicit enabled state, otherwise saved state/default false. |
| `rerankingEnabled`, `rerankerProvider`, `rerankerModel` | Disabled by default; models default to Cohere `rerank-v4.0-pro`, Jina `jina-reranker-v3.5`, OpenAI `gpt-5.6-luna`. |
| `rerankerCandidates` | OpenAI candidate setting, integer `1..100`, default `10`; CLI `--reranker-candidates`. See retrieval formula above. |
| `indexPath`, `include`, `exclude`, `maxFileSize` | Index path, glob arrays, and positive byte limit as described above. |
| `embeddingBatchSize` | Positive batch cap. Defaults/maxima: OpenAI `32`, Jina `64`; larger configured values are capped. Interactive setup defaults to `32`. |
| `embeddingBaseUrl`, `descriptionBaseUrl`, `rerankerBaseUrl` | Operation-specific HTTP(S) endpoint overrides; JSON only. Include the API version, e.g. `http://localhost:8080/v1`. |
| `providerTimeoutMs` | Positive request timeout, default `60000`, capped at `300000`; connect timeout is 10 seconds. |
| `providerMaxRetries` | Integer `0..5`, default `2`, for ordinary retryable HTTP failures. |
| `retryDelayMs` | Nonnegative exponential-backoff base, default `250`; delays and numeric `Retry-After` are capped at 5 seconds. |
| `parallelism` | Defaults to `10`; bounds concurrent embedding batches and callable descriptions within each file (also `config parallelism`). File descriptions run first, files are processed serially, and each successful HTTP result is cached immediately. |
| `verbose` | External model-call notices go to stderr once per kind/provider/model per process by default; `true` (also `--verbose`) reports every outgoing attempt, including retries. Kinds are `vectors`, `descriptions`, and `reranking`; notices identify the actual model, including fallback, without credentials, URLs, or input. Construction and cache hits produce no model-call notices. |

Aliases `embeddingProvider`, `embeddingModel`, `embeddingDimensions`, and
`fallbackModel` normalize to their canonical properties; canonical values win.
Embedding profiles include `strategyVersion: "rust-v1"`. Description profiles
identify the configured primary with `strategyVersion: "callable-purpose-v2"`,
even while fallback serves requests. Base URLs and credentials are not part of
these profiles; changing an endpoint alone does not invalidate cached vectors.

Embedding URLs may already end in `/embeddings`; Cohere/Jina reranking URLs may
end in `/rerank`. Description and OpenAI reranking bases receive the required
protocol path. OpenAI descriptions use Responses; OpenCode protocol routing is
model-dependent (Responses, Chat Completions, Messages, or Gemini). Overrides
must serve the selected protocol. Live `models`/config catalog validation uses
the public OpenCode endpoints, independently of these overrides.

Credentials are resolved only when needed, in this order:

1. Operation key: `embeddingApiKey`, `descriptionApiKey`, or `rerankerApiKey`.
2. Provider key: `openaiApiKey`, `jinaApiKey`, `cohereApiKey`, or `opencodeApiKey`.
3. Environment: `OPENAI_API_KEY`, `JINA_API_KEY`, `COHERE_API_KEY`, or
   `OPENCODE_API_KEY`.
4. For OpenCode, the matching `opencode`/`opencode-go` entry's `key` in
   `$XDG_DATA_HOME/opencode/auth.json`, or `~/.local/share/opencode/auth.json`.

Prefer environment/stored credentials to committing keys in root config.
Description failover and empty-output recovery allow at most six total attempts,
without nested HTTP retries; invalid shared credentials (401) and redirects stop
failover. See [implementation](implementation.md) for retry details and developer
checks.
