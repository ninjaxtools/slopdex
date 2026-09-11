import { DatabaseSync } from "node:sqlite";

import { describe, expect, it, onTestFinished } from "vitest";

import { analyzeCohesion } from "../src/analysis/cohesion.js";
import { CodeIndex } from "../src/code-index.js";
import { formatCohesionSummary, formatSimilarityClusters, formatSimilaritySummary } from "../src/format.js";
import { crossSearch } from "../src/search/cross-search.js";
import type { CrossSearchOptions, CrossSearchResult, EmbeddingProvider, SummaryProvider } from "../src/types.js";
import { temporaryRoot, write } from "./helpers.js";

// Balanced is not the best code or purpose match, but is the best combined match.
const vectors: Record<string, { code: number[]; summary: number[] }> = {
  anchor: { code: [1, 0], summary: [1, 0] },
  implementation: { code: [1, 0], summary: [0, 1] },
  purpose: { code: [0, 1], summary: [1, 0] },
  balanced: { code: [0.8, 0.6], summary: [0.8, 0.6] },
};

const provider: EmbeddingProvider = {
  profile: { provider: "test", model: "fusion", dimensions: 2 },
  embedDocuments: async (inputs) => inputs.map((input) => {
    const summary = input.startsWith("purpose:");
    const name = summary ? input.slice("purpose:".length) : /symbol: (\w+)/.exec(input)![1]!;
    return vectors[name]![summary ? "summary" : "code"];
  }),
  embedQuery: async () => [1, 0],
};

async function makeIndex(names: string[], summaryModel: string | null = "purpose-model"): Promise<CodeIndex> {
  const root = temporaryRoot();
  const summaryProvider: SummaryProvider = {
    profile: { provider: "test", model: summaryModel ?? "unused", strategyVersion: "v1" },
    summarize: async ({ callable }) => `purpose:${callable.name}`,
  };
  const paths = names.map((name) => `${name === "balanced" ? "remote" : "src"}/${name}.ts`);
  names.forEach((name, index) => write(root, paths[index]!, `export function ${name}() {\n  return 1;\n}\n`));
  const index = new CodeIndex({ rootDir: root, provider, summaryProvider });
  onTestFinished(() => index.close());
  await index.updateFiles({ upsert: paths });
  if (summaryModel !== null) await index.useSummaries();
  return index;
}

async function collect(options: CrossSearchOptions): Promise<CrossSearchResult[]> {
  const results: CrossSearchResult[] = [];
  for await (const result of crossSearch(options)) results.push(result);
  return results;
}

function removeSummary(index: CodeIndex, name: string): void {
  const db = new DatabaseSync(index.indexPath);
  db.prepare("UPDATE functions SET summary = NULL, summary_embedding_id = NULL WHERE name = ?").run(name);
  db.close();
}

describe("combined code and purpose analysis", () => {
  it("ranks the average before top-k and applies half-open thresholds to the combined score", async () => {
    const index = await makeIndex(Object.keys(vectors));
    const options: CrossSearchOptions = {
      source: index, sourceFilter: { type: "all", path: "src/anchor.ts" }, limitPerFunction: 1,
    };
    const [result] = await collect(options);
    expect(result!.matches.map((match) => match.function.name)).toEqual(["balanced"]);
    const match = result!.matches[0]!;
    expect(match.similarity).toBeCloseTo(0.8);
    expect(match.codeSimilarity).toBeCloseTo(0.8);
    expect(match.summarySimilarity).toBeCloseTo(0.8);
    expect(result!.scoring).toMatchObject({
      similarityMode: "code-summary-average", similarityWeights: { code: 0.5, summary: 0.5 },
    });

    const [range] = await collect({ ...options, minSimilarity: 0.5, maxSimilarity: 0.7 });
    expect(range!.matches[0]).toMatchObject({
      function: { name: "implementation" }, similarity: 0.5, codeSimilarity: 1, summarySimilarity: 0,
    });
    expect(await collect({ ...options, minSimilarity: 0.81 })).toEqual([]);
    expect(formatSimilaritySummary(result!.matches, result!.source)).toContain("[combined 50/50; code 0.8000, summary 0.8000]");
    expect(formatSimilarityClusters([result!], true)).toContain("combined 50% code + 50% summary");
  });

  it("fuses cross-index matches with different summary-generator models", async () => {
    const source = await makeIndex(["anchor"], "source-model");
    const target = await makeIndex(["implementation", "purpose", "balanced"], "target-model");
    const [result] = await collect({ source, target, limitPerFunction: 1, minSimilarity: 0.7 });
    expect(result!.matches[0]!.function.name).toBe("balanced");
    expect(result!.matches[0]!.similarity).toBeCloseTo(0.8);
    expect(result!.scoring).toMatchObject({
      similarityMode: "code-summary-average",
      sourceSummaryProfile: { model: "source-model" }, targetSummaryProfile: { model: "target-model" },
    });
    const [range] = await collect({ source, target, minSimilarity: 0.5, maxSimilarity: 0.7, limitPerFunction: 1 });
    expect(range!.matches[0]!.function.name).toBe("implementation");
    expect(range!.matches[0]!.similarity).toBe(0.5);
  });

  it.each([[false, false], [true, false], [false, true]])(
    "uses code-only scoring without complete indexes (source=%s, target=%s)", async (sourceSummaries, targetSummaries) => {
      const source = await makeIndex(["anchor"], sourceSummaries ? "source-model" : null);
      const target = await makeIndex(["implementation", "purpose", "balanced"], targetSummaries ? "target-model" : null);
      const [result] = await collect({ source, target, limitPerFunction: 1 });
      expect(result!.matches[0]).toMatchObject({ function: { name: "implementation" }, similarity: 1 });
      expect(result!.matches[0]).not.toHaveProperty("summarySimilarity");
      expect(result!.scoring).toMatchObject({ similarityMode: "code", similarityWeights: { code: 1, summary: 0 } });
    },
  );

  it.each(["source", "target"])("falls back for the entire cross-index analysis if %s summaries are incomplete", async (side) => {
    const source = await makeIndex(["anchor", "purpose"]);
    const target = await makeIndex(["implementation", "balanced"]);
    removeSummary(side === "source" ? source : target, side === "source" ? "purpose" : "balanced");
    const [result] = await collect({ source, target, sourceFilter: { type: "all", path: "src/anchor.ts" }, limitPerFunction: 1 });
    expect(result!.scoring!.similarityMode).toBe("code");
    expect(result!.matches[0]!.function.name).toBe("implementation");
    expect(result!.matches[0]!.similarity).toBe(1);
  });

  it("uses fused neighbors for reciprocity, gaps, file metrics, and groups", async () => {
    const index = await makeIndex(Object.keys(vectors));
    const report = await analyzeCohesion({ source: index, neighbors: 1, minSimilarity: 0.6 });
    expect(report.parameters).toMatchObject({
      similarityMode: "code-summary-average", similarityWeights: { code: 0.5, summary: 0.5 },
    });
    expect(report.repository.summaryProfile).toEqual(index.summaryProvider.profile);
    expect(report.summary).toMatchObject({ semanticEdges: 3, sameFileRatio: 0, sameFolderRatio: 0, remoteRatio: 1 });
    expect(report.summary.weightedMeanDistance).toBeCloseTo(3);
    const first = report.pairs[0]!;
    expect([first.left.name, first.right.name].sort()).toEqual(["anchor", "balanced"]);
    expect(first.reciprocal).toBe(true);
    expect(first.similarity).toBeCloseTo(0.8);
    expect(first.codeSimilarity).toBeCloseTo(0.8);
    expect(first.summarySimilarity).toBeCloseTo(0.8);
    expect(first.semanticWeight).toBeCloseTo(0.5);
    expect(first.cohesionGap).toBeCloseTo(0.5 * (1 - Math.exp(-1.5)));
    expect(report.pairs.slice(1).every((pair) => pair.reciprocal === false)).toBe(true);
    expect(report.files.find((file) => file.path === "remote/balanced.ts")!.externalAffinity).toBeCloseTo(1);
    expect(report.files[0]!.strongestExternalMatch!.summarySimilarity).toBeCloseTo(0.8);
    expect(report.groups).toHaveLength(1);
    expect(report.groups[0]!.memberCount).toBe(4);
    expect(report.groups[0]!.minimumEdgeSimilarity).toBeCloseTo(0.7);
    expect(formatCohesionSummary(report)).toContain("similarity: combined 50% code + 50% summary");

    const scoped = await analyzeCohesion({ source: index, neighbors: 1, minSimilarity: 0.6, sourceFilter: { type: "all", path: "src/anchor.ts" } });
    expect(scoped.pairs).toHaveLength(1);
    expect(scoped.pairs[0]!.reciprocal).toBeNull();
    const range = await analyzeCohesion({ source: index, neighbors: 1, minSimilarity: 0.5, maxSimilarity: 0.6, sourceFilter: { type: "all", path: "src/anchor.ts" } });
    expect(range.pairs).toHaveLength(1);
    expect(range.pairs[0]!.similarity).toBe(0.5);
  });

  it.each([false, true])("falls back for same-index cross-search and cohesion (incomplete=%s)", async (incomplete) => {
    const index = await makeIndex(Object.keys(vectors), incomplete ? "summary-model" : null);
    if (incomplete) removeSummary(index, "purpose");
    const sourceFilter = { type: "all", path: "src/anchor.ts" } as const;
    const [result] = await collect({ source: index, sourceFilter, limitPerFunction: 1 });
    expect(result!.scoring!.similarityMode).toBe("code");
    expect(result!.matches[0]!.function.name).toBe("implementation");
    const report = await analyzeCohesion({ source: index, sourceFilter, neighbors: 1 });
    expect(report.parameters.similarityMode).toBe("code");
    expect(report.pairs[0]!.similarity).toBe(1);
    expect(report.pairs[0]).not.toHaveProperty("summarySimilarity");
    expect(report.summary.sameFolderRatio).toBe(1);
  });

  it("preserves candidate filters, self-exclusion, and symmetric deduplication", async () => {
    const index = await makeIndex(Object.keys(vectors));
    const anchor = index.allFunctions().find((callable) => callable.name === "anchor")!;
    const filtered = index.similarToFunction(anchor.id, {
      includeSummaries: true, limit: 1, minSimilarity: 0,
      excludePaths: ["remote/balanced.ts"], nameRegex: "anchor|purpose", minLines: 3,
    });
    expect(filtered.map((match) => match.function.name)).toEqual(["purpose"]);
    const deduplicated = await collect({ source: index, limitPerFunction: 10, crossFileOnly: true });
    const pairs = deduplicated.flatMap((result) => result.matches.map((match) => [result.source.id, match.function.id].sort().join(":")));
    expect(pairs).toHaveLength(6);
    expect(new Set(pairs).size).toBe(6);
    const symmetric = await collect({ source: index, limitPerFunction: 10, includeSymmetricDuplicates: true });
    expect(symmetric.flatMap((result) => result.matches)).toHaveLength(12);
    expect(await collect({ source: index, minLines: 4 })).toEqual([]);
  });

  it("keeps text queries code-only and summary-only even with a complete summary index", async () => {
    const index = await makeIndex(Object.keys(vectors));
    const code = await index.similaritySearch({ query: "meaning", minSimilarity: 0.99 });
    const summary = await index.searchSummary({ query: "meaning", minSimilarity: 0.99 });
    expect(code.map((match) => match.function.name)).toEqual(["anchor", "implementation"]);
    expect(summary.map((match) => match.function.name)).toEqual(["anchor", "purpose"]);
    expect([...code, ...summary].every((match) => match.summarySimilarity === undefined)).toBe(true);
  });

  it("excludes same-file fused neighbors before applying the cross-search limit", async () => {
    const index = await makeIndex(Object.keys(vectors));
    write(index.rootDir, "src/shared.ts", `export function anchor() {\n  return 1;\n}\nexport function balanced() {\n  return 1;\n}\n`);
    await index.updateFiles({ delete: ["src/anchor.ts", "remote/balanced.ts"], upsert: ["src/shared.ts"] });
    const results = await collect({
      source: index, sourceFilter: { type: "all", path: "src/shared.ts" },
      crossFileOnly: true, limitPerFunction: 1,
    });
    const anchor = results.find((result) => result.source.name === "anchor")!;
    expect(anchor.matches[0]!.function.name).toBe("implementation");
    expect(anchor.matches[0]!.similarity).toBe(0.5);
    expect(results.every((result) => result.matches.every((match) => match.function.path !== result.source.path))).toBe(true);
  });
});
