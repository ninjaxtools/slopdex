import type { IndexedFunction, SimilarityResult } from "./types.js";

function functionName(value: Pick<IndexedFunction, "path" | "qualifiedName">): string {
  return `${value.path} :: ${value.qualifiedName}`;
}

export function formatSimilaritySummary(
  matches: readonly SimilarityResult[],
  source?: IndexedFunction,
): string {
  const lines = matches.map((match) => `${source ? "  " : ""}${match.similarity.toFixed(4)}  ${functionName(match.function)}`);
  if (!source) return lines.length > 0 ? lines.join("\n") : "No matches.";
  return [functionName(source), ...(lines.length > 0 ? lines : ["  No matches."])].join("\n");
}
