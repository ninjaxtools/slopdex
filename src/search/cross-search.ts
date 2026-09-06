import { IncompatibleIndexError } from "../errors.js";
import type { CrossSearchOptions, CrossSearchResult } from "../types.js";
import { assertPositiveInteger, throwIfAborted } from "../utils.js";

export async function* crossSearch(options: CrossSearchOptions): AsyncGenerator<CrossSearchResult> {
  const target = options.target ?? options.source;
  const sourceProfile = JSON.stringify(options.source.status().embeddingProfile);
  const targetProfile = JSON.stringify(target.status().embeddingProfile);
  if (sourceProfile !== targetProfile) {
    throw new IncompatibleIndexError("Cross-search requires identical embedding profiles.");
  }

  const limit = options.limitPerFunction ?? 5;
  assertPositiveInteger(limit, "limitPerFunction");
  const sourceFunctions = await options.source.sourceFunctions(options.sourceFilter ?? { type: "all" });
  const sameIndex = target.indexPath === options.source.indexPath;
  const seenPairs = new Set<string>();
  for (let index = 0; index < sourceFunctions.length; index += 1) {
    throwIfAborted(options.signal);
    const source = sourceFunctions[index]!;
    const vector = options.source.vectorForFunction(source.id);
    const candidates = sameIndex
      ? options.source.similarToFunction(source.id, {
        limit,
        minSimilarity: options.minSimilarity ?? -1,
        ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
      })
      : target.searchByVector(vector, {
        limit,
        minSimilarity: options.minSimilarity ?? -1,
        ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
      });
    const matches = sameIndex && !options.includeSymmetricDuplicates
      ? candidates.filter((match) => {
        const pair = source.id < match.function.id
          ? `${source.id}:${match.function.id}`
          : `${match.function.id}:${source.id}`;
        if (seenPairs.has(pair)) return false;
        seenPairs.add(pair);
        return true;
      })
      : candidates;
    if (matches.length > 0) yield { source, matches };
    options.onProgress?.({ completed: index + 1, total: sourceFunctions.length });
  }
}
