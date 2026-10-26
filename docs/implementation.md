# Implementation and distribution

Slopdex is a Rust command-line application. The npm package
`@ninjaxtools/slopdex` provides installation and a small JavaScript launcher for
the native executable. See the [README](../README.md) for operator examples and
the [command reference](reference.md) for CLI documentation. Use
`slopdex --help` to inspect the commands supported by the installed executable.

## Architecture and code map

| Location | Responsibility |
| --- | --- |
| `Cargo.toml`, `Cargo.lock` | Cargo workspace, root/default member `slopdex`, binary target and locked workspace dependencies. |
| `xtask/Cargo.toml`, `xtask/src/main.rs` | Workspace verification and release checks; tooling member with `publish = false`, `release = false`, and `dist = false`. |
| `.cargo/config.toml` | `cargo verify` and `cargo release-check` aliases for xtask. |
| `npm/package.json` | npm release metadata, version synchronization and verification against cargo-dist's generated package. |
| `src/main.rs`, `src/lib.rs` | Native entry point/error exit handling, public core modules, shared SHA-256 helper. |
| `src/cli.rs` | Clap commands/validation, root-selected JSON configuration, interactive prompts, summary/JSON/JSONL output, connected-component clusters. |
| `src/ui.rs` | Shared cliclack progress and diagnostic rendering on terminal stderr, plain redirected diagnostics, synchronized provider notices. |
| `src/engine.rs` | Filesystem/Git refresh, artifact reuse, description lifecycle, independent-index search/filtering/reranking, cross-search, and task explanation context. |
| `src/formats.rs` | Central code/docs/config/markup groups, language/extension membership, defaults, and format-selection validation. |
| `src/filter.rs` | Shared format-group selection, ordered path globs, qualified-name regexes, resolved semantic selection, and map kind selection with ancestor context. |
| `src/symbols.rs` | Stable normalization of bare names and queries for name-only symbol embeddings. |
| `src/map.rs` | Compact structure summaries from canonical metadata; display-only truncation. |
| `src/parse/mod.rs` | Shared parsing result types, file-language detection, and dispatch to code, Markdown, or configuration/markup parsing. |
| `src/parse/code.rs` | Tree-sitter callable extraction and diagnostics, byte-preserving TypeScript recovery, one-sweep call-site ownership (callables sorted by start byte become active during the pre-order walk; per-owner set deduplication). |
| `src/parse/structure.rs`, `src/parse/imports.rs` | Canonical declarations, signatures, hierarchy, source ranges, and imported bindings/aliases. |
| `src/parse/syntax.rs` | Shared AST helpers, quote-safe key text, and the `Descent` guard (512 levels or a 768 KiB stack budget, whichever comes first): the recursive code/structure walkers stop descending and record one diagnostic instead of overflowing the stack; simple helpers use explicit stacks. |
| `src/parse/markdown.rs` | Structural heading hierarchy and separate bounded Markdown search chunks, fence and comment handling; heading names collapse whitespace and fall back to `<anonymous@line:col>`. |
| `src/parse/data.rs` | Configuration/markup structure (iterative work-stack walk, no depth limit) and bounded document search chunks. |
| `src/parse/shell.rs` | Bash heredoc/function recovery; expansion boundaries are matched by parsing exponentially growing windows so bodies with thousands of expansions stay linear. |
| `src/callgraph.rs` | Snapshot callable graph and outgoing symbol-reference resolution: language-aware bare-name scoping, `impl`/static receiver matching, namespace-only `.` receivers through imports, Rust `super::` and Go package import paths. |
| `src/parse/descriptions.rs` | Shared source-comment attachment, Python function/class docstrings, and file/symbol description extraction. |
| `src/models.rs`, `src/providers/` | Provider-independent LLM/vector/reranking traits, hosted implementations and local Ollama embeddings, credentials and endpoint overrides, protocol routing, response validation and bounded retries. |
| `src/storage.rs` | Worktree SQLite bindings, attached global artifacts, streamed snapshot digests, transactional live-state reconciliation, schema validation. |
| `src/cache.rs` | Authoritative global SQLite store and validated, best-effort S3 provider artifacts. |
| `src/git.rs`, `src/registry.rs` | Git candidate discovery/checkpoints, repository relatedness evidence, and self-retiring per-artifact single-flight locks. |
| `src/vectors.rs`, `src/vectors/shared.rs` | Checked F32 cosine USearch primitives and immutable shared bases with exact worktree deltas/membership masks. |
| `tests/rust_integration.rs` | Engine/CLI integration coverage using temporary repositories, real SQLite/USearch, and local mock HTTP providers. |
| `target/release/slopdex` | Locally built executable (`slopdex.exe` on Windows). |
| `.github/workflows/rust.yml` | `cargo verify` on Linux, macOS and Windows for main/master pushes, pull requests and manual runs; no Node setup. |
| `dist-workspace.toml` | cargo-dist version, release targets, native runners, GitHub hosting and npm publication configuration. |
| `release.toml` | cargo-release version synchronization, pre-release checks, commit and tag naming. |
| `.github/workflows/release.yml` | Generated cargo-dist workflow: release planning, platform archives/checksums, GitHub Releases and npm publication. |
| `.github/workflows/publish-npm.yml` | Reusable npm trusted-publishing job, called by the generated release workflow with OIDC permissions. |

Rust owns application behavior and local development tooling. cargo-dist generates
the npm installer and JavaScript launcher. The native executable runs directly
without Node; in CI, Node is used only by the trusted npm publishing workflow.

The package exposes the native CLI through its launcher; it has no JavaScript
library entry point or TypeScript declarations. Verification covers the Cargo
workspace. CLI options and output are implemented in Rust.

### Root configuration and execution flow

`cli::run` parses and validates arguments before opening an engine. Help/version
exit through Clap, model listing fetches public catalogs, and configuration
actions read/write JSON without opening SQLite. An explicit `--root` wins;
otherwise the root is the Git checkout root discovered from the process working
directory, falling back to that directory when outside a repository.
Configuration selects an
explicit `--config` first, then an existing `<root>/.slopdex/config.json`, otherwise
`$XDG_CONFIG_HOME/slopdex/config.json` (fallback `~/.config/slopdex/config.json`
when `XDG_CONFIG_HOME` is unset, empty, or not absolute). Only that file is loaded; repository
and user settings are not merged. A missing selected file means defaults.
The index defaults to
`$XDG_CACHE_HOME/slopdex/worktrees-v1/<canonical-root-sha256>/index.sqlite`.
The global store defaults to `$XDG_CACHE_HOME/slopdex/global-v1.sqlite`.
No former default index or cache is copied, migrated, imported, or used as a seed.
Explicit relative
config/index paths, including JSON `indexPath`, resolve against the working
directory. CLI overrides are applied after canonicalizing supported config aliases.
Config updates use a same-directory temporary file, `sync_all`, and rename.
Embedding-only interactive setup offers the selected config versus repository
`.slopdex/config.json`, defaulting to repository settings when they differ;
explicit `--config` bypasses destination selection. Noninteractive setup retains
normal config selection. Settings files are never merged.

`update --setup` saves embedding setup before opening the engine and then follows
the ordinary update path. `config embeddings` invokes the same staged setup
without opening an index. Explicit global embedding flags select noninteractive
setup; only provider/model/dimensions overrides are persisted, not unrelated global
flags. Without setup, embedding flags still apply only to the current invocation.
The focused wizard never prompts for descriptions or reranking and never fetches
their model catalogs. The full wizard also accepts Ollama and prepares embeddings
before saving whenever embedding fields are configured. Both wizards stage alias
normalization and answers so cancellation cannot partially save settings.

Embedding provider/model selection clears stale dependent configuration. Provider
switches reset model/dimensions/embedding endpoint and provider-specific Ollama
settings; model switches invalidate dimensions and resolved digest/native-size/
family/context metadata. Known Qwen3 sizes are 1024/2560/4096 for 0.6b/4b/8b;
explicit CLI dimensions override these defaults. `config set provider/model`
uses the same invalidation rules without calling a provider. Other direct
configuration edits retain generic JSON/dotted-key behavior.

Ollama setup calls `providers::prepare_embeddings(&mut Value, pull)` before any
file write. It checks `/api/tags` and `/api/show`, optionally pulls a missing model,
validates a smoke embedding, and records `dimensions`, `ollamaModelDigest`,
`ollamaNativeDimensions`, and `ollamaContextLength` (the input strategy derives
from the model name at construction; no family is saved). Explicit
`--pull` is restricted to embedding setup with Ollama and persists auto-pull.
Hosted setup does not make network requests or resolve credentials. Installed
embedding-model listing is deferred until the installed-model option is chosen;
presets/custom entry require no listing request. CLI unit tests inject wizard responses and
preparation closures, asserting cancellation/failure never writes config without
using a real server, downloading models, or opening the per-user artifact store.

Ollama provider construction remains offline. Semantic engine opens resolve
missing model identity before cache/profile use; saved digest metadata permits
offline cache reuse. Before its first live embedding request, the provider checks
that the installed model still matches the pinned digest; a changed tag requires
re-setup, not a silent cache identity change. Its `ollama-v1` profile includes
digest, API endpoint, native/output dimensions, context length, and input strategy.
Local inference has no hosted fallback. Description providers and optional S3
artifact sharing remain independent, so local embeddings alone are not a promise
that all configured operations keep source off the network.

Search variants and cross-search validate that the resolved source index exists
before opening an engine; a missing index fails promptly with `slopdex update`
guidance and no provider calls or index/cache artifacts. Cross-search validates
its target index before opening or refreshing the source engine. Existing
indexes still normally refresh before the requested operation. A second
cross-search root selects its own config with the same user-config fallback
(unless `--target-config` is explicit), plus the same global overrides. The CLI
recognizes identical source/target database paths (including
symlinks and Unix hard links) and reuses the source engine rather than taking a
second lock. Provider construction validates configuration without making network
requests; credentials are resolved only on a request. When neither description
provider nor model is explicit, the saved description profile
can supply engine defaults.

Bare `map` supplies an implied `.` path, resolved against the selected root rather
than the invocation directory. It maps the checkout from repository subdirectories
and the working directory outside Git; explicit `--root` is still honored.
If the resolved index is missing, `map` directly parses eligible files and renders
structure without creating SQLite, locks, cache directories, or sidecars. This
fallback applies with `--no-reindex` and without Git; it preserves selectors,
call expansion, expanded source/Markdown rendering, and discovery/size rules.
When `-q` is supplied, the CLI first emits a stderr warning (`ui::warning`) that
there is no active index and `-q` is ignored, keeping stdout machine-readable. It
removes `symbolQuery` and `symbolThreshold`
from the local options before parsing, selection, and expanded heading rendering.
With an existing index, ordinary `map` uses the structure-only open/refresh path,
without provider requests or USearch sidecars; `--no-reindex` reads stored structure.
Map paths are root-relative (or absolute within the root) and select output from
the discovered universe. Semantic refresh subsequently prepares any missing
embeddings, including for files unchanged since an indexed map refresh.

With an existing index, `map -q` opens through `Engine::open_symbol_map`.
It still calls only `refresh_structure`, then lazily resolves semantic selection
over symbols, available descriptions, and heading titles from saved snapshots.
It may prepare name, description, and query embeddings through
providers or caches and open the separate `<index>.symbols.shared.json` channel.
If a read-only engine reports `NeedsWrite` during refresh, map, or selection,
the CLI reopens a writable symbol-map engine and retries; `--no-reindex` keeps
the saved snapshot while allowing selector cache population. Ordinary map does
not initialize a model. `StructureSource::selection` defaults to compiling local
filters, while the engine override resolves semantic selection; both map querying
and expanded heading-body rendering use the resolved selection.

The CLI's `Command::SearchSymbols(QueryArgs)` forwards `search-symbols` to the
engine. Canonical `search-docs` keeps `search-md` as a visible alias; the internal
`Command::SearchMd` variant forwards `search-docs`. `SearchArgs` serializes the
explicit `code`/`descriptions`/`docs`/`symbols` selector family: any true index
flag serializes all four booleans. Its internal `md` field accepts canonical
`--docs` and the visible `--md` alias but emits the `docs` option. Engine callers
may still use the legacy `md` option or `search-md` kind; both select Markdown
content only, never config or markup. Plain search includes the selected groups'
content indexes plus group-filtered shared symbols and available descriptions.
`CommandMode` centralizes
opening and refresh: pure `search-symbols` and `search --symbols` use
`Engine::open_symbol_map(root, index, config, readonly)` and `refresh_structure`,
as semantic map does; mixed content/symbol searches use normal engine refresh.
All search commands require an existing index. Read-only `NeedsWrite` retries
reopen the same mode writable, retaining structural-only refresh for pure symbols.
`--no-reindex` skips refresh but allows lazy name/query cache writes.

Callable graph traversal and outgoing reference annotations are independent.
`--callers`/`--callees` select graph levels, including same-file callees in grouped
declarations; `--expand-callers`/`--expand-callees` can raise those depths and force
indexed code. `--refs none|all|cross-file` filters annotations for conservatively
resolved callable and non-callable targets without adding declarations. Its
implicit default follows the resolved graph depths: `cross-file` when either is
positive, otherwise `none`, including expanded-detail and forced-code defaults.
JSON annotations use `references`; graph metadata keeps caller/callee depths and
`relatedCallables`. Text groups references by file in language-specific comments
directly beneath declarations, without an annotation label.
Serialized structural nodes use `referenceSites` for syntactic uses; resolved
output locations use `references`, so map declarations remain decodable as
structural nodes.

Only general `search` accepts one or more positional queries; dedicated search
variants and `faq` retain their single-query arguments. Multiword queries
must be quoted to remain one query. Each query runs independently through the
existing search/result-cache and optional reranking paths. The CLI OR-merges
results, deduplicating by indexed hit and result type and retaining the complete
best-ranking row by `rerankScore` when reranking is enabled, otherwise `similarity`.
Distinct result types remain independent, even for one declaration. The merged
rows share ranking and one overall output limit; reranker candidate budgets apply
independently per query. Single-query behavior and the result JSON shape remain
unchanged, with no per-query output wrapper.

`Command::Generate { action: GenerateAction::Descriptions }` uses normal content
open/refresh mode and opens writable. After the usual refresh (unless
`--no-reindex`), dispatch calls only `Engine::generate_descriptions()` and prints
its JSON statistics. Indexed description generation is explicit; ordinary refresh,
search, map, and status do not generate descriptions. Source-described files and
callables, and all Markdown files, are excluded from generation. Description text
is optional, but any available source or generated prose is indexed without an
enablement setting. CLI config validation rejects the removed setting.

`formats.rs` defines four groups: programming languages including shell (`code`),
Markdown (`docs`), JSON/YAML/TOML/Terraform/HCL (`config`), and HTML/XML/CSS
(`markup`). The default is `code,docs`; explicit `--formats` replaces the
default and accepts repeated/comma-separated groups or `all`. `SelectionArgs`
emits an array under `formats`; engine options also accept a comma-separated
string. Unknown/empty groups are rejected. Group membership is centralized in the
language/extension mapping, not per-command individual-filename heuristics.
`--config <path>` still selects settings and is independent of result formats.
Discovery and refresh still index all supported groups under their existing
ignore/include/exclude rules. Formats filter query/map eligibility, including
explicit paths and globs, and can be changed with `--no-reindex` against the saved
snapshot. Shared symbols and descriptions use the same group filter; result-cache
keys include the versioned format-group contract.

Shared selectors compile ordered `ignore::overrides` path rules and an ORed Rust
`RegexSet`; `-i` affects regexes only. Positive globs require a match, `!` excludes,
and the last matching rule wins. Query names use callable `qualifiedName` or
Markdown heading paths joined with `.`. Map shares qualified names, qualifies
extra declaration bindings in their enclosing scope, and exposes import paths
and aliases. Its kind/name
matches retain ancestors as context without expanding unmatched children.
Cross-search remains callable-code-only for both sources and neighbors. A format
selection omitting `code` returns no results. Path/name/semantic selectors apply
only to sources, intersecting path/Git selection.

Shared `SelectionArgs` serializes `symbolQuery` only for supplied `-q` values and
`symbolThreshold` only when supplied. Queries are ORed with default minimum
similarity `0.5`, independently of content `--threshold`. `Selection::compile`
validates string queries and a finite scalar threshold in `[-1,1]`; a semantic
selector starts with empty resolved match sets, so unresolved selection cannot
silently select everything. The engine fills name, path-qualified symbol, and file
match sets through `with_semantic_matches`.
Regex matching checks qualified names/aliases and, for functions, methods,
constructors, and generators, their exact source text. Source matching uses saved
snapshot byte ranges (or a callable search unit's saved `source`), without changing
anchors, Unicode, or line endings. It is lexical, including bodies, comments, and
literals; non-callable containers and Markdown keep name-only regex matching.
Both unindexed map's early limit probe and its final selection use this same rule.
Source is loaded lazily only for active regexes. Semantic matching uses normalized
`node.name` or `node.names` and available descriptions.
The two active families are ORed; an absent
family contributes no matches, and with neither supplied all names are eligible.
Path-aware matching shares this composition across callable, Markdown, and
cross-search filters; `symbol_matches_at` also accounts for declaration aliases.
Description matches select a particular symbol or, for a file description, the
file's declarations. Bare heading titles carry no parent semantics; ancestors are
context and receive body text only if directly selected.
`name_matches` stays regex-only and `-i` affects regexes only.

Normalization splits camelCase/PascalCase, acronyms, letter/digit boundaries, and
punctuation into lowercase words, making equivalent identifier spellings share
vectors. Embedding inputs contain names only, without bodies, signatures,
descriptions, or parent scopes. Symbol vectors use 256 native dimensions by
default, capped at configured embedding dimensions, with optional
`symbolDimensions`; OpenAI ada requires its full dimensions. They are derived
lazily from saved structures rather than from content search units. `-q` also
embeds available source/generated descriptions in this selector space, independently
of content embeddings and without invoking description generation.

Source descriptions use one attachment policy across parser backends: a standalone
contiguous comment group immediately before a declaration attaches to it when the
gap contains only whitespace and no blank line. Declaration wrappers such as
attributes, decorators, exports, and variable bindings are taken into account.
Trailing comments do not attach forward. A first comment group preceded only by
whitespace is the file description, even after leading blank lines; it can also
describe an adjacent first declaration. Delimiters and block-comment stars are
stripped while internal prose line breaks remain. Python functions/classes also
use a leading constant docstring; comments and docstrings combine with a paragraph
break. Source descriptions override generated prose and publish with structure,
even before semantic embeddings are prepared.

### SQLite bindings and global artifacts

SQLite is authoritative: worktree schema **4** stores live bindings and local
query results; global schema **1** stores reusable content. These are hard cutoffs,
not migration entry points. Older native/TypeScript layouts and former default
paths are never imported or used as compatibility seeds. See the
[rebuild instructions](reference.md#rebuilding-an-old-index).

| Store | Contents |
| --- | --- |
| Worktree `files` | Path, source hash, language, provenance, parser version, and structure hash; source text lives globally. |
| Worktree `symbols`, `symbol_names`, `search_units` | Canonical declarations/names, file-local hierarchy, stable live occurrence IDs, and search-unit metadata/input/content hashes. |
| Worktree `unit_embeddings`, `descriptions` | Profile/input associations and content/vector references, not duplicated artifact payloads. |
| Worktree `diagnostics`, `metadata`, `search_cache` | Saved failures, root/store binding, checkpoint/generation/policies, profiles, snapshot digest, and derived query results. |
| Global `sources`, `embeddings` | Content-addressed source text and little-endian F32 document/query vectors. |
| Global `cache`, `description_content` | Parse, resolved call-graph, search-unit source/input, and provider artifacts; interned description text, prompts, settings, profiles, and conversation messages. |
| Global `snapshots` | Unused. Retained, and still validated on open, only so binaries that persisted snapshot manifests can share the schema-1 store; nothing writes or reads it now, and `cache prune` empties it. |
| Global registry/upload tables | Repository relatedness evidence and best-effort remote upload bookkeeping. |
| Global `workspaces` | Canonical index/root registrations used for reachability and missing-worktree cleanup. |

The worktree connection attaches the global store as `global`. Both databases are
needed to read a saved snapshot. `artifactCachePath` selects the global store when
creating an index; its canonical path is persisted in the worktree binding.
Omitting the override subsequently retains that binding. A conflicting explicit
override requires a new worktree index path, even with `--force-reindex`.
Unavailable/incompatible global stores are errors, not workspace-cache fallbacks.

Reopening an initialized worktree index is read-only setup: schema, identity,
store binding, and incarnation are only written when tables are missing, the root
is being force-rebuilt, or a stored value differs, so a warm open neither waits for
nor competes with a live writer. Cold initialization (WAL activation and schema
creation) is serialized across processes by an `index-initialize` artifact lock
keyed by the canonical index path, taken before an empty file's schema is read.

The snapshot digest in worktree `metadata.snapshot` is the SHA-256 of a canonical
JSON document of every published binding (`files`, `symbols`, `names`, `units`,
`embeddings`, `descriptions`, `diagnostics`), the selection policy, and the parse
bindings. The document is serialized row by row straight into the hasher; it is
never held in memory or stored. Section order, queries, and column order are part
of the digest contract, because result-cache keys include the digest.

Callable occurrence identity includes path, qualified name, kind, and same-name
ordinal; Markdown identity includes path and chunk ordinal. Reconciliation retains
live integer IDs when occurrence identity is unchanged. Source, description, and
embedding payloads are globally referenced; structural JSON metadata remains where
appropriate. Canonical structure can publish without semantic vectors.

Artifact validity follows exact inputs and contracts:

- Parse keys contain parser version, detected language, and source hash, not path.
- Embedding keys contain the embedding profile, query/document role, and full input.
- Description keys hash `description-request-v1`, the configured LLM profile,
  generation settings, system instruction, and every ordered message role/content
  in the effective request. File prompts include path and complete source; callable
  turns include symbol/line range and the complete preceding conversation. Renames,
  line shifts, surrounding-file changes, or earlier answers can therefore change
  the request key. Saved artifacts retain content-hash references to these inputs.
- Rerankings and explanations use model configuration and complete request context,
  independently of generation-keyed worktree result caches.

Only explicit description generation invokes a generator. Source descriptions take
precedence. Each file's turns are sequential, while a window of files can proceed
in parallel. Identical effective requests are grouped before provider work. Parse,
embedding, description, reranking, and explanation misses acquire per-artifact locks under
`<global-store>.locks`, recheck the global cache, and retain the locks through local
persistence. Embedding batches acquire their deduplicated keys in sorted order
through `registry::lock_paths`. Unrelated keys and
worktrees can proceed concurrently; these are not repository-lifetime locks.
Lock waits are bounded to 30 seconds. Completed artifacts survive failed live
publication and can be reused on retry.

A lock file `<sha256(kind,key)>.lock` exists only while its key is held or awaited.
Releasing unlinks the file under its own lock and then marks the orphaned file
nonempty; a waiter that acquires a marked file, or (on Unix) one whose inode is no
longer the one at its path, retries on the current file. The lock directory is
therefore bounded by in-flight work instead of by every key ever used. The
`store`/`initialize` and `store`/`maintenance` locks are permanent: the former
guards WAL activation and is shared with older binaries; the latter is shared
for every Database/Artifacts lifetime and exclusive during prune. Per-key granularity is
deliberate. Hashing keys into a fixed set of bucket files would also bound the
directory, but ten parallel embedding batches of 32-64 keys would then almost
always share a bucket, serialize behind each other's provider calls, and exceed
the 30-second wait.

Optional S3 is consulted after a global provider-artifact miss. Remote hits are
validated and written to that same store, never imported into duplicate worktree
artifact tables. Missing referenced description content makes a remote answer a
miss. GET concurrency is bounded to 10; uploads are best effort, with conditional
creation for content objects. The `v3` zstd frames contain a checksum and payload;
decompressed size is bounded before checksum validation. Older remote namespaces
are not read. Source/parse storage, registry/Git metadata,
query results, and USearch files are not uploaded. Ordinary map does not fetch S3.
Backfill exports only artifacts belonging to the requesting worktree, not unrelated
repositories' entries in the global store.
Automatic garbage collection is not implemented. `cache::prune` (`slopdex cache
prune`) is the explicit maintenance entry point. Under an exclusive maintenance
lock it inventories registered/custom and default-cache indexes, removes exact
payload files for positively missing roots, and traces live sources, unit-content,
description content (including symbol metadata), and embedding associations.
All provider embeddings, generated descriptions and their generation fragments
are additional roots by default; the independent `--prune-vectors` and
`--prune-descriptions` flags remove only unreferenced results. Explanations,
rerankings, and unknown artifact kinds are retained conservatively. Workspace
tracking is initialized lazily when needed, without legacy pinning or retention
exemptions; obsolete pin bookkeeping is dropped during pruning. Untracked custom
indexes must be reopened writable before pruning or rebuilt afterward at a new
index path. Rebuilding registers the index and may call providers for missing
artifacts. Live referenced payloads from discovered indexes are never swept.
Unreferenced parse/call-graph artifacts, repository evidence, upload
markers, obsolete snapshots, and shared ANN bases are reconstructible cleanup
targets. Index/base locks are never unlinked; publication markers are removed
before base payloads, under the exact base lock. Atomic-write temporaries stay.
It also removes unused artifact locks and truncates the WAL. Deletion uses small
transactions and bounded waits, reporting a busy store rather than hanging. Freed pages
are reported from `freelist_count * page_size` and reused by later artifacts.
`--vacuum` runs `VACUUM` only after checking free space: twice the live data on
the store's filesystem (temporary copy plus WAL) and the live data in SQLite's
temporary directory; otherwise it refuses before rewriting anything.

### Refresh snapshots and locking

Engine lifetime locks at `<index>.lock` are shared in both read-only and writable
engines. `write_index` upgrades only around workspace publication and restores the
shared lock on success or failure. It checks workspace metadata before and after
lock conversion to reject stale prepared work. Required writes wait up to 10
seconds. Optional query-result and vector-pointer publication uses a nonblocking
exclusive attempt and is skipped when readers are active.

Warm writable opens use `Database::open_existing_with_config` without workspace
initialization writes. Cold opens briefly acquire exclusivity and recheck whether
a peer has already initialized the index rather than waiting for that peer's
subsequent reads. Provider work and global artifact preparation retain only the
shared workspace lock. `SharedIndex::prepare` builds or loads the global base
before the short exclusive workspace-pointer publication.

`NeedsWrite` allows CLI operations to reopen writable and retry in the same command
mode without making the whole command exclusive. SQLite uses WAL and foreign keys
for worktree bindings, with a 30-second busy timeout. Read-only opens also attach
the global store read-only. Config-only actions acquire no index lock.

1. Git refresh combines current status, the checkpoint-to-HEAD tree diff, previous
   dirty paths (including restored edits), missing parser structures, and saved
   read failures. Status paths discovery could never select (unsupported types,
   built-in excluded directories and lock files, config `include`/`exclude`,
   modified submodules) are dropped unless already indexed, so they neither count
   as dirty nor defeat the unchanged fast path; ignore-policy files always count.
   Undecodable (non-UTF-8) Git paths are outside every scope. Candidate-subset
   walking prunes unrelated directories while retaining ignore/glob/extension
   policy and forgets remembered directories that no longer exist. Missing
   checkpoints, non-Git/unborn roots, or discovery-policy changes require a full
   scan; branch switches need no ancestry. The walker skips non-UTF-8 names and
   unreadable or vanished entries below the root with one warning per scan.
2. Saved metadata fingerprints skip unchanged candidates in candidate walks and
   full scans alike (the check is independent of discovery policy, so non-Git
   roots and policy changes do not re-read unchanged files); changed candidates
   are read, content-hashed, and parsed through global artifacts. Unchanged
   repeated queries do not hash every indexed file. Saved diagnostics do not
   force work: parser diagnostics are a function of content and parser version,
   so such a file is skipped like any other, while a failed read (no content
   hash) is skipped while its fingerprint and the discovery policy (which
   carries `maxFileSize`) are unchanged and is not republished when a retry
   fails identically. Ignore/config/sparse-checkout policy is
   tracked separately, including Git-ignored policy files in remembered directories;
   there is no filesystem watcher.
3. Normal refresh rechecks HEAD, dirty-path provenance, file selection, policy,
   candidate metadata, and prepared file hashes before publication. This is
   optimistic validation, not an atomic filesystem snapshot. It reconciles structure,
   search units, diagnostics, checkpoint, snapshot digest, and result-cache changes
   transactionally in the worktree database. Global artifacts
   commit first on an independent connection: attached WAL databases do not provide
   cross-database crash atomicity. Rejected publication can leave reusable artifacts,
   never workspace references to uncommitted content. Dirty-path state is conservatively saved before publication so
   an interruption does not lose restore detection. Checkpoint-only updates retain
   the live generation and query results.
4. Semantic refresh prepares missing active-profile associations from saved source,
   including existing descriptions, without generating prose. Completed provider
   artifacts persist before the semantic publication transaction. Loading committed
   records clears in-memory derived indexes, cached file structures, and the
   symbol vocabulary index; channels open only when queried, and one command
   loads each file's structure and the symbol index at most once.

Repository relatedness is separate from validity. `registry.rs` relates linked
worktrees through canonical common Git directory identity and records observed
history roots/full commits as family discovery evidence. Sanitized remote URLs are
hints, never merge keys. Family membership and Git history never substitute for
content hashes, profiles, parser contracts, or exact snapshot membership.

`--no-reindex` skips refresh/live freshness checks and uses the saved snapshot and
bound global store, even after working-tree edits. It is not a read-only/network-off
flag: missing query/selector artifacts, reranking, explanations, and explicit
description generation can consult caches/S3 or call providers. A structure-only
snapshot still needs normal semantic refresh to prepare missing content vectors.
Explicit description generation still verifies HEAD and affected source before
publishing descriptions, even when refresh was skipped.
The CLI overwrites JSON `noReindex` with the flag's value. Unindexed map remains
direct local parsing; search/cross-search require an existing index.

Changing embedding profiles prepares another projection without resetting
structure. A schema-4 root mismatch can be explicitly rebuilt with
`--force-reindex --yes-really-rebuild-the-index`, retaining the global binding and
artifacts. The flag does not reset a compatible root, bypass schema
rejection, or change the store binding. Refresh has no divergence/ancestry gate.

### Shared vector bases and search

The engine lazily supplies `(occurrence ID, embedding key, vector)` snapshots to
`SharedIndex::open`. Each content group has a separate ANN channel and worktree
pointer: `<index>.code.shared.json`, `<index>.docs.shared.json`,
`<index>.config.shared.json`, and `<index>.markup.shared.json`. Shared descriptions
and symbols retain `<index>.descriptions.shared.json` and
`<index>.symbols.shared.json`; selection filters eligible occurrences by group.
Global bases live under `<global-store>.indexes/<contract>/`. The contract includes
the ANN profile, dimensions, native/USearch versions, metric/scoring, and graph
settings. Content ANN profiles add `indexChannel: "content"` and `formatGroup`
to a copy of the provider vector profile; the description channel uses
`indexChannel: "descriptions"`. This prevents a content group from opening a
mixed-group or description ANN base, even when vector dimensions match.
Provider vector profiles and embedding artifact keys are unchanged: identical
paid inputs still reuse the same global/S3 vectors independently of ANN grouping.
Switching formats therefore does not regenerate matching embeddings. There is no
concatenated/fusion index. File descriptions are scored directly.

Bases are immutable and deduplicate embedding keys, mapping them to checked native
IDs rather than worktree occurrence IDs. A pointer records base identity, exact
occurrence-to-embedding membership, delta hashes, and a fingerprint. Membership
masks exclude deleted/foreign base entries; repeated embeddings fan out to eligible
live occurrences. New vectors use an exact worktree delta. A missing/stale pointer
chooses the suitable base with greatest overlap; ties prefer smaller bases, then
base ID. Less than 50% active base overlap or a delta exceeding
`max(64, base-size / 4)` compacts into a new base. Unchanged membership causes no
pointer or base rewrite.

Base publication holds only a per-base lock, rechecks for an existing valid base,
and writes the `.base.json` publication marker last. The lock wait is bounded to
30 seconds; a publisher that times out builds the base in memory, as a read-only
open does, and leaves publication to the holder or a later open. Bases are never
reconciled in place: the native manifest's `generation` is always 0 and remains
only as part of the sidecar format. Native binaries/manifests
retain full checksum, configuration/count/key validation and bounds-checked copied
loads into owned memory, not mmap. Missing/corrupt derived files rebuild from SQLite
vectors without provider calls. Read-only shared-index opens publish no files and
can build an in-memory fallback; missing embedding artifacts are a separate issue.

USearch uses F32 cosine HNSW with connectivity 16, insertion expansion 128, and
search expansion 64. Membership/eligibility filters run inside base traversal;
candidates are rescored with F64 cosine arithmetic. Exact delta hits merge with
base hits before thresholds/limits. Base recall remains approximate: there is no
full-base exhaustive scan fallback. Code/description hits merge by maximum
similarity; cross-search ranks by code, with description scores supplementary.
Symbol search scores normalized names/aliases and returns structural IDs, never
native vocabulary IDs. Reranking applies only to retrieved query candidates,
not cross-search. Detailed result/rendering contracts are in the
[reference](reference.md#reranking-clusters-and-output).

### Provider calls and retry boundaries

The provider layer is blocking; `parallelism` (default `10`) bounds embedding
batches and concurrent file conversations. A file description precedes that file's
sequential callable turns; different files can proceed in parallel. Each successful
HTTP result is cached immediately. External model-call notices go to stderr
once per kind/provider/model per process using thread-safe deduplication;
`verbose: true` / `--verbose` reports every outgoing attempt, including retries.
Kinds are `vectors`, `descriptions`, and `reranking`; notices name the actual
model (including fallback), without credentials, URLs, or input. Construction
and cache hits produce no model-call notices. Operation-specific base URLs/API
keys override provider defaults as described in the
[reference](reference.md#root-configuration-and-provider-overrides). Public
catalog fetching uses its own default HTTP client and public OpenCode endpoints.

OpenAI embedding input is UTF-8-boundary-truncated at 8191 bytes; batches are
capped at 32. Jina uses `code.query`/`code.passage`, server truncation, and a cap of
64. Responses must contain one unique indexed, finite, nonzero vector of the
configured dimensions per input; vectors are normalized before persistence.
Descriptions use separate file/callable requests, with model-family protocol
routing for OpenCode. Task explanations include saved search context and optionally
related callable code from indexed snapshots, within the prompt byte limit.

Ordinary HTTP calls retry retryable transport/read failures and statuses
408/409/425/429/500/502/503/504/529. Defaults are two retries, a 60-second request
timeout, a 10-second connect timeout, and 250 ms exponential backoff. Config can
set 0–5 retries and a request timeout capped at 300 seconds; backoff honors numeric
`Retry-After` with an overall 5-second delay cap. Redirects are disabled; responses
are bounded to 32 MiB. Invalid JSON/validated payload failures are not ordinary
transport retries.

Description failover/empty-output recovery has a separate six-attempt total cap,
without nesting the HTTP retry loop. A successful fallback remains active in that
provider instance; a later eligible failure can switch back. Permanent model
failures are skipped within the current call, and shared-authentication 401 or
redirect failures stop failover. Output truncation (`length`/`max_tokens`/
`MAX_TOKENS`/Responses `incomplete`) and Gemini prompt blocks are permanent for
that model: the same body is never resent, the fallback model gets one attempt,
and without one the call fails immediately. Without fallback/empty output, ordinary
configured retry limits apply. Cache hits bypass provider calls, and a failed
refresh can be rerun to reuse its already committed artifacts.

Hosted endpoint policy lives in `providers/mod.rs`: `check_hosted_endpoint` runs at
construction for embeddings, descriptions, and (lazily) reranking, enforcing TLS
for non-loopback custom hosts and emitting the deduplicated custom-host warning;
`guard_foreign_credential` runs inside credential resolution and refuses
environment/stored keys for repository-configured remote hosts unless trusted.
The CLI loader marks repository-selected settings with the reserved
`configOrigin` key (`cli/config.rs::mark_repository_config`), discarding any
`trustedEndpointHosts` from that file and adopting the user config's list.
Profiles record endpoints through `profile_endpoint`, which drops query strings.

## npm installation

Tagged releases use **cargo-dist 0.32.0**, matching `treesitter-index`. Its generated
`@ninjaxtools/slopdex` npm package downloads the matching native archive from the
GitHub Release during installation. Supported binaries need no Rust/C++ compiler;
they require a compatible host and network access to GitHub. Direct binary
downloads are available from the same release.

`npm/package.json` retains only release metadata: `name`, `version`,
`description`, `license`, `repository`, and `publishConfig` with `access: public`.
It is used to verify cargo-dist's generated npm package. cargo-dist adds the
binary/launcher entries and installer files to the actual package; the generated
`slopdex-npm-package.tar.gz` is what the trusted publishing workflow publishes.
The metadata manifest is not a local installation package, and root `npm install`
is no longer a development or installation entry point. There is no root npm
manifest or lockfile.

## Development and checks

Source builds require Rust (`rustc` and Cargo), a C/C++ compiler and platform
build tools for native dependencies. `Cargo.toml` declares the minimum supported
toolchain as `rust-version = "1.90"`: the sources need let-chains (Rust 1.88,
edition 2024) and the locked `tree-sitter-language` requires 1.90. Current stable
Rust is what CI builds with. Typical setups are Rust via
[rustup](https://rustup.rs/) plus GCC/G++ and Make on Linux, Xcode Command Line
Tools on macOS, or the MSVC toolchain and Visual Studio Build Tools with the
**Desktop development with C++** workload and Windows SDK on Windows.

From the repository root:

```bash
cargo verify
cargo run --locked -- update
cargo run --locked -- search "keep the repository index synchronized"
cargo install --path . --locked
slopdex --version
slopdex --help
```

The root `slopdex` package remains the workspace's default member, so ordinary
`cargo run` commands select the application. The `xtask` member is excluded from
publishing, cargo-release releases and cargo-dist distribution.

`.cargo/config.toml` defines these aliases:

| Command | Expansion |
| --- | --- |
| `cargo verify` | `cargo run --locked --package xtask -- check` |
| `cargo release-check` | `cargo run --locked --package xtask -- release-check` |

`cargo verify` validates the npm package name and checks its version, description,
and license against Cargo metadata, confirms with `cargo package --list` that the
crate ships `README.md`, `LICENSE` and the sources but none of the paths excluded
in `Cargo.toml` (`evals/`, artwork, `.github/`, `npm/`), checks formatting and all
workspace targets, runs Clippy and
workspace tests, then builds the release CLI and smoke-tests `--version` and
`--help`. The CLI smoke checks need no provider credentials or network calls.
`.github/workflows/rust.yml` runs `cargo verify` directly, without Node; a newer
push to the same ref cancels the run it supersedes.

### Tests

`cargo test --locked --workspace` (part of `cargo verify`) needs no network,
credentials or running services: providers are loopback mocks, and every test
keeps its index and artifact store inside its own temporary directory. The
integration fixtures refuse an in-process open that would fall back to the
per-user artifact store.

Tests that need an external service are marked `#[ignore]`, so a normal run
lists them as ignored instead of reporting a pass that exercised nothing. Run
them explicitly with `-- --ignored` and the variable they name; without it they
fail with a message saying what to set:

| Test | Requirement | Command |
| --- | --- | --- |
| `minio_shares_artifacts_across_machine_local_caches_and_survives_outage` | A MinIO server with `minioadmin`/`minioadmin` credentials | `SLOPDEX_MINIO_ENDPOINT=http://127.0.0.1:9000 cargo test --locked --test rust_integration minio_shares_artifacts -- --ignored` |

`cargo test --locked --workspace -- --ignored` runs every such test at once.

Run `cargo verify` before finishing code changes. `AGENTS.md` also instructs
coding agents to run it as their final verification step.

`cargo release-check` runs verification followed by `dist generate --check` and
`dist plan`. It first checks that cargo-dist's `dist` executable is on `PATH`
and otherwise stops immediately, naming the install command for the pinned
version below.

## Building and publishing packages

### GitHub binary releases and npm publication

`dist-workspace.toml` is the source of truth for releases. The setup follows
`treesitter-index`: cargo-dist generates `.github/workflows/release.yml` and builds
with Cargo's `dist` profile (which inherits `release`). Do not hand-edit that
workflow; after changing release configuration, regenerate it with the pinned tool:

```bash
cargo install cargo-dist --locked --version 0.32.0
dist generate
dist generate --check
dist plan
```

The release matrix uses native runners because USearch and Tree-sitter compile
C/C++ dependencies:

| Target | Runner | Archive |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | `slopdex-x86_64-unknown-linux-gnu.tar.xz` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | `slopdex-aarch64-unknown-linux-gnu.tar.xz` |
| `x86_64-apple-darwin` | `macos-15-intel` | `slopdex-x86_64-apple-darwin.tar.xz` |
| `aarch64-apple-darwin` | `macos-15` | `slopdex-aarch64-apple-darwin.tar.xz` |
| `x86_64-pc-windows-msvc` | `windows-2022` | `slopdex-x86_64-pc-windows-msvc.zip` |

Pull requests validate the release plan. Pushing a version tag such as
`v0.20.0` (matching the version in `Cargo.toml`) builds the archives, per-archive
SHA-256 checksums, `sha256.sum`, source archive, and
`slopdex-npm-package.tar.gz`. The workflow creates a GitHub Release with download
links, then publishes the generated npm package as `@ninjaxtools/slopdex`.
Prerelease tags create prerelease GitHub Releases; npm publication is skipped for
prereleases by default.

Before releasing, configure **Trusted Publisher** in the npm settings for
`@ninjaxtools/slopdex`. Choose **GitHub Actions** and enter:

| npm field | Value |
| --- | --- |
| Organization or user | `ninjaxtools` |
| Repository | `slopdex` |
| Workflow filename | `release.yml` |
| Environment name | Leave blank |
| Allowed actions | Enable direct publishing with `npm publish` |

Use **`release.yml`**, the calling workflow, even though the publish command lives
in the reusable `publish-npm.yml`. npm validates the caller's workflow identity.
Both workflows receive `id-token: write` for the publishing job; no `NPM_TOKEN`
secret is required. The job uses Node 24 and npm 11 (at least 11.5.1 is required),
and npm obtains short-lived credentials from GitHub OIDC during `npm publish`.
GitHub release uploads still use the automatically supplied `GITHUB_TOKEN`.

The custom publisher is registered through `publish-jobs` and
`github-custom-job-permissions` in `dist-workspace.toml`, so `dist generate`
preserves this setup. See [npm's trusted publishing guide](https://docs.npmjs.com/trusted-publishers).

### One-command releases

Install cargo-release once (configuration verified with 1.1.5), along with the
pinned cargo-dist version above:

```bash
cargo install cargo-release --locked --version 1.1.5
```

Commit your implementation/configuration changes first and start with a clean
working tree. Preview the release, then execute it:

```bash
cargo release patch --no-publish
cargo release patch --no-publish --execute
```

For example, `patch` advances `0.19.0` to `0.19.1`; use `minor` for `0.20.0`,
or specify an exact version. `release.toml` makes cargo-release:

1. Update `Cargo.toml` and `Cargo.lock`.
2. Synchronize the version in `npm/package.json` (including prerelease versions),
   the only file in the configured version replacements.
3. Run the pre-release hook `['cargo', 'release-check']`: workspace verification,
   generated workflow consistency check, and release plan.
4. Create the release commit and annotated `v<version>` tag, then push to `origin`.

The pushed tag triggers the **Release** workflow, which uploads binaries and
publishes to npm using trusted publishing. `--no-publish` disables publishing to
**crates.io**; it does not disable GitHub/npm publication triggered by that tag.
It is also the repository default. No separate `npm publish` is necessary.
The dry run executes the verification hook but does not bump versions, commit,
tag, or push. A dirty working tree or other failed preflight check must be fixed
before executing a release.

To verify a native archive locally without publishing:

```bash
dist build --artifacts=local --target x86_64-unknown-linux-gnu
```

Artifacts are written to `target/distrib/`. Use your host's target triple when
testing on another platform. Released npm packages fetch these archives rather
than shipping all binaries or falling back to a Rust source build. Unsupported
platforms can build from the checkout.
