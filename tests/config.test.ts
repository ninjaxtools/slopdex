import { existsSync, readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { configFilePath, indexConfigOptions, loadConfig, readConfigFile, resolveConfig, writeConfigFile } from "../src/config.js";
import { CodeIndexError } from "../src/errors.js";
import { DEFAULT_PARALLELISM } from "../src/utils.js";
import { temporaryRoot, write } from "./helpers.js";

describe("effective configuration", () => {
  it("loads defaults without creating a config file or index", () => {
    const root = temporaryRoot();
    expect(loadConfig(root)).toEqual({
      rootDir: root, indexPath: path.join(root, ".slopdex/index.sqlite"),
      provider: "openai", parallelism: DEFAULT_PARALLELISM, verbose: false,
    });
    expect(existsSync(path.join(root, ".slopdex"))).toBe(false);
    expect(loadConfig(root, path.join(root, "missing.json")).provider).toBe("openai");
  });

  it("merges explicit overrides before validation and preserves saved verbose and policy settings", () => {
    const root = temporaryRoot();
    const configPath = path.join(root, "custom.json");
    const saved = {
      provider: "obsolete", model: 3, dimensions: "invalid", indexPath: null,
      descriptionProvider: "obsolete", descriptionModel: 3, descriptionFallbackModel: false,
      verbose: true, parallelism: 4, include: ["src/**"], exclude: [],
      maxFileSize: 200, embeddingBatchSize: 2,
    };
    write(root, "custom.json", JSON.stringify(saved));
    const config = loadConfig(root, configPath, {
      provider: "jina", model: "custom-embedding", dimensions: "128", index: "relative.sqlite",
      "description-provider": "opencode-go", "description-model": "primary",
      "description-fallback-model": "fallback", verbose: false,
    });
    expect(config).toMatchObject({
      provider: "jina", model: "custom-embedding", dimensions: 128,
      descriptionProvider: "opencode-go", descriptionModel: "primary", descriptionFallbackModel: "fallback",
      verbose: true,
    });
    expect(indexConfigOptions(config)).toEqual({
      rootDir: root, indexPath: path.resolve("relative.sqlite"),
      verbose: true, parallelism: 4, include: ["src/**"], exclude: [], maxFileSize: 200, embeddingBatchSize: 2,
    });
    expect(readConfigFile(configPath)).toEqual(saved);
    expect(resolveConfig(root, { verbose: false }, { verbose: true }).verbose).toBe(true);
  });

  it("keeps configured relative paths relative to the working directory", () => {
    const root = temporaryRoot();
    expect(configFilePath(root, "settings.json")).toBe(path.resolve("settings.json"));
    expect(resolveConfig(root, { indexPath: "saved.sqlite" }).indexPath).toBe(path.resolve("saved.sqlite"));
  });

  it.each([null, [], "config", 42, true])("rejects non-object JSON: %j", (value) => {
    const root = temporaryRoot();
    write(root, ".slopdex/config.json", JSON.stringify(value));
    expect(() => loadConfig(root)).toThrow("Config must contain a JSON object");
    expect(() => resolveConfig(root, value)).toThrow(CodeIndexError);
  });

  it("reports malformed JSON with its file path", () => {
    const root = temporaryRoot();
    write(root, ".slopdex/config.json", "{broken");
    expect(() => loadConfig(root)).toThrow(new CodeIndexError(`Could not parse config JSON: ${configFilePath(root)}`));
  });

  it.each([
    ["provider", "other"], ["descriptionProvider", "other"],
    ["model", 3], ["model", " "], ["indexPath", false], ["indexPath", ""],
    ["descriptionModel", []], ["descriptionFallbackModel", " "],
    ["dimensions", 0], ["dimensions", 1.5], ["dimensions", "2"], ["dimensions", Infinity],
    ["maxFileSize", 0], ["maxFileSize", -1], ["embeddingBatchSize", "32"],
    ["embeddingBatchSize", NaN], ["parallelism", 0], ["parallelism", 1.5],
    ["include", "src/**"], ["include", [1]], ["exclude", null], ["exclude", [false]],
    ["descriptionsEnabled", "false"], ["rerankingEnabled", 1], ["verbose", null],
  ])("rejects invalid %s = %j at the boundary", (key, value) => {
    expect(() => resolveConfig(".", { [key]: value })).toThrow(CodeIndexError);
  });

  it.each(["0", "", "-1", "1.5", "NaN", "Infinity"])("rejects invalid CLI dimensions %j even over valid saved dimensions", (dimensions) => {
    expect(() => resolveConfig(".", { dimensions: 128 }, { dimensions })).toThrow("dimensions must be a positive integer");
  });

  it("validates active rerankers and keeps valid dormant preferences", () => {
    expect(() => resolveConfig(".", { rerankingEnabled: true })).toThrow("Unsupported reranker provider");
    expect(() => resolveConfig(".", { rerankingEnabled: true, rerankerProvider: "jina", rerankerModel: 3 })).toThrow("rerankerModel");
    for (const rerankerCandidates of [0, 1.5, "10", 101]) {
      expect(() => resolveConfig(".", { rerankingEnabled: true, rerankerProvider: "openai", rerankerCandidates })).toThrow("rerankerCandidates");
    }
    const preferences = { rerankingEnabled: false, rerankerProvider: "openai", rerankerModel: "custom", rerankerCandidates: 20 };
    expect(resolveConfig(".", preferences)).toMatchObject(preferences);
    expect(resolveConfig(".", { rerankingEnabled: true, rerankerProvider: "jina", rerankerCandidates: "unused" })).not.toHaveProperty("rerankerCandidates");
  });

  it("allows repairing malformed rerankers and round-trips unknown fields without writing defaults", () => {
    const root = temporaryRoot();
    const configPath = configFilePath(root);
    write(root, ".slopdex/config.json", JSON.stringify({
      rerankingEnabled: true, rerankerProvider: "cohere", rerankerModel: 3, extension: { future: true },
    }));
    const raw = readConfigFile(configPath);
    raw.rerankingEnabled = false;
    writeConfigFile(configPath, raw);
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toEqual(raw);
    expect(loadConfig(root)).not.toHaveProperty("rerankerModel");
    expect(readConfigFile(configPath)).not.toHaveProperty("parallelism");
    raw.parallelism = 0;
    expect(() => writeConfigFile(configPath, raw)).toThrow("parallelism");
    expect(readConfigFile(configPath)).not.toHaveProperty("parallelism");
    expect(existsSync(`${configPath}.tmp-${process.pid}`)).toBe(false);
    expect(existsSync(path.join(root, ".slopdex/index.sqlite"))).toBe(false);
  });
});
