# Project checks

- after making code changes, run `cargo verify` from the repository root

# Test isolation

- Tests that open an Engine, Database, or Artifacts store must use an `artifactCachePath` inside their own `tempfile::TempDir` (or reopen an index already bound to that store). Never use the real per-user cache or mutate process-wide cache environment variables in parallel tests; sharing tests may share a store only within their own fixture. Isolate CLI subprocesses through configuration or per-child cache environment variables.

# Shared-store concurrency

- Changes to shared-store opening, journal mode, or schema initialization must cover simultaneous cold opens and warm opens while another connection holds a write transaction. SQLite's busy timeout does not make lock upgrades safe: serialize initialization across processes using canonical paths, and keep already-initialized WAL opens free of unnecessary write transactions.
- Put test start barriers before fallible setup, use bounded waits for completion, and join every worker before propagating errors. Do not hide concurrency failures by serializing the test suite or adding retries to test assertions.
