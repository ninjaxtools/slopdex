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

`search` does a vector searche of code, and markdown, and when enabled generated code descriptions.

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

Semantic search requires an embedding-provider API key from OpenAI or Jina. `slopdex map` doesn't
use vector search and works locally without a provider or API key.

```console
$ export OPENAI_API_KEY="your-api-key"
$ slopdex search "validate an authenticated session"
...
```

The index tracks the current git commit and is created or updated on every command and stored in `.slopdex/index.sqlite`. Add `.slopdex/` to your repository's `.gitignore` to prevent it from being committed.

### Code map/skeleton

```console
$ slopdex map src docs
$ slopdex map src -g '*.rs' -k fns -e 'refresh|search'
$ slopdex map -g '*.md' -e '^Guide\.Setup' -i --format json
$ slopdex map --no-reindex
```

The `map` command can be used to generate a source code skeleton that strips most of the code implementation but retains many of the structurally useful information that can act as an index into the source code. The resulting index output is most useful to conserve tokens and improve the agentic programming experience.

See the [selector reference](docs/reference.md#shared-selectors).

### Search

By default all available indexes are searched and ranked together.

```console
$ slopdex search "keep the repository index synchronized"
0.4284  tests/languages.test.ts :: refresh
0.4200  src/cli.ts :: refreshIndex
0.4113  src/code-index.ts :: CodeIndex.updateFromGit
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

$ slopdex descriptions enable
$ slopdex search-descriptions "keep the repository index synchronized"
```

You can list and configure one of OpenCode's models like this (or use the interactive config):

```console
$ slopdex models opencode-go
$ slopdex config model opencode-go/gpt-5.6-luna
$ slopdex config fallback-model opencode-go/muse-spark-1.3-contributor
```

The fallback-model is used if the main model reports an error, and if the fallback-model reports an error the main model is tried again.

File and function descriptions are cached. Function descriptions will only be regenerated when functions change. File descriptions need to be regenerated explcitily with the `reindex-files` command.

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

For the best matching files (configured with `--describe-full-file-threshold`, default `0.8`) the full file contents are used.

### Find duplicate code

Compare functions across files, exclude short wrappers, and group matches into clusters:

```console
$ slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9
Cluster 1 (3 functions, similarity 0.9124-0.9568)
  src/auth/session.ts:18:1 :: validateSession
  src/http/middleware.ts:42:1 :: authenticate
  src/users/user-service.ts:27:3 :: UserService.authenticate
...
```

Using `--cross-file-only` is useful to exclude similar code in the same file.

Cross-search defaults to `--threshold 0.8` (query search defaults to `0.3`).
Clusters join matches transitively, so a low threshold can connect many groups
into one large cluster. Use `0.9` for stricter duplicate detection, or explicitly
pass `--threshold 0.3` for the pre-rewrite default.

You can use threshold ranges as well:

```console
$ slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9
$ slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.85-0.9
```

### Restrict functions used in the cross-search

Only use uncommitted working-tree functions as sources:

```console
$ slopdex cross-search --uncommitted --cross-file-only --min-lines 4 --threshold 0.9
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
### Use with agents

Just put this in your `AGENTS.md` file, no skill required:

```
- use `treesitter-index -g <glob> <files or directories...>` early to obtain a compact structural skeleton before reading the full file, and then perform targeted reads for implementation details. Line numbers are indicated in square brackets (e.g [5] means line 5, and [5-10] means lines 5 to 10). To filter for specific symbols, use `-e`. To only include specific kinds of symbols use `-k` with `imports`, `fns`, `consts`, `types`, or `classes`. For example: `treesitter-index -g "*.ts" -i -e "manager|main" src`.
- use semantic code search to find code with: `slopdex search "..." --threshold 0.5`
- when reviewing uncommitted code avoid introducing duplicates by looking for related matches: `slopdex cross-search --uncommitted --threshold 0.8`
- don't use `*` as the glob since that circumvents the default ignore rules.
```

Any other use, like doing a full `cross-search` is probably better done interactively with the agent, in which case you can just ask the agent to run `slopdex --help` to get usage information.

### Reranking

Optionally a second-stage reranker can be enabled for all query-search commands:

```console
$ export COHERE_API_KEY="your-api-key"
$ slopdex config reranker cohere
# Or: export JINA_API_KEY="your-api-key" && slopdex config reranker jina
# Or use an LLM: export OPENAI_API_KEY="your-api-key" && slopdex config reranker openai
```

### Compare repositories

```console
$ slopdex cross-search \
  --target-root /path/to/other/repo \
  --target-index /path/to/other/repo/.slopdex/index.sqlite \
  --threshold 0.9
```

### Inspect index health

If some functions can't be indexed a warning is printed. Index errors can be investigated and fixed with the `index-errors` command to ensure the index is complete.

```console
$ slopdex status
$ slopdex index-errors --format summary
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
