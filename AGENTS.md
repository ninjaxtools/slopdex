# Project checks

- after making code changes, run `cargo verify` from the repository root

# Test isolation

- Tests that open an Engine, Database, or Artifacts store must use an `artifactCachePath` inside their own `tempfile::TempDir` (or reopen an index already bound to that store). Never use the real per-user cache or mutate process-wide cache environment variables in parallel tests; sharing tests may share a store only within their own fixture. Isolate CLI subprocesses through configuration or per-child cache environment variables.
