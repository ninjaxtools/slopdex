# Implementation and distribution

Slopdex is a Rust command-line application. The npm package
`@ninjaxtools/slopdex` provides installation and a small JavaScript launcher for
the native executable. See the [README](../README.md) for operator examples and
the [command reference](reference.md) for CLI documentation. Use
`slopdex --help` to inspect the commands supported by the installed executable.

## Architecture and code map

| Location | Responsibility |
| --- | --- |
| `Cargo.toml`, `Cargo.lock` | Root Rust package `slopdex`, version `0.19.0`, binary target and locked native dependencies. |
| `src/main.rs`, `src/lib.rs` | Native entry point/error exit handling, public core modules, shared SHA-256 helper. |
| `src/cli.rs` | Clap commands/validation, root-selected JSON configuration, interactive prompts, summary/JSON/JSONL output, connected-component clusters. |
| `src/engine.rs` | Filesystem/Git refresh, artifact reuse, description lifecycle, search/filtering/fusion/reranking, cross-search, and task explanation context. |
| `src/parser.rs` | Tree-sitter callable extraction and diagnostics, byte-preserving TypeScript recovery, heading-aware bounded Markdown chunks. |
| `src/providers.rs` | Blocking hosted embeddings/descriptions/rerankers, credentials and endpoint overrides, protocol routing, response validation and bounded retries. |
| `src/storage.rs` | Authoritative SQLite records, artifact/result caches, transactional live-state reconciliation, compatible legacy artifact import. |
| `src/vectors.rs` | Persistent incremental filtered F32 cosine USearch HNSW indexes and validated sidecar publication/recovery. |
| `tests/rust_integration.rs` | Engine/CLI integration coverage using temporary repositories, real SQLite/USearch, and local mock HTTP providers. |
| `target/release/slopdex` | Locally built executable (`slopdex.exe` on Windows). |
| `scripts/native-common.mjs` | Package-relative paths, OS/CPU/libc selection and native version checks. |
| `scripts/native-install.mjs` | npm install lifecycle: validate an existing executable, select a bundled prebuilt or compile the bundled sources. |
| `scripts/native-launcher.mjs` | npm `slopdex` entry point; forwards arguments, working directory, environment, standard streams, exit status and termination signals. |
| `scripts/native-stage.mjs` | Copy a verified host release build into the prebuilt distribution layout. |
| `scripts/native-test.mjs` | Node built-in test runner coverage for installation and launching. |
| `scripts/smoke.mjs` | Offline checks of the real native CLI's version and help from outside the package directory. |
| `.github/workflows/rust.yml` | Rust/npm checks on Linux, macOS and Windows for main-branch pushes, pull requests and manual runs. |
| `dist-workspace.toml` | cargo-dist version, release targets, native runners, GitHub hosting and npm publication configuration. |
| `.github/workflows/release.yml` | Generated cargo-dist workflow: release planning, platform archives/checksums, GitHub Releases and npm publication. |

Rust owns application behavior. Node is used only for npm installation and
process launching; these scripts use Node built-ins and have no npm dependencies.
The native executable can also be invoked directly without Node.

The package exposes the native CLI through its launcher; it has no JavaScript
library entry point or TypeScript declarations. Application checks run against
the root Cargo package. CLI options and output are implemented in Rust.

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

### SQLite namespace and artifacts

SQLite is the authority; native tables use the `rust_` namespace so they can
coexist with original legacy tables. The native identity schema is **1**,
independent of the legacy TypeScript schema **11**.

| Table | Contents |
| --- | --- |
| `rust_metadata` | Identity (canonical root, schema, embedding profile), generation, Git checkpoint, description enabled/profile settings, legacy-import marker. |
| `rust_files` | Path-keyed serialized file snapshots: source/hash/language, Git or working-tree provenance, file description/hash/vector key, diagnostics. |
| `rust_items` | Callable/Markdown records, stable integer IDs, unique logical identities, metadata/source, code/chunk and optional callable-description vector references. |
| `rust_embeddings` | Content-addressed document/query embeddings stored as little-endian F32 blobs. |
| `rust_cache` | Parsed-file artifacts, generated descriptions, and imported legacy description artifacts, keyed by kind and cache key. |
| `rust_search_cache` | Serialized query and cross-search result rows, separate from reusable model/parse artifacts. |

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

Legacy import accepts nonempty `metadata` only at schema 11. It copies description
settings independently of embedding compatibility. Matching provider/model/
dimensions permit reuse of compatible document vectors and file/callable
descriptions; OpenAI inputs over 8191 bytes are not vector-equivalent under the
native truncation policy. Imported artifacts and the import marker commit in one
transaction. Original tables remain intact and `vec0` is never loaded or mutated.
Refresh builds native live records from current files; import alone does not
populate a legacy live snapshot for offline searches.

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
still import artifacts, write metadata, or repair sidecars; it is not read-only.
Offline status/cross-search use the saved native snapshot, while uncached query
embedding/reranking and task descriptions still call providers. The CLI sets
`noReindex` from `--no-reindex`, overriding a JSON value. `reindex-files` is an
explicit operation on saved source snapshots, not a new filesystem scan after
the automatic refresh.

An incompatible native identity can be reset with the confirmed force-reindex
flag: live records, metadata, and result caches are cleared while reusable
artifacts/vectors remain. A compatible identity is not reset just because the
flag is present. The accepted rebuild-on-divergence option currently adds no
engine behavior; refresh does not enforce checkpoint ancestry.

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

The checkout's `package.json` and `scripts/native-*.mjs` remain available for local
development and source installation. Their packaging behavior is described below;
the release workflow publishes cargo-dist's generated npm package instead.

### Installing from the checkout or a locally packed tarball

From the repository root, install the checkout globally with:

```bash
npm run install:local
slopdex --version
slopdex --help
```

This development package requires Node.js 24 or newer and includes `Cargo.toml`, `Cargo.lock`
and `src/` so installation also works when no prebuilt is bundled for the host.
The installer checks executable versions against `package.json`. It reuses a
matching local executable, otherwise probes the matching prebuilt and copies it
to `target/release/`. If a prebuilt is absent, has the wrong version or cannot run
on the host, it executes:

```bash
cargo build --locked --release --bin slopdex \
  --manifest-path <package>/Cargo.toml --target-dir <package>/target
```

**Source installation requires Rust (`rustc` and Cargo), a C/C++ compiler and
platform build tools.** Native Rust dependencies include compiled C/C++ code.
Use current stable Rust unless `Cargo.toml` specifies a newer minimum. Common
toolchain setups are:

- Linux: Rust via [rustup](https://rustup.rs/), plus GCC/G++ and Make (for example,
  `build-essential` on Debian/Ubuntu).
- macOS: Rust via rustup and Xcode Command Line Tools (`xcode-select --install`).
- Windows: Rust's MSVC toolchain and Visual Studio Build Tools with the
  **Desktop development with C++** workload and Windows SDK, available in the
  shell running npm.

Source builds need access to the Cargo registry unless dependencies are already
cached, and sufficient time and disk space for a release build. Compilation
errors fail npm installation with the compiler output and toolchain requirements.
A usable bundled prebuilt needs no Rust/C++ toolchain and triggers no download.
Prebuilts are selected from the package itself; the installer does not fetch
executables from GitHub or a separate service.

Install scripts must be enabled for automatic setup. After an installation with
`--ignore-scripts`, run `node scripts/native-install.mjs` from the installed
package directory, or reinstall with scripts enabled. The launcher can directly
run an executable bundled for the host but does not compile on first invocation.
An unusable executable reports how to repair the installation.

## Development and checks

Install the native build prerequisites above, then:

```bash
npm install --ignore-scripts  # npm tooling has no dependencies; defer native build
npm run check
npm run dev -- --help
npm run install:local
```

| npm command | Native command / purpose |
| --- | --- |
| `npm run fmt` | `cargo fmt --all` |
| `npm run fmt:check` | `cargo fmt --all -- --check` |
| `npm run typecheck` | `cargo check --locked --all-targets` |
| `npm run clippy` | `cargo clippy --locked --all-targets -- -D warnings` |
| `npm test`, `npm run test:run` | `cargo test --locked` |
| `npm run test:npm` | Installer/launcher tests using Node's test runner. |
| `npm run build` | `cargo build --locked --release --bin slopdex` |
| `npm run smoke` | Real executable version and help; no provider credentials or network calls. |
| `npm run dev -- <arguments>` | `cargo run --locked --bin slopdex -- <arguments>` |
| `npm run check` | Format check, Cargo check, Clippy, Rust tests, npm tests, release build and smoke. |
| `npm run install:local` | Release build followed by `npm install -g .`. |

The npm installer fixes the target directory to `<package>/target` and removes
`CARGO_BUILD_TARGET` from its build environment. Use a host-native Cargo
configuration when installing; user-level Cargo cross-compilation settings can
still affect builds. For developer commands, Cargo's normal environment and
configuration apply; the npm launcher expects a host build in `target/release/`.

Installer tests use executable/compiler fixtures to check prebuilt selection,
source fallback, version mismatches, missing compilers, failed builds, paths with
spaces, global npm bin links, argument/environment/stream forwarding and signals.
POSIX fixture tests are skipped on Windows; CI additionally builds and smoke-tests
the actual Windows executable. Application behavior belongs in the Rust tests.

## Building and publishing packages

A source-only npm tarball is supported:

```bash
npm pack                    # prepack builds Rust and runs native CLI smoke
```

To bundle a prebuilt, build and stage it on its actual host:

```bash
npm run build
node scripts/native-stage.mjs
npm pack
```

The distribution layout is `native/<platform-key>/slopdex` (or `slopdex.exe`).
Keys use Node's OS and architecture names, with a libc suffix on Linux:

```text
native/linux-x64-gnu/slopdex
native/linux-arm64-gnu/slopdex
native/linux-x64-musl/slopdex
native/darwin-x64/slopdex
native/darwin-arm64/slopdex
native/win32-x64/slopdex.exe
```

Only keys actually staged are shipped by a local `npm pack`. Linux GNU and musl
binaries are never interchanged. This local installer can fall back to compiling
the bundled sources when no runnable prebuilt exists. Do not stage cross-compiled
output under the build host's key.

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

Before releasing, add **`NPM_TOKEN`** to this repository's GitHub Actions secrets.
The token needs publish access to `@ninjaxtools/slopdex`. GitHub release uploads use
the workflow's automatically supplied `GITHUB_TOKEN` with `contents: write`.

Release procedure:

1. Choose an unused version and update the Cargo/npm manifests and lockfiles.
2. Run `npm run check`, `dist generate --check`, and `dist plan`.
3. Commit the release changes, including the generated workflow and dist config.
4. Create and push the matching `v<version>` tag. The **Release** workflow handles
   binary uploads and npm publication; a separate `npm publish` is unnecessary.

To verify a native archive locally without publishing:

```bash
dist build --artifacts=local --target x86_64-unknown-linux-gnu
```

Artifacts are written to `target/distrib/`. Use your host's target triple when
testing on another platform. Released npm packages fetch these archives rather
than shipping all binaries or falling back to a Rust source build. Unsupported
platforms can build from the checkout.

Keep the version in `package.json`, `package-lock.json`, `Cargo.toml` and
`Cargo.lock` synchronized before tagging. Native version checks reject a mismatch.
After npm metadata changes, regenerate its lockfile with:

```bash
npm install --package-lock-only --ignore-scripts
```

`target/`, staged `native/` files and npm tarballs are ignored by Git. npm's
explicit files list includes the staged binaries and Rust sources while leaving
build caches out of the published package.
