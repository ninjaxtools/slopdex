import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import path from "node:path";

import type { DescriptionProviderName } from "./descriptions/openai.js";
import type { CodeIndexOptions } from "./types.js";
import { CodeIndexError } from "./errors.js";
import { assertPositiveInteger, DEFAULT_PARALLELISM } from "./utils.js";

export interface FileConfig {
  provider?: "openai" | "jina";
  model?: string;
  dimensions?: number;
  indexPath?: string;
  include?: string[];
  exclude?: string[];
  maxFileSize?: number;
  embeddingBatchSize?: number;
  parallelism?: number;
  descriptionProvider?: DescriptionProviderName;
  descriptionModel?: string;
  descriptionFallbackModel?: string;
  descriptionsEnabled?: boolean;
  rerankerProvider?: "cohere" | "jina" | "openai";
  rerankerModel?: string;
  rerankerCandidates?: number;
  rerankingEnabled?: boolean;
  verbose?: boolean;
}

export type OpenCodeDescriptionProvider = Exclude<DescriptionProviderName, "openai">;

export interface PublishedModel {
  provider: OpenCodeDescriptionProvider;
  model: string;
}

export const RERANKER_DEFAULT_MODELS = {
  cohere: "rerank-v4.0-pro", jina: "jina-reranker-v3.5", openai: "gpt-5.6-luna",
} as const;

// Keep configuration and CLI startup independent of the provider SDKs.
export function isDescriptionProviderName(value: unknown): value is DescriptionProviderName {
  return value === "openai" || value === "opencode" || value === "opencode-go";
}

export interface ConfigOverrides {
  provider?: string | undefined;
  model?: string | undefined;
  dimensions?: string | undefined;
  index?: string | undefined;
  "description-provider"?: string | undefined;
  "description-model"?: string | undefined;
  "description-fallback-model"?: string | undefined;
  verbose?: boolean | undefined;
}

export interface EffectiveConfig extends FileConfig {
  rootDir: string;
  indexPath: string;
  provider: "openai" | "jina";
  parallelism: number;
  verbose: boolean;
}

export function configFilePath(rootDir: string, configuredPath?: string): string {
  return path.resolve(configuredPath ?? path.join(rootDir, ".slopdex", "config.json"));
}

function configObject(value: unknown, source = "configuration"): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new CodeIndexError(`Config must contain a JSON object: ${source}`);
  }
  return value as Record<string, unknown>;
}

// Editing uses the raw object so a config command can repair an invalid setting
// and preserve unknown fields. Runtime consumers must use resolveConfig/loadConfig.
export function readConfigFile(configPath: string): Record<string, unknown> {
  if (!existsSync(configPath)) return {};
  const contents = readFileSync(configPath, "utf8");
  let value: unknown;
  try {
    value = JSON.parse(contents);
  } catch (error) {
    throw new CodeIndexError(`Could not parse config JSON: ${configPath}`, { cause: error });
  }
  return configObject(value, configPath);
}

export function validateConfig(value: unknown): FileConfig {
  const config = { ...configObject(value) };
  if (config.provider !== undefined && config.provider !== "openai" && config.provider !== "jina") {
    throw new CodeIndexError(`Unsupported provider: ${String(config.provider)}`);
  }
  if (config.descriptionProvider !== undefined && !isDescriptionProviderName(config.descriptionProvider)) {
    throw new CodeIndexError(`Unsupported description provider: ${String(config.descriptionProvider)}`);
  }
  for (const key of ["model", "indexPath", "descriptionModel", "descriptionFallbackModel"] as const) {
    optionalString(config, key);
  }
  for (const key of ["dimensions", "maxFileSize", "embeddingBatchSize", "parallelism"] as const) {
    optionalPositiveInteger(config, key);
  }
  for (const key of ["descriptionsEnabled", "rerankingEnabled", "verbose"] as const) {
    if (config[key] !== undefined && typeof config[key] !== "boolean") {
      throw new CodeIndexError(`${key} must be a boolean.`);
    }
  }
  for (const key of ["include", "exclude"] as const) {
    const patterns = config[key];
    if (patterns !== undefined && (!Array.isArray(patterns) || patterns.some((entry) => typeof entry !== "string"))) {
      throw new CodeIndexError(`${key} must be an array of strings.`);
    }
  }
  if (config.rerankingEnabled === true) {
    if (config.rerankerProvider !== "cohere" && config.rerankerProvider !== "jina" && config.rerankerProvider !== "openai") {
      throw new CodeIndexError(`Unsupported reranker provider: ${String(config.rerankerProvider)}`);
    }
    optionalString(config, "rerankerModel");
    if (config.rerankerProvider === "openai") {
      optionalPositiveInteger(config, "rerankerCandidates");
      if (typeof config.rerankerCandidates === "number" && config.rerankerCandidates > 100) {
        throw new CodeIndexError("rerankerCandidates must not exceed 100.");
      }
    } else {
      delete config.rerankerCandidates;
    }
  } else {
    // Preserve valid dormant preferences for the wizard, but do not expose
    // malformed legacy settings to runtime consumers. The raw file is retained.
    if (config.rerankerProvider !== "cohere" && config.rerankerProvider !== "jina" && config.rerankerProvider !== "openai") {
      delete config.rerankerProvider;
    }
    if (typeof config.rerankerModel !== "string" || !config.rerankerModel.trim()) delete config.rerankerModel;
    if (typeof config.rerankerCandidates !== "number" || !Number.isInteger(config.rerankerCandidates)
      || config.rerankerCandidates < 1 || config.rerankerCandidates > 100) delete config.rerankerCandidates;
  }
  return config as FileConfig;
}

export function resolveConfig(rootDir: string, value: unknown, overrides: ConfigOverrides = {}): EffectiveConfig {
  const merged = { ...configObject(value) };
  for (const [option, key] of [
    ["provider", "provider"], ["model", "model"], ["index", "indexPath"],
    ["description-provider", "descriptionProvider"], ["description-model", "descriptionModel"],
    ["description-fallback-model", "descriptionFallbackModel"],
  ] as const) {
    if (overrides[option] !== undefined) merged[key] = overrides[option];
  }
  if (overrides.dimensions !== undefined) merged.dimensions = Number(overrides.dimensions);
  // parseArgs supplies false when --verbose is absent; it must not erase a saved true.
  if (overrides.verbose) merged.verbose = true;
  const config = validateConfig(merged);
  const resolvedRoot = path.resolve(rootDir);
  return {
    ...config,
    rootDir: resolvedRoot,
    indexPath: path.resolve(config.indexPath ?? path.join(resolvedRoot, ".slopdex", "index.sqlite")),
    provider: config.provider ?? "openai",
    parallelism: config.parallelism ?? DEFAULT_PARALLELISM,
    verbose: config.verbose ?? false,
  };
}

export function loadConfig(rootDir: string, configuredPath?: string, overrides: ConfigOverrides = {}): EffectiveConfig {
  return resolveConfig(rootDir, readConfigFile(configFilePath(rootDir, configuredPath)), overrides);
}

export function writeConfigFile(configPath: string, config: unknown): void {
  validateConfig(config);
  mkdirSync(path.dirname(configPath), { recursive: true });
  const temporaryPath = `${configPath}.tmp-${process.pid}`;
  writeFileSync(temporaryPath, `${JSON.stringify(config, null, 2)}\n`);
  renameSync(temporaryPath, configPath);
}

export function indexConfigOptions(config: EffectiveConfig): Pick<CodeIndexOptions,
  "rootDir" | "indexPath" | "parallelism" | "verbose" | "include" | "exclude" | "maxFileSize" | "embeddingBatchSize"
> {
  return {
    rootDir: config.rootDir,
    indexPath: config.indexPath,
    parallelism: config.parallelism,
    verbose: config.verbose,
    ...(config.include !== undefined ? { include: config.include } : {}),
    ...(config.exclude !== undefined ? { exclude: config.exclude } : {}),
    ...(config.maxFileSize !== undefined ? { maxFileSize: config.maxFileSize } : {}),
    ...(config.embeddingBatchSize !== undefined ? { embeddingBatchSize: config.embeddingBatchSize } : {}),
  };
}

function optionalString(config: Record<string, unknown>, key: string): void {
  const value = config[key];
  if (value !== undefined && (typeof value !== "string" || !value.trim())) {
    throw new CodeIndexError(`${key} must be a non-empty string.`);
  }
}

function optionalPositiveInteger(config: Record<string, unknown>, key: string): void {
  const value = config[key];
  if (value === undefined) return;
  if (typeof value !== "number") throw new CodeIndexError(`${key} must be a positive integer.`);
  assertPositiveInteger(value, key);
}
