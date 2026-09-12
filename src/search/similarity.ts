import type { AnalysisSimilarity, IndexStatus } from "../types.js";

/** Select one scoring mode for the entire analysis, never a per-pair fallback. */
export function analysisSimilarity(source: IndexStatus, target: IndexStatus = source): AnalysisSimilarity {
  const complete = (status: IndexStatus): boolean => status.descriptionsEnabled
    && status.descriptionProfile !== null
    && status.descriptionCount === status.functionCount
    && status.fileDescriptionCount === status.describableFileCount;
  return complete(source) && complete(target)
    ? { similarityMode: "code-description-file-average", similarityWeights: { code: 1 / 3, description: 1 / 3, fileDescription: 1 / 3 } }
    : { similarityMode: "code", similarityWeights: { code: 1, description: 0, fileDescription: 0 } };
}
