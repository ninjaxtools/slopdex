import { linkSync, symlinkSync } from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";

import { describe, expect, it } from "vitest";

import { CodeIndex } from "../src/code-index.js";
import { crossSearch } from "../src/search/cross-search.js";
import { FakeEmbeddingProvider, temporaryRoot, write } from "./helpers.js";
import type { DescriptionProvider, EmbeddingProvider, Reranker } from "../src/types.js";

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

  it("persists query embeddings by profile, operation, and input", async () => {
    const root = temporaryRoot();
    write(root, "function.ts", "export function one() { return 1; }\n");
    class QueryCountingProvider extends FakeEmbeddingProvider {
      public queries: string[] = [];

      public override async embedQuery(input: string): Promise<number[]> {
        this.queries.push(input);
        return await super.embedQuery(input);
      }
    }
    const provider = new QueryCountingProvider();
    const first = new CodeIndex({ rootDir: root, provider });
    await first.updateFiles({ upsert: ["function.ts"] });
    await first.similaritySearch({ query: "same query" });
    await first.similaritySearch({ query: "same query" });
    first.close();

    const reopened = new CodeIndex({ rootDir: root, provider });
    await reopened.similaritySearch({ query: "same query" });
    expect(provider.queries).toEqual(["same query"]);
    reopened.close();
  });

  it("reranks a wider candidate set while preserving embedding similarity", async () => {
    const root = temporaryRoot();
    write(root, "functions.ts", ["one", "two", "three", "four", "five"]
      .map((name) => `export function ${name}() { return ${JSON.stringify(name)}; }`)
      .join("\n"));
    const requests: Array<{ query: string; documents: readonly string[]; limit?: number }> = [];
    const reranker: Reranker = {
      profile: { provider: "test", model: "controlled" },
      rerank: async (query, documents, options) => {
        requests.push({ query, documents, ...(options?.limit !== undefined ? { limit: options.limit } : {}) });
        return [{ index: 4, score: 0.98 }];
      },
    };
    const index = new CodeIndex({
      rootDir: root,
      provider: {
        profile: { provider: "controlled", model: "equal", dimensions: 2 },
        embedDocuments: async (inputs) => inputs.map(() => [1, 0]),
        embedQuery: async () => [1, 0],
      },
      reranker,
    });
    await index.updateFiles({ upsert: ["functions.ts"] });

    const results = await index.similaritySearch({ query: "find the fifth function", limit: 1 });

    expect(requests).toHaveLength(1);
    expect(requests[0]).toMatchObject({ query: "find the fifth function", limit: 1 });
    expect(requests[0]!.documents).toHaveLength(5);
    expect(requests[0]!.documents[4]).toContain("symbol: five");
    expect(results).toMatchObject([{ similarity: 1, rerankScore: 0.98, function: { name: "five" } }]);
    index.close();
  });

  it("reranks description searches using candidate purpose text", async () => {
    const root = temporaryRoot();
    write(root, "functions.ts", "export function one() { return 1; }\nexport function two() { return 2; }\n");
    const documents: string[][] = [];
    const descriptionProvider: DescriptionProvider = {
      profile: { provider: "test", model: "purposes", strategyVersion: "test-v1" },
      describeFile: async () => "Contains numbered functions.",
      describe: async ({ callable }) => `Purpose of ${callable.name}.`,
    };
    const reranker: Reranker = {
      profile: { provider: "test", model: "controlled" },
      rerank: async (_query, inputs) => {
        documents.push([...inputs]);
        return [{ index: 1, score: 0.9 }];
      },
    };
    const index = new CodeIndex({
      rootDir: root,
      provider: {
        profile: { provider: "controlled", model: "equal", dimensions: 2 },
        embedDocuments: async (inputs) => inputs.map(() => [1, 0]),
        embedQuery: async () => [1, 0],
      },
      descriptionProvider,
      reranker,
    });
    await index.updateFiles({ upsert: ["functions.ts"] });
    await index.useDescriptions();

    const results = await index.searchDescription({ query: "second purpose", limit: 1 });

    expect(documents[0]).toEqual([
      "path: functions.ts\nsymbol: one\ndescription: Purpose of one.",
      "path: functions.ts\nsymbol: two\ndescription: Purpose of two.",
    ]);
    expect(results).toMatchObject([{ rerankScore: 0.9, function: { name: "two" } }]);
    index.close();
  });

  it("rejects invalid output from custom rerankers", async () => {
    const root = temporaryRoot();
    write(root, "function.ts", "export function one() { return 1; }\n");
    const index = new CodeIndex({
      rootDir: root,
      provider: new FakeEmbeddingProvider(),
      reranker: {
        profile: { provider: "broken", model: "test" },
        rerank: async () => [{ index: 5, score: Number.NaN }],
      },
    });
    await index.updateFiles({ upsert: ["function.ts"] });

    await expect(index.similaritySearch({ query: "one" })).rejects.toThrow(/broken returned invalid reranking results/);
    index.close();
  });
});

describe("cross search", () => {
  it("rejects indexes from before the cache schema cutover", async () => {
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
    database.exec("UPDATE metadata SET value = '4' WHERE key = 'schema_version';");
    database.close();

    expect(() => new CodeIndex({ rootDir: root, provider })).toThrow(/Unsupported index schema version 4/);
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

  it("can re-rank semantic matches by physical distance", async () => {
    const root = temporaryRoot();
    write(root, "src/feature/functions.ts", `
export function source() { return 1; }
export function sameFile() { return 2; }
`);
    write(root, "src/feature/sibling.ts", "export function sibling() { return 3; }\n");
    write(root, "packages/remote.ts", "export function remote() { return 4; }\n");
    const index = new CodeIndex({ rootDir: root, provider: {
      profile: { provider: "controlled", model: "equal", dimensions: 2, strategyVersion: "callable-v1" },
      embedDocuments: async (inputs) => inputs.map(() => [1, 0]),
      embedQuery: async () => [1, 0],
    } });
    await index.updateFiles({ upsert: ["src/feature/functions.ts", "src/feature/sibling.ts", "packages/remote.ts"] });

    const results = [];
    for await (const result of crossSearch({
      source: index,
      sourceFilter: { type: "all", path: "src/feature/functions.ts" },
      limitPerFunction: 3,
      includeSymmetricDuplicates: true,
      cohesion: true,
      minLines: 1,
    })) results.push(result);

    expect(results[0]!.matches.map((match) => match.function.name)).toEqual(["remote", "sibling", "sameFile"]);
    expect(results[0]!.matches.map((match) => match.physicalDistance)).toEqual([4, 1, 0]);
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

  it("filters source and match qualified names before applying the per-function limit", async () => {
    const sourceRoot = temporaryRoot();
    const targetRoot = temporaryRoot();
    write(sourceRoot, "source.ts", `class Source {
  keep() {
    return 1;
  }
  ignore() {
    return 2;
  }
}\n`);
    write(targetRoot, "target.ts", `class Target {
  ignore() {
    return 1;
  }
  keep() {
    return 2;
  }
}\n`);
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
    for await (const result of crossSearch({ source, target, nameRegex: "\\.keep$", limitPerFunction: 1 })) {
      results.push(result);
    }

    expect(results).toHaveLength(1);
    expect(results[0]!.source.qualifiedName).toBe("Source.keep");
    expect(results[0]!.matches.map((match) => match.function.qualifiedName)).toEqual(["Target.keep"]);
    await expect(crossSearch({ source, target, nameRegex: "[" }).next()).rejects.toThrow(/Invalid name regex/);
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

  it("applies a half-open similarity range before limiting matches", async () => {
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
      maxSimilarity: 1,
      limitPerFunction: 1,
      minLines: 1,
    })) results.push(result);

    expect(results).toHaveLength(1);
    expect(results[0]!.matches.map((match) => match.function.name)).toEqual(["rangeMarker"]);
    expect(results[0]!.matches[0]!.similarity).toBeGreaterThanOrEqual(0.85);
    expect(results[0]!.matches[0]!.similarity).toBeLessThan(1);
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
