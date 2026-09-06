#!/usr/bin/env node

import { existsSync, readFileSync, rmSync } from "node:fs";
import path from "node:path";
import { parseArgs } from "node:util";

import { CodeIndex } from "./code-index.js";
import { JinaEmbeddingProvider } from "./embeddings/jina.js";
import { OpenAIEmbeddingProvider } from "./embeddings/openai.js";
import { CodeIndexError } from "./errors.js";
import { formatSimilaritySummary } from "./format.js";
import { crossSearch } from "./search/cross-search.js";
import type { CodeIndexOptions, EmbeddingProvider, IndexedFunction, SimilarityResult, UpdateStats } from "./types.js";

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

const parsed = parseArgs({
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
    "added-since": { type: "string" },
    "source-path": { type: "string" },
    limit: { type: "string" },
    "min-similarity": { type: "string" },
    threshold: { type: "string" },
    format: { type: "string" },
    "include-symmetric-duplicates": { type: "boolean", default: false },
    "rebuild-on-divergence": { type: "boolean", default: false },
    help: { type: "boolean", short: "h", default: false },
  },
});

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
  const initialized = await initializeMissingIndex(
    indexOptions,
    "index",
    command === "update-git" ? parsed.values.target ?? "HEAD" : "HEAD",
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
        const stats = initialized ?? await index.updateFromGit({
          ...(parsed.values.target ? { target: parsed.values.target } : {}),
          rebuildOnDivergence: parsed.values["rebuild-on-divergence"],
        });
        printJson(stats);
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
        if (outputFormat() === "summary") process.stdout.write(`${formatSimilaritySummary(results)}\n`);
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
  const targetOptions: CodeIndexOptions | undefined = targetRoot && targetPath
    ? {
      ...sourceOptions,
      rootDir: path.resolve(targetRoot),
      indexPath: path.resolve(targetPath),
      provider,
    }
    : undefined;
  if (targetOptions) await initializeMissingIndex(targetOptions, "target index", "HEAD");
  const target = targetOptions ? new CodeIndex({ ...targetOptions, readOnly: true }) : undefined;
  const format = outputFormat();
  const threshold = similarityThreshold();
  try {
    for await (const result of crossSearch({
      source,
      ...(target ? { target } : {}),
      sourceFilter: parsed.values["added-since"]
        ? {
          type: "added-since",
          commit: parsed.values["added-since"],
          ...(parsed.values["source-path"] ? { path: parsed.values["source-path"] } : {}),
        }
        : {
          type: "all",
          ...(parsed.values["source-path"] ? { path: parsed.values["source-path"] } : {}),
        },
      limitPerFunction: numberOption(parsed.values.limit, 5, "limit"),
      minSimilarity: threshold.min,
      ...(threshold.max !== undefined ? { maxSimilarity: threshold.max } : {}),
      includeSymmetricDuplicates: parsed.values["include-symmetric-duplicates"],
    })) {
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

async function initializeMissingIndex(
  options: CodeIndexOptions,
  label: string,
  target: string,
): Promise<UpdateStats | null> {
  const indexPath = path.resolve(options.indexPath ?? path.join(path.resolve(options.rootDir), ".slopdex", "index.sqlite"));
  if (existsSync(indexPath)) return null;

  process.stderr.write(`slopdex: ${label} not found at ${indexPath}; initializing automatically from committed ${target}.\n`);
  const index = new CodeIndex({ ...options, indexPath });
  let initialized = false;
  try {
    const stats = await index.updateFromGit({ target });
    initialized = true;
    return stats;
  } finally {
    index.close();
    if (!initialized) {
      for (const suffix of ["", "-shm", "-wal", "-journal"]) rmSync(`${indexPath}${suffix}`, { force: true });
    }
  }
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

function similarityThreshold(): { min: number; max?: number } {
  const threshold = parsed.values.threshold;
  const minimum = parsed.values["min-similarity"];
  if (threshold !== undefined && minimum !== undefined) {
    throw new CodeIndexError("Use either --threshold or --min-similarity, not both.");
  }
  if (threshold === undefined) return { min: numberOption(minimum, -1, "min-similarity") };

  const number = "[+-]?(?:\\d+(?:\\.\\d*)?|\\.\\d+)(?:e[+-]?\\d+)?";
  const range = new RegExp(`^\\s*(${number})\\s*-\\s*(${number})\\s*$`, "i").exec(threshold);
  if (!range) return { min: numberOption(threshold, -1, "threshold") };
  const min = Number(range[1]);
  const max = Number(range[2]);
  if (!Number.isFinite(min) || !Number.isFinite(max)) throw new CodeIndexError("threshold range bounds must be numbers.");
  if (min > max) throw new CodeIndexError("threshold range minimum must not exceed its maximum.");
  return { min, max };
}

function outputFormat(): "json" | "summary" {
  const value = parsed.values.format ?? "json";
  if (value !== "json" && value !== "summary") throw new CodeIndexError("format must be json or summary.");
  return value;
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
  update-git                          Index committed changes since the checkpoint
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
  --limit <number>                    Search result limit
  --threshold <number|range>          Show similarities at/above a value or within a range
  --min-similarity <number>           Minimum raw cosine similarity
  --format <json|summary>             Similarity output format (default: json)
  --include-symmetric-duplicates      Show both directions of same-index matches
  --added-since <commit>              Restrict cross-search source functions
  --source-path <path>                Restrict cross-search sources to a file or directory
  --target-root <path>                Root of a second indexed codebase
  --target-index <path>               SQLite path of a second index

Examples:
  Show metadata for the current index:
    slopdex status

  Index specific working-tree files:
    slopdex update-files src/service.ts src/model.ts

  Remove deleted files from the index:
    slopdex delete-files src/removed.ts

  Index the latest committed Git snapshot:
    slopdex update-git --target HEAD

  Find functions matching a semantic query:
    slopdex search "validate an authenticated session" --limit 10

  Review similar functions as duplicate-code candidates:
    slopdex cross-search --format summary --threshold 0.8 --limit 5

  Review functions under a path against the whole codebase:
    slopdex cross-search --source-path src/services --format summary
`);
}
