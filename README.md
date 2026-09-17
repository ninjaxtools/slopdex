This repository employs the use of LLMs for [automatic programming](https://antirez.com/news/159).

This readme is written by a human.

# slopdex

<img src="sloppy-dexter.png" width="280" align="right" alt="Dexter, the sloppy slime" />

Slopdex helps with doing analysis on codebases that contain a lot of AI generated ~~slime~~ code.

The main supported functions are

- `slopdex search <query>`
   <br/>find code similar to the query
- `slopdex cross-search`
   <br/>find clusters of similar code
- `slopdex describe <query>`
   <br/>explain code relevant to a task (uses an LLM)

`cross-search` helps with finding duplicated code, and with `--cohesion` helps with identifying similar code that is not necessarily duplicated but spread out across the codebase, which could indicate that a refactoring could make it more cohesive.

## Getting started

An embedding-provider API key is required. You can use either OpenAI or Jina.

```bash
npm install -g @ninjaxtools/slopdex

export OPENAI_API_KEY="your-api-key"
# Or
# export JINA_API_KEY="your-api-key" # and pass --provider jina

slopdex search "validate an authenticated session"
```

The index tracks the current git commit and is created or updated on every command and stored in `.slopdex/index.sqlite`. Add `.slopdex/` to your repository's `.gitignore` to prevent it from being committed.

### Search code

By default only vector embeddings of code is used for search.

```bash
$ slopdex search "keep the repository index synchronized"
0.4284  tests/languages.test.ts :: refresh
0.4200  src/cli.ts :: refreshIndex
0.4113  src/code-index.ts :: CodeIndex.updateFromGit
...
```

You can also enable optional description generation which will automatically generate file and function descriptions with a configured LLM provider and include them in the search.

I use OpenCode Go usually with DeepSeek or Muse Spark, which are fairly good low-cost models. If you
sign up for OpenCode Go through [this link](https://opencode.ai/go?ref=RAR3Z744DZ), we both receive
$5 in credit.

```bash
export OPENAI_API_KEY="your-api-key"
# Or
# export OPENCODE_API_KEY="your-api-key" # for OpenCode Zen/Go descriptions
# Or
# opencode auth login                    # use stored credentials instead of env variables

slopdex descriptions enable
slopdex search-description "keep the repository index synchronized"
```

You can list and configure one of OpenCode's models like this:

```bash
slopdex models opencode-go
slopdex config model opencode-go/gpt-5.6-luna
```

### Search and describe

The intention of the `describe` command is to use a low-cost model to summarise vector search findings to provide more relevant pre-processed results to a more powerful calling agent. It first performs a vector search and then uses the results to provide a tailored summary/description of relevant code and pre-generated generated file and function descriptions. 

```bash
slopdex describe "I want to implement a new rpc endpoint"
```

For the best matching files (configured with `--describe-full-file-threshold`, default `0.8`) the full file contents are used.

### Find duplicate code

Compare functions across files, exclude short wrappers, and group matches into clusters:

```bash
$ slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9
Cluster 1 (3 functions, similarity 0.9124-0.9568)
  src/auth/session.ts:18:1 :: validateSession
  src/http/middleware.ts:42:1 :: authenticate
  src/users/user-service.ts:27:3 :: UserService.authenticate
...
```

Using `--cross-file-only` is useful to exclude similar code in the same file.

Review adjacent bands with threshold ranges:

```bash
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9
slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.85-0.9
```

### Restrict functions used in the cross-search

Only use uncommitted working-tree functions as sources:

```bash
slopdex cross-search --uncommitted --cross-file-only --min-lines 4 --threshold 0.9
```

Only use functions changed since origin/main as sources:

```bash
slopdex cross-search --changed-since origin/main --threshold 0.9
```

Only use matching symbols under src/services as sources:

```bash
slopdex cross-search --source-path src/services -e '^UserService\.' --threshold 0.9
```

### Find related code stored far apart

When code is similar but not actually duplicated, then `--cohesion` can help find similar code that exists far apart in the filesystem tree, which could potentially be refactored to make it more cohesive, by ordering matches from farthest to nearest.

```bash
slopdex cross-search --cross-file-only --cohesion --threshold 0.8
```
### Use with agents

Just put this in your `AGENTS.md` file, no skill required:

```
- use semantic code search to find code with: `slopdex search "..." --threshold 0.5`
- when reviewing uncommitted code avoid introducing duplicates by looking for related matches: `slopdex cross-search --uncommitted --threshold 0.8`
```

Any other use, like doing a full `cross-search` is probably better done interactively with the agent, in which case you can just ask the agent to run `slopdex --help` to get usage information.

### Reranking

Optionally a second-stage reranker can be enabled for `search` and `search-description`:

```bash
export COHERE_API_KEY="your-api-key"
slopdex config reranker cohere
# Or: export JINA_API_KEY="your-api-key" && slopdex config reranker jina
# Or use an LLM: export OPENAI_API_KEY="your-api-key" && slopdex config reranker openai
```

### Compare repositories

```bash
slopdex cross-search \
  --target-root /path/to/other/repo \
  --target-index /path/to/other/repo/.slopdex/index.sqlite \
  --threshold 0.9
```

### Inspect index health

If some functions can't be indexed a warning is printed. Index errors can be investigated and fixed with the `index-errors` command to ensure the index is complete.

```bash
slopdex status
slopdex index-errors --format summary
slopdex --version
```

## Commands

See [Command reference](docs/reference.md).
