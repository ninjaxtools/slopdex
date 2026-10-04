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
| `src/engine.rs` | Filesystem/Git refresh, artifact reuse, description lifecycle, search/filtering/fusion/reranking, cross-search, and task explanation context. |
| `src/filter.rs` | Shared ordered path globs, qualified-name regexes, resolved normalized semantic names, and map kind selection with ancestor context. |
| `src/symbols.rs` | Stable normalization of bare names and queries for name-only symbol embeddings. |
| `src/map.rs` | Compact structure summaries from canonical metadata; display-only truncation. |
| `src/parse/mod.rs` | Shared parsing result types, file-language detection, and dispatch to code or Markdown parsing. |
| `src/parse/code.rs` | Tree-sitter callable extraction and diagnostics, byte-preserving TypeScript recovery. |
| `src/parse/structure.rs`, `src/parse/imports.rs` | Canonical declarations, signatures, hierarchy, source ranges, and imported bindings/aliases. |
| `src/parse/markdown.rs` | Structural heading hierarchy and separate bounded Markdown search chunks, fence and comment handling. |
| `src/models.rs`, `src/providers/` | Provider-independent LLM/vector/reranking traits and hosted implementations, credentials and endpoint overrides, protocol routing, response validation and bounded retries. |
| `src/storage.rs` | Authoritative SQLite records, artifact/result caches, transactional live-state reconciliation, schema validation. |
| `src/vectors.rs` | Persistent incremental filtered F32 cosine USearch HNSW indexes and validated sidecar publication/recovery. |
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
`<root>/.slopdex/config.json` and a per-workspace index under the user's XDG
cache (canonical root SHA-256). A compatible old default index is copied with
SQLite backup on first use, including committed WAL changes. Explicit relative
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
requests; credentials are resolved only on a request. Saved description state
and, when neither provider nor model is explicit, the saved description profile
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
It still calls only `refresh_structure`, then lazily resolves semantic name
selection from saved structures. It may prepare name/query embeddings through
providers or caches and build the separate `<index>.symbols.usearch` sidecar.
If a read-only engine reports `NeedsWrite` during refresh, map, or selection,
the CLI reopens a writable symbol-map engine and retries; `--no-reindex` keeps
the saved snapshot while allowing symbol cache population. Ordinary map does
not initialize a model. `StructureSource::selection` defaults to compiling local
filters, while the engine override resolves semantic names; both map querying
and expanded heading-body rendering use the resolved selection.

The CLI's `Command::SearchSymbols(QueryArgs)` forwards `search-symbols` to the
engine. `SearchArgs` adds `symbols` to the explicit `code`/`descriptions`/`md`
selector family: any true stream flag serializes all four booleans. Plain search
keeps its existing content streams, with symbols opt-in. `CommandMode` centralizes
opening and refresh: pure `search-symbols` and `search --symbols` use
`Engine::open_symbol_map(root, index, config, readonly)` and `refresh_structure`,
as semantic map does; mixed content/symbol searches use normal engine refresh.
All search commands require an existing index. Read-only `NeedsWrite` retries
reopen the same mode writable, retaining structural-only refresh for pure symbols.
`--no-reindex` skips refresh but allows lazy name/query cache writes.

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
selector starts with an empty resolved-name set, so unresolved selection cannot
silently select everything. The engine fills the union through `with_symbol_names`.
Regex matching remains qualified-name/alias based, while semantic matching uses
normalized `node.name` or `node.names`. The two families intersect independently,
including when different aliases match. Bare heading titles carry no parent
semantics; ancestors are context and receive body text only if directly selected.
`name_matches` stays regex-only and `-i` affects regexes only.

Normalization splits camelCase/PascalCase, acronyms, letter/digit boundaries, and
punctuation into lowercase words, making equivalent identifier spellings share
vectors. Embedding inputs contain names only, without bodies, signatures,
descriptions, or parent scopes. Symbol vectors use 256 native dimensions by
default, capped at configured embedding dimensions, with optional
`symbolDimensions`; OpenAI ada requires its full dimensions. They are derived
lazily from saved structures rather than from content search units.

### SQLite schema and artifacts

SQLite is the authority. Native schema **3** separates canonical structure,
search units, and model-specific embedding associations into normalized tables.
Compatibility JSON `data` snapshots remain alongside explicit columns in files,
search units, and diagnostics; the layout does not fully deduplicate payloads.

| Table | Contents |
| --- | --- |
| `metadata` | Identity (canonical root and schema), generation, Git checkpoint, active embedding profile, description enabled/profile settings. |
| `files` | Path-keyed source/hash/language, provenance, parser version/structure hash, and compatibility snapshot. |
| `symbols` | File-local declaration IDs/parents, order, kinds, qualified names, complete signatures, source ranges, and metadata. |
| `symbol_names` | Ordered declared/imported names and aliases linked to symbols. |
| `search_units` | Callable/Markdown records with stable integer IDs, unique logical identities, optional symbol links, compatibility data, and embedding-input hashes. |
| `unit_embeddings` | Search-unit vector associations by role, profile, and input hash. |
| `descriptions` | Live file/callable descriptions, source hashes, and optional vector references. |
| `diagnostics` | Per-file read/parse/extraction diagnostics with structured columns and compatibility data. |
| `embeddings` | Content-addressed document/query embeddings stored as little-endian F32 blobs. |
| `cache` | Durable parse and model-generated artifacts, keyed by kind and content-addressed cache key. |
| `description_content` | Unique description generation inputs (system instructions, prompts, settings, profiles, and file-description context) keyed by their content hash; cached answer records reference these hashes. |
| `search_cache` | Serialized query and cross-search result rows, separate from reusable model/parse artifacts. |

`src/cache.rs` adds a second SQLite database under the per-user cache for
content-addressed provider artifacts. On a workspace miss, it reads this shared
cache and optionally S3 before a provider request, then hydrates the workspace
SQLite database. Completed provider work is saved to the workspace first, then
written to the shared local cache and S3 best effort. Remote lookups issue up to
10 concurrent GETs at a time. Description records contain only the answer and
content hashes; distinct generation inputs are stored once in the shared cache's
`artifacts(kind='content')` rows and as S3 content-addressed objects. A remote
hit fetches any missing content objects before publishing the answer locally.
S3 uploads of content objects use conditional creation, avoiding duplicate object
versions when another index has already uploaded the same input. All remote object
values are zstd-compressed in the `v3` namespace; reads bound decompressed size
before checking the uncompressed payload checksum. Workspace and shared SQLite
values retain their existing representation.
No remote object contains workspace item IDs or Git snapshot state. Ordinary map does not
fetch remote artifacts.

Callable identity hashes path, qualified name, kind, and same-name occurrence;
Markdown identity hashes path and chunk ordinal. Reconciliation preserves item
IDs across source edits/line shifts when identity is unchanged. Deleting a file
cascades to its structure and search units. Foreign keys link units to symbols
and vector associations to the embedding table. Structure publication can create
search units with no embeddings; canonical records do not depend on a model.

Parse keys contain parser version, path, and source hash. Embedding keys contain
the embedding profile, query/document operation, and full input. A file description
key hashes only its content hash and system instruction; a callable description
key hashes its qualified symbol name, source hash, file-description text, and
system instruction. Cached answers carry their original path and hashes referencing
the prompt (including source), system instruction, configured generator profile,
and relevant settings as inspectable provenance outside the key. Those inputs are
stored once per content hash. Renames and model changes can therefore
reuse a matching description, including during explicit regeneration. Completed
artifacts are persisted independently of the final live update and result cache,
remaining reusable across generation changes and failed-refresh retries. The
engine persists each successful embedding batch as it completes, so later
failures do not discard earlier paid batches. Unchanged callable descriptions
and stale file descriptions can survive profile changes until work explicitly
requires their regeneration; a profile change is not a blanket regeneration.

Rerankings and task explanations also have durable artifact caches, independent
of the generation-keyed search-result cache. Their keys include the model
configuration and complete query/documents or explanation prompt. An unrelated
structural change can invalidate result rows without repeating those paid calls.

Query-result keys include generation, enabled state, effective config, query,
kind, and options. Cross-search keys include source/target generations, canonical
target database path, scoring kind, resolved changed-since commit, saved checkpoint,
and options. A moving Git branch is resolved before cache lookup and its ancestry
is checked again. Dirty live publication clears the local search-result cache.

This is a hard schema cutoff: all schema-2 databases, old `rust_`-prefixed tables,
and TypeScript layouts are rejected. These incompatible layouts are not imported
or migrated, and `--force-reindex` cannot bypass old-layout rejection. Remove the
existing SQLite index and rebuild with `slopdex update`, or select a new database with
`slopdex --index /path/to/new-index.sqlite update`. See the
[rebuild instructions](reference.md#rebuilding-an-old-index) for the default path.

### Refresh snapshots and locking

The engine acquires a nonblocking exclusive filesystem lock at `<index>.lock`
before opening SQLite and holds it for its entire lifetime, including provider
requests, live publication, vector reconciliation, and searches. Contention fails
with a retry-after-completion message; there is no engine lock-wait/retry loop.
SQLite uses WAL, foreign keys, and a 30-second busy timeout. Config-only actions
do not acquire this index lock.

Refresh proceeds as follows:

1. Read current Git HEAD if available and collect dirty/untracked paths. Walk the
   current filesystem with ignore rules, built-in exclusions, include/exclude
   globs, supported extensions, and the size limit.
2. Compare source hashes, parser versions, and provenance to saved files. Parse
   changed/failed files and remove paths no longer eligible. Save reusable parse
   artifacts as they complete. Read/size failures become
   diagnostic file records, and parser errors can coexist with healthy callables.
3. Before structural publication, verify HEAD is unchanged and re-read every
   successfully parsed file to verify its hash. Failures abort publication but leave
   completed parse artifacts available for retry. This is optimistic validation, not an
   atomic filesystem snapshot; unchanged files and discovery are not revalidated
   as a full filesystem transaction.
4. In one SQLite transaction, reconcile files, symbols, pending search units,
   diagnostics, checkpoint, generation, and result-cache invalidation.
   Generation advances for live-record changes; checkpoint-only updates do not
   increment it. Ordinary map reads this structure without opening sidecars.
5. Semantic refresh prepares missing active-profile embeddings and enabled
   descriptions, including for files unchanged since a map refresh. Each completed
   model artifact is saved immediately. After rechecking HEAD and prepared source
   hashes, a second transaction publishes semantic associations and descriptions.
   Provider or publication failure leaves the committed structure available and
   paid artifacts reusable. Incomplete configured semantic projections cannot be
   searched with `--no-reindex`; refresh finishes their preparation first.
6. Load the committed semantic snapshot and reconcile USearch indexes. Description
   enabled/profile settings are saved separately from live-record publication.

Git records provenance and a checkpoint, while all indexed source comes from the
working tree. `update --target` supports HEAD only. Changed-since selection
parses base-commit files and compares qualified-name/source-hash pairs;
uncommitted selection uses saved file provenance, including unchanged callables
inside dirty files. Without HEAD, files are working-tree records.

Native `noReindex` skips refresh entirely, even for an empty database. Opening can
still write metadata or, for semantic operations, repair sidecars; it is not
read-only. Ordinary indexed map reads saved structure without provider or sidecar
work; unindexed map still parses current files. Search/cross-search require an existing
database even with `--no-reindex`.
Offline status/cross-search use the saved native snapshot, while uncached query
embedding/reranking and task descriptions still call providers. The CLI sets
`noReindex` from `--no-reindex`, overriding a JSON value. `index reindex-files` is an
explicit operation on saved source snapshots, not a new filesystem scan after
the automatic refresh.

Embedding profiles are separate from structural identity. Changing provider,
model, or dimensions selects/prepares a new semantic projection without resetting
structure; older cached profile artifacts remain reusable.

Within schema 3, an incompatible root can be reset with
`--force-reindex --yes-really-rebuild-the-index`: live records, non-identity
metadata, and result caches are cleared while reusable artifacts/vectors remain.
A compatible identity is not reset just because the flag is present. The accepted rebuild-on-divergence
option currently adds no engine behavior; refresh does not enforce checkpoint
ancestry.

### Persistent vector snapshots and search

The engine supplies a complete authoritative `(item ID, vector)` snapshot to
each `VectorIndex::open`. There are code and Markdown indexes at embedding
dimension `D`; complete enabled descriptions additionally produce description
fusion at `2D` and combined fusion at `3D`. Each component is unit-normalized
before concatenation, so cosine of the concatenation is exactly the arithmetic
mean of component cosines, up to F32 rounding. Query fusion repeats the query
unit vector; cross-search compares corresponding code/description/file vectors.
Cross-search falls back to code for the whole comparison unless both sides have
complete descriptions and identical configured description profiles.

USearch uses cosine HNSW, F32 storage, connectivity 16, insertion expansion 128,
and search expansion 64. Data is copied into owned indexes, not memory-mapped.
Eligibility predicates run inside graph traversal. Returned candidates are rescored
with F64 cosine arithmetic over the stored F32 vectors before threshold filtering,
avoiding CPU-specific SIMD approximation errors at score boundaries. The engine
widens retrieval as needed for threshold ranges, then applies score bounds and
result limits.
Symbol hits are scored from normalized bare names/aliases, taking the maximum
similarity per structural node. They include all structural kinds and headings
without content units. The ordinary ranking threshold and limit apply; optional
`symbolQuery` is an additional selector with independent `symbolThreshold`.
Mixed searches retain separate function, Markdown/document, and symbol rows,
then apply global ranking and limit rather than fusing symbol and content scores.
The result contract is `{type:"symbol", symbol:{...StructureNode,path,sourceMode},
similarity, symbolSimilarity}`. Node IDs are file-local; vocabulary/vector IDs
must never reach presentation.

CLI ranked-hit construction reads symbol rows from `symbol`, matching their saved
node by qualified name and range rather than Markdown chunk context. Text combines
annotations when content and symbol rows match one declaration; expanded score
details include `symbolSimilarity`. Expanded direct heading hits obtain their
saved body through `markdown_map_bodies`, leaving ancestors heading-only. Other
symbol kinds use ordinary structure rendering and optional indexed source display.
No body is used to rank pure symbols. Call-graph key lookup excludes noncallable
nodes before expansion; callable symbol JSON rows receive related callable and
callee metadata as function rows do.

Exact fusion does not make HNSW exhaustive: neighbor membership/recall remain
approximate, with no all-pairs/exact-scan fallback. Reranking consumes the
retrieved query candidates; cross-search is never sent to a reranker.

Each `<index>.<kind>.usearch` has a `.manifest.json` containing format/USearch
versions, dimensions, generation, per-ID hashes of original F32 bits, a snapshot
fingerprint, and binary hash. Opening validates the manifest, binary, native
configuration, count, and keys. A valid changed snapshot removes deleted/replaced
keys and adds new/replacement vectors, preserving unchanged graph entries and
reusing deleted slots. Identical snapshots cause no sidecar writes; generation
changes with identical vectors only replace the manifest. Failed reconciliation
or missing/corrupt/incompatible sidecars trigger a rebuild from SQLite vectors.

Publication writes/syncs temporary files and renames the binary first, manifest
last, syncing parent directories on Unix. The pair is not one atomic filesystem
operation: a crash between renames produces a hash mismatch and recovery rebuild
on next open. SQLite has already committed, so it always supplies the authoritative
snapshot for that repair; no model request is needed. The engine's lifetime lock
prevents another Slopdex command from observing interleaved publication.

### Provider calls and retry boundaries

The provider layer is blocking; the engine bounds concurrent embedding batches
and callable descriptions within each file using `parallelism` (default `10`).
File descriptions run first, files are processed serially, and each successful
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
