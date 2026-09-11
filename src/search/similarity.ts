import type { AnalysisSimilarity, IndexStatus } from "../types.js";

/** Select one scoring mode for the entire analysis, never a per-pair fallback. */
export function analysisSimilarity(source: IndexStatus, target: IndexStatus = source): AnalysisSimilarity {
  const complete = (status: IndexStatus): boolean => status.summariesEnabled
    && status.summaryProfile !== null
    && status.summaryCount === status.functionCount;
  return complete(source) && complete(target)
    ? { similarityMode: "code-summary-average", similarityWeights: { code: 0.5, summary: 0.5 } }
    : { similarityMode: "code", similarityWeights: { code: 1, summary: 0 } };
}
