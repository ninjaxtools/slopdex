import { describe, expect, it } from "vitest";

import { formatSimilaritySummary } from "../src/format.js";
import type { IndexedFunction, SimilarityResult } from "../src/types.js";

function indexedFunction(path: string, qualifiedName: string): IndexedFunction {
  return { path, qualifiedName } as IndexedFunction;
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
