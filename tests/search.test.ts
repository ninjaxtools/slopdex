import { linkSync, symlinkSync } from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";

import { describe, expect, it } from "vitest";

import { CodeIndex } from "../src/code-index.js";
import { crossSearch } from "../src/search/cross-search.js";
import { FakeEmbeddingProvider, temporaryRoot, write } from "./helpers.js";
import type { EmbeddingProvider } from "../src/types.js";

describe("similarity search", () => {
  it("returns multiple results in descending similarity order", async () => {
    const root = temporaryRoot();
    write(root, "functions.ts", `
export function authenticateSession(session: string) { return session === "valid"; }
export function validateSession(session: string) { return Boolean(session); }
export function addNumbers(a: number, b: number) { return a + b; }
`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["functions.ts"] });

    const results = await index.similaritySearch({ query: "authenticate session", limit: 3 });
    expect(results).toHaveLength(3);
    expect(results[0]!.similarity).toBeGreaterThanOrEqual(results[1]!.similarity);
    expect(results[1]!.similarity).toBeGreaterThanOrEqual(results[2]!.similarity);
    index.close();
  });
});

describe("cross search", () => {
  it("backfills callable line counts when migrating a version-1 index", async () => {
    const root = temporaryRoot();
    write(root, "function.ts", `export function migrated() {
  return 1;
}\n`);
    const provider = new FakeEmbeddingProvider();
    const index = new CodeIndex({ rootDir: root, provider });
    await index.updateFiles({ upsert: ["function.ts"] });
    const indexPath = index.indexPath;
    index.close();
    const database = new DatabaseSync(indexPath);
    database.exec("UPDATE functions SET line_count = 1; UPDATE metadata SET value = '1' WHERE key = 'schema_version';");
    database.close();

    const migrated = new CodeIndex({ rootDir: root, provider });
    expect(migrated.allFunctions()[0]!.lineCount).toBe(3);
    migrated.close();
    const verified = new DatabaseSync(indexPath, { readOnly: true });
    expect(verified.prepare("SELECT value FROM metadata WHERE key = 'schema_version'").get()).toEqual({ value: "2" });
    verified.close();
  });

  it("can include symmetric matches for every source function while excluding itself", async () => {
    const root = temporaryRoot();
    write(root, "functions.ts", `
export function one(value: string) { return value.trim(); }
export function two(value: string) { return value.trim(); }
export function three(value: number) { return value * 2; }
`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["functions.ts"] });

    const results = [];
    for await (const result of crossSearch({
      source: index,
      limitPerFunction: 2,
      includeSymmetricDuplicates: true,
      minLines: 1,
    })) results.push(result);
    expect(results).toHaveLength(3);
    expect(results.every((result) => result.matches.every((match) => match.function.id !== result.source.id))).toBe(true);
    expect(results.every((result) => result.matches[0]!.similarity >= result.matches[1]!.similarity)).toBe(true);
    index.close();
  });

  it("lists each same-index function pair only once by default", async () => {
    const root = temporaryRoot();
    write(root, "functions.ts", `
export function one(value: string) { return value.trim(); }
export function two(value: string) { return value.trim(); }
`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["functions.ts"] });

    const results = [];
    for await (const result of crossSearch({ source: index, minLines: 1 })) results.push(result);

    expect(results).toHaveLength(1);
    expect(results[0]!.source.name).toBe("one");
    expect(results[0]!.matches.map((match) => match.function.name)).toEqual(["two"]);
    index.close();
  });

  it("defaults to callables spanning at least two lines and filters candidates before limiting", async () => {
    const sourceRoot = temporaryRoot();
    const targetRoot = temporaryRoot();
    write(sourceRoot, "source.ts", `
export function shortSource() { return 1; }
export function longSource() {
  return 2;
}
`);
    write(targetRoot, "target.ts", `
export function shortTarget() { return 1; }
export function longTarget() {
  return 2;
}
`);
    const provider: EmbeddingProvider = {
      profile: { provider: "controlled", model: "test", dimensions: 2, strategyVersion: "callable-v1" },
      embedDocuments: async (inputs) => inputs.map(() => [1, 0]),
      embedQuery: async () => [1, 0],
    };
    const source = new CodeIndex({ rootDir: sourceRoot, provider });
    const target = new CodeIndex({ rootDir: targetRoot, provider });
    await source.updateFiles({ upsert: ["source.ts"] });
    await target.updateFiles({ upsert: ["target.ts"] });

    const results = [];
    for await (const result of crossSearch({ source, target, limitPerFunction: 1 })) results.push(result);

    expect(source.allFunctions().map((callable) => [callable.name, callable.lineCount])).toEqual([
      ["shortSource", 1],
      ["longSource", 3],
    ]);
    expect(results).toHaveLength(1);
    expect(results[0]!.source.name).toBe("longSource");
    expect(results[0]!.matches.map((match) => match.function.name)).toEqual(["longTarget"]);
    source.close();
    target.close();
  });

  it("excludes same-file matches before applying the per-function limit", async () => {
    const root = temporaryRoot();
    write(root, "same.ts", `
export function one(value: string) { return value.trim(); }
export function two(value: string) { return value.trim(); }
`);
    write(root, "other.ts", `export function external(value: string) { return value.trim(); }\n`);
    const provider: EmbeddingProvider = {
      profile: { provider: "controlled", model: "test", dimensions: 2, strategyVersion: "callable-v1" },
      embedDocuments: async (inputs) => inputs.map(() => [1, 0]),
      embedQuery: async () => [1, 0],
    };
    const index = new CodeIndex({ rootDir: root, provider });
    await index.updateFiles({ upsert: ["same.ts", "other.ts"] });

    const results = [];
    for await (const result of crossSearch({
      source: index,
      sourceFilter: { type: "all", path: "same.ts" },
      limitPerFunction: 1,
      includeSymmetricDuplicates: true,
      crossFileOnly: true,
      minLines: 1,
    })) results.push(result);

    expect(results).toHaveLength(2);
    expect(results.every((result) => result.matches.length === 1)).toBe(true);
    expect(results.every((result) => result.matches[0]!.function.path === "other.ts")).toBe(true);
    index.close();
  });

  it("allows matching paths when cross-searching different repository roots", async () => {
    const sourceRoot = temporaryRoot();
    const targetRoot = temporaryRoot();
    write(sourceRoot, "same.ts", `export function source(value: string) { return value.trim(); }\n`);
    write(targetRoot, "same.ts", `export function target(value: string) { return value.trim(); }\n`);
    const provider = new FakeEmbeddingProvider();
    const source = new CodeIndex({ rootDir: sourceRoot, provider });
    const target = new CodeIndex({ rootDir: targetRoot, provider });
    await source.updateFiles({ upsert: ["same.ts"] });
    await target.updateFiles({ upsert: ["same.ts"] });

    const results = [];
    for await (const result of crossSearch({ source, target, crossFileOnly: true, minLines: 1 })) results.push(result);

    expect(results[0]!.matches[0]!.function.path).toBe("same.ts");
    source.close();
    target.close();
  });

  it("excludes the same file exposed through overlapping repository roots", async () => {
    const targetRoot = temporaryRoot();
    const sourceRoot = `${targetRoot}/nested`;
    write(sourceRoot, "same.ts", `export function shared(value: string) { return value.trim(); }\n`);
    const provider = new FakeEmbeddingProvider();
    const source = new CodeIndex({ rootDir: sourceRoot, provider });
    const target = new CodeIndex({ rootDir: targetRoot, provider });
    await source.updateFiles({ upsert: ["same.ts"] });
    await target.updateFiles({ upsert: ["nested/same.ts"] });

    const results = [];
    for await (const result of crossSearch({ source, target, crossFileOnly: true, minLines: 1 })) results.push(result);

    expect(results).toEqual([]);
    source.close();
    target.close();
  });

  it("excludes the same file indexed through a symlinked directory", async () => {
    const sourceRoot = temporaryRoot();
    const targetRoot = path.join(sourceRoot, "nested");
    write(targetRoot, "same.ts", `export function shared(value: string) { return value.trim(); }\n`);
    symlinkSync("nested", path.join(sourceRoot, "linked"), "dir");
    const provider = new FakeEmbeddingProvider();
    const source = new CodeIndex({ rootDir: sourceRoot, provider });
    const target = new CodeIndex({ rootDir: targetRoot, provider });
    await source.updateFiles({ upsert: ["linked/same.ts"] });
    await target.updateFiles({ upsert: ["same.ts"] });

    const results = [];
    for await (const result of crossSearch({ source, target, crossFileOnly: true, minLines: 1 })) results.push(result);

    expect(results).toEqual([]);
    source.close();
    target.close();
  });

  it("excludes the same file indexed through hard links", async () => {
    const sourceRoot = temporaryRoot();
    const targetRoot = temporaryRoot();
    write(sourceRoot, "same.ts", `export function shared(value: string) { return value.trim(); }\n`);
    linkSync(path.join(sourceRoot, "same.ts"), path.join(targetRoot, "alias.ts"));
    const provider = new FakeEmbeddingProvider();
    const source = new CodeIndex({ rootDir: sourceRoot, provider });
    const target = new CodeIndex({ rootDir: targetRoot, provider });
    await source.updateFiles({ upsert: ["same.ts"] });
    await target.updateFiles({ upsert: ["alias.ts"] });

    const results = [];
    for await (const result of crossSearch({ source, target, crossFileOnly: true, minLines: 1 })) results.push(result);

    expect(results).toEqual([]);
    source.close();
    target.close();
  });

  it("restricts source functions to a file or recursive directory without restricting matches", async () => {
    const root = temporaryRoot();
    write(root, "src/selected.ts", `export function selected(value: string) { return value.trim(); }\n`);
    write(root, "src/nested/deep.ts", `export function nested(value: string) { return value.trim(); }\n`);
    write(root, "src-other.ts", `export function prefix(value: string) { return value.trim(); }\n`);
    write(root, "outside.ts", `export function outside(value: string) { return value.trim(); }\n`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["src/selected.ts", "src/nested/deep.ts", "src-other.ts", "outside.ts"] });

    const directoryResults = [];
    for await (const result of crossSearch({
      source: index,
      sourceFilter: { type: "all", path: "src" },
      limitPerFunction: 10,
      includeSymmetricDuplicates: true,
      minLines: 1,
    })) directoryResults.push(result);
    expect(directoryResults.map((result) => result.source.path).sort()).toEqual(["src/nested/deep.ts", "src/selected.ts"]);
    expect(directoryResults.every((result) => result.matches.some((match) => match.function.path === "outside.ts"))).toBe(true);

    const fileResults = [];
    for await (const result of crossSearch({
      source: index,
      sourceFilter: { type: "all", path: "src/selected.ts" },
      includeSymmetricDuplicates: true,
      minLines: 1,
    })) fileResults.push(result);
    expect(fileResults.map((result) => result.source.path)).toEqual(["src/selected.ts"]);
    index.close();
  });

  it("searches from one compatible index into another", async () => {
    const sourceRoot = temporaryRoot();
    const targetRoot = temporaryRoot();
    write(sourceRoot, "source.ts", `export function source(value: string) { return value.trim(); }\n`);
    write(targetRoot, "target.ts", `
export function matching(value: string) { return value.trim(); }
export function unrelated(a: number, b: number) { return a + b; }
`);
    const source = new CodeIndex({ rootDir: sourceRoot, provider: new FakeEmbeddingProvider() });
    const target = new CodeIndex({ rootDir: targetRoot, provider: new FakeEmbeddingProvider() });
    await source.updateFiles({ upsert: ["source.ts"] });
    await target.updateFiles({ upsert: ["target.ts"] });

    const results = [];
    for await (const result of crossSearch({ source, target, limitPerFunction: 2, minLines: 1 })) results.push(result);
    expect(results).toHaveLength(1);
    expect(results[0]!.matches).toHaveLength(2);
    source.close();
    target.close();
  });

  it("omits source functions without matches after filtering", async () => {
    const root = temporaryRoot();
    write(root, "functions.ts", `
export function one() { return 1; }
export function two() { return 2; }
`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["functions.ts"] });
    const progress: number[] = [];

    const results = [];
    for await (const result of crossSearch({
      source: index,
      minSimilarity: 2,
      minLines: 1,
      onProgress: ({ completed }) => progress.push(completed),
    })) results.push(result);

    expect(results).toEqual([]);
    expect(progress).toEqual([1, 2]);
    index.close();
  });

  it("applies an inclusive similarity range before limiting matches", async () => {
    const root = temporaryRoot();
    write(root, "source.ts", `export function sourceMarker() { return 1; }\n`);
    write(root, "close.ts", `export function closeMarker() { return 1; }\n`);
    write(root, "range.ts", `export function rangeMarker() { return 1; }\n`);
    write(root, "far.ts", `export function farMarker() { return 1; }\n`);
    const provider: EmbeddingProvider = {
      profile: { provider: "controlled", model: "test", dimensions: 2, strategyVersion: "callable-v1" },
      embedDocuments: async (inputs) => inputs.map((input) => {
        if (input.includes("rangeMarker")) return [0.9, Math.sqrt(0.19)];
        if (input.includes("farMarker")) return [0.7, Math.sqrt(0.51)];
        return [1, 0];
      }),
      embedQuery: async () => [1, 0],
    };
    const index = new CodeIndex({ rootDir: root, provider });
    await index.updateFiles({ upsert: ["source.ts", "close.ts", "range.ts", "far.ts"] });

    const results = [];
    for await (const result of crossSearch({
      source: index,
      sourceFilter: { type: "all", path: "source.ts" },
      minSimilarity: 0.85,
      maxSimilarity: 0.95,
      limitPerFunction: 1,
      minLines: 1,
    })) results.push(result);

    expect(results).toHaveLength(1);
    expect(results[0]!.matches.map((match) => match.function.name)).toEqual(["rangeMarker"]);
    expect(results[0]!.matches[0]!.similarity).toBeGreaterThanOrEqual(0.85);
    expect(results[0]!.matches[0]!.similarity).toBeLessThanOrEqual(0.95);
    index.close();
  });

  it("accepts equivalent provider profiles regardless of property insertion order", async () => {
    const sourceRoot = temporaryRoot();
    const targetRoot = temporaryRoot();
    write(sourceRoot, "source.ts", `export function source() { return 1; }\n`);
    write(targetRoot, "target.ts", `export function target() { return 1; }\n`);
    const base = new FakeEmbeddingProvider();
    const reordered: EmbeddingProvider = {
      profile: {
        dimensions: 8,
        strategyVersion: "callable-v1",
        model: "deterministic",
        provider: "fake",
      },
      embedDocuments: (inputs) => base.embedDocuments(inputs),
      embedQuery: (input) => base.embedQuery(input),
    };
    const source = new CodeIndex({ rootDir: sourceRoot, provider: base });
    const target = new CodeIndex({ rootDir: targetRoot, provider: reordered });
    await source.updateFiles({ upsert: ["source.ts"] });
    await target.updateFiles({ upsert: ["target.ts"] });

    const results = [];
    for await (const result of crossSearch({ source, target, minLines: 1 })) results.push(result);
    expect(results).toHaveLength(1);
    source.close();
    target.close();
  });

  it("supports a read-only cross-search target", async () => {
    const sourceRoot = temporaryRoot();
    const targetRoot = temporaryRoot();
    write(sourceRoot, "source.ts", `export function source() { return 1; }\n`);
    write(targetRoot, "target.ts", `export function target() { return 1; }\n`);
    const provider = new FakeEmbeddingProvider();
    const source = new CodeIndex({ rootDir: sourceRoot, provider });
    const writableTarget = new CodeIndex({ rootDir: targetRoot, provider });
    await source.updateFiles({ upsert: ["source.ts"] });
    await writableTarget.updateFiles({ upsert: ["target.ts"] });
    writableTarget.close();

    const target = new CodeIndex({ rootDir: targetRoot, provider, readOnly: true });
    const results = [];
    for await (const result of crossSearch({ source, target, minLines: 1 })) results.push(result);
    expect(results[0]!.matches[0]!.function.name).toBe("target");
    source.close();
    target.close();
  });
});
