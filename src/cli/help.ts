export function printHelp(): void {
  process.stdout.write(`Usage: slopdex <command> [arguments] [options]

Commands:
  models [opencode|opencode-go]        List current published OpenCode models
  config                              Interactively configure all settings
  config model <model|provider/model>  Validate and save an OpenCode description model
  config fallback-model <model>        Validate and save a same-provider fallback model
  config descriptions <enable|disable> Save description state without opening an index
  config parallelism <count>           Save the concurrent provider request limit (default: 10)
  config reranker <provider|disable>    Save Cohere, Jina, or OpenAI reranking settings
  status                              Show index metadata
  index-errors                        List persisted file and function indexing failures
  update-files <path...>              Index specific working-tree files
  reindex-files                       Regenerate stale file descriptions
  delete-files <path...>              Remove specific files from the index
  update-git                          Index a Git snapshot plus working-tree changes
  search <query>                      Search code, descriptions, and Markdown
  search-code <query>                 Search function code only
  search-descriptions <query>         Search function descriptions only
  search-md <query>                   Search heading-aware Markdown chunks only
  describe <query>                    Explain relevant existing code for a task
  descriptions <enable|disable>       Enable or disable automatic purpose descriptions
  cross-search                        Find nearest functions for each source function

Examples:
  Search code:
    slopdex search "validate an authenticated session"

    slopdex search "keep the repository index synchronized"
    0.4284  tests/languages.test.ts :: refresh
    0.4200  src/cli.ts :: refreshIndex
    0.4113  src/code-index.ts :: CodeIndex.updateFromGit
    ...

  Search Markdown documentation:
    slopdex search-md "configure the embedding provider"

  Search only callable source:
    slopdex search-code "keep the repository index synchronized"

  Enable descriptions, then search by their meaning:
    slopdex descriptions enable
    slopdex search-descriptions "keep the repository index synchronized"

    slopdex models opencode-go
    slopdex config model opencode-go/gpt-5.6-luna
    slopdex config fallback-model opencode-go/muse-spark-1.3-contributor

  Explain existing code for a task:
    slopdex describe "I want to implement a new rpc endpoint"

    Relevant vector-search matches are sent to the configured description model
    together with file descriptions, callable descriptions, and callable source.
    Files scoring above --describe-full-file-threshold (default 0.8) are included
    in full. The output explains what exists and how it fits together for the
    task without proposing an implementation.

  Find duplicate code:
    Compare functions across files, exclude short wrappers, and group matches into clusters:
      slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9

      Cluster 1 (3 functions, similarity 0.9124-0.9568)
        src/auth/session.ts:18:1 :: validateSession
        src/http/middleware.ts:42:1 :: authenticate
        src/users/user-service.ts:27:3 :: UserService.authenticate

      This found three substantial authentication functions in separate files with very
      high similarity. Review them for repeated validation or session logic that could be shared.
      Middleware and service locations may be intentional architectural layers, so this is
      evidence to inspect rather than proof they should merge. Connected components may use
      transitive links, so every function need not directly match every other function.

    Using --cross-file-only is useful to exclude similar code in the same file.

    Review adjacent bands with threshold ranges:
      slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9
      slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.85-0.9

  Restrict functions used in cross-search:
    Only use uncommitted working-tree functions as sources:
      slopdex cross-search --uncommitted --cross-file-only --min-lines 4 --threshold 0.9

    Only use functions changed since origin/main as sources:
      slopdex cross-search --changed-since origin/main --threshold 0.9

    Only use matching symbols under src/services as sources:
      slopdex cross-search --source-path src/services -e '^UserService\\.' --threshold 0.9

  Find related code stored far apart:
    --cohesion orders matches from farthest to nearest, which can highlight similar code
    that could be made more cohesive through an abstraction:
      slopdex cross-search --cross-file-only --cohesion --threshold 0.8

      src/auth/session.ts :: validateSession
        0.9400  packages/http/middleware.ts :: authenticate  [distance 4]
        0.9300  src/auth/token.ts :: validateToken  [distance 1]

      --cohesion keeps cross-search's semantic matches but orders each source's matches
      from greatest to least path distance. Similarity breaks distance ties. This highlights
      related functions stored far apart without introducing a separate analysis command.

  Use with agents:
    slopdex search "..." --threshold 0.5
    slopdex cross-search --uncommitted --threshold 0.8

  Reranking (second-stage reranker for query searches):
    slopdex config reranker cohere
    slopdex config reranker jina
    slopdex config reranker openai

  Compare repositories:
    slopdex cross-search --target-root /path/to/other/repo --target-index /path/to/other/repo/.slopdex/index.sqlite --threshold 0.9

  Inspect index health:
    slopdex status
    slopdex index-errors --format summary
    slopdex --version

Options:
  --version                           Show the package version
  --root <path>                       Repository root (default: current directory)
  --config <path>                     Config file (default: .slopdex/config.json)
  --index <path>                      SQLite index path
  --provider <openai|jina>            Embedding provider
  --model <name>                      Embedding model
  --description-provider <name>       Description provider: openai, opencode, or opencode-go
  --description-model <name>          Description model (provider default: gpt-5.6-luna or muse-spark-1.3-contributor)
  --description-fallback-model <name> Fallback description model on the same provider
  --reranker-candidates <number>       Candidates sent to the OpenAI LLM reranker (default: 10)
  --dimensions <number>               Embedding dimensions
  --target <ref>                      Target ref for update-git (default: HEAD)
  --rebuild-on-divergence             Rebuild after a rebase or branch change (requires confirmation)
  --force-reindex                     Rebuild an incompatible existing index (requires confirmation)
  --yes-really-rebuild-the-index      Confirm a destructive index rebuild
  --no-reindex                        Skip worktree overlays or reuse a non-Git index
  --callables                         With reindex-files, also regenerate callable descriptions
  --code                              With search, include only the code index unless combined
  --descriptions                      With search, include only descriptions unless combined
  --md                                With search, include only the Markdown index unless combined
  --ignore-errors                     Silence warnings about persisted indexing errors
  --verbose                           Log every external model call instead of one per kind/model
  --limit <number>                    Output limit (default: unlimited; capped by reranker maximum)
  --matches <number>                  Cross-search matches per source function (default: 5)
  --threshold <number|range>          Show similarities at/above a value or within a range (default: 0.3)
  --describe-full-file-threshold <number> Include whole files scoring above this similarity for describe (default: 0.8)
  --format <json|summary|clusters>    Output format (default: summary; cross-search: clusters)
  --cohesion                          Re-rank cross-search matches by physical distance
  --include-symmetric-duplicates      Show both directions of same-index matches
  --cross-file-only                   Exclude matches from the source file
  --min-lines <number>               Minimum callable length for cross-search (default: 2)
  -e, --regexp <regex>               Filter qualified symbols (analysis: sources only)
  --regex <regex>                    Alias for -e/--regexp
  --changed-since <commit>            Search added, modified, or moved functions
  --uncommitted                       Search functions from uncommitted files
  --source-path <path>                Restrict cross-search sources to a file or directory
  --target-root <path>                Root of a second indexed codebase
  --target-index <path>               SQLite path of a second index
  --target-config <path>              Config file for a second indexed codebase

Other Examples:
  Save description state and parallelism without opening an index:
    slopdex config descriptions enable
    slopdex config parallelism 10

  Index specific working-tree files:
    slopdex update-files src/service.ts src/model.ts

  Regenerate stale file descriptions, optionally continuing through callable descriptions:
    slopdex reindex-files
    slopdex reindex-files --callables

  Remove deleted files from the index:
    slopdex delete-files src/removed.ts

  Index HEAD and overlay uncommitted working-tree changes:
    slopdex update-git --target HEAD

  Combine source restrictions (all must match):
    slopdex cross-search -e 'validate' --source-path src --changed-since origin/main --uncommitted

  Filter semantic search results by qualified symbol before applying the limit:
    slopdex search "validate session" -e '^Session\\.' --limit 10
`);
}
