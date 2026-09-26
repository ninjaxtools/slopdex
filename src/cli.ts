#!/usr/bin/env node

import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { parseArgs } from "node:util";

import { CodeIndexError } from "./errors.js";
import { formatSimilarityClusters, formatSimilaritySummary } from "./format.js";
import { clearProgress, TerminalProgress } from "./progress.js";
import { compileNameRegex } from "./utils.js";
import {
  configFilePath, indexConfigOptions, isDescriptionProviderName, loadConfig,
  readConfigFile, RERANKER_DEFAULT_MODELS, validateConfig, writeConfigFile,
  type OpenCodeDescriptionProvider, type PublishedModel,
} from "./config.js";
import { printHelp } from "./cli/help.js";
import { ensureIndexUpdated, resolveIndexPath, storedDescriptionProfile } from "./cli/index-lifecycle.js";
import {
  formatMarkdownResult,
  formatSearchResult,
  presentFunction,
  presentMatch,
  presentSearchResult,
  printJson,
} from "./cli/presentation.js";
import {
  createDescriptionProvider,
  createProvider,
  createReranker,
  descriptionRefresh,
} from "./cli/provider-factories.js";
import type { CodeIndex } from "./code-index.js";
import type { DescriptionProviderName, OpenAIDescriptionProvider } from "./descriptions/openai.js";
import type {
  CodeIndexOptions,
  CrossSearchOptions,
  CrossSearchResult,
  CrossSearchSourceFilter,
  EmbeddingProvider,
  SearchIndex,
} from "./types.js";

declare const __SLOPDEX_VERSION__: string;

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
  const descriptionProvider = await createDescriptionProvider(config, command === "describe" ? {
    required: true,
    storedProfile: storedDescriptionProfile(config.indexPath, config.rootDir),
  } : undefined);
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
  diagnosticIndexes.add(resolveIndexPath(indexOptions));
  const updateStats = await ensureIndexUpdated(
    indexOptions,
    "index",
    updateTarget,
    parsed.values["rebuild-on-divergence"],
    parsed.values["force-reindex"],
    parsed.values["no-reindex"],
    descriptionRefresh(config, command === "descriptions" ? descriptionsAction : null),
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
    diagnosticIndexes.add(resolveIndexPath(targetOptions));
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

function selectedSearchIndexes(): SearchIndex[] | undefined {
  const indexes: SearchIndex[] = [];
  if (parsed.values.code) indexes.push("code");
  if (parsed.values.descriptions) indexes.push("descriptions");
  if (parsed.values.md) indexes.push("markdown");
  return indexes.length > 0 ? indexes : undefined;
}
