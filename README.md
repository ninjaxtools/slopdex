This repository employs the use of LLMs for [automatic programming](https://antirez.com/news/159).

This readme is written by a human.

# slopdex

<picture>
  <source media="(max-width: 600px)" srcset="sloppy-dexter.png" width="600" />
  <img src="sloppy-dexter.png" width="280" align="right" alt="Dexter, the sloppy slime" />
</picture>

Slopdex helps with doing analysis on codebases that contain a lot of AI generated ~~slop~~ slime.

The main supported functions are

- `slopdex map [PATH]...`
   <br/>show a map/skeleton of the code structure
- `slopdex cross-search`
   <br/>find clusters of similar code/docs
- `slopdex search <query>`
   <br/>find code/docs similar to the query
- `slopdex describe <query>`
   <br/>explain code relevant to a task

`map` shows a map/skeleton of the code, excluding implementation details like function bodies. This can be helpful to get a concise map of the code to allow an LLM to explore the codebase incrementally.

`search` does a vector search across all supported code/docs/config files.

`cross-search` also does a vector search but compares all functions with each other (scope can be limited with additional options) and helps with finding duplicated code, or code that is not necessarily duplicated but spread out across the codebase (with `--cohesion`).

`describe` does a `search` first, then passes the result through the configured LLM model to create a tailored description.

## Installation

Install from npm:

```sh
npm install -g @ninjaxtools/slopdex
```

You can also download a binary from one of the releases: https://github.com/ninjaxtools/slopdex/releases

Or you can build from source:

```sh
cargo install --path . --locked
```

## Getting started

Vector search requires an embedding-provider API key from OpenAI or Jina.

```console
$ export OPENAI_API_KEY="your-api-key"
$ slopdex update
$ slopdex search "validate an authenticated session"
...
```

The index is created with the `update` command and refreshed before use. Git checkouts use status, commit-tree differences, and previously dirty paths to refresh incrementally, without hashing every unchanged file on repeated queries. `--no-reindex` uses the saved snapshot without checking freshness; missing query artifacts can still require provider calls.

Each worktree has its own bindings in `$XDG_CACHE_HOME/slopdex/worktrees-v1/<root-hash>/index.sqlite` (normally under `~/.cache`). Source, parse, embedding, and provider artifacts and immutable snapshots live in the authoritative `global-v1.sqlite` store and are reused across worktrees. Vector search lazily shares immutable index bases with worktree-specific deltas and membership masks. Older indexes/caches are not migrated or imported; run `slopdex update` to create the new index. Project settings are stored in `.slopdex/config.json` (add `.slopdex` to `.gitignore`). See also [cache configuration](docs/reference.md#shared-provider-artifacts).

### Code map/skeleton

`slopdex map` works locally without a provider or API key.

```console
$ slopdex map src docs
$ slopdex map src -g '*.rs' -k fns -e 'refresh|search'
$ slopdex map src --private
$ slopdex map -g '*.md' -e '^Guide\.Setup' -i --format json
$ slopdex map docs --detail expanded
$ slopdex map --no-reindex
$ slopdex map src -q 'validate session' -q 'authenticate user' -k fns
$ slopdex map docs -q 'installation' --symbol-threshold 0.6 --detail expanded
```

The `map` command can be used to generate a source code skeleton that strips most of the code implementation but retains structurally useful information that can act as an index into the source code, which improves context usage.

Symbols can be filtered by using `-e <regex>` or `-q <query>` (OR). The `-q` filter uses vector search on symbol names and markdown heading titles.

Use `--detail expanded` to to show full code or markdown of matched symbols.

See the [selector reference](docs/reference.md#shared-selectors).

> [!NOTE]
> Add this to your `AGENTS.md` to let the agent use the map command to explore the codebase:
>
> ```text
> - Start discovery with one scoped `slopdex map -g "<glob>" -i -e "<term|term>" -q "<short symbol concept>" --private --callers 2 --callees 2 <paths...>`. Use known paths or `.`. Immediately read plausible implementation ranges and follow calls in source; grep for usages or missing links. Map again only for unknown declaration locations. Batch reads; skip repeated inventories and setup/help.
> ```

### Search

By default all indexes are searched and ranked together.

```console
$ slopdex search "keep the repository index synchronized"
*** src/engine.rs
@@ 914-1082 @@ score=0.4284
impl Engine
  pub fn search(&self, query: &str, kind: &str, options: &Value) -> Result<Vec<Value>>

*** src/engine.rs
@@ 376-409 @@ score=0.4200
impl Engine
  pub fn map(&self, options: &Value) -> Result<Vec<Value>>
...
```

To search indexes individually:

```console
$ slopdex search-code "configure the embedding provider"
$ slopdex search-md "configure the embedding provider" 
$ slopdex search-code 'reject expired credentials' -q 'validate session' --symbol-threshold 0.6
```

Or pass any combination of `--symbols` `--code`, `--descriptions`, `--md`:

```console
$ slopdex search-symbols 'validate session' --threshold 0.6 --limit 20
$ slopdex search 'installation' --symbols -g '*.md' --detail expanded
$ slopdex search 'validate session' --code --symbols
$ slopdex search-symbols 'read settings' -q 'configuration' --symbol-threshold 0.7
```

Descriptions refer to source comments found above symbols or at the start of a file. Since not all symbols have descriptions, missing descriptions can optionally be generated with `generate descriptions`, but required an LLM to be configured.

I use OpenCode Go usually with DeepSeek or Muse Spark, which are fairly good low-cost models. If you
sign up for OpenCode Go through [this link](https://opencode.ai/go?ref=RAR3Z744DZ), we both receive
$5 in credit.

```console
$ export OPENAI_API_KEY="your-api-key"
# Or
# export OPENCODE_API_KEY="your-api-key" # for OpenCode Zen/Go descriptions
# Or
# opencode auth login                    # use stored credentials instead of env variables

$ slopdex generate descriptions
$ slopdex search-descriptions "keep the repository index synchronized"
```

You can list and configure one of OpenCode's models like this (or use the interactive config):

```console
$ slopdex help models opencode-go
$ slopdex config set descriptionProvider opencode-go
$ slopdex config set descriptionModel gpt-5.6-luna
$ slopdex config set descriptionFallbackModel muse-spark-1.3-contributor
```

The fallback-model is used if the main model reports an error, and if the fallback-model reports an error the main model is tried again.

Generated descriptions are cached by the full effective request: configured profile, settings, system instruction, and ordered conversation messages. Paths, source, line ranges, and earlier answers can affect reuse.

> [!NOTE]
> Add this to your `AGENTS.md` to use semantic code search with your agent:
>
> ```text
> - use semantic code search to find code with: `slopdex search "<query>" --threshold 0.3 --limit 20`; vary the search query if you get no results
> ```

See the [attachment rules](docs/reference.md#descriptions).

### Interactive Config

To configure all configurable settings interactively run:

```console
$ slopdex config
```

### Search and describe

The intention of the `describe` command is to use a low-cost model to summarise vector search findings to provide more relevant pre-processed results to a more powerful calling agent. It first performs a vector search and then uses the results to provide a tailored summary/description of relevant code.

```console
$ slopdex describe "I want to implement a new rpc endpoint"
```

First a `search` is performed with `--detail expanded`, `--expand-callers 2`, and `--expand-callees 2`, which will include code and comments/descriptions. The result is given to an LLM for interpretation which generates an explanation which will be emitted along with the `compact` search references. This provides a tailored explanation for the query based on the `expanded` search result along with the `compact` search result and thereby acts as a compaction of the `expanded` result.

### Find duplicate code

Compare functions across files, exclude short wrappers, and group matches into clusters.

```console
$ slopdex cross-search --cross-file-only --lines 4 --threshold 0.9
*** Cluster 1 · 3 symbols · 50 lines · similarity 0.91-0.96
src/auth/session.ts:18-29:validateSession
src/http/middleware.ts:42-57:authenticate
src/users/user-service.ts:27-48:UserService.authenticate
...
```

Using `--cross-file-only` is useful to exclude similar code in the same file.

Clusters are ranked by highest pair similarity × distinct covered source lines,
descending. Overlapping symbols in the same file count their shared lines once.

You can use threshold and line-count ranges as well. Range starts are inclusive
and ends are exclusive, so `--lines 4-20` selects functions with 4 through 19
lines:

```console
$ slopdex cross-search --cross-file-only --lines 4 --threshold 0.9
$ slopdex cross-search --cross-file-only --lines 4-20 --threshold 0.85-0.9
```
> [!NOTE]
> Add this to your `AGENTS.md` to detect and refactor duplicate code before it is committed:
>
> ```text
> - when reviewing uncommitted determine if similar code elsewhere warrants a refactor: `slopdex cross-search --uncommitted --cross-file-only --lines 4 --threshold 0.8`
> ```

### Restrict functions used in the cross-search

Only use uncommitted working-tree functions as sources:

```console
$ slopdex cross-search --uncommitted --cross-file-only --lines 4 --threshold 0.9
```

Only use functions changed since origin/main as sources:

```console
$ slopdex cross-search --changed-since origin/main --threshold 0.9
```

Only use matching symbols under src/services as sources:

```console
$ slopdex cross-search --source-path src/services -e '^UserService\.' --threshold 0.9
```

### Find related code stored far apart

When code is similar but not actually duplicated, then `--cohesion` can help find similar code that exists far apart in the filesystem tree, which could potentially be refactored to make it more cohesive, by ordering matches from farthest to nearest.

```console
$ slopdex cross-search --cross-file-only --cohesion --threshold 0.8
```

### Reranking

Optionally a second-stage reranker can be enabled for all query-search commands:

```console
$ export COHERE_API_KEY="your-api-key"
$ slopdex config set rerankerProvider cohere
$ slopdex config set rerankingEnabled true
# Or: export JINA_API_KEY="your-api-key" && slopdex config set rerankerProvider jina
# Or use an LLM: export OPENAI_API_KEY="your-api-key" && slopdex config set rerankerProvider openai
```

### Compare repositories

```console
$ slopdex cross-search \
  --target-root /path/to/other/repo \
  --target-index /path/to/other/index.sqlite \
  --threshold 0.9
```

Pass the target's `indexPath` from `slopdex --root /path/to/other/repo --format json status` (or a custom `--index` path) as `--target-index`.

### Inspect index health

If some functions can't be indexed a warning is printed. Index errors can be investigated and fixed with the `index errors` command to ensure the index is complete.

```console
$ slopdex status
$ slopdex index errors --format summary
$ slopdex --version
```

## Related Work

Code similarity search

- [treepeat](https://github.com/dsummersl/treepeat)

Structured code queries:

- [CodeGraphContext](https://github.com/CodeGraphContext/CodeGraphContext)
- [CoderLM](https://github.com/JaredStewart/coderlm)

Structured code queries and edits:

- [srgn](https://github.com/alexpovel/srgn)
- [comby](https://github.com/comby-tools/comby)
- [ast-grep](https://ast-grep.github.io/guide/introduction)
- [gritql](https://github.com/biomejs/gritql)

Semantic code search with vector embeddings:

- [qmd](https://github.com/tobi/qmd)
- [grepai](https://github.com/yoanbernabeu/grepai)
- [cocoindex-code](https://github.com/cocoindex-io/cocoindex-code)
- [codana](https://github.com/bartolli/codanna)
- [open-codebase-index](https://github.com/Helweg/open-codebase-index)

## Publishing

```console
$ cargo release minor --no-publish --execute
```

## Commands

See [Command reference](docs/reference.md).
