import type { AnalysisSimilarity, IndexStatus } from "../types.js";

/**
 * Floor the persisted similarity cache is routinely built for. Analysis entry
 * points anchor their refresh floor here unless the caller asks for less, so
 * sweeping thresholds at or above the default shares one cache band: higher
 * floors read it for free and only a lower floor triggers an expansion
 * recompute. Matches the CLI default threshold.
 */
export const SIMILARITY_CACHE_FLOOR_ANCHOR = 0.3;

/** Anchor a requested refresh floor so threshold sweeps share one cache band. */
export function similarityCacheFloor(minSimilarity?: number): number {
  return Math.min(minSimilarity ?? -1, SIMILARITY_CACHE_FLOOR_ANCHOR);
}

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
