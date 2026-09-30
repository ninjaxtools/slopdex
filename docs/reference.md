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
| `search-descriptions <query>` | Search callable/file description fusion; descriptions must be enabled and complete. Alias: `search-description`. | Ranked declaration excerpts |
| `search-md <query>` | Search heading-aware `.md`/`.markdown` chunks. | Ranked heading paths |
| `describe <query>` | Search, then ask the configured description model to explain the existing code/docs relevant to the query. | Explanation text |
| `cross-search` | Find similar callable neighbors in this index or a second repository. | Clusters; summary with `--cohesion` |
| `map [PATH]...` | Refresh local structure and show code declarations/Markdown headings without providers or vector sidecars. | Summary |
| `status` | Refresh and report counts, generation, checkpoint, profiles, and backends. | JSON object |
| `index-errors` | Refresh and report saved read/parse/extraction diagnostics. | Summary |
| `update` | Explicitly refresh the current working tree and Git HEAD, when available. Alias: `refresh`; `--target` accepts only `HEAD`. | JSON refresh statistics |
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
`map` refreshes only local structure; semantic commands also prepare missing
embeddings and enabled descriptions.

On terminal stderr, cliclack displays progress for catalog loading, opening and
refreshing indexes, searches, explanations, and description regeneration.
Replaceable status text waits until its task or message has lasted at least
200 ms; faster tasks produce only permanent notices and completion summaries.
Known-size tasks show completed/target counts and, when measured progress
predicts more than one second of work, a native cliclack progress bar with an
estimated time remaining. With no completed items to estimate from, the bar
waits until one second has elapsed.
These tasks include files indexed,
embeddings and descriptions generated, vector snapshots processed, indexes
searched, candidates rescored/reranked, and source functions compared. Embedding
counts measure individual inputs, including partial batches. Parent progress
stays visible during nested work; completed stages retain their final counts.
Unknown-size operations, such as repository discovery or a model's text response,
use a spinner. Provider notices appear above the progress display. Warnings and
runtime errors use the same terminal styling. Redirected stderr and `TERM=dumb`
use plain diagnostics without animations; result data on stdout retains its
 text/JSON/JSONL format.

## Structure map

```text
slopdex map [PATH]... [-g GLOB]... [-e REGEXP]... [-i] [-k KIND]... [--private] [--callers N] [--callees N] [--detail compact|standard|expanded] [--format text|json]
```

With no paths, map selects the indexed repository. Paths select files or recursive
directories and are relative to `--root`, including when invoked from another
working directory. Absolute paths within the root are accepted; paths outside it
are rejected. Missing paths produce warnings on stderr and are ignored. Multiple
paths form a union, intersected with the shared selectors.

Map normally refreshes local file snapshots, declarations, headings, search units,
and diagnostics in SQLite. It makes no provider requests, generates no descriptions
or embeddings, and does not open/rebuild USearch sidecars. `map --no-reindex` reads stored
SQLite structure without reading current source files. Selection narrows output,
not repository indexing; the normal indexing ignore/include/exclude rules apply.

Private and unexported symbols are omitted by default according to each language's
visibility conventions. Pass `--private` to include them. Ancestors needed to
identify a selected public symbol remain as structural context.

`-k`, `--kind` (alias `--kinds`) accepts repeated or comma-separated kinds.
`fns`/`functions` groups functions, methods, constructors, and generators;
`methods` selects methods alone. `types` groups aliases, classes, structs, unions,
interfaces, traits, and enums. Other selectors include `imports`, `modules`,
`consts`, `variables`, `fields`, `variants`, `impls`, `macros`, and `headings`.
Kinds are ORed and intersect name regexes. Matching nodes retain their ancestors
as context; a matching parent does not automatically include unmatched children.

`--callers N` and `--callees N` each default to `0` in compact and standard
detail, or `1` with `--detail expanded`. An explicit `0` disables that direction
even in expanded mode. When positive, they expand
selected callables by up to N call-graph edges in the specified direction. Related
callables are shown even when they are outside the requested paths, glob, name,
kind, visibility, or result limit. Expansion stops at cycles and deduplicates
symbols. It is also available on `search`, `search-code`, `search-descriptions`,
`search-md`, `describe`, and `cross-search`; Markdown-only results have no callable
to expand. Cross-index results expand within their respective indexes. Calls are
extracted from indexed source; only uniquely resolved static targets, including
supported explicit imports, form edges. Dynamic receivers and unresolved calls
do not create speculative links. Run without `--no-reindex` once after upgrading
to build the new call metadata.

In text output, a virtual language-specific comment directly below each caller
identifies displayed callees by repository-relative file and qualified symbol,
for example `# calls src/task.py :: execute`. These are annotations, not source
lines. JSON adds `callDepth` and `callees` to expanded map nodes and
`relatedCallables`/`callees` to callable search results when expansion is enabled.
Cross-search adds the same information to source and match objects; describe adds
it to returned functions. With both depths zero, existing output is unchanged.
In expanded text output, `--expand-code-threshold` (default `0.9`) adds the actual
indexed source code of a matched callable when its similarity is **strictly above**
the threshold. The option accepts a finite value in `[-1,1]`; it does not change
search filtering, result limits, or JSON output. For cross-search, a source or
cluster member uses its highest displayed match similarity. Unscored map entries
and call-graph-only neighbors remain declaration-only.

Text output prints declarations in source order using `*** path` file headers and
`@@ start-end @@` source ranges. Adjacent code declarations of the same kind and
scope share a hunk. A container and its members also share one when every
indexed member is selected, adjacent, and fills the container's range; the
hunk uses the parent range. Filters never fold across an omitted symbol.
Markdown headings share one when only blank lines separate
them, including parent and child headings. Prose or code between declarations
starts a new hunk. In file order, each ancestor is printed once. Hunks contain only
declaration signatures: no executable body, body braces, per-line number prefix,
or omission marker is printed. Nested declarations are indented. Code ranges
cover original declarations; Markdown map ranges cover heading lines, not whole
sections. The renderer uses indexed declaration metadata (and remains offline with
`--no-reindex`). Long signatures are truncated at the default detail level.
JSON is an array of
file objects with `path` and `nodes`, retaining full selected metadata: file-local
`id`/`parentId`, kind, names, `qualifiedName`, signature, attributes, import
bindings, heading level, and source ranges. Byte offsets are zero-based and
half-open; lines and UTF-8 byte columns are one-based with exclusive ends.

```bash
slopdex map src docs -g '*.rs' -g '*.md'
slopdex map src -k fns,types -e '^Engine\.'
slopdex map src --private
slopdex map -k imports -e 'HashMap|MapAlias' --format json
slopdex map docs -k headings -e '^Guide\.Setup' -i --no-reindex
```

## Shared selectors

Map, query searches, `describe`, and cross-search accept:

- `-g`, `--glob <GLOB>`: repeatable, case-sensitive globs over root-relative file
  paths. Uses ignore-style override rules: positive patterns include, `!` patterns
  exclude, and the last matching rule wins. If any positive rule exists, a path
  must match one; with only exclusions, other paths remain eligible. These select
  the indexed universe and never reopen files excluded by indexing configuration
  or discovery ignore rules. Quote patterns, for example `-g '*.rs' -g '!tests/**'`.
- `-e`, `--regexp <REGEXP>` (alias `--regex`): repeatable, case-sensitive **Rust
  regexes**, ORed together. Callable searches match `qualifiedName`, with no anchor
  rewriting or implicit bare-name fallback. Map uses the same qualified-name contract,
  including extra bindings qualified in their enclosing scope, and matches import
  paths and aliases. Markdown
  names are heading paths joined with `.`, for example `Guide.Setup`.
  Look-around and backreferences are unsupported.
- `-i`, `--ignore-case`: case-insensitive regex matching; does not change glob
  matching.

Path and name selection intersect and apply before query result limits, including
to Markdown results. Cross-search applies these selectors **only to sources**,
intersecting `--source-path`, `--changed-since`, and `--uncommitted`; candidate
neighbors remain eligible regardless of source selectors. `-k` is map-only.

## Search and analysis options

Common query/cross-search filters:

- `--threshold <number|min-max>`: default `0.8` for cross-search and `0.3` for
  query commands (including `describe`); finite endpoints in `[-1,1]`,
  inclusive minimum and exclusive maximum. A range requires minimum < maximum.
- `--limit <positive integer>`: default unlimited. Caps query results (including
  Markdown), or the context matches for `describe`. For cross-search it caps
  emitted clusters or matched-source rows **after all selected sources are
  searched**.
- `-g`, `-e`, and `-i`: the [shared selectors](#shared-selectors).

`search` accepts `--code`, `--descriptions`, and `--md`. With no selector, it
searches all available kinds; any selector makes selection explicit. Explicit
description search requires complete enabled descriptions.

Cross-search options:

| Option | Behavior |
| --- | --- |
| `--matches <number>` | Maximum neighbors retrieved per source, default `5`. Symmetric-pair suppression can reduce the emitted count. |
| `--lines <number|min-max>` | Line count for both sources and candidates, default `2`: the minimum is inclusive and the optional maximum is exclusive. A single number means at least that many lines. `--min-lines` is an alias. |
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
slopdex cross-search --cross-file-only --lines 4-20 --threshold 0.85-0.9
slopdex cross-search --uncommitted --cross-file-only --threshold 0.9
slopdex cross-search --changed-since origin/main --threshold 0.9
slopdex cross-search --source-path src/services -e '^UserService\.' --threshold 0.9
slopdex cross-search --cross-file-only --cohesion --threshold 0.8
slopdex cross-search --target-root /path/to/other/repo \
  --target-index /path/to/other/index.sqlite --threshold 0.9
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

Text is the default for `map`, `search`, `cross-search`, and `describe`.
`--detail compact` (default) prints declaration signatures and scores only;
`--detail standard` also shows declaration attributes, a 160-character
Markdown body preview; `--detail expanded` additionally shows full signatures,
saved file and callable descriptions, Markdown chunk text, component scores,
and observed cross-search cluster edges. File descriptions are rendered as
language-specific comments immediately after the file header. Callable
descriptions are flattened into one language-specific comment on the
declaration line. Explicit `search-descriptions` or `search --descriptions`
shows both descriptions even at compact detail. For example:

```text
*** src/auth/session.rs
// Session types and token validation.

@@ 14-22 @@ score=0.9123
impl SessionService
  pub fn validate(&self, token: &str) -> Result<Session>  // Validates the token and returns its session.
```

Rust, JavaScript/TypeScript, Go, and Java use `//` comments; C uses `/* */`,
Python uses `#`, and Markdown uses HTML comments. Descriptions are generated annotations,
not lines from the indexed source; their line numbers are not part of hunk ranges.
This setting affects text presentation, not which symbols are selected or the
contents of JSON. Map's `-k` and `--private` remain selection filters.

```text
*** src/auth/session.rs
@@ 11-39 @@
impl SessionService

@@ 14-22 @@
  pub fn validate(&self, token: &str) -> Result<Session>
```

Search prints ranked excerpts with `score=...` (and `similarity=...` if
reranked) on the hunk header. Every ranked code result includes its ancestor
declarations above the match, even when another hit has the same ancestors;
the hunk range still identifies the matched symbol. Markdown search likewise
repeats the complete heading path for each result, since hits are not in file
order. Its range identifies the matched chunk; expanded detail prints the full
chunk text. Mixed search without an explicit description selector keeps saved
descriptions at expanded detail.
Cross-search clusters use the same excerpts under a cluster header, sharing a
file header for consecutive members from the same index and file. Cross-index
source and target roles remain distinct even when paths match. The
similarity range belongs to observed edges, not individual members. With
`--cohesion` or `--format text`, cross-search can instead print source/match
groups with per-match scores and optional filesystem distance.

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

`--format json` produces map/query/diagnostic/model arrays, configuration/status
objects, and **JSONL for cross-search** (one object per matched source).
`--format text` (legacy alias `summary`) selects the text view;
`--format clusters` is valid only for cross-search without cohesion. Results go
to stdout and warnings/errors to stderr. Clap argument errors exit with `2`;
runtime/configuration/domain failures exit with `1`; success/help/version exit
with `0`.

### Task explanations

`describe` sends the query and the expanded text search output (file headers,
declaration skeletons, scores, generated descriptions where available, and
Markdown content) to the configured description provider. Below that output,
`@@ Full source code for best matching files provided below @@` introduces
complete **indexed** files, each headed by `*** <path>`. Files whose highest
match similarity is strictly above `--describe-full-file-threshold` (default
`0.8`) are considered in descending similarity order. The source section is
limited to 96 KiB and the entire prompt to 128 KiB; files that do not fit are
omitted from the source section while their search results remain in the first
section. Expanded search output is limited to 64 KiB and marked if truncated.
The instruction asks for an explanation of existing code/docs with paths and
symbols, not an implementation proposal.

The engine does not reread live files for this context. Provider retries/failover
still apply. JSON output contains
`query`, `description`, `files`, and `functions`; full source and embedding input
are stripped from returned function metadata. Reranker order informs the prompt,
but `rerankScore` is not copied to the returned `functions` array.
Text output labels the generated prose `Explanation` and follows it with
`References` in the shared excerpt syntax. The prose is model-generated; its
format and wording are not deterministic.

## Index lifecycle and persistence

### SQLite authority and USearch sidecars

The default database is `$XDG_CACHE_HOME/slopdex/workspaces/<root-hash>/index.sqlite`
(normally `~/.cache/slopdex/...`; `<root-hash>` is SHA-256 of the canonical
workspace path). Distinct worktrees have independent indexes. SQLite is authoritative
for file snapshots, callable/chunk records, provenance, diagnostics, descriptions,
document/query vectors, reusable artifacts, metadata, and cached search results.
Native schema **3** has normalized `files`, `symbols`, `symbol_names`,
`search_units`, `unit_embeddings`, `descriptions`, and `diagnostics` tables, plus
`cache`, `description_content`, `embeddings`, `metadata`, and `search_cache`. Compatibility JSON snapshots
remain alongside columns; this is not a fully deduplicated representation.
Canonical structure and search units exist independently of semantic embeddings.
After a map-only refresh, a semantic command's normal refresh prepares missing
vectors for its active profile.

Content-addressed parse and model-generated artifacts are durable independently
of live generations and search-result caches. Completed description/embedding
work is persisted before final live publication, so retries after an interrupted
refresh can reuse it. Live changes update the generation and invalidate search
results without discarding those reusable artifacts.

### Shared provider artifacts

An additional per-user SQLite cache at `$XDG_CACHE_HOME/slopdex/artifacts-v2.sqlite`
shares provider artifacts across local workspaces, even when their workspace indexes
are different. An optional S3-compatible bucket shares embeddings, file/callable
descriptions, rerankings, and generated explanations between machines. A lookup
checks the workspace index, then the per-user cache, then S3 (if configured),
before calling a provider. Hits are validated and copied into the workspace index.
The bucket stores global content-addressed objects, independent of repository
identity. A description answer records hashes for its original system instruction,
generation prompt, configured model profile, settings, and file-description context.
Each distinct input is stored once by content hash in the per-user SQLite cache
and as an S3 content object. A remote hit fetches missing referenced objects
before using the answer; a missing object is a cache miss. S3 content objects
are created conditionally, so repeated uploads do not create new versions of
an existing prompt. The prompt contains
source text (a whole file for file descriptions), so anyone with bucket access
can read these content objects and vectors. Git metadata, search results, and USearch files are
not uploaded.

S3 is best effort: missing objects, outages, and failed uploads cannot prevent
provider work. Missing keys are fetched with bounded concurrency (up to 10), one
GET per object; S3 has no portable multi-key GET. Uploads follow each successful
local write. All S3 objects under `<prefix>/v3/<kind>/<first-two-key-characters>/<key>`
are zstd-compressed frames containing the SHA-256 checksum of the uncompressed
payload, a newline, and the payload. This includes embeddings, description
records and their referenced content, rerankings, and explanations; the local
SQLite caches remain uncompressed. Earlier `v2` S3 objects are not read, and
existing workspace artifacts can be republished under `v3` on refresh. Embedding identities distinguish
query/document inputs, provider/model/dimensions, and custom endpoints. Description
keys intentionally exclude paths and model settings: file keys include only the
source-content hash and system instruction; callable keys include qualified symbol
name, callable source hash, file-description text, and system instruction. The
path and content hashes are saved alongside the cached answer for inspection and
future invalidation policy. Thus a rename or model-setting change can reuse a
matching answer, even when the prompt for a new request would differ. Existing live descriptions
retain their normal reuse/staleness policy. Map never contacts S3. An uncached
semantic query with `--no-reindex` may consult S3 before its provider call.
Opening an existing workspace index also imports its valid provider artifacts into
the per-user cache. When S3 is enabled later, the next successful semantic refresh
uploads existing paid artifacts best effort. Legacy description keys do not match
these relaxed keys; there is no legacy description-cache import policy.

Example `.slopdex/config.json` fields (credentials come from standard AWS
environment variables or credentials files, not this JSON):

```json
{
  "artifactS3": {
    "bucket": "my-slopdex-cache",
    "region": "us-east-1",
    "endpoint": "http://127.0.0.1:9000",
    "pathStyle": true,
    "prefix": "slopdex"
  }
}
```

`endpoint` is optional for AWS S3. S3-compatible services such as MinIO
typically need `pathStyle: true` (the default when `endpoint` is provided).
`artifactCachePath` optionally overrides the per-user shared SQLite cache path.
To run the MinIO integration test against a local server with
`minioadmin`/`minioadmin` credentials, set `SLOPDEX_MINIO_ENDPOINT` (for example,
`SLOPDEX_MINIO_ENDPOINT=http://127.0.0.1:9000 cargo test --test rust_integration minio_shares_artifacts`).

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
`reindex-files` refreshes stale file descriptions; `--callables` also prepares
callable descriptions in those files. Matching cached artifacts are reused,
including when the configured model has changed. Merely changing that model does not
regenerate all existing descriptions.

File descriptions use the complete file source. Each callable request is a
separate request containing its source, symbol, path, and file-description
context. File generation asks for one paragraph; callable generation asks for
one sentence. Previously saved descriptions are reused until regenerated and
multi-line callable descriptions are flattened for inline display. There is no
continuing per-file chat conversation. `status` reports
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

`map --no-reindex` needs neither providers nor sidecar repair. A structure-only
index can have search units without vectors; `--no-reindex` does not prepare those
missing vectors. Run a semantic command with normal refresh to prepare them.

Native index identity includes canonical root and schema. Embedding profiles
(provider/model/dimensions/strategy) are separate projections: changing a model
does not reset structural data, and cached vectors from older profiles remain
available for reuse. Within schema 3, changing the root requires
`--force-reindex --yes-really-rebuild-the-index`. This clears native live records,
non-identity metadata, and search results while retaining artifact/vector caches.
A compatible index follows normal refresh even with that flag. It conflicts with
`--no-reindex`. `--rebuild-on-divergence` is accepted with the same confirmation,
but the current engine refreshes the working tree without a divergence gate;
the flag adds no engine behavior.

### Rebuilding an old index

Native schema **3** is a hard cutoff: **all schema-2 indexes**, old
`rust_`-prefixed native tables, and TypeScript layouts (including schema 11) are
rejected with instructions to remove the existing SQLite index and rebuild, or
choose a new index path. There is no legacy import or migration, and neither
`--force-reindex` nor `--no-reindex` bypasses old-layout rejection.

For an old explicit database, stop any Slopdex process using it, then remove
that database and its `-wal`/`-shm` companions. A schema-3 index at the former
default `.slopdex/index.sqlite` is copied automatically to the new XDG default
location on first use; older incompatible layouts are skipped and rebuilt at
the new location. To rebuild at a fresh default location, remove the current
index path reported by `slopdex status --format json` and its WAL/SHM companions.
Alternatively, select a new path:

```bash
slopdex --index /path/to/new-index.sqlite update
```

For a custom database, remove that SQLite file and its `-wal`/`-shm` companions,
then run `slopdex --index /path/to/index.sqlite update`. To rebuild at a new,
unused path instead:

```bash
slopdex --index /path/to/new-index.sqlite update
```

Use that same `--index` path on subsequent commands, or save it as `indexPath` in
config. Rebuilding scans current files and regenerates embeddings and enabled
descriptions using the configured providers; old database artifacts and saved
description settings are not imported. Derived USearch sidecars are reconciled
or rebuilt from the new SQLite snapshot.

Use `slopdex map` instead of `update` to build only local structure without
provider calls; semantic refresh can prepare embeddings later.

### Coverage and failures

Supported extensions: TS/TSX (`ts`, `mts`, `cts`, `tsx`), JS/JSX (`js`, `mjs`,
`cjs`, `jsx`), Python (`py`, `pyw`), Rust, Go, Java, C (`c`, `h`), shell
(`sh`, `bash`, `zsh`), Markdown
(`md`, `markdown`), JSON (`json`), Terraform/HCL (`tf`, `tfvars`, `hcl`),
YAML (`yaml`, `yml`), TOML (`toml`), XML (`xml`, `svg`, `xsd`, `xsl`, `xslt`),
HTML (`html`, `htm`), and CSS (`css`). Code search indexes recognized
callables; Markdown search uses bounded heading-aware chunks. General `search`
also indexes bounded content chunks from configuration and markup files (JSON
result type `document`, with `chunk` content). `search-md` / `--md` selects
Markdown only. `map` exposes keys, sections, blocks, elements, and CSS rules
and declarations for these formats.

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
index defaults to the per-workspace XDG cache path described above. Explicit relative config/index
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
| `artifactCachePath`, `artifactS3` | Shared local cache override and optional best-effort S3 bucket/region/endpoint/prefix/pathStyle, as described above. |
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
