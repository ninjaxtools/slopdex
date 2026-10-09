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
cargo run --locked -- update
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
| `search <query>` | Search all indexes: callable code, available descriptions, Markdown/documents, and symbols. | Summary |
| `search-code <query>` | Search only the callable code index. | Summary |
| `search-descriptions <query>` | Search only available source/generated callable, symbol, and file descriptions. | Ranked declaration/file excerpts |
| `search-md <query>` | Search only the heading-aware `.md`/`.markdown` content index. | Ranked heading paths |
| `search-symbols <query>` | Search only the bare symbol-name/alias and Markdown heading-title index. | Ranked declaration/heading excerpts |
| `describe <query>` | Search, then ask the configured description model to explain the existing code/docs relevant to the query. | Explanation text |
| `cross-search` | Find similar callable neighbors in this index or a second repository. | Clusters; summary with `--cohesion` |
| `map [PATH]...` | Show code declarations/Markdown headings; refresh existing index structure or parse directly if no index exists. | Summary |
| `status` | Refresh and report counts, generation, checkpoint, profiles, and backends. | JSON object |
| `index errors` | Refresh and report saved read/parse/extraction diagnostics. | Summary |
| `update` | Create or explicitly refresh the index from the current working tree and Git HEAD, when available; `--target` accepts only `HEAD`. | JSON refresh statistics |
| `generate descriptions` | Refresh normally, then explicitly generate missing or stale file and callable descriptions from indexed source. | JSON statistics |
| `help models [opencode\|opencode-go]` | Fetch one or both live public OpenCode catalogs without opening an index or requiring credentials. | Qualified `provider/model` lines |
| `config [prefix]` | Configure matching settings interactively without opening an index. | Updated-setting summary |
| `config set <key> <value>` | Set a configuration value directly; JSON literals support booleans, numbers, arrays and objects. | Updated-setting summary |

Configuration examples:

```bash
slopdex config
slopdex config description
slopdex help models opencode-go
slopdex config set descriptionProvider opencode-go
slopdex config set descriptionModel gpt-5.6-luna
slopdex config set descriptionFallbackModel muse-spark-1.3-contributor
slopdex config set rerankerProvider cohere
slopdex config set rerankingEnabled true
slopdex config set parallelism 10
```

`config set` validates configuration values before writing; values that are not JSON
literals are saved as strings. The global `--model` selects the embedding model.
Description configuration selects the models used by explicit
`generate descriptions` and task explanations; saving it does not generate descriptions.
Interactive configuration requires terminal stdin/stderr; it asks
about descriptions, reranking, embeddings, paths, filters, and common settings.
Use arrow keys and Enter to select providers and models; typing in an OpenCode
model menu filters the published catalog. Saved values are preselected, numeric
inputs validate inline, and Esc/Ctrl-C cancels without saving partial changes.
The wizard and its saved-settings summary render on stderr, so `--format json`
can write the resulting configuration to redirected stdout.
Advanced endpoint/HTTP settings can be set with `config set`.

Help, version, configuration, and model-catalog commands do not refresh the index.
Search commands and cross-search require an existing resolved index; run
`slopdex update` first. Existing indexes normally refresh before commands run.
`map` refreshes only local structure when indexed, or parses directly when the
index is missing. With an existing index, `map -q` lazily prepares symbol-name
vectors and query vectors using the embedding provider or cache. Without an
index, map warns on stdout that `-q` is ignored and proceeds with local mapping.
Semantic refresh prepares missing embeddings and reuses available descriptions,
without generating descriptions. Only `generate descriptions` generates indexed
file/callable descriptions.
`search-symbols` and `search --symbols` with no other content flags use the same
structure-only refresh as `map -q`; mixed content/symbol searches use normal
semantic refresh.

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
slopdex map [PATH]... [-g GLOB]... [-e REGEXP]... [-i] [-q SYMBOL_QUERY]... [--symbol-threshold NUMBER] [-k KIND]... [--private] [--callers N] [--callees N] [--expand-callers N] [--expand-callees N] [--detail compact|standard|expanded] [--format text|json]
```

With no paths, map selects the repository. Paths select files or recursive
directories and are relative to `--root`, including when invoked from another
working directory. Absolute paths and `..` components are accepted. A path outside
the selected root uses its own Git checkout root, configuration, and index, as if
invoked from that source's directory. Without Git, that directory is the root.
Explicit `--config` and `--index` overrides still apply and remain relative to the
invocation directory; relative paths saved in an external source's configuration
resolve from the source's directory. Missing paths produce warnings on stderr and
are ignored. Multiple paths form a union, intersected with the shared selectors;
paths in different workspaces are processed independently, with JSON results
combined into one array. Returned paths remain relative to their respective roots.

When the resolved index is missing, map parses eligible files directly
and renders the same structure, selectors, call-graph expansion, and expanded source/Markdown
output. It creates no database, lock files, cache directories, or sidecars, even
with `--no-reindex`, and works without Git. Ignore/include/exclude rules and
`maxFileSize` still apply.
If `-q` is supplied, map prints `slopdex: warning: no active index; -q is ignored.`
to stdout before the map output, including with `--format json`. All symbol
queries are ignored; other selectors, paths, and detail settings still apply.

With an existing index, map normally refreshes local file snapshots, declarations,
headings, search units, and diagnostics in SQLite. `map --no-reindex` reads stored
SQLite structure without reading current source files. Selection narrows output,
not repository discovery. Without `-q`, both paths make no provider requests or
generate descriptions, embeddings, or USearch sidecars.

`map -q 'validate session'` uses semantic selection over symbols, available
descriptions, and heading titles from saved snapshots.
Run `slopdex update` first to enable semantic selection; without an index, map
warns on stdout and ignores `-q` while performing normal local mapping.
Its refresh remains structure-only; name, description, and query vectors for
selection are prepared lazily using the configured provider or shared caches.
It does not prepare content indexes or generate prose. With `--no-reindex`, it
uses saved structures and descriptions but may still populate selector vectors
and the symbol cache/sidecar.

Private and unexported symbols are omitted by default according to each language's
visibility conventions. Pass `--private` to include them. Ancestors needed to
identify a selected public symbol remain as structural context.

`-k`, `--kind` (alias `--kinds`) accepts repeated or comma-separated kinds.
`fns`/`functions` groups functions, methods, constructors, and generators;
`methods` selects methods alone. `types` groups aliases, classes, structs, unions,
interfaces, traits, and enums. Other selectors include `imports`, `modules`,
`consts`, `variables`, `fields`, `variants`, `impls`, `macros`, and `headings`.
Kinds are ORed and restrict the union of regex and semantic matches. Matching nodes
retain their ancestors as context; a matching parent does not automatically include
unmatched children.

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

`--expand-callers N` and `--expand-callees N` also traverse up to N edges, and
include the full indexed code of related callables at those depths in text output,
regardless of `--detail` or `--expand-code-threshold`. The corresponding
`--callers`/`--callees` depth can be higher: levels beyond the expanded depth
still show declarations. The larger depth wins when both flags are given; a root
that is also another root's caller or callee can receive forced code. These flags
work wherever `--callers` and `--callees` work, including describe context and
cross-search clusters. Map JSON and related callable JSON nodes include indexed
`source` and `expandedCode: true` at forced-code depths alongside their existing
`callDepth` metadata.
`--expand-callables` is an alias for `--expand-callees`.

Callees are the functions a symbol calls; callers are the functions that call it.
In text output, each function's displayed callees appear immediately below its
declaration, indented two spaces deeper than the declaration. A single callee
appears on the same line as `callees:`, separated by one space:

```text
def outer():
  # callees: b.py:1-2:middle
```

Multiple callees appear under a standalone `callees:` header, grouped under one
repository-relative `path:` header per file. Paths are sorted lexically, with
symbols in numeric source order within each file. After each comment marker,
file-header text is indented
two spaces deeper than `callees:`; symbol text is indented another two spaces and
uses `start-end:qualifiedName`, or `line:qualifiedName` for single-line functions.
When a file has only one displayed symbol, its location follows the file's colon
on the same line with no intervening space, for example `c.py:7:leaf`.
All comment markers align with the `callees:` marker; only the nested contents
after the markers are indented. The whole block follows the owning function's
indentation, including for nested declarations. These lines use language-specific
comment markers, such as `#` for Python and `//` for Rust, and are generated
comments, not source lines. For example, a top-level `outer` calling `middle`
defined on lines 1–2 and a single-line `helper` on line 5 in `b.py`, plus `leaf`
on line 7 in `c.py`, renders:

```text
def outer():
  # callees:
  #   b.py:
  #     1-2:middle
  #     5:helper
  #   c.py:7:leaf
```

JSON `callees` string arrays remain flat locations in `path:start-end:qualifiedName`
format, or `path:line:qualifiedName` for single-line functions, for example
`["b.py:1-2:middle", "b.py:5:helper"]`, without file grouping, a `calls ` prefix,
or a ` :: ` separator.
JSON adds `callDepth` and `callees` to expanded map nodes and
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
starts a new hunk. In file order, each ancestor is printed once. Code hunks contain
declaration signatures: no executable body, body braces, per-line number prefix,
or omission marker is printed unless explicitly expanded. Nested declarations are
indented. Code ranges cover original declarations; Markdown map ranges cover full
sections, including subsections. With `--detail expanded`, Markdown maps also print
the full body beneath each selected heading, without repeating nested sections.
Headings included only as ancestor context remain heading-only, and filters omit
unmatched section bodies. The renderer uses source and declaration metadata from
the saved index or direct parsing. Indexed maps without `-q` remain offline with
`--no-reindex`; unindexed maps read current files even with that flag. Long signatures are
truncated at the default detail level.
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
slopdex map src -q 'validate session' -q 'authenticate user' -g '*.rs' -k fns
slopdex map docs -q 'installation' --symbol-threshold 0.6 --detail expanded
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
  or semantic matching.
- `-q`, `--symbol-query <QUERY>`: repeatable semantic selector over **symbols,
  available descriptions, and Markdown heading titles**. Repeated queries are ORed;
  the resulting matches are ORed with regex matches when `-e` is also supplied. A
  declaration matching either family is eligible, including matches on different
  aliases. Globs and map kind/private filters still restrict that union.
  Names/aliases and heading titles match without parent-name context. A description
  match selects its symbol; a matching file description selects declarations in
  that file, still subject to globs and map kind/private filters. A parent-name
  match alone does not select children. Ancestors have no expanded heading body unless
  they directly match. The positional search/describe query ranks the chosen
  streams; with `search-symbols` or `search --symbols` alone it ranks names.
- `--symbol-threshold <NUMBER>`: optional finite scalar in `[-1,1]`, default
  `0.5`; inclusive minimum semantic selector similarity, independent of `--threshold`.
  Ranges are not accepted.

Names and symbol queries normalize camelCase/PascalCase, snake_case, acronym and
letter/digit boundaries, and punctuation into lowercase space-separated words.
For example, `getHTTPResponse`, `get_http_response`, and `get HTTP response`
normalize to `get http response`. A query must contain at least one letter or
number after normalization. Symbol/heading matching uses name-only embeddings;
available descriptions also contribute to semantic selection. Source,
signatures, parent scopes, and Markdown bodies are not directly embedded for this selector.

Symbol embeddings default to 256 native dimensions, capped at the configured
content embedding dimensions. Optional JSON `symbolDimensions` sets a separate
symbol embedding dimension; OpenAI `text-embedding-ada-002` uses its full
dimensions. `-q` embeds available descriptions in the same selector space. A
separate symbol search channel is opened lazily from normalized names in saved
structures, using a shared base and a worktree `<index>.symbols.shared.json`
pointer. Ordinary map initializes no model.

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
- `-g`, `-e`, `-i`, `-q`, and `--symbol-threshold`: the [shared selectors](#shared-selectors).

`search` accepts the explicit index selectors `--code`, `--descriptions`, `--md`,
and `--symbols`. With no selector, it searches all indexes: callable code,
available descriptions, Markdown/document content, and symbol names/heading titles.
Any selector makes selection explicit, omitting unselected indexes. Description
search uses source descriptions and previously generated prose. Description text
is optional; existing descriptions are indexed without an enable/disable setting.
Run `slopdex generate descriptions` to prepare missing or stale generated prose
where no source description exists. `-q` is an additional shared filter,
not an index selector.

`search-symbols <query>` is the canonical name-only command; `search <query>
--symbols` without any other content flags is equivalent. Both require an
existing resolved index (`slopdex update` first), including with `--no-reindex`.
They refresh local structure only, without preparing content embeddings or
generating descriptions, then lazily prepare name/query vectors using the
configured embedding provider or cache. `--no-reindex` keeps the saved snapshot
but permits symbol-cache population and symbol-sidecar repair. Names can be
embedded even when the structure-only snapshot lacks content vectors.

All structural kinds are eligible: functions, methods, constants, variables,
types, imports and aliases, headings (including headings without body text),
and other declarations. Scores use the maximum bare-name/alias similarity;
parent names, signatures, code, descriptions, and heading bodies do not contribute.
`--threshold` applies its inclusive minimum/exclusive maximum and `--limit`
caps the ranked hits. Optional `-q` filters eligible symbols using names or descriptions
(ORed with `-e`, if supplied),
using `--symbol-threshold` independently of the ranking threshold.

Combining `--symbols` with `--code`, `--md`, or `--descriptions` uses normal
semantic refresh. Code and description hits for the same callable merge by maximum
similarity; file, Markdown/document, and symbol hits remain independent, with
global ranking and one global output limit. A
declaration can appear in both function and symbol rows; text combines their
annotations on the same declaration.

Cross-search options:

| Option | Behavior |
| --- | --- |
| `--matches <number>` | Maximum neighbors retrieved per source, default `5`. Symmetric-pair suppression can reduce the emitted count. |
| `--lines <number|min-max>` | Line count for both sources and candidates, default `2`: the minimum is inclusive and the optional maximum is exclusive. A single number means at least that many lines. `--min-lines` is an alias. |
| `--source-path <path>` | Source file or recursive directory, root-relative or absolute; external paths select their own root, configuration, and index, as for map. |
| `--changed-since <commit>` | Source callables differing from this ancestor of the indexed Git checkpoint; details below. |
| `--uncommitted` | Source callables whose indexed file has working-tree provenance. |
| `--cross-file-only` | Exclude candidates with the same root-qualified file path as the source. |
| `--include-symmetric-duplicates` | Keep both directions of same-index pairs. By default each unordered observed pair is emitted once. |
| `--cohesion` | Sort each source's selected matches by descending filesystem distance, then similarity. Defaults to summary; incompatible with clusters. |
| `--target-root <path> --target-index <path>` | Compare to another existing index; both are required together. The target is checked before the source engine opens or refreshes. Embedding profiles must match. |
| `--target-config <path>` | Config for the second root; defaults to `<target-root>/.slopdex/config.json`. Requires both target options. |

Examples:

```bash
slopdex update
slopdex search "keep the repository index synchronized"
slopdex search-code "configure the embedding provider"
slopdex search-code 'reject expired credentials' -q 'validate session' --symbol-threshold 0.6
slopdex search-md "configure the embedding provider"
slopdex search-symbols 'validate session' --threshold 0.6 --limit 20
slopdex search 'installation' --symbols -g '*.md' --detail expanded
slopdex search 'validate session' --code --symbols
slopdex search-symbols 'read settings' -q 'configuration' --symbol-threshold 0.7
slopdex config set descriptionProvider opencode-go
slopdex generate descriptions
slopdex search-descriptions "keep the repository index synchronized"
slopdex describe "I want to implement a new rpc endpoint"
slopdex cross-search --cross-file-only --lines 4-20 --threshold 0.85-0.9
slopdex cross-search --uncommitted --cross-file-only --threshold 0.9
slopdex cross-search --changed-since origin/main --threshold 0.9
slopdex cross-search --source-path src/services -e '^UserService\.' --threshold 0.9
slopdex cross-search -q 'load configuration' --cross-file-only --threshold 0.9
slopdex cross-search --cross-file-only --cohesion --threshold 0.8
slopdex --root /path/to/other/repo --index /path/to/other/index.sqlite update
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

### Scores, independent indexes, and ANN recall

SQLite stores the authoritative embeddings; USearch performs filtered F32 cosine
HNSW search over independent code, Markdown/document, and description indexes.
Code and description vectors use the configured content dimension; no concatenated
2D/3D vectors or combined index are used. Source or generated descriptions are
indexed whenever present. Missing descriptions are allowed and do not prevent
code search or require every callable to be described.

`search-code` ranks callable code alone. `search-descriptions` ranks available
callable/symbol descriptions and independent file descriptions. When both indexes
are selected, code and description retrieval apply score thresholds independently;
hits for the same search unit merge by **maximum matching similarity**, not an
average. File descriptions are scored directly against the query vector and can
produce a file hit even without callables. Markdown/document and symbol-name hits
retain their own scores; all selected results share global ranking and limits.

Query JSON exposes available component scores: `codeSimilarity`,
`descriptionSimilarity`, `functionDescriptionSimilarity` for callables, and
`fileDescriptionSimilarity`. These are supplementary measurements, not fusion
weights; a component may be present even when its index was not selected and
does not change the selected-index ranking. Symbol-name rows additionally expose
`symbolSimilarity`.

Cross-search always retrieves, thresholds, and ranks by code similarity. Its
optional callable/file description similarities compare the corresponding
descriptions when both sides have vectors. These supplementary scores do not
affect ranking, match limits, or thresholds, and require neither complete
description coverage nor matching description-generator profiles. JSON
`scoring` reports `similarityMode: "code"` and weights of code `1`, description
`0`, and file description `0`.

**Neighbor retrieval remains approximate.** Base membership masks and selectors
filter inside USearch graph traversal, not on an unfiltered top-k list. Worktree
deltas are searched exactly and merged with base hits. Threshold ranges can trigger wider
retrieval, but neither independent-index merging nor an unlimited output limit
guarantees exhaustive recall or exact top-k membership. There is no exhaustive
scan fallback over the full base for ANN retrieval; file descriptions are scored directly.
Similarity is model-dependent, not a probability of duplication.

### Reranking, clusters, and output

Text is the default for `map`, `search`, `cross-search`, and `describe`.
`--detail compact` (default) prints declaration signatures and scores only;
`--detail standard` also shows declaration attributes, file-hit description prose,
and a 160-character Markdown body preview; `--detail expanded` additionally shows full signatures,
saved file and callable descriptions, Markdown body text (full selected bodies for
map and direct symbol-heading hits, matched chunks for content search), component scores,
for file-oriented output. Clusters always print compact symbol locations. File descriptions are rendered as
language-specific comments immediately after the file header. Callable
descriptions are flattened into one language-specific comment on the
declaration line. Explicit `search-descriptions` or `search --descriptions`
shows both descriptions even at compact detail. For example:

```text
*** src/auth/session.rs  // score=0.92
// Session types and token validation.

@@ 14-22 @@
impl SessionService
  pub fn validate(&self, token: &str) -> Result<Session>  // score=0.91 | Validates the token and returns its session.
```

Rust, JavaScript/TypeScript, Go, and Java use `//` comments; C uses `/* */`,
Python uses `#`, and Markdown uses HTML comments. Description prose may come from
source comments/docstrings or explicit generation. It is displayed as an annotation;
its lines are not added to declaration hunk ranges.
This setting affects text presentation, not which symbols are selected or the
contents of JSON. Map's `-k` and `--private` remain selection filters.

```text
*** src/auth/session.rs
@@ 11-39 @@
impl SessionService

@@ 14-22 @@
  pub fn validate(&self, token: &str) -> Result<Session>
```

Search groups ranked hits by file, ordering files by their best score and keeping
declarations in source order. Each matched declaration carries `score=...` (and
`similarity=...` if reranked), with ancestor context emitted once. Markdown hits
retain heading context; expanded detail prints matched chunk text. Callable
description prose is shown at expanded detail or for an explicit description
selector, including compact detail.

File-description hits put their score on the file header, followed by prose at
standard/expanded detail or at compact detail for explicit description search.
They group naturally with symbol/function hits and have no synthetic function or
source hunk. File JSON rows have this shape:

```json
{
  "type": "file",
  "file": {
    "path": "src/auth/session.rs",
    "description": "Session types and token validation.",
    "sourceMode": "working-tree",
    "language": "rust"
  },
  "similarity": 0.8,
  "fileDescriptionSimilarity": 0.8
}
```

Symbol excerpts include ancestor context and a `symbol score=...` annotation;
expanded detail also shows `[symbol ...]` from `symbolSimilarity`. Direct heading
hits display their saved body in expanded output via the structure snapshot;
ancestor-only headings remain heading-only. Bodies/code are display context,
not inputs to symbol ranking. Only callable symbol hits seed call expansion.

Symbol JSON rows have this shape:

```json
{
  "type": "symbol",
  "symbol": {
    "id": 2,
    "kind": "heading",
    "name": "Setup",
    "qualifiedName": "Guide.Setup",
    "path": "guide.md",
    "sourceMode": "working-tree"
  },
  "similarity": 0.8,
  "symbolSimilarity": 0.8
}
```

The abbreviated `symbol` above contains the full serialized `StructureNode` in
actual output (ranges, signature, parent, aliases, and other metadata), plus
`path` and `sourceMode`. Its `id` is **file-local**, not a name-vocabulary/vector
ID. Callable symbol rows can include `relatedCallables` and `callees` when call
expansion is requested, as function rows do; noncallable symbols do not seed it.

Cross-search clusters retain their cluster header and group members under one
repository-relative `path:` header per file. File paths are sorted lexically;
symbols within each file keep numeric source order. Each symbol line is indented
two spaces and uses `start-end:qualifiedName` for multiline symbols or
`line:qualifiedName` for single-line symbols. Files with only one displayed symbol
print it directly after the file's colon on the same line, with no intervening
space. For example:

```text
*** Cluster 1 · 3 symbols · 12 lines · similarity 0.91-0.95
src/api/routes.ts:
  12:validateSession
  20-24:refreshSession
src/auth/session.ts:5-10:Session.validate
```

Cross-index members append `[source]` or `[target]` to their individual symbol
lines to keep their identities distinct. Identical paths share one file header
even when the symbols come from different indexes. The similarity range belongs
to observed edges, not individual members. File grouping is a text presentation;
JSON `callees` arrays keep flat `path:start-end:qualifiedName` locations. With
`--cohesion` or `--format text`, cross-search can instead print source/match groups
with per-match scores and optional filesystem distance.

Reranking applies to query commands, including the search inside `describe`, not
cross-search. Embedding thresholds are applied first. Cohere/Jina receive up to
five times an explicit result limit, or all retrieved threshold-passing candidates
without a limit. OpenAI receives up to
`min(100, max(limit or 100, rerankerCandidates))`; the configured candidate count
defaults to `10`. Without an explicit limit the OpenAI retrieval cap is **100**.
Query JSON retains `similarity` and adds `rerankScore`.

Clusters are connected components of observed callable matches, sorted by highest
pair similarity × distinct covered source lines, descending. Line coverage is the
union of member ranges per file; overlapping symbols count shared lines once,
and source/target indexes count separately even for identical paths. Exclusive
ends at column 1 do not count the ending line. Missing end lines default to the
start line. Ties use highest pair similarity, then covered lines (both
descending), then the first member's location/name. `--limit` applies after ranking.
Cluster headers show the covered line count. The displayed similarity range
covers observed links;
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
declaration skeletons, scores, available source or generated descriptions, and
Markdown content) to the configured description provider. The prompt includes
indexed code for callers and callees up to two edges away
by default, even below `--expand-code-threshold`. Use `--expand-callers N` and
`--expand-callees N` (or `--expand-callables N`) to override either depth;
explicit `0` disables that direction's automatic code expansion. These defaults
apply to the LLM context, not describe's JSON result or text references.
The entire prompt is limited to 128 KiB; expanded search output is marked if
truncated on a UTF-8 boundary. Complete files are not appended to the prompt.
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

### Create and refresh

Run `slopdex update` once for the selected root/index path before searching.
`search`, `search-code`, `search-md`, `search-descriptions`, `search-symbols`,
and `cross-search`
fail promptly if the resolved source index does not exist, with an instruction to
run `slopdex update`, including with `--no-reindex`. Cross-search also requires
the target index to exist and validates it before opening or refreshing the
source engine. Missing-index failures create no index/cache artifacts and make
no provider calls.

Existing indexes continue to refresh automatically unless `--no-reindex` is
given. Git is optional: indexing reads the working tree and records HEAD
when available. There is no legacy index migration, import, or seeding fallback.
An unindexed `map` is direct, local inspection; it does not initialize an index
for a subsequent search.

After an initial scan, Git refresh combines current status, the tree diff from
the saved checkpoint to HEAD, and previously dirty paths. Revisiting the latter
detects restored edits even when they no longer appear in status. Branch switches
do not require an ancestry relationship. Unchanged repeated queries do not read
and hash every indexed file. Missing checkpoints, non-Git/unborn roots, and
discovery-policy changes can require a full scan. Candidate files are still
content-hashed and checked before publication; Git metadata is not content-cache
validity. There is no filesystem watcher.

### SQLite authority and USearch sidecars

The default database is `$XDG_CACHE_HOME/slopdex/worktrees-v1/<root-hash>/index.sqlite`
(normally `~/.cache/slopdex/...`; `<root-hash>` is SHA-256 of the canonical
workspace path). Distinct worktrees have independent live bindings and search-result
caches. Native schema **4** has normalized `files`, `symbols`, `symbol_names`,
`search_units`, `unit_embeddings`, `descriptions`, and `diagnostics` tables, plus
`metadata` and `search_cache`. File records bind paths to source hashes; embedding
and description records bind live occurrences to global artifact keys. Source
text, parse artifacts, document/query vectors, provider artifacts, description
content, and immutable snapshot manifests are authoritative in the attached global
SQLite store, not duplicated paid-artifact caches in each worktree. Both databases
are needed to read the saved snapshot; a workspace database alone is not a portable
index. Structural metadata retains JSON where appropriate.
Canonical structure and search units exist independently of semantic embeddings.
After a map-only refresh, a semantic command's normal refresh prepares missing
vectors for its active profile.

Content-addressed parse and model-generated artifacts are durable independently
of live generations and search-result caches. Completed description/embedding
work is persisted before final live publication, so retries after an interrupted
refresh can reuse it. Live changes update the generation and invalidate search
results without discarding those reusable artifacts. Published content has an
immutable snapshot digest covering bindings and relevant parser/profile/policy
contracts. Automatic garbage collection is not implemented.

Repository relatedness is tracked separately. Linked worktrees are related through
their canonical common Git directory; observed history roots and full commit IDs
provide discovery evidence for related clones. Remote URLs are only sanitized
hints, not merge keys. Registry family membership, roots, commits, and remotes
never establish cache validity: reuse still depends on exact content and contracts.

### Shared provider artifacts

The per-user store at `$XDG_CACHE_HOME/slopdex/global-v1.sqlite` is the one
authoritative local artifact store shared across worktrees and repositories. It
holds source and parse artifacts, embeddings, description content and generation
records, rerankings, explanations, and immutable snapshot manifests. A provider
artifact lookup checks this store, then S3 (if configured), before calling a
provider. Remote hits are validated and written to the same global store; live
worktree records reference their keys rather than copying their payloads. A failed
or incompatible global store is an error, not a workspace-cache fallback.
An optional S3-compatible bucket shares embeddings, file/callable descriptions,
rerankings, and generated explanations between machines.
The bucket stores global content-addressed objects, independent of repository
identity. A description answer records hashes for its original system instruction,
generation prompt, configured model profile, settings, file-description context,
and ordered conversation messages.
Each distinct input is stored once by content hash in the global SQLite store
and as an S3 content object. A remote hit fetches missing referenced objects
before using the answer; a missing object is a cache miss. S3 content objects
are created conditionally, so repeated uploads do not create new versions of
an existing prompt. The prompt contains
source text (a whole file for file descriptions), so anyone with bucket access
can read these content objects and vectors. Source/parse storage and snapshot
manifests are local; Git metadata, search results, and USearch files are not uploaded.

S3 is best effort: missing objects, outages, and failed uploads cannot prevent
provider work. Missing keys are fetched with bounded concurrency (up to 10), one
GET per object; S3 has no portable multi-key GET. Uploads follow each successful
local write. All S3 objects under `<prefix>/v3/<kind>/<first-two-key-characters>/<key>`
are zstd-compressed frames containing the SHA-256 checksum of the uncompressed
payload, a newline, and the payload. This includes embeddings, description
records and their referenced content, rerankings, and explanations; the local
SQLite store remains uncompressed. Earlier `v2` S3 objects are not read. Existing
global provider artifacts can be published under `v3` on refresh. Embedding
identities distinguish
query/document inputs, provider/model/dimensions, and custom endpoints. Generated
description keys cover the configured LLM profile, generation settings, system
instruction, and every ordered message role/content in the effective request.
The file prompt includes path and complete source; callable turns include symbol,
line range, and preceding conversation. Renames, line shifts, surrounding-file
changes, or earlier answers can change the key; reuse requires an identical
effective request, not merely matching callable source. Source
descriptions are extracted locally and never sent to a generator for replacement.
Map without `-q` never contacts S3. An uncached
semantic query with `--no-reindex` may consult S3 before its provider call.
When S3 is enabled later, the next successful semantic refresh uploads existing
global paid artifacts best effort. There is no import or migration from old
workspace indexes, `artifacts-v2.sqlite`, or legacy description caches.

Parse, embedding, and description misses use per-artifact locks and recheck the
global store after acquiring them, preventing duplicate work across concurrent
worktrees. Locks remain held through local persistence, with a bounded 30-second
wait; unrelated artifact keys can proceed independently.

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
`artifactCachePath` optionally selects the authoritative global SQLite store.
Relative paths resolve from the process working directory. Each workspace index
persists its canonical global-store binding and keeps using it when the override
is omitted. An explicit override that conflicts with an existing binding requires
a new index path; `--force-reindex` does not change the binding. The workspace and
global SQLite paths must differ. An unavailable or incompatible store is not
silently replaced, migrated, or seeded from a workspace index.
To run the MinIO integration test against a local server with
`minioadmin`/`minioadmin` credentials, set `SLOPDEX_MINIO_ENDPOINT` (for example,
`SLOPDEX_MINIO_ENDPOINT=http://127.0.0.1:9000 cargo test --test rust_integration minio_shares_artifacts`).

Derived vector indexes are opened lazily per channel: code, Markdown,
descriptions, and name-only symbols. Worktree pointers sit beside the database as
`<index>.code.shared.json`, `<index>.markdown.shared.json`,
`<index>.descriptions.shared.json`, and `<index>.symbols.shared.json`.
There is no combined/fusion index. Compatible worktrees reuse immutable global
USearch bases under `<global-store>.indexes/<contract>/`, keyed by embedding
profile and native index contract. Each pointer
records exact worktree membership and delta hashes. Membership masks exclude
deleted or foreign occurrences; vectors absent from the base are searched as an
exact worktree delta. Large deltas or insufficient active overlap compact into a
new immutable base. Unchanged membership does not rewrite the pointer or base.
Missing, corrupt, or incompatible derived caches can be rebuilt from authoritative
SQLite vectors without model calls; missing embeddings are a separate provider
operation. F32 vectors use bounds-checked copies into owned memory, not mmap.

An engine holds `<index>.lock` for its lifetime: shared for read-only opens,
exclusive for writers. Contention waits up to 10 seconds before an index-in-use
error; retry after it finishes. Operations needing writes can reopen writable and
retry. SQLite uses WAL and a 30-second busy timeout. See [architecture](implementation.md#architecture-and-code-map)
for transaction and sidecar publication details.

### Descriptions

Description text is optional, but indexing it is always enabled. Source descriptions
are extracted during parsing and take precedence over generated descriptions:

- A standalone contiguous comment group immediately before a declaration attaches
  when only whitespace, with no blank line, separates them. Attributes, decorators,
  exports, and declaration bindings are included in the attachment anchor.
- A trailing comment does not describe the next declaration. Comment-looking text
  inside strings, heredocs, or fenced examples is not treated as a comment; shebangs
  are interpreter directives rather than description prose.
- The first contiguous comment group is the file description if only whitespace
  precedes it. Leading blank lines are allowed. An adjacent first declaration can
  receive the same description; a blank line separates file-only prose from it.
- Comment delimiters and leading block-comment stars are stripped, preserving
  internal prose line breaks. Python functions/classes also use a leading constant
  docstring, with delimiters removed and indentation cleaned. An attached comment
  and docstring combine with a paragraph break.

These rules cover supported code, configuration/markup, and Markdown headings.
Source descriptions for noncallable symbols are searchable too. Structural refresh
saves them without requiring a description provider; normal semantic refresh
prepares embeddings for available source/generated text.

`slopdex generate descriptions` is the sole generation entry point for indexed
file/callable prose. It skips source-described files and callables and all Markdown
files. A source-described file may still have undescribed callables generated,
using its source description as context. The command opens writable, refreshes
normally unless `--no-reindex`, then calls `Engine::generate_descriptions()` on
indexed source. With `--no-reindex`, it skips refresh, but generation still verifies
live HEAD and affected source before publishing descriptions. Updates, searches, maps, and status
never generate indexed descriptions automatically. There is no enable/disable control.

Matching generated artifacts are reused by the full effective request, including
the configured description profile, settings, system instruction, and ordered
conversation messages. Generation settings include provider/model, fallback model,
endpoint, timeout, retry count, and retry delay. Run `generate descriptions` after
such changes to regenerate applicable missing/stale generated prose. Source
descriptions are retained and never replaced by generation.

Description generation uses one conversation per file, starting with the complete
file source and a request for a one-paragraph file description. Its callables are
then described sequentially in source order, each requesting one sentence by
symbol and line range. The system instructions and conversation history remain
an identical prefix across turns so providers can reuse cached input tokens;
different files can run in parallel up to `parallelism`. Cached or saved file
descriptions seed the conversation without another generation request. Previously
saved callable descriptions are retained only while their file context remains
unchanged or a matching request is reused, and multi-line callable
descriptions are flattened for inline display. `status` reports
profiles, description counts, and stale-file-description count.

### Native offline reuse and recovery

`--no-reindex` (engine config `noReindex`) skips refresh and live freshness checks,
with or without Git, even for an empty index. It uses the saved snapshot and its
bound global store, regardless of later working-tree changes. It supports offline
inspection and cross-search of an existing native index; missing derived USearch
files can still be reconstructed locally. It does not make the database read-only or disable all
network operations: uncached query/selector vectors, reranking, `describe`, and
explicit description regeneration may consult artifact caches/S3 or call their
providers. Cached queries can run offline when the matching artifacts/results
exist. Catalog commands still fetch
their catalogs. Use the CLI flag: the CLI overwrites a JSON `noReindex` value with
the flag's value on every invocation.

`map --no-reindex` without `-q` needs neither providers nor sidecar repair. With no
index it parses current files directly; with an existing index it uses saved structure.
With an existing index, adding `-q` may populate name/description/query vectors and repair
the separate symbol channel pointer/base, using a provider when artifacts are missing.
Without an index, map warns on stdout that `-q` is ignored and parses current
files directly, including with `--no-reindex`.
A structure-only index can have search units without vectors; `--no-reindex`
does not prepare those missing vectors. Run a semantic command with normal
refresh to prepare them.

Native index identity includes canonical root and schema. Embedding profiles
(provider/model/dimensions/strategy) are separate projections: changing a model
does not reset structural data, and cached vectors from older profiles remain
available for reuse. Within schema 4, changing the root requires
`--force-reindex --yes-really-rebuild-the-index`. This clears native live records,
non-identity metadata, and search results while retaining the global-store binding
and global artifacts/vectors/snapshots.
A compatible index follows normal refresh even with that flag. It conflicts with
`--no-reindex`. `--rebuild-on-divergence` is accepted with the same confirmation,
but the current engine refreshes the working tree without a divergence gate;
the flag adds no engine behavior.

### Rebuilding an old index

Native schema **4** is a hard cutoff: **all earlier native indexes**, old
`rust_`-prefixed native tables, and TypeScript layouts (including schema 11) are
rejected with instructions to remove the existing SQLite index and rebuild, or
choose a new index path. These incompatible layouts are not imported or migrated;
neither `--force-reindex` nor `--no-reindex` bypasses old-layout rejection.

The `worktrees-v1` default is a fresh namespace. Former `.slopdex/index.sqlite`
and `workspaces/...` indexes and `artifacts-v2.sqlite` are never copied, imported,
or used as fallback seeds. Run `slopdex update` to create the new default index.
For an old explicit database, stop any Slopdex process using it before removing
that SQLite file and its `-wal`/`-shm` companions, then run update with the same
`--index` path. Prefer selecting a new, unused path instead:

```bash
slopdex --index /path/to/new-index.sqlite update
```

Use that same `--index` path on subsequent commands, or save it as `indexPath` in
config. Rebuilding scans current files and obtains embeddings from matching
global/S3 artifacts or the configured providers, including available source
descriptions. Run `generate descriptions` separately to prepare generated prose
where source descriptions are absent; old database artifacts and saved
description settings are not imported.
Shared derived indexes are opened lazily from the new SQLite snapshot.

Global store schema **1** likewise has no compatibility/import path. To select
a fresh `artifactCachePath` for an incompatible global store, also select a new
workspace index path so its binding points to that store. Do not delete a global
store merely to rebuild one worktree: other indexes can depend on it.

Use `slopdex map` to inspect local structure before creating an index. Run
`slopdex update` when ready to prepare an index for search.

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
`.mypy_cache`, `.pytest_cache`, and `target`. Generated dependency lock files are
also excluded at every depth: `*.lock`, `*.lockb`, `*.lockfile`, `*.locked`,
`*.lock.json`, `*.lock.yaml`, `*.lock.yml`, `*-lock.json`, `*-lock.yaml`,
`*-lock.yml`, and ecosystem-specific names such as
`package-lock.json`, `npm-shrinkwrap.json`, `pnpm-lock.yaml`, `packages-lock.json`,
`pylock.toml`, `go.sum`, `Package.resolved`, `.terraform.lock.hcl`, Gleam/Julia
lock manifests, and `dub.selections.json`, plus the contents of `esy.lock/`.
Dependency manifests such as `package.json`,
`Cargo.toml`, and `pyproject.toml` remain eligible. Inclusion globs cannot reopen
these files or pruned directories. Refresh removes deleted/newly excluded entries,
including lock files indexed by an older version.

Read/UTF-8/size failures and parser diagnostics are saved; healthy callable
siblings remain searchable where parsing permits. `maxFileSize` defaults to
1 MiB. `index errors` reports path, message, source mode, and available line
locations. `status` reports `indexingErrorCount` and `failedFileCount`.
Index-using commands warn about saved errors, including with `--no-reindex` and
for separate cross-search targets. `--ignore-errors` silences warnings without
clearing records. Help/version do not inspect diagnostics.

Provider failures abort semantic publication; the structural snapshot and completed
global artifacts remain reusable. Normal refresh rechecks HEAD, dirty provenance,
file selection, ignore policy, candidate metadata, and prepared file hashes before
committing. Rerun after edits settle or provider problems are resolved. This check
is not an atomic filesystem snapshot.

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
description profile supplies both. Description generation is always explicit;
the removed description enablement setting is rejected. Config writes preserve
other unknown properties
and replace the JSON file through a synced temporary file.

Example `.slopdex/config.json`:

```json
{
  "provider": "jina",
  "model": "jina-embeddings-v4",
  "dimensions": 1024,
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
| `symbolDimensions` | Optional native dimension for name-only symbol embeddings; default `min(256, dimensions)`. OpenAI ada uses full dimensions. Separate from content embeddings. |
| `descriptionProvider`, `descriptionModel` | OpenAI / `gpt-5.6-luna`; OpenCode Zen (`opencode`) and Go (`opencode-go`) default to `muse-spark-1.3-contributor`. Corresponding `--description-*` options override them. |
| `descriptionFallbackModel` | Optional same-provider fallback; CLI `--description-fallback-model`. Successful fallback stays active within that provider instance until it fails. |
| `rerankingEnabled`, `rerankerProvider`, `rerankerModel` | Disabled by default; models default to Cohere `rerank-v4.0-pro`, Jina `jina-reranker-v3.5`, OpenAI `gpt-5.6-luna`. |
| `rerankerCandidates` | OpenAI candidate setting, integer `1..100`, default `10`; CLI `--reranker-candidates`. See retrieval formula above. |
| `indexPath`, `include`, `exclude`, `maxFileSize` | Index path, glob arrays, and positive byte limit as described above. |
| `artifactCachePath`, `artifactS3` | Authoritative global SQLite store path (persistently bound to each index) and optional best-effort S3 bucket/region/endpoint/prefix/pathStyle, as described above. |
| `embeddingBatchSize` | Positive batch cap. Defaults/maxima: OpenAI `32`, Jina `64`; larger configured values are capped. Interactive setup defaults to `32`. |
| `embeddingBaseUrl`, `descriptionBaseUrl`, `rerankerBaseUrl` | Operation-specific HTTP(S) endpoint overrides; JSON only. Include the API version, e.g. `http://localhost:8080/v1`. |
| `providerTimeoutMs` | Positive request timeout, default `60000`, capped at `300000`; connect timeout is 10 seconds. |
| `providerMaxRetries` | Integer `0..5`, default `2`, for ordinary retryable HTTP failures. |
| `retryDelayMs` | Nonnegative exponential-backoff base, default `250`; delays and numeric `Retry-After` are capped at 5 seconds. |
| `parallelism` | Defaults to `10`; bounds concurrent embedding batches and file conversations (also `config set parallelism`). Each file's description precedes its sequential callable turns; different files can proceed in parallel. Successful artifacts are persisted immediately. |
| `verbose` | External model-call notices go to stderr once per kind/provider/model per process by default; `true` (also `--verbose`) reports every outgoing attempt, including retries. Kinds are `vectors`, `descriptions`, and `reranking`; notices identify the actual model, including fallback, without credentials, URLs, or input. Construction and cache hits produce no model-call notices. |

Aliases `embeddingProvider`, `embeddingModel`, `embeddingDimensions`, and
`fallbackModel` normalize to their canonical properties; canonical values win.
Embedding profiles include `strategyVersion: "rust-v1"`. Description profiles
identify the configured primary with `strategyVersion: "file-conversation-v3"`,
even while fallback serves requests, and include configured endpoint/fallback
information. Generation reuse includes settings, system instruction, and the
complete ordered effective conversation.
Credentials are not profile inputs. Content embedding profiles are independent
of the description-generator profile.

Embedding URLs may already end in `/embeddings`; Cohere/Jina reranking URLs may
end in `/rerank`. Description and OpenAI reranking bases receive the required
protocol path. OpenAI descriptions use Responses; OpenCode protocol routing is
model-dependent (Responses, Chat Completions, Messages, or Gemini). Overrides
must serve the selected protocol. Live `help models`/interactive config catalog selection uses
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
