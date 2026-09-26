#!/usr/bin/env node

import { existsSync, readFileSync, rmSync } from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { parseArgs } from "node:util";

import { CodeIndexError, GitUnavailableError, IncompatibleIndexError } from "./errors.js";
import { formatSimilarityClusters, formatSimilaritySummary } from "./format.js";
import { clearProgress, TerminalProgress } from "./progress.js";
import { compileNameRegex } from "./utils.js";
import {
  configFilePath, indexConfigOptions, isDescriptionProviderName, loadConfig,
  readConfigFile, RERANKER_DEFAULT_MODELS, resolveConfig, validateConfig, writeConfigFile,
  type EffectiveConfig, type OpenCodeDescriptionProvider, type PublishedModel,
} from "./config.js";
import type { CodeIndex } from "./code-index.js";
import type { DescriptionProviderName, OpenAIDescriptionProvider } from "./descriptions/openai.js";
import type {
  CodeIndexOptions,
  CrossSearchOptions,
  CrossSearchResult,
  CrossSearchSourceFilter,
  EmbeddingProvider,
  IndexedFunction,
  MarkdownSearchResult,
  SearchIndex,
  SearchResult,
  SimilarityResult,
  Reranker,
  DescriptionProfile,
  UpdateStats,
} from "./types.js";

declare const __SLOPDEX_VERSION__: string;

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
        "description-fallback-model": { type: "string" },
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
        matches: { type: "string" },
        threshold: { type: "string" },
        "describe-full-file-threshold": { type: "string" },
        format: { type: "string" },
        cohesion: { type: "boolean", default: false },
        "include-symmetric-duplicates": { type: "boolean", default: false },
        "cross-file-only": { type: "boolean", default: false },
        "rebuild-on-divergence": { type: "boolean", default: false },
        "force-reindex": { type: "boolean", default: false },
        "yes-really-rebuild-the-index": { type: "boolean", default: false },
        "no-reindex": { type: "boolean", default: false },
        callables: { type: "boolean", default: false },
        code: { type: "boolean", default: false },
        descriptions: { type: "boolean", default: false },
        md: { type: "boolean", default: false },
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

if (parsed.values.help || !command) {
  printHelp();
  process.exit(parsed.values.help ? 0 : 1);
}

// Read persisted diagnostics at exit so failed and update/delete commands report
// the final state rather than stale errors. Registered after the --help early
// exit, and uses only node:sqlite directly so --help never loads the heavy
// index/provider graph (ai SDK, tree-sitter, sqlite-vec).
const diagnosticIndexes = new Set<string>();
function readErrorCountsLight(indexPath: string): { errors: number; files: number } {
  const database = new DatabaseSync(indexPath, { readOnly: true });
  try {
    if (!database.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'indexing_errors'").get()) {
      return { errors: 0, files: 0 };
    }
    const counts = database.prepare("SELECT COUNT(*) AS errors, COUNT(DISTINCT path) AS files FROM indexing_errors").get() as { errors: number; files: number };
    return { errors: Number(counts.errors), files: Number(counts.files) };
  } finally {
    database.close();
  }
}
process.on("exit", () => {
  if (parsed.values["ignore-errors"]) return;
  try {
    const rootDir = path.resolve(parsed.values.root!);
    const config = loadConfig(rootDir, parsed.values.config, parsed.values);
    diagnosticIndexes.add(config.indexPath);
    if (parsed.values["target-index"]) diagnosticIndexes.add(path.resolve(parsed.values["target-index"]));
  } catch {
    // Invalid configuration is reported by main; diagnostics must not mask it.
  }
  for (const indexPath of diagnosticIndexes) {
    try {
      if (!existsSync(indexPath)) continue;
      const { errors, files } = readErrorCountsLight(indexPath);
      if (errors === 0) continue;
      process.stderr.write(`slopdex: warning: ${errors} unresolved indexing error(s) in ${files} file(s); run slopdex index-errors --index ${JSON.stringify(indexPath)} to inspect, or use --ignore-errors to silence this warning.\n`);
    } catch {
      // Opening an invalid index is handled by the command's normal error path.
    }
  }
});

void main().catch((error: unknown) => {
  clearProgress();
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
  const config = loadConfig(rootDir, parsed.values.config, parsed.values);
  if (command === "index-errors") {
    const indexPath = config.indexPath;
    const { readIndexErrors } = await import("./storage/database.js");
    const errors = existsSync(indexPath) ? readIndexErrors(indexPath) : [];
    if (outputFormat("summary") === "summary") {
      process.stdout.write(errors.length === 0 ? "No indexing errors.\n" : `${errors.map((error) =>
        `${error.path}${error.startLine === null ? "" : `:${error.startLine}:${error.startColumn}`} ${error.qualifiedName ? `:: ${error.qualifiedName} ` : ""}[${error.code}]\n  ${error.message}`,
      ).join("\n")}\n`);
    } else printJson(errors);
    return;
  }
  const provider = await createProvider(config);
  const reranker = ["search", "search-code", "search-description", "search-descriptions", "search-md", "describe"].includes(command!)
    ? await createReranker(config)
    : undefined;
  const descriptionProvider = await createDescriptionProvider(config);
  const progress = new TerminalProgress();
  const indexOptions: CodeIndexOptions = {
    ...indexConfigOptions(config),
    provider,
    onProgress: (value) => progress.update(value),
    ...(reranker ? { reranker } : {}),
    ...(descriptionProvider ? { descriptionProvider } : {}),
    onWarning: () => {}, // Persisted diagnostics are reported once per index at exit.
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
  const { CodeIndex } = await import("./code-index.js");
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
      case "search": {
        const query = positionals.join(" ").trim();
        if (!query) throw new CodeIndexError(`${command} requires a query.`);
        const threshold = similarityThreshold();
        const nameRegex = qualifiedNameRegex();
        const indexes = selectedSearchIndexes();
        const results = await index.search({
          query,
          ...(indexes ? { indexes } : {}),
          ...(nameRegex !== undefined ? { nameRegex } : {}),
          ...(parsed.values.limit === undefined ? {} : { limit: positiveIntegerOption(parsed.values.limit, 1, "limit") }),
          minSimilarity: threshold.min,
          ...(threshold.max !== undefined ? { maxSimilarity: threshold.max } : {}),
        });
        const format = outputFormat("summary");
        if (format === "clusters") throw new CodeIndexError("clusters format is only available for cross-search.");
        if (format === "summary") process.stdout.write(`${results.length > 0 ? results.map(formatSearchResult).join("\n") : "No matches."}\n`);
        else printJson(results.map(presentSearchResult));
        break;
      }
      case "search-code":
      case "search-description":
      case "search-descriptions": {
        const query = positionals.join(" ").trim();
        if (!query) throw new CodeIndexError(`${command} requires a query.`);
        const threshold = similarityThreshold();
        const nameRegex = qualifiedNameRegex();
        const descriptions = command === "search-description" || command === "search-descriptions";
        const results = await (descriptions ? index.searchDescription.bind(index) : index.searchCode.bind(index))({
          query,
          ...(nameRegex !== undefined ? { nameRegex } : {}),
          ...(parsed.values.limit === undefined ? {} : { limit: positiveIntegerOption(parsed.values.limit, 1, "limit") }),
          minSimilarity: threshold.min,
          ...(threshold.max !== undefined ? { maxSimilarity: threshold.max } : {}),
        });
        const format = outputFormat("summary");
        if (format === "clusters") throw new CodeIndexError("clusters format is only available for cross-search.");
        if (format === "summary") {
          process.stdout.write(`${descriptions
            ? results.map((match) => `${formatSimilaritySummary([match])}\n  ${match.function.description}`).join("\n")
            : formatSimilaritySummary(results)}\n`);
        } else printJson(results.map(presentMatch));
        break;
      }
      case "search-md": {
        const query = positionals.join(" ").trim();
        if (!query) throw new CodeIndexError("search-md requires a query.");
        const threshold = similarityThreshold();
        const results = await index.searchMarkdown({
          query,
          ...(parsed.values.limit === undefined ? {} : { limit: positiveIntegerOption(parsed.values.limit, 1, "limit") }),
          minSimilarity: threshold.min,
          ...(threshold.max !== undefined ? { maxSimilarity: threshold.max } : {}),
        });
        const format = outputFormat("summary");
        if (format === "clusters") throw new CodeIndexError("clusters format is only available for cross-search.");
        if (format === "summary") {
          process.stdout.write(`${results.length > 0 ? results.map(formatMarkdownResult).join("\n\n") : "No matches."}\n`);
        } else printJson(results);
        break;
      }
      case "describe":
        await runDescribe(index, descriptionProvider);
        break;
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

async function runDescribe(
  index: CodeIndex,
  descriptionProvider: OpenAIDescriptionProvider | undefined,
): Promise<void> {
  if (!descriptionProvider) throw new CodeIndexError("describe requires a description provider.");
  const query = positionals.join(" ").trim();
  const threshold = similarityThreshold();
  const nameRegex = qualifiedNameRegex();
  const context = await index.describe({
    query,
    ...(nameRegex !== undefined ? { nameRegex } : {}),
    ...(parsed.values.limit === undefined ? {} : { limit: positiveIntegerOption(parsed.values.limit, 1, "limit") }),
    minSimilarity: threshold.min,
    ...(threshold.max !== undefined ? { maxSimilarity: threshold.max } : {}),
    fullFileThreshold: describeFullFileThreshold(),
  });
  if (context.fileContentErrors.length > 0) {
    for (const message of context.fileContentErrors) process.stderr.write(`slopdex: ${message}\n`);
    process.stderr.write("slopdex: continuing without full file contents.\n");
  }
  let description: string;
  try {
    description = await descriptionProvider.describeContext(context);
  } catch (error) {
    if (!context.files.some((file) => file.content !== null)) throw error;
    process.stderr.write(`slopdex: ${error instanceof Error ? error.message : String(error)}\n`);
    process.stderr.write("slopdex: retrying the description without full file contents.\n");
    description = await descriptionProvider.describeContext({
      ...context,
      files: context.files.map((file) => (file.content === null ? file : { ...file, content: null })),
    });
  }
  const format = outputFormat("summary");
  if (format === "json") {
    printJson({
      query,
      description,
      files: context.files.map(({ path: filePath, similarity, description: fileDescription }) => ({
        path: filePath,
        similarity,
        description: fileDescription,
      })),
      functions: context.functions.map(({ source: _source, ...rest }) => rest),
    });
    return;
  }
  process.stdout.write(`${description}\n`);
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
    ? loadConfig(targetRootDir, parsed.values["target-config"], { ...parsed.values, index: targetPath })
    : undefined;
  const targetDescriptionProvider = targetConfig ? await createDescriptionProvider(targetConfig) : undefined;
  const targetOptions: CodeIndexOptions | undefined = targetConfig ? {
    ...indexConfigOptions(targetConfig),
    provider,
    ...(sourceOptions.parallelism !== undefined ? { parallelism: sourceOptions.parallelism } : {}),
    ...(sourceOptions.onProgress ? { onProgress: sourceOptions.onProgress } : {}),
    ...(sourceOptions.onWarning ? { onWarning: sourceOptions.onWarning } : {}),
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
  const { CodeIndex: CodeIndexClass } = await import("./code-index.js");
  const { crossSearch } = await import("./search/cross-search.js");
  const target = targetOptions ? new CodeIndexClass({ ...targetOptions, readOnly: true }) : undefined;
  const format = outputFormat(parsed.values.cohesion ? "summary" : "clusters");
  const threshold = similarityThreshold();
  const indexProgress = sourceOptions.onProgress;
  const outputLimit = parsed.values.limit === undefined
    ? undefined
    : positiveIntegerOption(parsed.values.limit, 1, "limit");
  const searchOptions: CrossSearchOptions = {
    source,
    ...(target ? { target } : {}),
    sourceFilter: crossSearchSourceFilter(),
    limitPerFunction: positiveIntegerOption(parsed.values.matches, 5, "matches"),
    minSimilarity: threshold.min,
    ...(threshold.max !== undefined ? { maxSimilarity: threshold.max } : {}),
    includeSymmetricDuplicates: parsed.values["include-symmetric-duplicates"],
    crossFileOnly: parsed.values["cross-file-only"],
    cohesion: parsed.values.cohesion,
    minLines: minimumLines(),
    // Per-source read progress (including read-repair scans) on the terminal bar.
    ...(indexProgress ? {
      onProgress: (value: { completed: number; total: number }) =>
        indexProgress({ phase: "cross-search", ...value }),
    } : {}),
  };
  try {
    if (format === "clusters") {
      const results: CrossSearchResult[] = [];
      for await (const result of crossSearch(searchOptions)) results.push(result);
      process.stdout.write(`${formatSimilarityClusters(results, !target, outputLimit)}\n`);
      return;
    }
    let emitted = 0;
    for await (const result of crossSearch(searchOptions)) {
      if (outputLimit !== undefined && emitted >= outputLimit) break;
      if (format === "summary") {
        process.stdout.write(`${formatSimilaritySummary(result.matches, result.source)}\n`);
      } else {
        process.stdout.write(`${JSON.stringify({
          source: presentFunction(result.source),
          matches: result.matches.map(presentMatch),
          scoring: result.scoring,
        })}\n`);
      }
      emitted += 1;
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
  const { CodeIndex } = await import("./code-index.js");
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
      const { resetIndexState } = await import("./storage/database.js");
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
  const { CodeIndex } = await import("./code-index.js");
  const rebuiltDescriptionProvider = descriptionProfile && !options.descriptionProvider
    ? await createDescriptionProvider(resolveConfig(options.rootDir, {
      indexPath,
      descriptionProvider: descriptionProfile.provider,
      descriptionModel: descriptionProfile.model,
      parallelism: options.parallelism,
      ...(options.verbose ? { verbose: true } : {}),
    }))
    : undefined;
  const index = new CodeIndex({
    ...options, indexPath,
    ...(rebuiltDescriptionProvider ? { descriptionProvider: rebuiltDescriptionProvider } : {}),
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

function storedDescriptionProfile(indexPath: string, rootDir: string): DescriptionProfile | null {
  if (!existsSync(indexPath)) return null;
  const db = new DatabaseSync(indexPath, { readOnly: true });
  try {
    const metadata = new Map((db.prepare("SELECT key, value FROM metadata").all() as Array<{ key: string; value: string }>)
      .map((row) => [row.key, row.value]));
    if (metadata.get("root_dir") !== path.resolve(rootDir)) return null;
    const value = metadata.get("description_profile");
    if (!value) return null;
    const profile = JSON.parse(value) as Partial<DescriptionProfile>;
    if (typeof profile.model !== "string" || !profile.model || !isDescriptionProviderName(profile.provider ?? "")) return null;
    return {
      provider: profile.provider!,
      model: profile.model,
      strategyVersion: typeof profile.strategyVersion === "string" ? profile.strategyVersion : "callable-purpose-v2",
    };
  } catch {
    return null;
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

async function createDescriptionProvider(config: EffectiveConfig): Promise<OpenAIDescriptionProvider | undefined> {
  if (command !== "describe" && !config.descriptionProvider && !config.descriptionModel && !config.descriptionFallbackModel) return undefined;
  let provider = config.descriptionProvider;
  let model = config.descriptionModel;
  if (command === "describe" && !provider && !model) {
    const stored = storedDescriptionProfile(config.indexPath, config.rootDir);
    if (stored) {
      provider = stored.provider as DescriptionProviderName;
      model = stored.model;
    }
  }
  const { OpenAIDescriptionProvider: Provider } = await import("./descriptions/openai.js");
  return new Provider({
    ...(provider ? { provider } : {}),
    ...(model ? { model } : {}),
    ...(config.descriptionFallbackModel ? { fallbackModel: config.descriptionFallbackModel } : {}),
    parallelism: config.parallelism,
    ...(config.verbose ? { verbose: true } : {}),
  });
}

function descriptionRefresh(config: EffectiveConfig): DescriptionRefreshHooks | undefined {
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
    ...(afterRefresh ? { afterRefresh: async (index: CodeIndex) => { await index.useDescriptions(); } } : {}),
  };
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
  const configPath = configFilePath(rootDir, parsed.values.config);
  let config = readConfigFile(configPath);
  const action = positionals[0];
  let result: Record<string, unknown>;
  if (!action) {
    if (!process.stdin.isTTY || !process.stdout.isTTY) {
      throw new CodeIndexError("config without an action requires an interactive terminal.");
    }
    const { configureInteractively } = await import("./config-wizard.js");
    config = { ...await configureInteractively(validateConfig(config), fetchPublishedModels) };
    result = { ...config };
  } else if (action === "descriptions") {
    config.descriptionsEnabled = positionals[1] === "enable";
    result = { descriptionsEnabled: config.descriptionsEnabled };
  } else if (action === "model") {
    const selected = await resolveConfiguredModel(positionals[1], parsed.values["description-model"], "config model");
    config.descriptionProvider = selected.provider;
    config.descriptionModel = selected.model;
    result = { descriptionProvider: selected.provider, descriptionModel: selected.model };
  } else if (action === "fallback-model") {
    const selected = await resolveConfiguredModel(
      positionals[1], parsed.values["description-fallback-model"], "config fallback-model",
      isDescriptionProviderName(config.descriptionProvider) ? config.descriptionProvider : undefined,
    );
    if (config.descriptionProvider && config.descriptionProvider !== selected.provider) {
      throw new CodeIndexError("Fallback model provider must match the configured description provider.");
    }
    config.descriptionProvider = selected.provider;
    config.descriptionFallbackModel = selected.model;
    result = { descriptionProvider: selected.provider, descriptionFallbackModel: selected.model };
  } else if (action === "parallelism") {
    const parallelism = Number(positionals[1]);
    config.parallelism = parallelism;
    result = { parallelism };
  } else {
    const provider = positionals[1] as "cohere" | "jina" | "openai" | "disable";
    if (provider === "disable") {
      config.rerankingEnabled = false;
      result = { rerankingEnabled: false };
    } else {
      const sameProvider = config.rerankerProvider === provider;
      const defaultModel = RERANKER_DEFAULT_MODELS[provider];
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

async function resolveConfiguredModel(
  positionalModel: string | undefined,
  optionModel: string | undefined,
  setting: "config model" | "config fallback-model",
  defaultProvider?: DescriptionProviderName,
): Promise<PublishedModel> {
  if (positionalModel && optionModel && positionalModel !== optionModel) {
    const option = setting === "config model" ? "--description-model" : "--description-fallback-model";
    throw new CodeIndexError(`${setting} argument and ${option} must match when both are supplied.`);
  }
  let reference = positionalModel ?? optionModel;
  if (!reference) throw new CodeIndexError(`${setting} requires a model ID or provider/model reference.`);
  let provider = parsed.values["description-provider"] ?? defaultProvider;
  const separator = reference.indexOf("/");
  if (separator !== -1) {
    const qualifiedProvider = reference.slice(0, separator);
    reference = reference.slice(separator + 1);
    if (parsed.values["description-provider"] && parsed.values["description-provider"] !== qualifiedProvider) {
      throw new CodeIndexError("Model reference and --description-provider must select the same provider.");
    }
    provider = qualifiedProvider;
  }
  if (!reference) throw new CodeIndexError(`${setting} requires a non-empty model ID.`);
  if (provider !== undefined) {
    if (!isOpenCodeDescriptionProvider(provider)) {
      throw new CodeIndexError(`${setting} provider must be opencode or opencode-go.`);
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
  const { descriptionProviderBaseUrl } = await import("./descriptions/openai.js");
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

async function createProvider(config: EffectiveConfig): Promise<EmbeddingProvider> {
  if (config.provider === "jina") {
    const { JinaEmbeddingProvider } = await import("./embeddings/jina.js");
    return new JinaEmbeddingProvider({
      ...(config.model ? { model: config.model } : {}),
      ...(config.dimensions ? { dimensions: config.dimensions } : {}),
      parallelism: config.parallelism,
      ...(config.verbose ? { verbose: true } : {}),
    });
  }
  const { OpenAIEmbeddingProvider } = await import("./embeddings/openai.js");
  return new OpenAIEmbeddingProvider({
    ...(config.model ? { model: config.model } : {}),
    ...(config.dimensions ? { dimensions: config.dimensions } : {}),
    parallelism: config.parallelism,
    ...(config.verbose ? { verbose: true } : {}),
  });
}

async function createReranker(config: EffectiveConfig): Promise<Reranker | undefined> {
  if (config.rerankingEnabled !== true) return undefined;
  if (config.rerankerProvider === "cohere" || config.rerankerProvider === "jina") {
    const { CohereReranker, JinaReranker } = await import("./rerankers/hosted.js");
    if (config.rerankerProvider === "cohere") {
      return new CohereReranker({
        ...(config.rerankerModel ? { model: config.rerankerModel } : {}),
        ...(config.verbose ? { verbose: true } : {}),
      });
    }
    return new JinaReranker({
      ...(config.rerankerModel ? { model: config.rerankerModel } : {}),
      ...(config.verbose ? { verbose: true } : {}),
    });
  }
  if (config.rerankerProvider === "openai") {
    const { OpenAILLMReranker } = await import("./rerankers/openai.js");
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
  if ((parsed.values["rebuild-on-divergence"] || parsed.values["force-reindex"])
    && !parsed.values["yes-really-rebuild-the-index"]) {
    const rebuildFlags = [
      parsed.values["rebuild-on-divergence"] ? "--rebuild-on-divergence" : undefined,
      parsed.values["force-reindex"] ? "--force-reindex" : undefined,
    ].filter((value): value is string => value !== undefined).join(" and ");
    throw new CodeIndexError(
      `Using ${rebuildFlags} requires --yes-really-rebuild-the-index.`,
    );
  }
  if (parsed.values["reranker-candidates"] !== undefined
    && (command !== "config" || positionals[0] !== "reranker" || positionals[1] !== "openai")) {
    throw new CodeIndexError("--reranker-candidates is only available with config reranker openai.");
  }
  if (parsed.values["describe-full-file-threshold"] !== undefined && command !== "describe") {
    throw new CodeIndexError("--describe-full-file-threshold is only available for describe.");
  }
  if (parsed.values.cohesion && command !== "cross-search") {
    throw new CodeIndexError("--cohesion is only available for cross-search.");
  }
  if (parsed.values.matches !== undefined && command !== "cross-search") {
    throw new CodeIndexError("--matches is only available for cross-search.");
  }
  if ((parsed.values.code || parsed.values.descriptions || parsed.values.md) && command !== "search") {
    throw new CodeIndexError("--code, --descriptions, and --md are only available for search.");
  }
  const nameRegex = qualifiedNameRegex();
  if (nameRegex !== undefined) {
    if (!["search", "search-code", "search-description", "search-descriptions", "describe", "cross-search"].includes(command!)) {
      throw new CodeIndexError("-e/--regexp/--regex is only available for function searches, describe, and cross-search.");
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
      if (positionals.length === 0) return;
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
      if (positionals[0] === "fallback-model") {
        if (positionals.length > 2 || (!positionals[1] && !parsed.values["description-fallback-model"])) {
          throw new CodeIndexError("config fallback-model requires a model ID or provider/model reference.");
        }
        return;
      }
      if (positionals[0] === "parallelism") {
        const value = Number(positionals[1]);
        if (positionals.length !== 2 || !Number.isInteger(value) || value < 1) {
          throw new CodeIndexError("config parallelism requires a positive integer.");
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
      throw new CodeIndexError("config accepts no arguments for interactive setup, or requires descriptions, model, fallback-model, parallelism, or reranker.");
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
    case "search-code":
    case "search-description":
    case "search-descriptions": {
      if (!positionals.join(" ").trim()) throw new CodeIndexError(`${command} requires a query.`);
      validateOutputLimit();
      similarityThreshold();
      if (outputFormat("summary") === "clusters") throw new CodeIndexError("clusters format is only available for cross-search.");
      return;
    }
    case "search-md":
      if (!positionals.join(" ").trim()) throw new CodeIndexError("search-md requires a query.");
      validateOutputLimit();
      similarityThreshold();
      if (outputFormat("summary") === "clusters") throw new CodeIndexError("clusters format is only available for cross-search.");
      return;
    case "describe":
      if (!positionals.join(" ").trim()) throw new CodeIndexError("describe requires a query.");
      validateOutputLimit();
      similarityThreshold();
      describeFullFileThreshold();
      if (outputFormat("summary") === "clusters") throw new CodeIndexError("clusters format is only available for cross-search.");
      return;
    case "cross-search":
      if (Boolean(parsed.values["target-root"]) !== Boolean(parsed.values["target-index"])) {
        throw new CodeIndexError("Cross-index search requires both --target-root and --target-index.");
      }
      if (parsed.values["target-config"] && !parsed.values["target-root"]) {
        throw new CodeIndexError("--target-config requires --target-root and --target-index.");
      }
      validateMatches(5);
      validateOutputLimit();
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

function validateMatches(defaultValue: number): void {
  positiveIntegerOption(parsed.values.matches, defaultValue, "matches");
}

function validateOutputLimit(): void {
  if (parsed.values.limit !== undefined) positiveIntegerOption(parsed.values.limit, 1, "limit");
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

function similarityThreshold(defaultMin = 0.3): { min: number; max?: number } {
  const threshold = parsed.values.threshold;
  if (threshold === undefined) return { min: defaultMin };

  const number = "[+-]?(?:\\d+(?:\\.\\d*)?|\\.\\d+)(?:e[+-]?\\d+)?";
  const range = new RegExp(`^\\s*(${number})\\s*-\\s*(${number})\\s*$`, "i").exec(threshold);
  if (!range) return { min: numberOption(threshold, defaultMin, "threshold") };
  const min = Number(range[1]);
  const max = Number(range[2]);
  if (!Number.isFinite(min) || !Number.isFinite(max)) throw new CodeIndexError("threshold range bounds must be numbers.");
  if (min >= max) throw new CodeIndexError("threshold range minimum must be less than its maximum.");
  return { min, max };
}

function describeFullFileThreshold(): number {
  return numberOption(parsed.values["describe-full-file-threshold"], 0.8, "describe-full-file-threshold");
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

function presentSearchResult(value: SearchResult) {
  if (value.type === "markdown") return value;
  const { type, ...result } = value;
  return { type, ...presentMatch(result) };
}

function formatSearchResult(value: SearchResult): string {
  return value.type === "function" ? formatSimilaritySummary([value]) : formatMarkdownResult(value);
}

function formatMarkdownResult(result: MarkdownSearchResult): string {
  const score = result.rerankScore === undefined
    ? result.similarity.toFixed(4)
    : `${result.rerankScore.toFixed(4)} rerank (${result.similarity.toFixed(4)} similarity)`;
  const heading = result.chunk.headingPath.join(" > ");
  return `${score}  ${result.chunk.path}:${result.chunk.startLine}${heading ? ` :: ${heading}` : ""}\n${result.chunk.content}`;
}

function selectedSearchIndexes(): SearchIndex[] | undefined {
  const indexes: SearchIndex[] = [];
  if (parsed.values.code) indexes.push("code");
  if (parsed.values.descriptions) indexes.push("descriptions");
  if (parsed.values.md) indexes.push("markdown");
  return indexes.length > 0 ? indexes : undefined;
}

function printJson(value: unknown): void {
  process.stdout.write(`${JSON.stringify(value, null, 2)}\n`);
}

function printHelp(): void {
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
