#!/usr/bin/env node

import { existsSync, readFileSync, rmSync } from "node:fs";
import path from "node:path";
import { parseArgs } from "node:util";

import { CodeIndex } from "./code-index.js";
import { JinaEmbeddingProvider } from "./embeddings/jina.js";
import { OpenAIEmbeddingProvider } from "./embeddings/openai.js";
import { CodeIndexError, GitUnavailableError, IncompatibleIndexError } from "./errors.js";
import { formatSimilarityClusters, formatSimilaritySummary } from "./format.js";
import { crossSearch } from "./search/cross-search.js";
import type {
  CodeIndexOptions,
  CrossSearchOptions,
  CrossSearchResult,
  CrossSearchSourceFilter,
  EmbeddingProvider,
  IndexedFunction,
  SimilarityResult,
  UpdateStats,
} from "./types.js";

interface FileConfig {
  provider?: "openai" | "jina";
  model?: string;
  dimensions?: number;
  indexPath?: string;
  include?: string[];
  exclude?: string[];
  maxFileSize?: number;
  embeddingBatchSize?: number;
}

const parsed = (() => {
  try {
    return parseArgs({
      args: process.argv.slice(2),
      allowPositionals: true,
      strict: true,
      options: {
        root: { type: "string", default: process.cwd() },
        config: { type: "string" },
        index: { type: "string" },
        provider: { type: "string" },
        model: { type: "string" },
        dimensions: { type: "string" },
        target: { type: "string" },
        "target-root": { type: "string" },
        "target-index": { type: "string" },
        "target-config": { type: "string" },
        "changed-since": { type: "string" },
        uncommitted: { type: "boolean", default: false },
        "source-path": { type: "string" },
        limit: { type: "string" },
        threshold: { type: "string" },
        format: { type: "string" },
        "include-symmetric-duplicates": { type: "boolean", default: false },
        "rebuild-on-divergence": { type: "boolean", default: false },
        "force-rebuild": { type: "boolean", default: false },
        "no-reindex": { type: "boolean", default: false },
        help: { type: "boolean", short: "h", default: false },
      },
    });
  } catch (error) {
    process.stderr.write(`slopdex: ${error instanceof Error ? error.message : String(error)}\n`);
    process.exit(2);
  }
})();

const [command, ...positionals] = parsed.positionals;

if (parsed.values.help || !command) {
  printHelp();
  process.exit(parsed.values.help ? 0 : 1);
}

void main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : String(error);
  process.stderr.write(`slopdex: ${message}\n`);
  process.exitCode = error instanceof CodeIndexError ? 2 : 1;
});

async function main(): Promise<void> {
  validateInvocation();
  const rootDir = path.resolve(parsed.values.root!);
  const config = loadConfig(rootDir, parsed.values.config);
  const provider = createProvider(config);
  const indexOptions: CodeIndexOptions = {
    rootDir,
    provider,
    onWarning: (message) => process.stderr.write(`slopdex: warning: ${message}\n`),
    ...(parsed.values.index || config.indexPath ? { indexPath: parsed.values.index ?? config.indexPath } : {}),
    ...(config.include ? { include: config.include } : {}),
    ...(config.exclude ? { exclude: config.exclude } : {}),
    ...(config.maxFileSize ? { maxFileSize: config.maxFileSize } : {}),
    ...(config.embeddingBatchSize ? { embeddingBatchSize: config.embeddingBatchSize } : {}),
  };
  const updateTarget = command === "update-git" ? parsed.values.target ?? "HEAD" : "HEAD";
  const updateStats = await ensureIndexUpdated(
    indexOptions,
    "index",
    updateTarget,
    parsed.values["rebuild-on-divergence"],
    parsed.values["force-rebuild"],
    parsed.values["no-reindex"],
  );
  const index = new CodeIndex(indexOptions);
  try {
    switch (command) {
      case "status":
        printJson(index.status());
        break;
      case "update-files":
        if (positionals.length === 0) throw new CodeIndexError("update-files requires at least one path.");
        printJson(await index.updateFiles({ upsert: positionals }));
        break;
      case "delete-files":
        if (positionals.length === 0) throw new CodeIndexError("delete-files requires at least one path.");
        printJson(await index.updateFiles({ delete: positionals }));
        break;
      case "update-git": {
        printJson(updateStats);
        break;
      }
      case "search": {
        const query = positionals.join(" ").trim();
        if (!query) throw new CodeIndexError("search requires a query.");
        const threshold = similarityThreshold();
        const results = await index.similaritySearch({
          query,
          limit: numberOption(parsed.values.limit, 10, "limit"),
          minSimilarity: threshold.min,
          ...(threshold.max !== undefined ? { maxSimilarity: threshold.max } : {}),
        });
        const format = outputFormat();
        if (format === "clusters") throw new CodeIndexError("clusters format is only available for cross-search.");
        if (format === "summary") process.stdout.write(`${formatSimilaritySummary(results)}\n`);
        else printJson(results.map(presentMatch));
        break;
      }
      case "cross-search":
        await runCrossSearch(index, indexOptions, provider);
        break;
      default:
        throw new CodeIndexError(`Unknown command: ${command}`);
    }
  } finally {
    index.close();
  }
}

async function runCrossSearch(
  source: CodeIndex,
  sourceOptions: CodeIndexOptions,
  provider: EmbeddingProvider,
): Promise<void> {
  const targetRoot = parsed.values["target-root"];
  const targetPath = parsed.values["target-index"];
  if ((targetRoot && !targetPath) || (!targetRoot && targetPath)) {
    throw new CodeIndexError("Cross-index search requires both --target-root and --target-index.");
  }
  const targetRootDir = targetRoot ? path.resolve(targetRoot) : undefined;
  const resolvedTargetPath = targetPath ? path.resolve(targetPath) : undefined;
  const usesSourceAsTarget = targetRootDir === source.rootDir && resolvedTargetPath === source.indexPath;
  const targetConfig = targetRootDir && !usesSourceAsTarget
    ? loadConfig(targetRootDir, parsed.values["target-config"])
    : undefined;
  const targetOptions: CodeIndexOptions | undefined = targetRootDir && resolvedTargetPath && !usesSourceAsTarget ? {
    rootDir: targetRootDir,
    indexPath: resolvedTargetPath,
    provider,
    ...(sourceOptions.onWarning ? { onWarning: sourceOptions.onWarning } : {}),
    ...(targetConfig?.include ? { include: targetConfig.include } : {}),
    ...(targetConfig?.exclude ? { exclude: targetConfig.exclude } : {}),
    ...(targetConfig?.maxFileSize ? { maxFileSize: targetConfig.maxFileSize } : {}),
    ...(targetConfig?.embeddingBatchSize ? { embeddingBatchSize: targetConfig.embeddingBatchSize } : {}),
  } : undefined;
  if (targetOptions) {
    await ensureIndexUpdated(
      targetOptions,
      "target index",
      "HEAD",
      parsed.values["rebuild-on-divergence"],
      parsed.values["force-rebuild"],
      parsed.values["no-reindex"],
    );
  }
  const target = targetOptions ? new CodeIndex({ ...targetOptions, readOnly: true }) : undefined;
  const format = outputFormat();
  const threshold = similarityThreshold();
  const searchOptions: CrossSearchOptions = {
    source,
    ...(target ? { target } : {}),
    sourceFilter: crossSearchSourceFilter(),
    limitPerFunction: numberOption(parsed.values.limit, 5, "limit"),
    minSimilarity: threshold.min,
    ...(threshold.max !== undefined ? { maxSimilarity: threshold.max } : {}),
    includeSymmetricDuplicates: parsed.values["include-symmetric-duplicates"],
  };
  try {
    if (format === "clusters") {
      const results: CrossSearchResult[] = [];
      for await (const result of crossSearch(searchOptions)) results.push(result);
      process.stdout.write(`${formatSimilarityClusters(results, !target)}\n`);
      return;
    }
    for await (const result of crossSearch(searchOptions)) {
      if (format === "summary") {
        process.stdout.write(`${formatSimilaritySummary(result.matches, result.source)}\n`);
      } else {
        process.stdout.write(`${JSON.stringify({
          source: presentFunction(result.source),
          matches: result.matches.map(presentMatch),
        })}\n`);
      }
    }
  } finally {
    target?.close();
  }
}

async function ensureIndexUpdated(
  options: CodeIndexOptions,
  label: string,
  target: string,
  rebuildOnDivergence: boolean,
  forceRebuild: boolean,
  noReindex: boolean,
): Promise<UpdateStats> {
  const initialized = await initializeMissingIndex(options, label, target, noReindex);
  if (initialized) return initialized;
  try {
    const index = new CodeIndex(options);
    try {
      return await refreshIndex(index, label, target, rebuildOnDivergence, noReindex);
    } finally {
      index.close();
    }
  } catch (error) {
    if (!forceRebuild || !(error instanceof IncompatibleIndexError)) throw error;
    process.stderr.write(
      `slopdex: warning: ${label} is incompatible (${error.message}); rebuilding automatically because --force-rebuild was specified.\n`,
    );
    const indexPath = resolveIndexPath(options);
    removeIndexArtifacts(indexPath);
    return initializeIndex(options, indexPath, label, target, noReindex);
  }
}

async function initializeMissingIndex(
  options: CodeIndexOptions,
  label: string,
  target: string,
  noReindex: boolean,
): Promise<UpdateStats | null> {
  const indexPath = resolveIndexPath(options);
  if (existsSync(indexPath)) return null;

  process.stderr.write(
    `slopdex: ${label} not found at ${indexPath}; initializing automatically from ${target}${noReindex ? "" : " and the working tree"}.\n`,
  );
  return initializeIndex(options, indexPath, label, target, noReindex);
}

async function initializeIndex(
  options: CodeIndexOptions,
  indexPath: string,
  label: string,
  target: string,
  noReindex: boolean,
): Promise<UpdateStats> {
  const index = new CodeIndex({ ...options, indexPath });
  let initialized = false;
  try {
    const stats = await refreshIndex(index, label, target, false, noReindex);
    initialized = true;
    return stats;
  } finally {
    index.close();
    if (!initialized) {
      removeIndexArtifacts(indexPath);
    }
  }
}

async function refreshIndex(
  index: CodeIndex,
  label: string,
  target: string,
  rebuildOnDivergence: boolean,
  noReindex: boolean,
): Promise<UpdateStats> {
  try {
    return await index.updateFromGit({ target, rebuildOnDivergence, includeWorkingTree: !noReindex });
  } catch (error) {
    if (!(error instanceof GitUnavailableError)) throw error;
    const status = index.status();
    if (noReindex && status.fileCount > 0) {
      process.stderr.write(
        `slopdex: warning: no Git repository is available for ${label}; full working-tree re-index skipped because --no-reindex was specified.\n`,
      );
      return {
        filesUpdated: 0,
        filesDeleted: 0,
        functionsAdded: 0,
        functionsUpdated: 0,
        functionsDeleted: 0,
        embeddingsCreated: 0,
        checkpoint: status.gitCheckpoint,
      };
    }
    process.stderr.write(
      `slopdex: warning: no Git repository is available for ${label}; re-indexing all source files from the working tree.\n`,
    );
    return await index.updateFromWorkingTree();
  }
}

function resolveIndexPath(options: CodeIndexOptions): string {
  return path.resolve(options.indexPath ?? path.join(path.resolve(options.rootDir), ".slopdex", "index.sqlite"));
}

function removeIndexArtifacts(indexPath: string): void {
  for (const suffix of ["", "-shm", "-wal", "-journal"]) rmSync(`${indexPath}${suffix}`, { force: true });
}

function loadConfig(rootDir: string, configuredPath?: string): FileConfig {
  const configPath = path.resolve(configuredPath ?? path.join(rootDir, ".slopdex", "config.json"));
  if (!existsSync(configPath)) return commandLineConfig({});
  const value = JSON.parse(readFileSync(configPath, "utf8")) as FileConfig;
  return commandLineConfig(value);
}

function commandLineConfig(config: FileConfig): FileConfig {
  const dimensions = parsed.values.dimensions
    ? numberOption(parsed.values.dimensions, 0, "dimensions")
    : config.dimensions;
  const providerValue = parsed.values.provider ?? config.provider;
  if (providerValue !== undefined && providerValue !== "openai" && providerValue !== "jina") {
    throw new CodeIndexError(`Unsupported provider: ${providerValue}`);
  }
  const provider: FileConfig["provider"] = providerValue === "openai" || providerValue === "jina"
    ? providerValue
    : undefined;
  return {
    ...config,
    ...(provider ? { provider } : {}),
    ...(parsed.values.model ? { model: parsed.values.model } : {}),
    ...(dimensions ? { dimensions } : {}),
  };
}

function createProvider(config: FileConfig): EmbeddingProvider {
  if ((config.provider ?? "openai") === "jina") {
    return new JinaEmbeddingProvider({
      ...(config.model ? { model: config.model } : {}),
      ...(config.dimensions ? { dimensions: config.dimensions } : {}),
    });
  }
  return new OpenAIEmbeddingProvider({
    ...(config.model ? { model: config.model } : {}),
    ...(config.dimensions ? { dimensions: config.dimensions } : {}),
  });
}

function numberOption(value: string | undefined, defaultValue: number, name: string): number {
  if (value === undefined) return defaultValue;
  const parsedValue = Number(value);
  if (!Number.isFinite(parsedValue)) throw new CodeIndexError(`${name} must be a number.`);
  return parsedValue;
}

function validateInvocation(): void {
  switch (command) {
    case "status":
    case "update-git":
      return;
    case "update-files":
      if (positionals.length === 0) throw new CodeIndexError("update-files requires at least one path.");
      return;
    case "delete-files":
      if (positionals.length === 0) throw new CodeIndexError("delete-files requires at least one path.");
      return;
    case "search": {
      if (!positionals.join(" ").trim()) throw new CodeIndexError("search requires a query.");
      validateLimit(10);
      similarityThreshold();
      if (outputFormat() === "clusters") throw new CodeIndexError("clusters format is only available for cross-search.");
      return;
    }
    case "cross-search":
      if (Boolean(parsed.values["target-root"]) !== Boolean(parsed.values["target-index"])) {
        throw new CodeIndexError("Cross-index search requires both --target-root and --target-index.");
      }
      if (parsed.values["target-config"] && !parsed.values["target-root"]) {
        throw new CodeIndexError("--target-config requires --target-root and --target-index.");
      }
      validateLimit(5);
      similarityThreshold();
      outputFormat();
      crossSearchSourceFilter();
      return;
    default:
      throw new CodeIndexError(`Unknown command: ${command}`);
  }
}

function validateLimit(defaultValue: number): void {
  const limit = numberOption(parsed.values.limit, defaultValue, "limit");
  if (!Number.isInteger(limit) || limit < 1) throw new CodeIndexError("limit must be a positive integer.");
}

function similarityThreshold(): { min: number; max?: number } {
  const threshold = parsed.values.threshold;
  if (threshold === undefined) return { min: -1 };

  const number = "[+-]?(?:\\d+(?:\\.\\d*)?|\\.\\d+)(?:e[+-]?\\d+)?";
  const range = new RegExp(`^\\s*(${number})\\s*-\\s*(${number})\\s*$`, "i").exec(threshold);
  if (!range) return { min: numberOption(threshold, -1, "threshold") };
  const min = Number(range[1]);
  const max = Number(range[2]);
  if (!Number.isFinite(min) || !Number.isFinite(max)) throw new CodeIndexError("threshold range bounds must be numbers.");
  if (min > max) throw new CodeIndexError("threshold range minimum must not exceed its maximum.");
  return { min, max };
}

function outputFormat(): "json" | "summary" | "clusters" {
  const value = parsed.values.format ?? "json";
  if (value !== "json" && value !== "summary" && value !== "clusters") {
    throw new CodeIndexError("format must be json, summary, or clusters.");
  }
  return value;
}

function crossSearchSourceFilter(): CrossSearchSourceFilter {
  const changedSince = parsed.values["changed-since"];
  const uncommitted = parsed.values.uncommitted;
  if (changedSince && uncommitted) throw new CodeIndexError("Use either --changed-since or --uncommitted, not both.");
  const pathOption = parsed.values["source-path"] ? { path: parsed.values["source-path"] } : {};
  if (changedSince) return { type: "changed-since", commit: changedSince, ...pathOption };
  if (uncommitted) return { type: "uncommitted", ...pathOption };
  return { type: "all", ...pathOption };
}

function presentFunction(value: IndexedFunction) {
  const { embeddingInput: _embeddingInput, embeddingId: _embeddingId, ...result } = value;
  return result;
}

function presentMatch(value: SimilarityResult) {
  return { similarity: value.similarity, function: presentFunction(value.function) };
}

function printJson(value: unknown): void {
  process.stdout.write(`${JSON.stringify(value, null, 2)}\n`);
}

function printHelp(): void {
  process.stdout.write(`Usage: slopdex <command> [arguments] [options]

Commands:
  status                              Show index metadata
  update-files <path...>              Index specific working-tree files
  delete-files <path...>              Remove specific files from the index
  update-git                          Index a Git snapshot plus working-tree changes
  search <query>                      Search functions by semantic similarity
  cross-search                        Find nearest functions for each source function

Options:
  --root <path>                       Repository root (default: current directory)
  --config <path>                     Config file (default: .slopdex/config.json)
  --index <path>                      SQLite index path
  --provider <openai|jina>            Embedding provider
  --model <name>                      Embedding model
  --dimensions <number>               Embedding dimensions
  --target <ref>                      Target ref for update-git (default: HEAD)
  --rebuild-on-divergence             Rebuild after a rebase or branch change
  --force-rebuild                     Rebuild an incompatible existing index
  --no-reindex                        Skip worktree overlays or reuse a non-Git index
  --limit <number>                    Search result limit
  --threshold <number|range>          Show similarities at/above a value or within a range
  --format <json|summary|clusters>    Similarity output format (default: json)
  --include-symmetric-duplicates      Show both directions of same-index matches
  --changed-since <commit>            Search added, modified, or moved functions
  --uncommitted                       Search functions from uncommitted files
  --source-path <path>                Restrict cross-search sources to a file or directory
  --target-root <path>                Root of a second indexed codebase
  --target-index <path>               SQLite path of a second index
  --target-config <path>              Config file for a second indexed codebase

Examples:
  Show metadata for the current index:
    slopdex status

  Index specific working-tree files:
    slopdex update-files src/service.ts src/model.ts

  Remove deleted files from the index:
    slopdex delete-files src/removed.ts

  Index HEAD and overlay uncommitted working-tree changes:
    slopdex update-git --target HEAD

  Find functions matching a semantic query:
    slopdex search "validate an authenticated session" --limit 10

  Review similar functions as duplicate-code candidates:
    slopdex cross-search --format summary --threshold 0.8 --limit 5

  Review functions under a path against the whole codebase:
    slopdex cross-search --source-path src/services --format summary
`);
}
