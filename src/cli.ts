#!/usr/bin/env node

import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { parseArgs } from "node:util";

import { CodeIndex } from "./code-index.js";
import { JinaEmbeddingProvider } from "./embeddings/jina.js";
import { OpenAIEmbeddingProvider } from "./embeddings/openai.js";
import { CodeIndexError, GitUnavailableError, IncompatibleIndexError } from "./errors.js";
import { formatSimilarityClusters, formatSimilaritySummary } from "./format.js";
import { CohereReranker, JinaReranker } from "./rerankers/hosted.js";
import { OpenAILLMReranker } from "./rerankers/openai.js";
import { crossSearch } from "./search/cross-search.js";
import {
  descriptionProviderBaseUrl,
  isDescriptionProviderName,
  OpenAIDescriptionProvider,
  type DescriptionProviderName,
} from "./descriptions/openai.js";
import { readIndexErrorCounts, readIndexErrors, resetIndexState } from "./storage/database.js";
import { compileNameRegex } from "./utils.js";
import type {
  CodeIndexOptions,
  CrossSearchOptions,
  CrossSearchResult,
  CrossSearchSourceFilter,
  EmbeddingProvider,
  IndexedFunction,
  SimilarityResult,
  Reranker,
  DescriptionProfile,
  UpdateStats,
} from "./types.js";

declare const __SLOPDEX_VERSION__: string;

interface FileConfig {
  provider?: "openai" | "jina";
  model?: string;
  dimensions?: number;
  indexPath?: string;
  include?: string[];
  exclude?: string[];
  maxFileSize?: number;
  embeddingBatchSize?: number;
  descriptionProvider?: DescriptionProviderName;
  descriptionModel?: string;
  descriptionsEnabled?: boolean;
  rerankerProvider?: "cohere" | "jina" | "openai";
  rerankerModel?: string;
  rerankerCandidates?: number;
  rerankingEnabled?: boolean;
  verbose?: boolean;
}

type OpenCodeDescriptionProvider = Exclude<DescriptionProviderName, "openai">;

interface PublishedModel {
  provider: OpenCodeDescriptionProvider;
  model: string;
}

interface DescriptionRefreshHooks {
  beforeRefresh?: (index: CodeIndex) => void | Promise<void>;
  afterRefresh?: (index: CodeIndex) => void | Promise<void>;
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
        "description-provider": { type: "string" },
        "description-model": { type: "string" },
        "reranker-candidates": { type: "string" },
        dimensions: { type: "string" },
        target: { type: "string" },
        "target-root": { type: "string" },
        "target-index": { type: "string" },
        "target-config": { type: "string" },
        "changed-since": { type: "string" },
        uncommitted: { type: "boolean", default: false },
        "source-path": { type: "string" },
        "min-lines": { type: "string" },
        regex: { type: "string" },
        regexp: { type: "string", short: "e" },
        limit: { type: "string" },
        threshold: { type: "string" },
        format: { type: "string" },
        cohesion: { type: "boolean", default: false },
        "include-symmetric-duplicates": { type: "boolean", default: false },
        "cross-file-only": { type: "boolean", default: false },
        "rebuild-on-divergence": { type: "boolean", default: false },
        "force-reindex": { type: "boolean", default: false },
        "no-reindex": { type: "boolean", default: false },
        callables: { type: "boolean", default: false },
        "ignore-errors": { type: "boolean", default: false },
        verbose: { type: "boolean", default: false },
        version: { type: "boolean", default: false },
        help: { type: "boolean", short: "h", default: false },
      },
    });
  } catch (error) {
    process.stderr.write(`slopdex: ${error instanceof Error ? error.message : String(error)}\n`);
    process.exit(2);
  }
})();

const [command, ...positionals] = parsed.positionals;
const descriptionsAction = command === "descriptions" ? positionals[0] : undefined;

if (parsed.values.version) {
  const version = typeof __SLOPDEX_VERSION__ === "string"
    ? __SLOPDEX_VERSION__
    : (JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")) as { version: string }).version;
  process.stdout.write(`${version}\n`);
  process.exit(0);
}

// Read persisted diagnostics at exit so cached, failed, and help invocations also
// report them, and update/delete commands report the final state rather than stale errors.
const diagnosticIndexes = new Set<string>();
process.on("exit", () => {
  if (parsed.values["ignore-errors"]) return;
  try {
    const rootDir = path.resolve(parsed.values.root!);
    const config = loadConfig(rootDir, parsed.values.config);
    diagnosticIndexes.add(path.resolve(parsed.values.index ?? config.indexPath ?? path.join(rootDir, ".slopdex/index.sqlite")));
    if (parsed.values["target-index"]) diagnosticIndexes.add(path.resolve(parsed.values["target-index"]));
  } catch {
    // Invalid configuration is reported by main; diagnostics must not mask it.
  }
  for (const indexPath of diagnosticIndexes) {
    try {
      if (!existsSync(indexPath)) continue;
      const { errors, files } = readIndexErrorCounts(indexPath);
      if (errors === 0) continue;
      process.stderr.write(`slopdex: warning: ${errors} unresolved indexing error(s) in ${files} file(s); run slopdex index-errors --index ${JSON.stringify(indexPath)} to inspect, or use --ignore-errors to silence this warning.\n`);
    } catch {
      // Opening an invalid index is handled by the command's normal error path.
    }
  }
});

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
  if (command === "models") {
    await runModels();
    return;
  }
  if (command === "config") {
    await runConfig(rootDir);
    return;
  }
  const config = loadConfig(rootDir, parsed.values.config);
  if (command === "index-errors") {
    const indexPath = path.resolve(parsed.values.index ?? config.indexPath ?? path.join(rootDir, ".slopdex/index.sqlite"));
    const errors = existsSync(indexPath) ? readIndexErrors(indexPath) : [];
    if (outputFormat("summary") === "summary") {
      process.stdout.write(errors.length === 0 ? "No indexing errors.\n" : `${errors.map((error) =>
        `${error.path}${error.startLine === null ? "" : `:${error.startLine}:${error.startColumn}`} ${error.qualifiedName ? `:: ${error.qualifiedName} ` : ""}[${error.code}]\n  ${error.message}`,
      ).join("\n")}\n`);
    } else printJson(errors);
    return;
  }
  const provider = createProvider(config);
  const reranker = command === "search" || command === "search-description" ? createReranker(config) : undefined;
  const descriptionProvider = createDescriptionProvider(config);
  const indexOptions: CodeIndexOptions = {
    rootDir,
    provider,
    ...(reranker ? { reranker } : {}),
    ...(descriptionProvider ? { descriptionProvider } : {}),
    onWarning: () => {}, // Persisted diagnostics are reported once per index at exit.
    ...(parsed.values.index || config.indexPath ? { indexPath: parsed.values.index ?? config.indexPath } : {}),
    ...(config.include ? { include: config.include } : {}),
    ...(config.exclude ? { exclude: config.exclude } : {}),
    ...(config.maxFileSize ? { maxFileSize: config.maxFileSize } : {}),
    ...(config.embeddingBatchSize ? { embeddingBatchSize: config.embeddingBatchSize } : {}),
    ...(config.verbose ? { verbose: true } : {}),
  };
  const updateTarget = command === "update-git" ? parsed.values.target ?? "HEAD" : "HEAD";
  const updateStats = await ensureIndexUpdated(
    indexOptions,
    "index",
    updateTarget,
    parsed.values["rebuild-on-divergence"],
    parsed.values["force-reindex"],
    parsed.values["no-reindex"],
    descriptionRefresh(config),
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
      case "reindex-files":
        printJson(await index.reindexFiles({ includeCallables: parsed.values.callables }));
        break;
      case "descriptions":
        printJson(descriptionsAction === "enable" ? await index.useDescriptions() : index.disableDescriptions());
        break;
      case "search":
      case "search-description": {
        const query = positionals.join(" ").trim();
        if (!query) throw new CodeIndexError(`${command} requires a query.`);
        const threshold = similarityThreshold();
        const nameRegex = qualifiedNameRegex();
        const results = await (command === "search-description" ? index.searchDescription.bind(index) : index.similaritySearch.bind(index))({
          query,
          ...(nameRegex !== undefined ? { nameRegex } : {}),
          limit: numberOption(parsed.values.limit, 10, "limit"),
          minSimilarity: threshold.min,
          ...(threshold.max !== undefined ? { maxSimilarity: threshold.max } : {}),
        });
        const format = outputFormat("summary");
        if (format === "clusters") throw new CodeIndexError("clusters format is only available for cross-search.");
        if (format === "summary") {
          process.stdout.write(`${command === "search-description"
            ? results.map((match) => `${formatSimilaritySummary([match])}\n  ${match.function.description}`).join("\n")
            : formatSimilaritySummary(results)}\n`);
        } else printJson(results.map(presentMatch));
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
  const targetDescriptionProvider = targetConfig ? createDescriptionProvider(targetConfig) : undefined;
  const targetOptions: CodeIndexOptions | undefined = targetRootDir && resolvedTargetPath && !usesSourceAsTarget ? {
    rootDir: targetRootDir,
    indexPath: resolvedTargetPath,
    provider,
    ...(sourceOptions.onWarning ? { onWarning: sourceOptions.onWarning } : {}),
    ...(targetConfig?.include ? { include: targetConfig.include } : {}),
    ...(targetConfig?.exclude ? { exclude: targetConfig.exclude } : {}),
    ...(targetConfig?.maxFileSize ? { maxFileSize: targetConfig.maxFileSize } : {}),
    ...(targetConfig?.embeddingBatchSize ? { embeddingBatchSize: targetConfig.embeddingBatchSize } : {}),
    ...(targetConfig?.verbose ? { verbose: true } : {}),
    ...(targetDescriptionProvider ? { descriptionProvider: targetDescriptionProvider } : {}),
  } : undefined;
  if (targetOptions) {
    await ensureIndexUpdated(
      targetOptions,
      "target index",
      "HEAD",
      parsed.values["rebuild-on-divergence"],
      parsed.values["force-reindex"],
      parsed.values["no-reindex"],
      descriptionRefresh(targetConfig!),
    );
  }
  const target = targetOptions ? new CodeIndex({ ...targetOptions, readOnly: true }) : undefined;
  const format = outputFormat(parsed.values.cohesion ? "summary" : "clusters");
  const threshold = similarityThreshold();
  const searchOptions: CrossSearchOptions = {
    source,
    ...(target ? { target } : {}),
    sourceFilter: crossSearchSourceFilter(),
    limitPerFunction: numberOption(parsed.values.limit, 5, "limit"),
    minSimilarity: threshold.min,
    ...(threshold.max !== undefined ? { maxSimilarity: threshold.max } : {}),
    includeSymmetricDuplicates: parsed.values["include-symmetric-duplicates"],
    crossFileOnly: parsed.values["cross-file-only"],
    cohesion: parsed.values.cohesion,
    minLines: minimumLines(),
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
          scoring: result.scoring,
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
  hooks?: DescriptionRefreshHooks,
): Promise<UpdateStats> {
  diagnosticIndexes.add(resolveIndexPath(options));
  const initialized = await initializeMissingIndex(options, label, target, noReindex, hooks);
  if (initialized) return initialized;
  try {
    const index = new CodeIndex(options);
    try {
      await hooks?.beforeRefresh?.(index);
      const stats = await refreshIndex(index, label, target, rebuildOnDivergence, noReindex);
      await hooks?.afterRefresh?.(index);
      return stats;
    } finally {
      index.close();
    }
  } catch (error) {
    if (!forceRebuild || !(error instanceof IncompatibleIndexError)) throw error;
    process.stderr.write(
      `slopdex: warning: ${label} is incompatible (${error.message}); rebuilding automatically because --force-reindex was specified.\n`,
    );
    const indexPath = resolveIndexPath(options);
    const descriptionProfile = descriptionProfileForRebuild(indexPath, options.rootDir);
    try {
      resetIndexState(indexPath, options.rootDir, options.provider.profile);
    } catch (resetError) {
      if (!(resetError instanceof IncompatibleIndexError)) throw resetError;
      removeIndexArtifacts(indexPath);
    }
    return initializeIndex(options, indexPath, label, target, noReindex, descriptionProfile, hooks);
  }
}

async function initializeMissingIndex(
  options: CodeIndexOptions,
  label: string,
  target: string,
  noReindex: boolean,
  hooks?: DescriptionRefreshHooks,
): Promise<UpdateStats | null> {
  const indexPath = resolveIndexPath(options);
  if (existsSync(indexPath)) return null;

  process.stderr.write(
    `slopdex: ${label} not found at ${indexPath}; initializing automatically from ${target}${noReindex ? "" : " and the working tree"}.\n`,
  );
  return initializeIndex(options, indexPath, label, target, noReindex, null, hooks);
}

async function initializeIndex(
  options: CodeIndexOptions,
  indexPath: string,
  label: string,
  target: string,
  noReindex: boolean,
  descriptionProfile: DescriptionProfile | null = null,
  hooks?: DescriptionRefreshHooks,
): Promise<UpdateStats> {
  const index = new CodeIndex({
    ...options, indexPath,
    ...(descriptionProfile && !options.descriptionProvider
      ? { descriptionProvider: new OpenAIDescriptionProvider({
        provider: descriptionProfile.provider as DescriptionProviderName,
        model: descriptionProfile.model,
        ...(options.verbose ? { verbose: true } : {}),
      }) } : {}),
  });
  try {
    if (descriptionProfile) await index.useDescriptions();
    await hooks?.beforeRefresh?.(index);
    const stats = await refreshIndex(index, label, target, false, noReindex);
    await hooks?.afterRefresh?.(index);
    return stats;
  } finally {
    index.close();
  }
}

function descriptionProfileForRebuild(indexPath: string, rootDir: string): DescriptionProfile | null {
  const db = new DatabaseSync(indexPath, { readOnly: true });
  try {
    const metadata = new Map((db.prepare("SELECT key, value FROM metadata").all() as Array<{ key: string; value: string }>)
      .map((row) => [row.key, row.value]));
    if (metadata.get("root_dir") !== path.resolve(rootDir) || metadata.get("descriptions_enabled") !== "true") return null;
    const profile = JSON.parse(metadata.get("description_profile")!) as DescriptionProfile;
    if (!isDescriptionProviderName(profile.provider)) {
      throw new CodeIndexError("Rebuilding this description index requires its custom description provider through the library API.");
    }
    return profile;
  } finally {
    db.close();
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
  const descriptionProviderValue = parsed.values["description-provider"] ?? config.descriptionProvider;
  if (descriptionProviderValue !== undefined && !isDescriptionProviderName(descriptionProviderValue)) {
    throw new CodeIndexError(`Unsupported description provider: ${descriptionProviderValue}`);
  }
  if (config.rerankingEnabled !== undefined && typeof config.rerankingEnabled !== "boolean") {
    throw new CodeIndexError("rerankingEnabled must be a boolean.");
  }
  if (config.verbose !== undefined && typeof config.verbose !== "boolean") {
    throw new CodeIndexError("verbose must be a boolean.");
  }
  if (config.rerankingEnabled === true) {
    if (config.rerankerProvider !== "cohere" && config.rerankerProvider !== "jina" && config.rerankerProvider !== "openai") {
      throw new CodeIndexError(`Unsupported reranker provider: ${String(config.rerankerProvider)}`);
    }
    if (config.rerankerModel !== undefined
      && (typeof config.rerankerModel !== "string" || !config.rerankerModel.trim())) {
      throw new CodeIndexError("rerankerModel must be a non-empty string.");
    }
    if (config.rerankerProvider === "openai" && config.rerankerCandidates !== undefined
      && (typeof config.rerankerCandidates !== "number" || !Number.isInteger(config.rerankerCandidates) || config.rerankerCandidates < 1)) {
      throw new CodeIndexError("rerankerCandidates must be a positive integer.");
    }
    if (config.rerankerProvider === "openai" && typeof config.rerankerCandidates === "number" && config.rerankerCandidates > 100) {
      throw new CodeIndexError("rerankerCandidates must not exceed 100.");
    }
  }
  return {
    ...config,
    ...(provider ? { provider } : {}),
    ...(parsed.values.model ? { model: parsed.values.model } : {}),
    ...(descriptionProviderValue ? { descriptionProvider: descriptionProviderValue } : {}),
    ...(parsed.values["description-model"] ? { descriptionModel: parsed.values["description-model"] } : {}),
    ...(dimensions ? { dimensions } : {}),
    ...(parsed.values.verbose ? { verbose: true } : {}),
  };
}

function createDescriptionProvider(config: FileConfig): OpenAIDescriptionProvider | undefined {
  if (!config.descriptionProvider && !config.descriptionModel) return undefined;
  return new OpenAIDescriptionProvider({
    ...(config.descriptionProvider ? { provider: config.descriptionProvider } : {}),
    ...(config.descriptionModel ? { model: config.descriptionModel } : {}),
    ...(config.verbose ? { verbose: true } : {}),
  });
}

function descriptionRefresh(config: FileConfig): DescriptionRefreshHooks | undefined {
  if (command === "descriptions") {
    return descriptionsAction === "disable"
      ? { beforeRefresh: (index) => { index.disableDescriptions(); } }
      : enableDescriptionRefresh(false);
  }
  if (config.descriptionsEnabled === true) return enableDescriptionRefresh();
  if (config.descriptionsEnabled === false) return { beforeRefresh: (index) => { index.disableDescriptions(); } };
  return undefined;
}

function enableDescriptionRefresh(afterRefresh = true): DescriptionRefreshHooks {
  return {
    beforeRefresh: (index) => {
      const status = index.status();
      if (status.descriptionsEnabled && !sameDescriptionProfile(status.descriptionProfile, index.descriptionProvider.profile)) {
        index.disableDescriptions();
      }
    },
    ...(afterRefresh ? { afterRefresh: async (index: CodeIndex) => { await index.useDescriptions(); } } : {}),
  };
}

function sameDescriptionProfile(left: DescriptionProfile | null, right: DescriptionProfile): boolean {
  return left?.provider === right.provider
    && left.model === right.model
    && left.strategyVersion === right.strategyVersion;
}

async function runModels(): Promise<void> {
  const positionalProvider = positionals[0];
  const optionProvider = parsed.values["description-provider"];
  if (positionalProvider && optionProvider && positionalProvider !== optionProvider) {
    throw new CodeIndexError("models provider and --description-provider must match when both are supplied.");
  }
  const requested = positionalProvider ?? optionProvider;
  if (requested !== undefined && !isOpenCodeDescriptionProvider(requested)) {
    throw new CodeIndexError("models provider must be opencode or opencode-go.");
  }
  const providers: OpenCodeDescriptionProvider[] = requested ? [requested] : ["opencode", "opencode-go"];
  const models = (await Promise.all(providers.map(fetchPublishedModels))).flat();
  const format = outputFormat("summary");
  if (format === "clusters") throw new CodeIndexError("clusters format is only available for cross-search.");
  if (format === "json") printJson(models);
  else process.stdout.write(`${models.map(({ provider, model }) => `${provider}/${model}`).join("\n")}\n`);
}

async function runConfig(rootDir: string): Promise<void> {
  const configPath = path.resolve(parsed.values.config ?? path.join(rootDir, ".slopdex", "config.json"));
  const config = readConfigFile(configPath);
  const action = positionals[0];
  let result: Record<string, unknown>;
  if (action === "descriptions") {
    config.descriptionsEnabled = positionals[1] === "enable";
    result = { descriptionsEnabled: config.descriptionsEnabled };
  } else if (action === "model") {
    const selected = await resolveConfiguredModel(positionals[1]);
    config.descriptionProvider = selected.provider;
    config.descriptionModel = selected.model;
    result = { descriptionProvider: selected.provider, descriptionModel: selected.model };
  } else {
    const provider = positionals[1] as "cohere" | "jina" | "openai" | "disable";
    if (provider === "disable") {
      config.rerankingEnabled = false;
      result = { rerankingEnabled: false };
    } else {
      const sameProvider = config.rerankerProvider === provider;
      const defaultModel = provider === "cohere"
        ? "rerank-v4.0-pro"
        : provider === "jina" ? "jina-reranker-v3.5" : "gpt-5.6-luna";
      const model = positionals[2]
        ?? (sameProvider && typeof config.rerankerModel === "string" && config.rerankerModel.trim() ? config.rerankerModel : defaultModel);
      config.rerankerProvider = provider;
      config.rerankerModel = model;
      if (provider === "openai") {
        const savedCandidates = sameProvider && typeof config.rerankerCandidates === "number"
          && Number.isInteger(config.rerankerCandidates) && config.rerankerCandidates > 0
          ? config.rerankerCandidates
          : 10;
        config.rerankerCandidates = openAIRerankerCandidateCount(
          parsed.values["reranker-candidates"], savedCandidates, "reranker candidate count",
        );
      } else {
        delete config.rerankerCandidates;
      }
      config.rerankingEnabled = true;
      result = {
        rerankingEnabled: true,
        rerankerProvider: provider,
        rerankerModel: model,
        ...(provider === "openai" ? { rerankerCandidates: config.rerankerCandidates } : {}),
      };
    }
  }
  writeConfigFile(configPath, config);
  const format = outputFormat("summary");
  if (format === "clusters") throw new CodeIndexError("clusters format is only available for cross-search.");
  if (format === "json") printJson({ configPath, ...result });
  else process.stdout.write(`Updated ${configPath}: ${Object.entries(result).map(([key, value]) => `${key}=${value}`).join(" ")}\n`);
}

async function resolveConfiguredModel(positionalModel: string | undefined): Promise<PublishedModel> {
  const optionModel = parsed.values["description-model"];
  if (positionalModel && optionModel && positionalModel !== optionModel) {
    throw new CodeIndexError("config model argument and --description-model must match when both are supplied.");
  }
  let reference = positionalModel ?? optionModel;
  if (!reference) throw new CodeIndexError("config model requires a model ID or provider/model reference.");
  let provider = parsed.values["description-provider"];
  const separator = reference.indexOf("/");
  if (separator !== -1) {
    const qualifiedProvider = reference.slice(0, separator);
    reference = reference.slice(separator + 1);
    if (provider && provider !== qualifiedProvider) {
      throw new CodeIndexError("Model reference and --description-provider must select the same provider.");
    }
    provider = qualifiedProvider;
  }
  if (!reference) throw new CodeIndexError("config model requires a non-empty model ID.");
  if (provider !== undefined) {
    if (!isOpenCodeDescriptionProvider(provider)) {
      throw new CodeIndexError("config model provider must be opencode or opencode-go.");
    }
    const models = await fetchPublishedModels(provider);
    if (!models.some(({ model }) => model === reference)) {
      throw new CodeIndexError(`Unknown ${provider} model: ${reference}`);
    }
    return { provider, model: reference };
  }
  const matches = (await Promise.all([
    fetchPublishedModels("opencode"),
    fetchPublishedModels("opencode-go"),
  ])).flat().filter(({ model }) => model === reference);
  if (matches.length === 0) throw new CodeIndexError(`Unknown OpenCode model: ${reference}`);
  if (matches.length > 1) {
    throw new CodeIndexError(`Model ${reference} is available from multiple providers; use provider/model.`);
  }
  return matches[0]!;
}

async function fetchPublishedModels(provider: OpenCodeDescriptionProvider): Promise<PublishedModel[]> {
  const url = `${descriptionProviderBaseUrl(provider)}/models`;
  let response: Response;
  try {
    response = await fetch(url);
  } catch (error) {
    throw new CodeIndexError(`Could not fetch ${provider} models: ${error instanceof Error ? error.message : String(error)}`, { cause: error });
  }
  if (!response.ok) {
    throw new CodeIndexError(`Could not fetch ${provider} models (${response.status}): ${(await response.text()).slice(0, 500)}`);
  }
  let body: { data?: Array<{ id?: unknown }> };
  try {
    body = await response.json() as { data?: Array<{ id?: unknown }> };
  } catch (error) {
    throw new CodeIndexError(`${provider} returned a malformed model list.`, { cause: error });
  }
  if (!Array.isArray(body.data) || body.data.some((item) => typeof item.id !== "string" || item.id.length === 0)) {
    throw new CodeIndexError(`${provider} returned a malformed model list.`);
  }
  return [...new Set(body.data.map((item) => item.id as string))].map((model) => ({ provider, model }));
}

function isOpenCodeDescriptionProvider(value: string): value is OpenCodeDescriptionProvider {
  return value === "opencode" || value === "opencode-go";
}

function readConfigFile(configPath: string): FileConfig {
  if (!existsSync(configPath)) return {};
  const value = JSON.parse(readFileSync(configPath, "utf8")) as unknown;
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new CodeIndexError(`Config must contain a JSON object: ${configPath}`);
  }
  return value as FileConfig;
}

function writeConfigFile(configPath: string, config: FileConfig): void {
  mkdirSync(path.dirname(configPath), { recursive: true });
  const temporaryPath = `${configPath}.tmp-${process.pid}`;
  writeFileSync(temporaryPath, `${JSON.stringify(config, null, 2)}\n`);
  renameSync(temporaryPath, configPath);
}

function createProvider(config: FileConfig): EmbeddingProvider {
  if ((config.provider ?? "openai") === "jina") {
    return new JinaEmbeddingProvider({
      ...(config.model ? { model: config.model } : {}),
      ...(config.dimensions ? { dimensions: config.dimensions } : {}),
      ...(config.verbose ? { verbose: true } : {}),
    });
  }
  return new OpenAIEmbeddingProvider({
    ...(config.model ? { model: config.model } : {}),
    ...(config.dimensions ? { dimensions: config.dimensions } : {}),
    ...(config.verbose ? { verbose: true } : {}),
  });
}

function createReranker(config: FileConfig): Reranker | undefined {
  if (config.rerankingEnabled !== true) return undefined;
  if (config.rerankerProvider === "cohere") {
    return new CohereReranker({
      ...(config.rerankerModel ? { model: config.rerankerModel } : {}),
      ...(config.verbose ? { verbose: true } : {}),
    });
  }
  if (config.rerankerProvider === "jina") {
    return new JinaReranker({
      ...(config.rerankerModel ? { model: config.rerankerModel } : {}),
      ...(config.verbose ? { verbose: true } : {}),
    });
  }
  if (config.rerankerProvider === "openai") {
    return new OpenAILLMReranker({
      ...(config.rerankerModel ? { model: config.rerankerModel } : {}),
      ...(config.rerankerCandidates ? { candidateCount: config.rerankerCandidates } : {}),
      ...(config.verbose ? { verbose: true } : {}),
    });
  }
  throw new CodeIndexError("rerankerProvider is required when rerankingEnabled is true.");
}

function numberOption(value: string | undefined, defaultValue: number, name: string): number {
  if (value === undefined) return defaultValue;
  const parsedValue = Number(value);
  if (!Number.isFinite(parsedValue)) throw new CodeIndexError(`${name} must be a number.`);
  return parsedValue;
}

function validateInvocation(): void {
  if (parsed.values["reranker-candidates"] !== undefined
    && (command !== "config" || positionals[0] !== "reranker" || positionals[1] !== "openai")) {
    throw new CodeIndexError("--reranker-candidates is only available with config reranker openai.");
  }
  if (parsed.values.cohesion && command !== "cross-search") {
    throw new CodeIndexError("--cohesion is only available for cross-search.");
  }
  const nameRegex = qualifiedNameRegex();
  if (nameRegex !== undefined) {
    if (!["search", "search-description", "cross-search"].includes(command!)) {
      throw new CodeIndexError("-e/--regexp/--regex is only available for search, search-description, and cross-search.");
    }
    compileNameRegex(nameRegex, parsed.values.regex !== undefined ? "--regex value" : "-e/--regexp value");
  }
  switch (command) {
    case "models":
      if (positionals.length > 1) throw new CodeIndexError("models accepts at most one provider.");
      if (outputFormat("summary") === "clusters") throw new CodeIndexError("clusters format is only available for cross-search.");
      return;
    case "config":
      if (outputFormat("summary") === "clusters") throw new CodeIndexError("clusters format is only available for cross-search.");
      if (positionals[0] === "descriptions") {
        if (positionals.length !== 2 || (positionals[1] !== "enable" && positionals[1] !== "disable")) {
          throw new CodeIndexError("config descriptions requires enable or disable.");
        }
        return;
      }
      if (positionals[0] === "model") {
        if (positionals.length > 2 || (!positionals[1] && !parsed.values["description-model"])) {
          throw new CodeIndexError("config model requires a model ID or provider/model reference.");
        }
        return;
      }
      if (positionals[0] === "reranker") {
        const provider = positionals[1];
        if ((provider !== "cohere" && provider !== "jina" && provider !== "openai" && provider !== "disable")
          || positionals.length > (provider === "disable" ? 2 : 3)
          || (positionals[2] !== undefined && !positionals[2].trim())) {
          throw new CodeIndexError("config reranker requires cohere, jina, openai, or disable, optionally followed by a model ID.");
        }
        if (provider === "openai") openAIRerankerCandidateCount(parsed.values["reranker-candidates"], 10, "reranker candidate count");
        return;
      }
      throw new CodeIndexError("config requires descriptions, model, or reranker.");
    case "index-errors":
      if (outputFormat("summary") === "clusters") throw new CodeIndexError("clusters format is only available for cross-search.");
      return;
    case "status":
    case "update-git":
      return;
    case "reindex-files":
      if (positionals.length > 0) throw new CodeIndexError("reindex-files does not accept positional arguments.");
      return;
    case "descriptions":
      if (positionals.length !== 1 || (descriptionsAction !== "enable" && descriptionsAction !== "disable")) {
        throw new CodeIndexError("descriptions requires enable or disable.");
      }
      return;
    case "update-files":
      if (positionals.length === 0) throw new CodeIndexError("update-files requires at least one path.");
      return;
    case "delete-files":
      if (positionals.length === 0) throw new CodeIndexError("delete-files requires at least one path.");
      return;
    case "search":
    case "search-description": {
      if (!positionals.join(" ").trim()) throw new CodeIndexError(`${command} requires a query.`);
      validateLimit(10);
      similarityThreshold();
      if (outputFormat("summary") === "clusters") throw new CodeIndexError("clusters format is only available for cross-search.");
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
      if (parsed.values.cohesion && outputFormat("summary") === "clusters") {
        throw new CodeIndexError("clusters format does not preserve cohesion match order; use summary or json.");
      }
      outputFormat(parsed.values.cohesion ? "summary" : "clusters");
      minimumLines();
      crossSearchSourceFilter();
      return;
    default:
      throw new CodeIndexError(`Unknown command: ${command}`);
  }
}

function validateLimit(defaultValue: number): void {
  positiveIntegerOption(parsed.values.limit, defaultValue, "limit");
}

function positiveIntegerOption(value: string | undefined, defaultValue: number, name: string): number {
  const parsedValue = numberOption(value, defaultValue, name);
  if (!Number.isInteger(parsedValue) || parsedValue < 1) throw new CodeIndexError(`${name} must be a positive integer.`);
  return parsedValue;
}

function openAIRerankerCandidateCount(value: string | undefined, defaultValue: number, name: string): number {
  const count = positiveIntegerOption(value, defaultValue, name);
  if (count > 100) throw new CodeIndexError(`${name} must not exceed 100.`);
  return count;
}

function similarityThreshold(defaultMin = -1): { min: number; max?: number } {
  const threshold = parsed.values.threshold;
  if (threshold === undefined) return { min: defaultMin };

  const number = "[+-]?(?:\\d+(?:\\.\\d*)?|\\.\\d+)(?:e[+-]?\\d+)?";
  const range = new RegExp(`^\\s*(${number})\\s*-\\s*(${number})\\s*$`, "i").exec(threshold);
  if (!range) return { min: numberOption(threshold, -1, "threshold") };
  const min = Number(range[1]);
  const max = Number(range[2]);
  if (!Number.isFinite(min) || !Number.isFinite(max)) throw new CodeIndexError("threshold range bounds must be numbers.");
  if (min >= max) throw new CodeIndexError("threshold range minimum must be less than its maximum.");
  return { min, max };
}

function outputFormat(defaultValue: "json" | "summary" | "clusters"): "json" | "summary" | "clusters" {
  const value = parsed.values.format ?? defaultValue;
  if (value !== "json" && value !== "summary" && value !== "clusters") {
    throw new CodeIndexError("format must be json, summary, or clusters.");
  }
  return value;
}

function minimumLines(): number {
  const value = numberOption(parsed.values["min-lines"], 2, "min-lines");
  if (!Number.isInteger(value) || value < 1) throw new CodeIndexError("min-lines must be a positive integer.");
  return value;
}

function qualifiedNameRegex(): string | undefined {
  if (parsed.values.regexp !== undefined && parsed.values.regex !== undefined && parsed.values.regexp !== parsed.values.regex) {
    throw new CodeIndexError("-e/--regexp and --regex are aliases and cannot use different values.");
  }
  return parsed.values.regexp ?? parsed.values.regex;
}

function crossSearchSourceFilter(): CrossSearchSourceFilter {
  const changedSince = parsed.values["changed-since"];
  const uncommitted = parsed.values.uncommitted;
  const nameRegex = qualifiedNameRegex();
  const restrictions = {
    ...(parsed.values["source-path"] ? { path: parsed.values["source-path"] } : {}),
    ...(nameRegex !== undefined ? { nameRegex } : {}),
  };
  if (changedSince) return { type: "changed-since", commit: changedSince, ...(uncommitted ? { uncommitted: true } : {}), ...restrictions };
  if (uncommitted) return { type: "uncommitted", ...restrictions };
  return { type: "all", ...restrictions };
}

function presentFunction(value: IndexedFunction) {
  const { embeddingInput: _embeddingInput, embeddingId: _embeddingId, descriptionEmbeddingId: _descriptionEmbeddingId, ...result } = value;
  return result;
}

function presentMatch(value: SimilarityResult) {
  const { function: callable, ...scores } = value;
  return { ...scores, function: presentFunction(callable) };
}

function printJson(value: unknown): void {
  process.stdout.write(`${JSON.stringify(value, null, 2)}\n`);
}

function printHelp(): void {
  process.stdout.write(`Usage: slopdex <command> [arguments] [options]

Commands:
  models [opencode|opencode-go]        List current published OpenCode models
  config model <model|provider/model>  Validate and save an OpenCode description model
  config descriptions <enable|disable> Save description state without opening an index
  config reranker <provider|disable>    Save Cohere, Jina, or OpenAI reranking settings
  status                              Show index metadata
  index-errors                        List persisted file and function indexing failures
  update-files <path...>              Index specific working-tree files
  reindex-files                       Regenerate stale file descriptions
  delete-files <path...>              Remove specific files from the index
  update-git                          Index a Git snapshot plus working-tree changes
  search <query>                      Search functions by semantic similarity
  descriptions <enable|disable>       Enable or disable automatic purpose descriptions
  search-description <query>          Search functions using description embeddings
  cross-search                        Find nearest functions for each source function

Languages (automatically detected with Tree-sitter):
  Python (.py, .pyw), JavaScript (.js, .mjs, .cjs), JSX (.jsx),
  TypeScript (.ts, .mts, .cts), TSX (.tsx), Rust (.rs), Go (.go),
  Java (.java), and C (.c, .h). Indexes named callables with bodies.

File discovery respects root and nested .gitignore rules, including for tracked files.
Working-tree updates use current rules; committed-only snapshots use rules from that commit.

Analysis Examples:
  Duplicate Analysis:
    slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9 --limit 5

    Cluster 1 (3 functions, similarity 0.9124-0.9568)
      src/auth/session.ts:18:1 :: validateSession
      src/http/middleware.ts:42:1 :: authenticate
      src/users/user-service.ts:27:3 :: UserService.authenticate

    This found three substantial authentication functions in separate files with very
    high similarity. Review them for repeated validation or session logic that could be shared.
    Middleware and service locations may be intentional architectural layers, so this is
    evidence to inspect rather than proof they should merge. Connected components may use
    transitive links, so every function need not directly match every other function.

  Physical Cohesion Review:
    slopdex cross-search --cohesion --threshold 0.8 --limit 20 --format summary

    src/auth/session.ts :: validateSession
      0.9400  packages/http/middleware.ts :: authenticate  [distance 4]
      0.9300  src/auth/token.ts :: validateToken  [distance 1]

    --cohesion keeps cross-search's semantic matches but orders each source's matches
    from greatest to least path distance. Similarity breaks distance ties. This highlights
    related functions stored far apart without introducing a separate analysis command.

Reading Analysis Output:
  With complete description indexes, search and cross-search combine code, callable
  description, and file description similarity equally. Cross-repository search requires complete descriptions on both sides;
  otherwise the analysis uses code-only similarity. Thresholds and limits apply after fusion.
  JSON includes component scores and description-generator profiles for reproducibility.
  Compare results only when similarity mode and weights match.
  Compare values only with the same embedding profile and similar threshold, source-filter,
  and minimum-line settings.

  Duplicate cluster line:
    Cluster N           Display order by function count, then name; not severity
    functions           Unique connected callables; higher may mean a larger duplicate family
    similarity range    Raw cosine edge range; higher means stronger model-assessed resemblance
                       A high minimum means all observed links are strong; a wide range may
                       indicate that a weaker transitive edge joined tighter matches
    callable location   path:line:column :: qualifiedFunctionName; inspect architectural roles

  Cohesion re-ranking:
    distance            0 for the same file, 1 for files in the same folder, and
                       1 plus directory-tree hops for files in different folders
    ordering            Greater distance first; similarity breaks ties
    JSON                Each match includes physicalDistance when --cohesion is enabled

Options:
  --version                           Show the package version
  --root <path>                       Repository root (default: current directory)
  --config <path>                     Config file (default: .slopdex/config.json)
  --index <path>                      SQLite index path
  --provider <openai|jina>            Embedding provider
  --model <name>                      Embedding model
  --description-provider <name>       Description provider: openai, opencode, or opencode-go
  --description-model <name>          Description model (provider default: gpt-5.6-sol or gpt-5.6-luna)
  --reranker-candidates <number>       Candidates sent to the OpenAI LLM reranker (default: 10)
  --dimensions <number>               Embedding dimensions
  --target <ref>                      Target ref for update-git (default: HEAD)
  --rebuild-on-divergence             Rebuild after a rebase or branch change
  --force-reindex                     Rebuild an incompatible existing index
  --no-reindex                        Skip worktree overlays or reuse a non-Git index
  --callables                         With reindex-files, also regenerate callable descriptions
  --ignore-errors                     Silence warnings about persisted indexing errors
  --verbose                           Log every external model call instead of one per kind/model
  --limit <number>                    Search result limit
  --threshold <number|range>          Show similarities at/above a value or within a range
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
  List published OpenCode Go models and save one for future index commands:
    slopdex models opencode-go
    slopdex config model opencode-go/gpt-5.6-luna
    slopdex config descriptions enable

  Show metadata for the current index:
    slopdex status

  Inspect indexing failures without refreshing the index or calling providers:
    slopdex index-errors --format summary

  Index specific working-tree files:
    slopdex update-files src/service.ts src/model.ts

  Regenerate stale file descriptions, optionally continuing through callable descriptions:
    slopdex reindex-files
    slopdex reindex-files --callables

  Remove deleted files from the index:
    slopdex delete-files src/removed.ts

  Index HEAD and overlay uncommitted working-tree changes:
    slopdex update-git --target HEAD

  Find functions matching a semantic query:
    slopdex search "validate an authenticated session" --limit 10

  Enable hosted reranking for query searches:
    slopdex config reranker cohere

  Use OpenAI LLM reranking with high reasoning over the top 10 candidates:
    slopdex config reranker openai

  Enable purpose descriptions, then search by their meaning:
    slopdex descriptions enable
    slopdex search-description "maintain the repository index" --format summary

  Review functions under a path against the whole codebase:
    slopdex cross-search --source-path src/services --format summary

  Review matching source symbols against the whole index:
    slopdex cross-search -e '^UserService\\.' --source-path src --format summary

  Combine source restrictions (all must match):
    slopdex cross-search -e 'validate' --source-path src --changed-since origin/main --uncommitted

  Filter semantic search results by qualified symbol before applying the limit:
    slopdex search "validate session" -e '^Session\\.' --limit 10

  -e/--regexp/--regex uses a case-sensitive JavaScript regex on qualified names. For cross-search
  it filters sources only; targets keep their normal eligibility rules.
  With --changed-since and --uncommitted, sources must be changed since the commit and
  belong to an uncommitted file. --source-path and the regex filter further narrow that intersection.
`);
}
