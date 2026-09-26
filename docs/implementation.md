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
| `src/parse/mod.rs` | Shared parsing result types, file-language detection, and dispatch to code or Markdown parsing. |
| `src/parse/code.rs` | Tree-sitter callable extraction and diagnostics, byte-preserving TypeScript recovery. |
| `src/parse/markdown.rs` | Heading-aware bounded Markdown chunks, fence and comment handling. |
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
`<root>/.slopdex/config.json` and `<root>/.slopdex/index.sqlite`. Explicit relative
config/index paths, including JSON `indexPath`, resolve against the working
directory. CLI overrides are applied after canonicalizing supported config aliases.
Config saves use a same-directory temporary file, `sync_all`, and rename.

An index command opens an engine, normally refreshes it, then runs the requested
operation. A second cross-search root loads its own config plus the same global
overrides. The CLI recognizes identical source/target database paths (including
symlinks and Unix hard links) and reuses the source engine rather than taking a
second lock. Provider construction validates configuration without making network
requests; credentials are resolved only on a request. Saved description state
and, when neither provider nor model is explicit, the saved description profile
can supply engine defaults.

### SQLite schema and artifacts

SQLite is the authority. Native schema **2** uses six unprefixed tables and the
`items_path` index on `items(path)`.

| Table | Contents |
| --- | --- |
| `metadata` | Identity (canonical root, schema, embedding profile), generation, Git checkpoint, description enabled/profile settings. |
| `files` | Path-keyed serialized file snapshots: source/hash/language, Git or working-tree provenance, file description/hash/vector key, diagnostics. |
| `items` | Callable/Markdown records, stable integer IDs, unique logical identities, metadata/source, code/chunk and optional callable-description vector references. |
| `embeddings` | Content-addressed document/query embeddings stored as little-endian F32 blobs. |
| `cache` | Parsed-file artifacts and generated descriptions, keyed by kind and cache key. |
| `search_cache` | Serialized query and cross-search result rows, separate from reusable model/parse artifacts. |

Callable identity hashes path, qualified name, kind, and same-name occurrence;
Markdown identity hashes path and chunk ordinal. Reconciliation preserves item
IDs across source edits/line shifts when identity is unchanged. Deleting a file
cascades to its items. Foreign keys also link item vectors to the embedding table.

Parse keys contain parser version, path, and source hash. Embedding keys contain
the embedding profile, query/document operation, and full input. Description keys
include the configured generator profile and file or callable source identity;
explicit callable regeneration also includes the enclosing file hash. Completed
artifacts are persisted independently of the final live update. The engine
persists each successful embedding batch before requesting the next, so later
failures do not discard earlier paid batches. Unchanged callable descriptions
and stale file descriptions can survive profile changes until work explicitly
requires their regeneration; a profile change is not a blanket regeneration.

Query-result keys include generation, enabled state, effective config, query,
kind, and options. Cross-search keys include source/target generations, canonical
target database path, scoring kind, resolved changed-since commit, saved checkpoint,
and options. A moving Git branch is resolved before cache lookup and its ancestry
is checked again. Dirty live publication clears the local search-result cache.

This is a clean schema break: databases with the old `rust_`-prefixed tables or
TypeScript layouts are rejected. There is no legacy import or migration, and
`--force-reindex` cannot bypass old-layout rejection. Remove the existing SQLite
index and rebuild with `slopdex update-git`, or select a new database with
`slopdex --index /path/to/new-index.sqlite update-git`. See the
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
2. Compare source hashes and provenance to saved files. Prepare changed/failed
   files and files missing enabled descriptions; remove paths no longer eligible.
   Save reusable parse/model artifacts as they complete. Read/size failures become
   diagnostic file records, and parser errors can coexist with healthy callables.
3. Before publication, verify HEAD is unchanged and re-read every successfully
   prepared file to verify its hash. Failures abort live publication but leave
   completed artifacts available for retry. This is optimistic validation, not an
   atomic filesystem snapshot; unchanged files and discovery are not revalidated
   as a full filesystem transaction.
4. In one SQLite transaction, reconcile live files/items, checkpoint, generation,
   and result-cache invalidation. Generation advances for file/item changes;
   checkpoint-only updates do not increment it. Description enabled/profile
   metadata is written separately after that transaction.
5. Load the committed SQLite snapshot and reconcile the derived USearch indexes.

Git records provenance and a checkpoint, while all indexed source comes from the
working tree. `update-git --target` supports HEAD only. Changed-since selection
parses base-commit files and compares qualified-name/source-hash pairs;
uncommitted selection uses saved file provenance, including unchanged callables
inside dirty files. Without HEAD, files are working-tree records.

Native `noReindex` skips refresh entirely, even for an empty database. Opening can
still write metadata or repair sidecars; it is not read-only.
Offline status/cross-search use the saved native snapshot, while uncached query
embedding/reranking and task descriptions still call providers. The CLI sets
`noReindex` from `--no-reindex`, overriding a JSON value. `reindex-files` is an
explicit operation on saved source snapshots, not a new filesystem scan after
the automatic refresh.

Within the current table layout, an incompatible native identity (such as a
changed root or embedding profile) can be reset with
`--force-reindex --yes-really-rebuild-the-index`: live records, metadata, and result
caches are cleared while reusable artifacts/vectors remain. A compatible identity
is not reset just because the flag is present. The accepted rebuild-on-divergence
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
Eligibility predicates run inside graph traversal. The engine widens retrieval
as needed for threshold ranges, then applies score bounds and result limits.
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
whole indexed files; there is no whole-file-removal retry path.

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
