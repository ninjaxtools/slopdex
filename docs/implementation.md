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
| `src/filter.rs` | Shared ordered path globs, qualified-name regexes, resolved semantic selection, and map kind selection with ancestor context. |
| `src/symbols.rs` | Stable normalization of bare names and queries for name-only symbol embeddings. |
| `src/map.rs` | Compact structure summaries from canonical metadata; display-only truncation. |
| `src/parse/mod.rs` | Shared parsing result types, file-language detection, and dispatch to code or Markdown parsing. |
| `src/parse/code.rs` | Tree-sitter callable extraction and diagnostics, byte-preserving TypeScript recovery. |
| `src/parse/structure.rs`, `src/parse/imports.rs` | Canonical declarations, signatures, hierarchy, source ranges, and imported bindings/aliases. |
| `src/parse/markdown.rs` | Structural heading hierarchy and separate bounded Markdown search chunks, fence and comment handling. |
| `src/parse/descriptions.rs` | Shared source-comment attachment, Python function/class docstrings, and file/symbol description extraction. |
| `src/models.rs`, `src/providers/` | Provider-independent LLM/vector/reranking traits and hosted implementations, credentials and endpoint overrides, protocol routing, response validation and bounded retries. |
| `src/storage.rs` | Worktree SQLite bindings, attached global artifacts, immutable snapshot manifests, transactional live-state reconciliation, schema validation. |
| `src/cache.rs` | Authoritative global SQLite store and validated, best-effort S3 provider artifacts. |
| `src/git.rs`, `src/registry.rs` | Git candidate discovery/checkpoints, repository relatedness evidence, and per-artifact single-flight locks. |
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
actions read/write JSON without opening SQLite. The root defaults to the process
working directory, with no upward Git-root discovery. Default paths are
`<root>/.slopdex/config.json` and
`$XDG_CACHE_HOME/slopdex/worktrees-v1/<canonical-root-sha256>/index.sqlite`.
The global store defaults to `$XDG_CACHE_HOME/slopdex/global-v1.sqlite`.
No former default index or cache is copied, migrated, imported, or used as a seed.
Explicit relative
config/index paths, including JSON `indexPath`, resolve against the working
directory. CLI overrides are applied after canonicalizing supported config aliases.
Config saves use a same-directory temporary file, `sync_all`, and rename.

Search variants and cross-search validate that the resolved source index exists
before opening an engine; a missing index fails promptly with `slopdex update`
guidance and no provider calls or index/cache artifacts. Cross-search validates
its target index before opening or refreshing the source engine. Existing
indexes still normally refresh before the requested operation. A second
cross-search root loads its own config plus the same global overrides. The CLI
recognizes identical source/target database paths (including
symlinks and Unix hard links) and reuses the source engine rather than taking a
second lock. Provider construction validates configuration without making network
requests; credentials are resolved only on a request. When neither description
provider nor model is explicit, the saved description profile
can supply engine defaults.

If the resolved index is missing, `map` directly parses eligible files and renders
structure without creating SQLite, locks, cache directories, or sidecars. This
fallback applies with `--no-reindex` and without Git; it preserves selectors,
call expansion, expanded source/Markdown rendering, and discovery/size rules.
When `-q` is supplied, the CLI first writes a warning to stdout that there is no
active index and `-q` is ignored. It removes `symbolQuery` and `symbolThreshold`
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
engine. `SearchArgs` adds `symbols` to the explicit `code`/`descriptions`/`md`
selector family: any true index flag serializes all four booleans. Plain search
includes all indexes, including symbols and available descriptions.
`CommandMode` centralizes
opening and refresh: pure `search-symbols` and `search --symbols` use
`Engine::open_symbol_map(root, index, config, readonly)` and `refresh_structure`,
as semantic map does; mixed content/symbol searches use normal engine refresh.
All search commands require an existing index. Read-only `NeedsWrite` retries
reopen the same mode writable, retaining structural-only refresh for pure symbols.
`--no-reindex` skips refresh but allows lazy name/query cache writes.

`Command::Generate { action: GenerateAction::Descriptions }` uses normal content
open/refresh mode and opens writable. After the usual refresh (unless
`--no-reindex`), dispatch calls only `Engine::generate_descriptions()` and prints
its JSON statistics. Indexed description generation is explicit; ordinary refresh,
search, map, and status do not generate descriptions. Source-described files and
callables, and all Markdown files, are excluded from generation. Description text
is optional, but any available source or generated prose is indexed without an
enablement setting. CLI config validation rejects the removed setting.

Shared selectors compile ordered `ignore::overrides` path rules and an ORed Rust
`RegexSet`; `-i` affects regexes only. Positive globs require a match, `!` excludes,
and the last matching rule wins. Query names use callable `qualifiedName` or
Markdown heading paths joined with `.`. Map shares qualified names, qualifies
extra declaration bindings in their enclosing scope, and exposes import paths
and aliases. Its kind/name
matches retain ancestors as context without expanding unmatched children.
Cross-search applies selectors only to sources, intersecting path/Git selection.

Shared `SelectionArgs` serializes `symbolQuery` only for supplied `-q` values and
`symbolThreshold` only when supplied. Queries are ORed with default minimum
similarity `0.5`, independently of content `--threshold`. `Selection::compile`
validates string queries and a finite scalar threshold in `[-1,1]`; a semantic
selector starts with empty resolved match sets, so unresolved selection cannot
silently select everything. The engine fills name, path-qualified symbol, and file
match sets through `with_semantic_matches`.
Regex matching remains qualified-name/alias based, while semantic matching uses
normalized `node.name` or `node.names` and available descriptions.
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
| Global `snapshots` | Immutable content-addressed manifests of published bindings and parser/profile/policy contracts. |
| Global registry/upload tables | Repository relatedness evidence and best-effort remote upload bookkeeping. |

The worktree connection attaches the global store as `global`. Both databases are
needed to read a saved snapshot. `artifactCachePath` selects the global store when
creating an index; its canonical path is persisted in the worktree binding.
Omitting the override subsequently retains that binding. A conflicting explicit
override requires a new worktree index path, even with `--force-reindex`.
Unavailable/incompatible global stores are errors, not workspace-cache fallbacks.

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
persistence. Embedding batches acquire keys in sorted order. Unrelated keys and
worktrees can proceed concurrently; these are not repository-lifetime locks.
Lock waits are bounded to 30 seconds. Completed artifacts survive failed live
publication and can be reused on retry.

Optional S3 is consulted after a global provider-artifact miss. Remote hits are
validated and written to that same store, never imported into duplicate worktree
artifact tables. Missing referenced description content makes a remote answer a
miss. GET concurrency is bounded to 10; uploads are best effort, with conditional
creation for content objects. The `v3` zstd frames contain a checksum and payload;
decompressed size is bounded before checksum validation. Older remote namespaces
are not read. Source/parse storage, snapshot manifests, registry/Git metadata,
query results, and USearch files are not uploaded. Ordinary map does not fetch S3.
Backfill exports only artifacts belonging to the requesting worktree, not unrelated
repositories' entries in the global store.
Automatic garbage collection is not implemented.

### Refresh snapshots and locking

Engine lifetime locks at `<index>.lock` are shared for read-only opens and exclusive
for writers, with a bounded 10-second contention wait. `NeedsWrite` allows CLI
operations to reopen writable and retry in the same command mode. SQLite uses WAL,
foreign keys for worktree bindings, and a 30-second busy timeout; read-only opens
also attach the global store read-only. Config-only actions acquire no index lock.

1. Git refresh combines current status, the checkpoint-to-HEAD tree diff, previous
   dirty paths (including restored edits), missing parser structures, and saved
   failures. Candidate-subset walking prunes unrelated directories while retaining
   ignore/glob/extension policy. Missing checkpoints, non-Git/unborn roots, or
   discovery-policy changes require a full scan; branch switches need no ancestry.
2. Saved metadata fingerprints skip unchanged candidates; changed candidates are
   read, content-hashed, and parsed through global artifacts. Unchanged repeated
   queries do not hash every indexed file. Ignore/config/sparse-checkout policy is
   tracked separately, including Git-ignored policy files in remembered directories;
   there is no filesystem watcher.
3. Normal refresh rechecks HEAD, dirty-path provenance, file selection, policy,
   candidate metadata, and prepared file hashes before publication. This is
   optimistic validation, not an atomic filesystem snapshot. It reconciles structure,
   search units, diagnostics, checkpoint, snapshot digest, and result-cache changes
   transactionally in the worktree database. Global artifacts and snapshot manifests
   commit first on an independent connection: attached WAL databases do not provide
   cross-database crash atomicity. Rejected publication can leave reusable artifacts,
   never workspace references to uncommitted content. Dirty-path state is conservatively saved before publication so
   an interruption does not lose restore detection. Checkpoint-only updates retain
   the live generation and query results.
4. Semantic refresh prepares missing active-profile associations from saved source,
   including existing descriptions, without generating prose. Completed provider
   artifacts persist before the semantic publication transaction. Loading committed
   records clears in-memory derived indexes; channels open only when queried.

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
artifacts/snapshots. The flag does not reset a compatible root, bypass schema
rejection, or change the store binding. Refresh has no divergence/ancestry gate.

### Shared vector bases and search

The engine lazily supplies `(occurrence ID, embedding key, vector)` snapshots to
`SharedIndex::open` for code, Markdown/document content, descriptions, and symbols.
Worktree channel pointers are `<index>.<kind>.shared.json`; global bases live under
`<global-store>.indexes/<contract>/`. The contract includes the embedding profile,
dimensions, native/USearch versions, metric/scoring, and graph settings. There is
no concatenated/fusion index. File descriptions are scored directly.

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
and writes the `.base.json` publication marker last. Native binaries/manifests
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
redirect failures stop failover. Without fallback/empty output, ordinary configured
retry limits apply. Cache hits bypass provider calls, and a failed refresh can be
rerun to reuse its already committed artifacts.

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
build tools for native dependencies. Use current stable Rust unless `Cargo.toml`
specifies a newer minimum. Typical setups are Rust via
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
and license against Cargo metadata, checks formatting and all workspace targets, runs Clippy and
workspace tests, then builds the release CLI and smoke-tests `--version` and
`--help`. The CLI smoke checks need no provider credentials or network calls.
`.github/workflows/rust.yml` runs `cargo verify` directly, without Node.

Run `cargo verify` before finishing code changes. `AGENTS.md` also instructs
coding agents to run it as their final verification step.

`cargo release-check` runs verification followed by `dist generate --check` and
`dist plan`. Install the pinned cargo-dist version below before running release
checks.

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
