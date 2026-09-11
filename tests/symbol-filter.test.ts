import { describe, expect, it, onTestFinished, vi } from "vitest";

import { analyzeCohesion } from "../src/analysis/cohesion.js";
import { CodeIndex } from "../src/code-index.js";
import { crossSearch } from "../src/search/cross-search.js";
import type { CrossSearchOptions, CrossSearchResult, EmbeddingProvider } from "../src/types.js";
import { commitAll, initGit, temporaryRoot, write } from "./helpers.js";

const provider: EmbeddingProvider = {
  profile: { provider: "controlled", model: "equal", dimensions: 2 },
  embedDocuments: async (inputs) => inputs.map(() => [1, 0]),
  embedQuery: async () => [1, 0],
};

function openIndex(root: string, embeddingProvider = provider): CodeIndex {
  const index = new CodeIndex({
    rootDir: root, provider: embeddingProvider,
    summaryProvider: {
      profile: { provider: "test", model: "purpose", strategyVersion: "v1" },
      summarize: async ({ callable }) => `Purpose of ${callable.qualifiedName}`,
    },
  });
  onTestFinished(() => index.close());
  return index;
}

async function collect(options: CrossSearchOptions): Promise<CrossSearchResult[]> {
  const results: CrossSearchResult[] = [];
  for await (const result of crossSearch(options)) results.push(result);
  return results;
}

describe("source symbol filtering", () => {
  it.each([
    { separate: false, summaries: false }, { separate: true, summaries: false },
    { separate: false, summaries: true }, { separate: true, summaries: true },
  ])("searches unrestricted targets (separate=$separate, summaries=$summaries)", async ({ separate, summaries }) => {
    const root = temporaryRoot();
    const targetRoot = separate ? temporaryRoot() : root;
    write(root, "src/source.ts", `class Source {
  keep() {
    return 1;
  }
  drop() {
    return 1;
  }
  short() { return 1; }
}
`);
    write(targetRoot, "remote/target.ts", "export function other() {\n  return 1;\n}\n");
    const source = openIndex(root);
    const target = separate ? openIndex(targetRoot) : source;
    await source.updateFromWorkingTree();
    if (separate) await target.updateFromWorkingTree();
    if (summaries) {
      await source.useSummaries();
      if (separate) await target.useSummaries();
    }
    const options: CrossSearchOptions = {
      source, target,
      sourceFilter: { type: "all", path: "src", nameRegex: "^Source\\.(keep|short)$" },
      minLines: 2, crossFileOnly: true, limitPerFunction: 1, minSimilarity: 0.8, maxSimilarity: 1.1,
    };
    const results = await collect(options);
    expect(results).toHaveLength(1);
    expect(results[0]!.source.qualifiedName).toBe("Source.keep");
    expect(results[0]!.matches.map((match) => match.function.qualifiedName)).toEqual(["other"]);
    expect(results[0]!.scoring?.similarityMode).toBe(summaries ? "code-summary-average" : "code");
    expect(await collect({ ...options, sourceFilter: { type: "all", nameRegex: "^missing$" } })).toEqual([]);
    await expect(collect({ ...options, sourceFilter: { type: "all", nameRegex: "[" } })).rejects.toThrow("Invalid name regex");
  });

  it("intersects symbol, path, changed-since, uncommitted, and minimum-line filters", async () => {
    const root = temporaryRoot();
    initGit(root);
    const source = (value: number) => `export function stableKeep() {\n  return 0;\n}
export function changedKeep() {\n  return ${value};\n}
export function changedDrop() {\n  return ${value};\n}
export function tinyKeep() { return ${value}; }\n`;
    write(root, "src/source.ts", source(0));
    write(root, "src/committed.ts", "export function committedKeep() {\n  return 0;\n}\n");
    write(root, "outside/source.ts", "export function outsideKeep() {\n  return 0;\n}\n");
    const base = commitAll(root, "base");
    write(root, "src/committed.ts", "export function committedKeep() {\n  return 1;\n}\n");
    commitAll(root, "committed change");
    write(root, "src/source.ts", source(1));
    write(root, "outside/source.ts", "export function outsideKeep() {\n  return 1;\n}\n");
    const index = openIndex(root);
    await index.updateFromGit();
    const filter = { type: "changed-since", commit: base, uncommitted: true, path: "src", nameRegex: "Keep$" } as const;
    expect((await index.sourceFunctions(filter)).map((item) => item.name)).toEqual(["changedKeep", "tinyKeep"]);
    const results = await collect({ source: index, sourceFilter: filter, minLines: 2, limitPerFunction: 20 });
    expect(results.map((result) => result.source.name)).toEqual(["changedKeep"]);
    expect(results[0]!.matches.map((match) => match.function.name).sort())
      .toEqual(["changedDrop", "committedKeep", "outsideKeep", "stableKeep"]);
  });

  it("reports scoped cohesion with unfiltered candidate neighbors", async () => {
    const root = temporaryRoot();
    write(root, "source.ts", "export function selected() {\n  return 1;\n}\n");
    write(root, "remote.ts", "export function other() {\n  return 1;\n}\n");
    const index = openIndex(root);
    await index.updateFromWorkingTree();
    const report = await analyzeCohesion({ source: index, sourceFilter: { type: "all", nameRegex: "^selected$" } });
    expect(report.summary).toMatchObject({ scope: "selected-sources", functionsAnalyzed: 1, candidateFunctions: 2, semanticEdges: 1 });
    expect(report.parameters.sourceFilter).toEqual({ type: "all", nameRegex: "^selected$" });
    expect(report.pairs[0]!.reciprocal).toBeNull();
    expect([report.pairs[0]!.left.name, report.pairs[0]!.right.name].sort()).toEqual(["other", "selected"]);
    const empty = await analyzeCohesion({ source: index, sourceFilter: { type: "all", nameRegex: "^missing$" } });
    expect(empty.summary).toMatchObject({ scope: "selected-sources", functionsAnalyzed: 0, candidateFunctions: 2, semanticEdges: 0 });
  });
});

describe("query result symbol filtering", () => {
  it.each(["similaritySearch", "searchSummary"] as const)("filters %s before applying the result limit", async (method) => {
    const root = temporaryRoot();
    write(root, "functions.ts", "function skip() {}\nclass Wanted { keep() {} }\n");
    const embedQuery = vi.fn(provider.embedQuery);
    const index = openIndex(root, { ...provider, embedQuery });
    await index.updateFromWorkingTree();
    await index.useSummaries();
    expect((await index[method]({ query: "purpose", limit: 1 }))[0]!.function.name).toBe("skip");
    expect((await index[method]({ query: "purpose", limit: 1, nameRegex: "^Wanted\\.keep$" }))[0]!.function.qualifiedName).toBe("Wanted.keep");
    expect(await index[method]({ query: "purpose", nameRegex: "^missing$" })).toEqual([]);
    expect(await index[method]({ query: "purpose", nameRegex: "" })).toHaveLength(2);
    embedQuery.mockClear();
    await expect(index[method]({ query: "purpose", nameRegex: "[" })).rejects.toThrow("Invalid name regex");
    expect(embedQuery).not.toHaveBeenCalled();
  });
});
