This repository employs the use of LLMs for [automatic programming](https://antirez.com/news/159).

This readme is written by a human.

# slopdex

<picture>
  <source media="(max-width: 600px)" srcset="sloppy-dexter.png" width="600" />
  <img src="sloppy-dexter.png" width="280" align="right" alt="Dexter, the sloppy slime" />
</picture>

Slopdex helps with doing analysis on codebases that contain a lot of AI generated ~~slop~~ slime.

The main supported functions are

- `slopdex search <query>`
   <br/>find code/docs similar to the query
- `slopdex cross-search`
   <br/>find clusters of similar code/docs
- `slopdex describe <query>`
   <br/>explain code relevant to a task
- `slopdex map [PATH]...`
   <br/>show a map/skeleton of the code structure

`search` does a vector search of code, configuration and markup files, and, when enabled, generated code descriptions.

`cross-search` also does a vector search but compares all functions with each other (scope can be limited with additional options) and helps with finding duplicated code, or code that is not necessarily duplicated but spread out across the codebase (with `--cohesion`).

`describe` does a `search` first, then passes the result through the configured LLM model to create a tailored description.

`map` shows a map/skeleton of the code, excluding implementation details like function bodies. This can be helpful to get a concise map of the code to allow an LLM to explore the codebase incrementally.

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
$ slopdex search "validate an authenticated session"
...
```

The index tracks the current Git commit and is created or updated on every command that uses it. Index data is stored in the current user's cache folder and project settings are stored in `.slopdex/config.json` (add `.slopdex` to `.gitignore`). See also [cache configuration](docs/reference.md#shared-provider-artifacts).

### Code map/skeleton

`slopdex map` doesn't use vector search and works locally without a provider or API key.

```console
$ slopdex map src docs
$ slopdex map src -g '*.rs' -k fns -e 'refresh|search'
$ slopdex map src --private
$ slopdex map -g '*.md' -e '^Guide\.Setup' -i --format json
$ slopdex map docs --detail expanded
$ slopdex map --no-reindex
```

The `map` command can be used to generate a source code skeleton that strips most of the code implementation but retains structurally useful information that can act as an index into the source code, which improves context usage.

Markdown headings show the full section's line range. Use `--detail expanded` to
also show the body text beneath selected headings, including long sections.

See the [selector reference](docs/reference.md#shared-selectors).

> [!NOTE]
> Add this to your `AGENTS.md` to let the agent use the map command to explore the codebase:
>
> ```text
- use `slopdex map --private -g "<glob>" -i -e "<regex>" <files or directories...>` to obtain a compact structural code skeleton, and then perform targeted reads using the line numbers for implementation details. Once a plausible entry point is mapped, **stop inventory browsing and read its implementation using the returned ranges**. Do not repeat declaration searches for names and locations already found.
> ```

### Search

By default all available indexes are searched and ranked together.

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

To search code/docs individually:

```console
$ slopdex search-code "configure the embedding provider"
$ slopdex search-md "configure the embedding provider" 
```

You can also enable optional description generation which will automatically generate file and function descriptions with a configured LLM provider and include them in the search.

I use OpenCode Go usually with DeepSeek or Muse Spark, which are fairly good low-cost models. If you
sign up for OpenCode Go through [this link](https://opencode.ai/go?ref=RAR3Z744DZ), we both receive
$5 in credit.

```console
$ export OPENAI_API_KEY="your-api-key"
# Or
# export OPENCODE_API_KEY="your-api-key" # for OpenCode Zen/Go descriptions
# Or
# opencode auth login                    # use stored credentials instead of env variables

$ slopdex config set descriptionsEnabled true
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

File and function descriptions are cached by source and description context, so renaming a file can reuse its descriptions. Function changes or a changed file-description context can require new callable descriptions. File descriptions after edits can be refreshed explicitly with `index reindex-files`.

> [!NOTE]
> Add this to your `AGENTS.md` to use semantic code search with your agent:
>
> ```text
> - use semantic code search to find code with: `slopdex search "<query>" --threshold 0.3 --limit 20`; vary the search query if you get no results
> ```

### Interactive Config

To configure all configurable settings interactively run:

```console
$ slopdex config
```

### Search and describe

The intention of the `describe` command is to use a low-cost model to summarise vector search findings to provide more relevant pre-processed results to a more powerful calling agent. It first performs a vector search and then uses the results to provide a tailored summary/description of relevant code and pre-generated generated file and function descriptions. 

```console
$ slopdex describe "I want to implement a new rpc endpoint"
```

First a `search` is performed with `--detail expanded` and `--expanded-callers 2` and `--expanded-callees 2` which expands generated descriptions and function implementations in the result, which is then given to an LLM which is asked to interpret the result, and the final output will be the result of the LLM interpretation as well as the `compact` results from the earlier search. This provides a tailored description for the specific query based on the `expanded` search results as well as the `compact` version of those results.

### Find duplicate code

Compare functions across files, exclude short wrappers, and group matches into clusters:

```console
$ slopdex cross-search --cross-file-only --lines 4 --threshold 0.9
*** Cluster 1 · 3 symbols · similarity 0.91-0.96
src/auth/session.ts:18-29:validateSession
src/http/middleware.ts:42-57:authenticate
src/users/user-service.ts:27-48:UserService.authenticate
...
```

Using `--cross-file-only` is useful to exclude similar code in the same file.

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
> - when reviewing uncommitted determine if similar code elsewhere warrants a refactor: `slopdex cross-search --uncommitted --cross-file-only --threshold 0.8`
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
