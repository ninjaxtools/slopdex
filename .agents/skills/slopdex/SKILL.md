---
name: slopdex
description: Use when indexing TypeScript or JavaScript code, running semantic function search, finding duplicate function candidates, or analyzing physical code cohesion with the slopdex command-line tool.
---

# Slopdex CLI

Use `slopdex` to index named JavaScript and TypeScript callables, search them by meaning, identify similar or duplicated functions, and find related functions scattered across a repository.

## Default Workflow

Run the command that satisfies the user's request immediately. Do not begin with `slopdex status`, `slopdex --help`, executable lookup, API-key probes, or version probes. Slopdex performs its own validation and reports missing credentials or incompatible state.

- For semantic search, run `slopdex search "<query>" --format summary --limit 10`.
- For duplicate-code clusters, run `slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9 --limit 5`.
- For related functions that are physically separated, run `slopdex cohesion --format summary --threshold 0.8 --neighbors 20 --limit 50`.
- For an explicit request to refresh the current index, run `slopdex update-git`.
- Use `slopdex status` only when the user asks for index metadata or checkpoint information.

Every command refreshes its index from committed `HEAD`, then overlays working-tree changes. A missing index is created automatically; let the requested command perform the refresh rather than initializing separately. Without Git or a Git repository, every command warns on stderr and fully re-indexes the working tree. Use `--no-reindex` only when explicitly asked to skip Git working-tree overlays or reuse an existing non-empty index without Git.

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

If a CLI command cannot find its source index, Slopdex prints a notice to stderr and automatically creates and populates it from committed `HEAD`, then overlays working-tree changes. Missing cross-search target indexes are initialized from the target repository's `HEAD` and working tree as well.

Index the current committed snapshot and working-tree overlay:

```bash
slopdex update-git
```

`update-git` reconciles the committed snapshot and, when the target is the checked-out `HEAD`, indexes staged, unstaged, and untracked changes. Historical or other-branch targets remain exact committed snapshots. The Git checkpoint remains the committed base hash. After the mandatory automatic refresh, explicitly re-index selected working-tree files with:

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

Rebuild automatically when an existing index is incompatible with the current provider, model, dimensions, strategy, schema, or repository:

```bash
slopdex update-git --force-rebuild
```

This removes the incompatible index and prints a warning before rebuilding it.

Skip the otherwise mandatory full working-tree refresh when Git is unavailable and a non-empty index already exists:

```bash
slopdex search "query" --no-reindex
```

Check index metadata and its Git checkpoint:

```bash
slopdex status
```

Git updates reconcile committed blobs first, then overlay working-tree contents. Explicit file updates do not advance the Git checkpoint.

## Failure Handling

- Preserve and report the exact failure from the requested command. Do not retry equivalent initialization commands.
- Provider authentication and configuration failures are actionable as printed. Never display key values, and do not probe whether keys are set unless the error specifically indicates missing credentials and the user asks for diagnosis.
- A bare system error such as `Invalid argument` is a Slopdex/runtime failure, not evidence that a different indexing command is needed. Stop retrying, report the command and error, and recommend diagnosing or updating Slopdex.
- `slopdex --help` is the supported capability reference. There is no `slopdex --version` option; never invoke it.

## Analysis Examples

### Duplicate Analysis

```bash
slopdex cross-search \
  --cross-file-only \
  --min-lines 4 \
  --threshold 0.9 \
  --limit 5
```

```text
Cluster 1 (3 functions, similarity 0.9124-0.9568)
  src/auth/session.ts:18:1 :: validateSession
  src/http/middleware.ts:42:1 :: authenticate
  src/users/user-service.ts:27:3 :: UserService.authenticate
```

This result found three substantial authentication functions in three files with very high similarity. Review them for repeated validation or session-handling logic that could move into one shared implementation. The middleware and service locations may represent intentional architectural layers, so treat the result as evidence to inspect rather than proof that the functions should be merged. The range describes observed links in a connected component; transitive clustering means every function is not necessarily directly similar to every other function.

### Cohesion Analysis

```bash
slopdex cohesion --format summary --threshold 0.8 --neighbors 20 --limit 50
```

```text
Cohesion: 184 functions analyzed, 37 semantic edges
  same file 35.1%  same folder 29.7%  remote 35.2%  mean distance 1.84

1. gap 0.6053  similarity 0.9400  distance 4  reciprocal
   src/auth/session.ts:18:1 :: validateSession
   packages/http/middleware.ts:42:1 :: authenticate
```

There is no universal pass/fail cutoff for cohesion, but this example has several warning signs. More than a third of weighted semantic affinity crosses folder boundaries, and the mean distance of 1.84 is above the same-folder distance of one. The top pair is strongly related at 0.94 similarity yet four distance units apart, producing a relatively high gap of 0.6053; the reciprocal match strengthens that signal. A more cohesive result under the same settings would concentrate affinity in the same-file and same-folder percentages, have a lower mean distance, and contain few high-gap remote pairs. Inspect whether shared authentication behavior belongs in one module, while accounting for the possibility that session and middleware responsibilities are intentionally separated. Compare modules or repository history rather than treating one percentage as a fixed quality threshold.

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

`--threshold` is the minimum raw cosine similarity. Use a half-open range such as `--threshold 0.85-0.95` to include the left bound and exclude the right bound.

## Duplicate Discovery

Cross-search defaults to connected clusters. Treat each cluster as a source-review candidate rather than proof of duplication. Lower `--threshold` to broaden discovery, or lower `--min-lines` when short wrappers are relevant.

Summary output can instead group matches beneath each source:

```text
src/users.ts :: Users.authenticate
  0.9321  src/session.ts :: validateSession
  0.8475  src/auth.ts :: authenticate
```

Interpret high similarity as a candidate requiring source review, not proof of duplication. Public facade methods, API wrappers, interface implementations, and test doubles often score highly while serving distinct roles.

Cross-search excludes one-line callables by default. Raise `--min-lines` for more substantial duplicate candidates, or use `--min-lines 1` when short wrappers are relevant:

```bash
slopdex cross-search --min-lines 4 --threshold 0.9
```

Filter both source and matching candidates by qualified callable name with a JavaScript regular expression:

```bash
slopdex cross-search --regex '^(User|Session)\.' --threshold 0.9
```

Use `--threshold <minimum>-<maximum>` for a half-open similarity range, such as `--threshold 0.85-0.95`. It includes the minimum, excludes the maximum, and is applied before `--limit`.

Review duplicate candidates iteratively from high confidence to lower-confidence bands instead of requesting one broad result set:

```bash
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9 --limit 5
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.85-0.9 --limit 5
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.8-0.85 --limit 5
```

Because range upper bounds are exclusive, adjacent passes do not repeat boundary candidates.

Same-index search reports each unordered pair once by default. Include both `A -> B` and `B -> A` only when explicitly needed:

```bash
slopdex cross-search --include-symmetric-duplicates
```

Exclude candidates from the source function's file when looking for duplication across files:

```bash
slopdex cross-search --cross-file-only --format summary --threshold 0.9
```

Group overlapping pairs into connected components and list each function once:

```bash
slopdex cross-search --format clusters --threshold 0.9
```

Restrict source functions to a file or recursive directory while still matching them against the whole codebase:

```bash
slopdex cross-search --source-path src/services --format summary --threshold 0.9
```

`--source-path` restricts only source functions. It can be combined with `--changed-since` or `--uncommitted`.

Restrict source functions to additions, modifications, and moves relative to a commit, including current working-tree changes:

```bash
slopdex cross-search --changed-since origin/main --format summary --threshold 0.9
```

Restrict source functions to uncommitted files:

```bash
slopdex cross-search --uncommitted --format summary --threshold 0.9
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
Use `--target-config <path>` when the target repository does not use `.slopdex/config.json`.

## Cohesion Analysis

Use JSON when passing the report to another tool or an LLM:

```bash
slopdex cohesion --format json --threshold 0.8 --neighbors 20 --limit 50
```

The report includes raw similarity, physical path distance, a combined cohesion-gap score, reciprocal-neighbor status, repository metrics, file-level external affinity, and connected groups for navigation. Reciprocity is `null` when the other endpoint was not searched because a source filter is active. Source code is omitted from JSON by default; add `--include-source` only when the consumer needs complete callable bodies.

Treat findings as review candidates. Tests, facades, adapters, and intentionally layered implementations can be semantically related while correctly living in separate locations.

## Output Formats

- `--format summary` is intended for human review and includes file and qualified function names.
- `--format clusters` groups overlapping pairs and lists each function once with its source line.
- The default `search` output is formatted JSON.
- The default `cross-search` output is connected clusters. Use `--format json` for JSONL with one object per source function.
- The default `cohesion` output is one compact JSON report. Use `--format summary` for ranked pairs.
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
--force-rebuild                     Rebuild an incompatible existing index
--limit <number>                    Result limit
--neighbors <number>                Semantic neighbors per function for cohesion
--threshold <number|range>          Similarity threshold or half-open range
--format <json|summary|clusters>    Output format
--include-source                    Include callable source in cohesion JSON
--cross-file-only                   Exclude matches from the source file
--min-lines <number>               Minimum cross-search callable length
--regex <regex>                    Match qualified callable names
--target-config <path>             Target repository configuration file
```

Run `slopdex --help` for the complete current option list.
