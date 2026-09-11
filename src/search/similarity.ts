import type { AnalysisSimilarity, IndexStatus } from "../types.js";

/** Select one scoring mode for the entire analysis, never a per-pair fallback. */
export function analysisSimilarity(source: IndexStatus, target: IndexStatus = source): AnalysisSimilarity {
  const complete = (status: IndexStatus): boolean => status.descriptionsEnabled
    && status.descriptionProfile !== null
    && status.descriptionCount === status.functionCount;
  return complete(source) && complete(target)
    ? { similarityMode: "code-description-average", similarityWeights: { code: 0.5, description: 0.5 } }
    : { similarityMode: "code", similarityWeights: { code: 1, description: 0 } };
}
