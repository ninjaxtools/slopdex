This repository employs the use of LLMs for [automatic programming](https://antirez.com/news/159).

This README is in my own words.

# slopdex

<picture>
  <source media="(max-width: 600px)" srcset="sloppy-dexter.png" width="600" />
  <img src="sloppy-dexter.png" width="280" align="right" alt="Dexter, the sloppy slime" />
</picture>

Slopdex helps with doing analysis on codebases that contain a lot of AI-generated ~~slop~~ slime.

The main supported functions are

- `slopdex map [PATH]...`
   <br/>show a map/skeleton of the code structure
- `slopdex lines [PATH]...`
   <br/>count lines of code/docs
- `slopdex cross-search`
   <br/>find clusters of similar code
- `slopdex search "QUERY1" ["QUERY2"...]`
   <br/>find code/docs similar to the query
- `slopdex faq "QUESTION"`
   <br/>explain code relevant to a task

`map` shows a map/skeleton of the code, excluding implementation details like function bodies. This can be helpful to get a concise map of the code to allow an LLM to explore the codebase incrementally.

`lines` shows a folder/file tree with line counts, or a ranked file list with `--output files`, allowing for ignoring comments and tests and aggregating across top folders.

`search` does a vector search across code/docs and optionally other formats.

`cross-search` also does a vector search but compares all functions with each other (scope can be limited with additional options) and helps with finding duplicated code, or code that is not necessarily duplicated but spread out across the codebase (with `--cohesion`).

`faq` caches LLM responses in a vector index.

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

## Code map/skeleton

`slopdex map` works locally without a provider or API key.

```console
$ slopdex map src docs
$ slopdex map src -g '*.rs' -k fns -e 'refresh|search'
$ slopdex map src -e 'assert\(|throw new Error|ǃ\('
$ slopdex map src --private
$ slopdex map -g '*.md' -e '^Guide\.Setup' -i --output json
$ slopdex map docs --detail expanded
$ slopdex map --no-reindex
$ slopdex map src -q 'validate session' -q 'authenticate user' -k fns
$ slopdex map docs -q 'installation' --symbol-threshold 0.6 --detail expanded
$ slopdex map src -e '^Engine\.map$' --callers 2 --callees 2 --refs none
$ slopdex map src --callers 0 --callees 0 --refs all
```

The `map` command can be used to generate a source code skeleton that strips most of the code implementation but retains structurally useful information that can act as an index into the source code, which improves context usage.

The following can be combined (OR) to show only the matching symbols:

- `-e <regex>` regex over symbols and function bodies
- `-q <query>` vector search on symbol names and markdown titles.

Use `--detail expanded` to show full code or markdown of matched symbols.

`--callers N` and `--callees N` show up to N levels of callable graph context. Under `--detail expanded` N defaults to 1.

`--refs none|all|cross-file` controls whether references are displayed as comments below a function.
Defaults to `cross-file` when `--callers` or `--callees` is non-zero, otherwise `none`. `all` shows intra-file references as well.

See the [outgoing references](docs/reference.md#callable-context-and-outgoing-references) and [selector reference](docs/reference.md#shared-selectors).

> [!NOTE]
> Add this to your `AGENTS.md` to let the agent use the map command to explore the codebase:
>
> ```text
> - Start discovery with one scoped `slopdex map -g "<glob>" -i -e "<term|term>" -q "<short symbol concept>" -q "<additional concept>" --private --callers 2 --callees 2 <paths...>` and immediately read plausible implementation ranges from the source files directly.
> - Avoid mapping repeatedly, frontload the map in one call.
> - Don't pipe the output through `head`, instead save it to a file and read from there.
> ```

## Count lines

```console
$ slopdex lines src docs
$ slopdex lines src --include-blank-lines
$ slopdex lines -g '*.rs' --exclude-comments --exclude-tests
$ slopdex lines --formats all --limit 10
$ slopdex lines src docs --output files --limit 10
$ slopdex lines src -e 'refresh|search' -i
$ slopdex lines -q 'validate session' --symbol-threshold 0.6 --output json
```

Counts whole file contents excluding empty lines (or include them with `--include-blank-lines`). With `-e` or `-q` it counts the source lines of matching symbols or documentation sections.

- `-g` can be used to match files and `--formats` to select formats (default: `code,docs`).
- `--exclude-comments` excludes language-specific comments or docstrings.
- `--exclude-tests` excludes `*.test.ts`, `*.spec.ts`, and `*_test.go` as well as inline tests, including rust's `#[test]` and `#[cfg(test)]`.
- `--limit 10` shows the largest files or folders by line count in the default tree.
- `--output files` outputs a descending list of files with line counts.

See the [line-count reference](docs/reference.md#line-counts) for details.

## Search

Vector search needs either an API key for OpenAI or Jina, or alternatively a local Ollama model can be configured.

```console
$ export OPENAI_API_KEY="your-api-key"
$ slopdex update
$ slopdex search "validate an authenticated session"
```

```console
$ slopdex config embeddings --provider ollama
```

To create a vector index, the `update` command needs to be run once in a Git repository. Any subsequent attempts to use the index will determine what needs to be updated using Git and update the index incrementally (use `--no-reindex` to skip this).

Each worktree has its own index under `~/.cache/slopdex`. There is also a global cache that is reused across worktrees, so hash-identical code will not cause unneeded model provider requests. Configuaration is stored in `~/.config/slopdex/config.json`. Global configuration can be overridden with a repository local `.slopdex/config.json` file. These folder respect the `$XDG_*` settings. See also [cache configuration](docs/reference.md#shared-provider-artifacts).

By default code and docs indexes are searched and ranked together, see `--formats` to enable other file types.

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
$ slopdex search-docs "configure the embedding provider"
$ slopdex search-code 'reject expired credentials' -q 'validate session' --symbol-threshold 0.6
```

Or pass any combination of `--symbols`, `--code`, `--descriptions`, and `--docs`:

```console
$ slopdex search-symbols 'validate session' --threshold 0.6 --limit 20
$ slopdex search 'installation' --symbols -g '*.md' --detail expanded
$ slopdex search 'validate session' --code --symbols
$ slopdex search-symbols 'read settings' -q 'configuration' --symbol-threshold 0.7
```

Descriptions refer to source comments found above symbols or at the start of a file. Missing descriptions/comments can optionally be generated with `generate descriptions`, but require an LLM to be configured (see below).

> [!NOTE]
> Add this to your `AGENTS.md` to use semantic search with your agent:
>
> ```text
> - Use semantic code search to find code with: `slopdex search "<query>" ["<query>"...] --threshold 0.3 --limit 20 --formats all`. Provide multiple alternative queries.
> ```

See the [attachment rules](docs/reference.md#descriptions).

## Find duplicate code

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

Clusters are ranked by highest pair similarity * lines, descending.

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
> - When reviewing uncommitted code determine if similar code elsewhere warrants a refactor to eliminate duplicate code: `slopdex cross-search --uncommitted --cross-file-only --threshold 0.8 --lines 4`. Exclude any code you determine to be a false positive from future searches with `slopdex config exclude-cross-search <file>:<symbol> ...`.
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

### Exclude symbols from cross-search

```console
$ slopdex config exclude-cross-search src/services/user.rs:UserService.validate ...
```

This will add the hash for the given functions to the `crossSearchExclusions` config setting. This is useful to exclude similar functions that however are intended to be similar and shouldn't be refactored from appearing repeatedly in `cross-search` results.

### Find related code stored far apart

When code is similar but not actually duplicated, then `--cohesion` can help find similar code that exists far apart in the filesystem tree, which could potentially be refactored to make it more cohesive, by ordering matches from farthest to nearest.

```console
$ slopdex cross-search --cross-file-only --cohesion --threshold 0.8
```

## Frequently asked questions

The `faq` command sends the given question together with relevant code and file maps to the configured LLM and caches the response in a vector index.

```console
$ slopdex faq "How are RPC endpoints registered?"
```

First a semantic search selects the relevant files and symbols for the question. For each candidate file, the LLM is then given its full declaration map in expanded detail, with source code for the directly relevant symbols and for callers/callees up to two edges away. The LLM generates an explanation along with a list of the file and file/symbol references from that context that it considers relevant. Only the explanation and those references (with line numbers taken from the map) are emitted and cached; references that do not appear in the context are silently dropped, and the context itself is discarded.

The result is indexed under the embeddings of the original question as well as the LLM response. Any subsequent questions will be use the index to try and get a match above the configured threshold (0.9 by default, configured with `--faq-threshold`), and reuse the response.

Changes in the referenced files invalidate the FAQ answer and repeat the LLM request.

## Inspect and export indexes

```sh
slopdex index list                              # index names, counts, and readiness
slopdex index list faq                          # compact entries in stored order
slopdex index list code -q "validate a session" # rank by stored embedding similarity
slopdex index list faq -e "authentication" -i   # filter question/answer text
slopdex index list faq --detail expanded        # full text and metadata
slopdex index list faq --output json > faq.json # full entries and vectors
slopdex index list code --output csv > code.csv
```

## Configure an LLM

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

The fallback model is used if the main model reports an error, and if the fallback model reports an error the main model is tried again.

## Other formats

All commands accept a `--formats` argument to select what file types should be considered:

| Group | Formats |
| --- | --- |
| `code` | code |
| `docs` | markdown |
| `config` | JSON, YAML, TOML, and Terraform/HCL |
| `markup` | HTML, XML, and CSS |
| `all` | all groups |

The default is `code,docs`.

```console
$ slopdex map --formats all
$ slopdex map --formats code --formats config
$ slopdex search "deployment settings" --formats config
```

## Interactive config

To configure all configurable settings interactively run:

```console
$ slopdex config
```

## Reranking

Optionally a second-stage reranker can be enabled for all query-search commands:

```console
$ export COHERE_API_KEY="your-api-key"
$ slopdex config set rerankerProvider cohere
$ slopdex config set rerankingEnabled true
# Or: export JINA_API_KEY="your-api-key" && slopdex config set rerankerProvider jina
# Or use an LLM: export OPENAI_API_KEY="your-api-key" && slopdex config set rerankerProvider openai
```

## Compare repositories

```console
$ slopdex cross-search \
  --target-root /path/to/other/repo \
  --target-index /path/to/other/index.sqlite \
  --threshold 0.9
```

Pass the target's `indexPath` from `slopdex --root /path/to/other/repo --output json status` (or a custom `--index` path) as `--target-index`.

## Index maintenance

If some functions can't be indexed a warning is printed. Index errors can be investigated and fixed with the `index errors` command to ensure the index is complete.

```console
$ slopdex status
$ slopdex index errors --output text
$ slopdex --version
```

`slopdex cache prune` removes unused rebuildable data, and preserves any generated vectors or descriptions. Add `--prune-vectors` or `--prune-descriptions` to explicitly remove unused results of that kind. To return all space to the filesystem sqlite databases need to be vacuumed with `--vacuum`.

```console
$ slopdex cache prune --vacuum
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
- [codanna](https://github.com/bartolli/codanna)
- [open-codebase-index](https://github.com/Helweg/open-codebase-index)

## Publishing

```console
$ cargo release minor --no-publish --execute
```

## Commands

See [Command reference](docs/reference.md).
