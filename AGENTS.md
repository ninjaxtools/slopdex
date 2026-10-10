# Project checks

- after making code changes, run `cargo verify` from the repository root

# Test isolation

- Tests that open an Engine, Database, or Artifacts store must use an `artifactCachePath` inside their own `tempfile::TempDir` (or reopen an index already bound to that store). Never use the real per-user cache or mutate process-wide cache environment variables in parallel tests; sharing tests may share a store only within their own fixture. Isolate CLI subprocesses through configuration or per-child cache environment variables.

# Cross-platform filesystem tests

- Temporary directories may have symlinked ancestors (notably macOS `/var` → `/private/var`), while subprocess `current_dir()` reports the physical path. Canonicalize an existing fixture base before deriving paths used in subprocess path assertions, including paths for files that do not exist yet. When testing filesystem identity rather than literal path spelling, compare canonical paths; do not change CLI path semantics just to satisfy an assertion.
- Never compare serialized filesystem paths as raw strings/JSON when the assertion is about the selected file. Canonicalize **both** reported and expected paths after the file exists (use `assert_cli_config_path` for CLI config results). Canonicalizing only the fixture is insufficient: Windows canonicalization adds a `\\?\` verbatim prefix, subprocess cwd may omit it, and ordinary paths may mix `/` and `\`. Do not strip prefixes or replace separators in production merely to make tests pass; retain literal comparisons only for an explicit path-spelling contract. Add Windows coverage for ordinary drive paths, verbatim paths, and mixed separators when changing path assertions or resolution.
- Cover cwd-relative path resolution through a deliberately symlinked temporary directory on Unix so Linux runs catch these portability regressions too. Keep environment and cwd changes scoped to child processes, not the parallel test process.

# Shared-store concurrency

- Changes to shared-store opening, journal mode, or schema initialization must cover simultaneous cold opens and warm opens while another connection holds a write transaction. SQLite's busy timeout does not make lock upgrades safe: serialize initialization across processes using canonical paths, and keep already-initialized WAL opens free of unnecessary write transactions.
- Put test start barriers before fallible setup, use bounded waits for completion, and join every worker before propagating errors. Do not hide concurrency failures by serializing the test suite or adding retries to test assertions.
