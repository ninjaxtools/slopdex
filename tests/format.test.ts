import { describe, expect, it } from "vitest";

import { formatSimilarityClusters, formatSimilaritySummary } from "../src/format.js";
import type { CrossSearchResult, IndexedFunction, SimilarityResult } from "../src/types.js";

function indexedFunction(path: string, qualifiedName: string, id = 0): IndexedFunction {
  return { id, path, qualifiedName, startLine: id, startColumn: id + 1 } as IndexedFunction;
}

describe("formatSimilaritySummary", () => {
  it("formats search matches with file and function names", () => {
    const matches: SimilarityResult[] = [
      { similarity: 0.93214, function: indexedFunction("src/session.ts", "Session.validate") },
      { similarity: 0.8, function: indexedFunction("src/auth.ts", "authenticate") },
    ];

    expect(formatSimilaritySummary(matches)).toBe([
      "0.9321  src/session.ts :: Session.validate",
      "0.8000  src/auth.ts :: authenticate",
    ].join("\n"));
  });

  it("indents cross-search matches beneath the source function", () => {
    const source = indexedFunction("src/source.ts", "Source.load");
    const matches: SimilarityResult[] = [
      { similarity: 0.75, function: indexedFunction("src/target.ts", "Target.fetch") },
    ];

    expect(formatSimilaritySummary(matches, source)).toBe([
      "src/source.ts :: Source.load",
      "  0.7500  src/target.ts :: Target.fetch",
    ].join("\n"));
  });

  it("makes empty summaries explicit", () => {
    expect(formatSimilaritySummary([])).toBe("No matches.");
    expect(formatSimilaritySummary([], indexedFunction("src/source.ts", "source"))).toBe([
      "src/source.ts :: source",
      "  No matches.",
    ].join("\n"));
  });
});

describe("formatSimilarityClusters", () => {
  it("merges overlapping pairs and lists each function once", () => {
    const one = indexedFunction("src/one.ts", "one", 1);
    const two = indexedFunction("src/two.ts", "two", 2);
    const three = indexedFunction("src/three.ts", "three", 3);
    const four = indexedFunction("src/four.ts", "four", 4);
    const five = indexedFunction("src/five.ts", "five", 5);
    const results: CrossSearchResult[] = [
      { source: one, matches: [{ function: two, similarity: 0.95 }] },
      { source: two, matches: [{ function: three, similarity: 0.85 }] },
      { source: four, matches: [{ function: five, similarity: 0.9 }] },
    ];

    expect(formatSimilarityClusters(results, true)).toBe([
      "Cluster 1 (3 functions, similarity 0.8500-0.9500)",
      "  src/one.ts:1:2 :: one",
      "  src/three.ts:3:4 :: three",
      "  src/two.ts:2:3 :: two",
      "",
      "Cluster 2 (2 functions, similarity 0.9000)",
      "  src/five.ts:5:6 :: five",
      "  src/four.ts:4:5 :: four",
    ].join("\n"));
  });

  it("makes empty cluster output explicit", () => {
    expect(formatSimilarityClusters([], true)).toBe("No clusters.");
  });

  it("labels source and target members in cross-index clusters", () => {
    const source = indexedFunction("src/same.ts", "same", 1);
    const target = indexedFunction("src/same.ts", "same", 1);
    expect(formatSimilarityClusters([
      { source, matches: [{ function: target, similarity: 0.9 }] },
    ], false)).toContain([
      "  [source] src/same.ts:1:2 :: same",
      "  [target] src/same.ts:1:2 :: same",
    ].join("\n"));
  });
});
