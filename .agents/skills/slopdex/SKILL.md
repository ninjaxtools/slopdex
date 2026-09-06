---
name: slopdex
description: Use when indexing TypeScript or JavaScript code, running semantic function search, or finding duplicate function candidates with the slopdex command-line tool.
---

# Slopdex CLI

Use `slopdex` to index named JavaScript and TypeScript callables, search them by meaning, and identify similar or duplicated functions.

## Default Workflow

Run the command that satisfies the user's request immediately. Do not begin with `slopdex status`, `slopdex --help`, executable lookup, API-key probes, or version probes. Slopdex performs its own validation and reports missing credentials or incompatible state.

- For semantic search, run `slopdex search "<query>" --format summary --limit 10`.
- For duplicate candidates, run `slopdex cross-search --format summary --threshold 0.9 --limit 5`.
- For an explicit request to refresh the committed index, run `slopdex update-git`.
- Use `slopdex status` only when the user asks for index metadata or checkpoint information.

Any command that needs a missing index automatically creates and populates it from committed `HEAD`. Let the requested command do this; do not initialize separately. In particular, `slopdex update-git --target HEAD` uses the same initialization path and cannot bypass an automatic-initialization failure.

## Prerequisites

- Run commands from the repository root or pass `--root <path>`.
- Set `OPENAI_API_KEY` or `JINA_API_KEY` for the configured embedding provider.
- Use Node.js 24 or newer.
- Store optional configuration in `.slopdex/config.json`:

```json
{
  "provider": "jina",
  "model": "jina-embeddings-v4",
  "dimensions": 1024,
  "exclude": ["**/fixtures/**"]
}
```

Do not expose API keys in commands, output, configuration files, or commits.

## Indexing

If a CLI command cannot find its source index, Slopdex prints a notice to stderr and automatically creates and populates it from committed `HEAD`. Missing cross-search target indexes are initialized from the target repository's `HEAD` as well. Automatic initialization requires a clean Git worktree.

Index the current committed snapshot:

```bash
slopdex update-git
```

`update-git` requires a clean Git worktree. It aborts for staged, unstaged, or untracked files. Do not commit or stash user changes without permission. Either ask the user to resolve the changes or explicitly index selected working-tree files:

```bash
slopdex update-files src/service.ts src/model.ts
```

Remove deleted files from the index when using explicit updates:

```bash
slopdex delete-files src/removed.ts
```

Index another commit or recover after changing to a divergent branch:

```bash
slopdex update-git --target HEAD
slopdex update-git --target HEAD --rebuild-on-divergence
```

Check index metadata and its Git checkpoint:

```bash
slopdex status
```

Git updates read committed blobs, not working-tree contents. Explicit file updates do not advance the Git checkpoint.

## Failure Handling

- Preserve and report the exact failure from the requested command. Do not retry equivalent initialization commands.
- A dirty-worktree error on first use means automatic Git initialization cannot proceed. Do not commit or stash changes; ask the user to clean the worktree, or use `update-files` only when indexing selected working-tree files actually satisfies the request.
- Provider authentication and configuration failures are actionable as printed. Never display key values, and do not probe whether keys are set unless the error specifically indicates missing credentials and the user asks for diagnosis.
- A bare system error such as `Invalid argument` is a Slopdex/runtime failure, not evidence that a different indexing command is needed. Stop retrying, report the command and error, and recommend diagnosing or updating Slopdex.
- `slopdex --help` is the supported capability reference. There is no `slopdex --version` option; never invoke it.

## Semantic Search

Search indexed functions by intent:

```bash
slopdex search "validate an authenticated session" --limit 10
```

Use compact human-readable output and filter weak results:

```bash
slopdex search "validate an authenticated session" \
  --format summary \
  --threshold 0.8 \
  --limit 10
```

`--threshold` is the minimum raw cosine similarity. `--min-similarity` is an equivalent legacy option; never pass both.

## Duplicate Discovery

Find similar functions within the current index:

```bash
slopdex cross-search --format summary --threshold 0.9 --limit 5
```

Summary output groups matches beneath each source:

```text
src/users.ts :: Users.authenticate
  0.9321  src/session.ts :: validateSession
  0.8475  src/auth.ts :: authenticate
```

Interpret high similarity as a candidate requiring source review, not proof of duplication. Public facade methods, API wrappers, interface implementations, and test doubles often score highly while serving distinct roles.

Use `--threshold <minimum>-<maximum>` for an inclusive similarity range, such as `--threshold 0.85-0.95`. The range is applied before `--limit`.

Same-index search reports each unordered pair once by default. Include both `A -> B` and `B -> A` only when explicitly needed:

```bash
slopdex cross-search --include-symmetric-duplicates
```

Restrict source functions to a file or recursive directory while still matching them against the whole codebase:

```bash
slopdex cross-search --source-path src/services --format summary --threshold 0.9
```

`--source-path` restricts only source functions. It can be combined with `--added-since`.

Restrict source functions to additions relative to a commit:

```bash
slopdex cross-search --added-since origin/main --format summary --threshold 0.9
```

Search against another compatible index:

```bash
slopdex cross-search \
  --target-root /path/to/other/repository \
  --target-index /path/to/other/repository/.slopdex/index.sqlite \
  --format summary \
  --threshold 0.8
```

Cross-index searches require identical provider, model, dimensions, and embedding strategy profiles.

## Output Formats

- `--format summary` is intended for human review and includes file and qualified function names.
- The default `search` output is formatted JSON.
- The default `cross-search` output is JSONL, with one object per source function. Process it as a stream rather than a single JSON array.
- Functions with no matches after threshold filtering are omitted from cross-search output.

Prefer JSON or JSONL when another command will consume the results. Prefer summary output when presenting candidates to a user.

## Common Options

```text
--root <path>                       Repository root
--config <path>                     Configuration file
--index <path>                      SQLite index path
--provider <openai|jina>            Embedding provider
--model <name>                      Embedding model
--dimensions <number>               Embedding dimensions
--limit <number>                    Result limit
--threshold <number>                Minimum raw cosine similarity
--format <json|summary>             Output format
```

Run `slopdex --help` for the complete current option list.
