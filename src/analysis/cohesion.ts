import path from "node:path";

import { CodeIndexError } from "../errors.js";
import { analysisSimilarity } from "../search/similarity.js";
import type {
  CohesionAnalysisOptions,
  CohesionFileReport,
  CohesionGroup,
  CohesionLocation,
  CohesionPair,
  CohesionReport,
  IndexedFunction,
  SimilarityResult,
  SimilarityScores,
} from "../types.js";
import { assertPositiveInteger, compileNameRegex, throwIfAborted } from "../utils.js";

interface CandidateEdge extends SimilarityScores {
  left: IndexedFunction;
  right: IndexedFunction;
}

export async function analyzeCohesion(options: CohesionAnalysisOptions): Promise<CohesionReport> {
  const status = options.source.status();
  const scoring = analysisSimilarity(status);
  const neighbors = options.neighbors ?? 20;
  const limit = options.limit ?? 50;
  const minSimilarity = options.minSimilarity ?? 0.8;
  const minLines = options.minLines ?? 2;
  const sourceFilter = options.sourceFilter ?? { type: "all" };
  assertPositiveInteger(neighbors, "neighbors");
  assertPositiveInteger(limit, "limit");
  assertPositiveInteger(minLines, "minLines");
  if (!Number.isFinite(minSimilarity) || minSimilarity < -1 || minSimilarity >= 1) {
    throw new CodeIndexError("minSimilarity must be at least -1 and less than 1.");
  }
  if (options.maxSimilarity !== undefined
    && (!Number.isFinite(options.maxSimilarity) || options.maxSimilarity <= minSimilarity)) {
    throw new CodeIndexError("maxSimilarity must be greater than minSimilarity.");
  }
  const nameRegex = compileNameRegex(options.nameRegex);
  const candidateFunctions = options.source.allFunctions()
    .filter((callable) => callable.lineCount >= minLines && (!nameRegex || nameRegex.test(callable.qualifiedName)));
  const candidateIds = new Set(candidateFunctions.map((callable) => callable.id));
  const sourceFunctions = (await options.source.sourceFunctions(sourceFilter))
    .filter((callable) => candidateIds.has(callable.id));

  const neighborCache = new Map<number, Set<number>>();
  const neighborsFor = (callable: IndexedFunction): SimilarityResult[] => {
    throwIfAborted(options.signal);
    const matches = options.source.similarToFunction(callable.id, {
      includeDescriptions: scoring.similarityMode === "code-description-file-average",
      limit: neighbors,
      minSimilarity,
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
      minLines,
      ...(options.nameRegex !== undefined ? { nameRegex: options.nameRegex } : {}),
    }).filter((match) => candidateIds.has(match.function.id));
    neighborCache.set(callable.id, new Set(matches.map((match) => match.function.id)));
    return matches;
  };

  const edges = new Map<string, CandidateEdge>();
  for (let index = 0; index < sourceFunctions.length; index += 1) {
    const source = sourceFunctions[index]!;
    for (const match of neighborsFor(source)) {
      const [left, right] = orderedFunctions(source, match.function);
      const key = pairKey(left.id, right.id);
      const existing = edges.get(key);
      if (!existing || match.similarity > existing.similarity) {
        const { function: _function, ...scores } = match;
        edges.set(key, { left, right, ...scores });
      }
    }
    options.onProgress?.({ completed: index + 1, total: sourceFunctions.length });
  }

  const scoredPairs = [...edges.values()].map((edge) => {
    const leftNeighbors = neighborCache.get(edge.left.id);
    const rightNeighbors = neighborCache.get(edge.right.id);
    const reciprocal = leftNeighbors && rightNeighbors
      ? leftNeighbors.has(edge.right.id) && rightNeighbors.has(edge.left.id)
      : null;
    return scorePair(edge, reciprocal, minSimilarity);
  }).sort(comparePairs);
  scoredPairs.forEach((pair, index) => {
    pair.rank = index + 1;
  });

  const repositoryScope = sourceFilter.type === "all" && sourceFilter.path === undefined && sourceFilter.nameRegex === undefined;
  const metrics = calculateMetrics(
    repositoryScope ? "repository" : "selected-sources",
    sourceFunctions.length,
    candidateFunctions.length,
    scoredPairs,
  );
  const reportedPairs = scoredPairs.slice(0, limit);
  const files = aggregateFiles(sourceFunctions, scoredPairs)
    .filter((file) => file.internalAffinity + file.sameFolderAffinity + file.externalAffinity > 0)
    .slice(0, limit);
  const groups = buildGroups(reportedPairs);
  return {
    schemaVersion: 3,
    repository: {
      generation: status.generation,
      gitCheckpoint: status.gitCheckpoint,
      embeddingProfile: status.embeddingProfile,
      descriptionProfile: status.descriptionProfile,
    },
    parameters: {
      ...scoring,
      neighbors,
      limit,
      minSimilarity,
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
      minLines,
      ...(options.nameRegex !== undefined ? { nameRegex: options.nameRegex } : {}),
      sourceFilter,
    },
    metrics,
    pairs: reportedPairs,
    files,
    groups,
  };
}

export function cohesionLocation(leftPath: string, rightPath: string): CohesionLocation {
  const normalizedLeft = leftPath.replaceAll("\\", "/");
  const normalizedRight = rightPath.replaceAll("\\", "/");
  const leftDirectory = directoryParts(normalizedLeft);
  const rightDirectory = directoryParts(normalizedRight);
  let commonLength = 0;
  while (commonLength < leftDirectory.length
    && commonLength < rightDirectory.length
    && leftDirectory[commonLength] === rightDirectory[commonLength]) {
    commonLength += 1;
  }
  const commonParts = leftDirectory.slice(0, commonLength);
  const commonAncestor = commonParts.length > 0 ? commonParts.join("/") : null;
  const sameFile = normalizedLeft === normalizedRight;
  const folderHops = sameFile
    ? 0
    : leftDirectory.length - commonLength + rightDirectory.length - commonLength;
  return {
    category: sameFile ? "same-file" : folderHops === 0 ? "same-folder" : "different-folder",
    physicalDistance: sameFile ? 0 : 1 + folderHops,
    folderHops,
    commonAncestor,
    sourceTestPair: isTestPath(normalizedLeft) !== isTestPath(normalizedRight),
  };
}

function scorePair(edge: CandidateEdge, reciprocal: boolean | null, minSimilarity: number): CohesionPair {
  const location = cohesionLocation(edge.left.path, edge.right.path);
  const semanticWeight = clamp((edge.similarity - minSimilarity) / (1 - minSimilarity));
  const separationWeight = 1 - Math.exp(-location.physicalDistance / 2);
  return {
    rank: 0,
    left: edge.left,
    right: edge.right,
    similarity: edge.similarity,
    ...(edge.codeSimilarity !== undefined ? { codeSimilarity: edge.codeSimilarity } : {}),
    ...(edge.descriptionSimilarity !== undefined ? { descriptionSimilarity: edge.descriptionSimilarity } : {}),
    ...(edge.fileDescriptionSimilarity !== undefined ? { fileDescriptionSimilarity: edge.fileDescriptionSimilarity } : {}),
    reciprocal,
    semanticWeight,
    separationWeight,
    cohesionGap: semanticWeight * separationWeight,
    location,
  };
}

function calculateMetrics(
  scope: CohesionReport["metrics"]["scope"],
  functionsAnalyzed: number,
  candidateFunctions: number,
  pairs: readonly CohesionPair[],
): CohesionReport["metrics"] {
  const totalWeight = pairs.reduce((sum, pair) => sum + pair.semanticWeight, 0);
  const sameFileWeight = pairs
    .filter((pair) => pair.location.category === "same-file")
    .reduce((sum, pair) => sum + pair.semanticWeight, 0);
  const sameFolderWeight = pairs
    .filter((pair) => pair.location.category === "same-folder")
    .reduce((sum, pair) => sum + pair.semanticWeight, 0);
  const remoteWeight = pairs
    .filter((pair) => pair.location.category === "different-folder")
    .reduce((sum, pair) => sum + pair.semanticWeight, 0);
  const weightedDistance = pairs.reduce(
    (sum, pair) => sum + pair.semanticWeight * pair.location.physicalDistance,
    0,
  );
  return {
    scope,
    functionsAnalyzed,
    candidateFunctions,
    semanticEdges: pairs.length,
    sameFileRatio: ratio(sameFileWeight, totalWeight),
    sameFolderRatio: ratio(sameFolderWeight, totalWeight),
    remoteRatio: ratio(remoteWeight, totalWeight),
    weightedMeanDistance: ratio(weightedDistance, totalWeight),
  };
}

function aggregateFiles(
  sourceFunctions: readonly IndexedFunction[],
  pairs: readonly CohesionPair[],
): CohesionFileReport[] {
  const sourceIds = new Set(sourceFunctions.map((callable) => callable.id));
  const counts = new Map<string, number>();
  for (const callable of sourceFunctions) counts.set(callable.path, (counts.get(callable.path) ?? 0) + 1);
  const reports = new Map<string, CohesionFileReport>();
  for (const [filePath, functionCount] of counts) {
    reports.set(filePath, {
      path: filePath,
      functionCount,
      internalAffinity: 0,
      sameFolderAffinity: 0,
      externalAffinity: 0,
      externalAffinityRatio: 0,
    });
  }
  for (const pair of pairs) {
    if (sourceIds.has(pair.left.id)) addFileAffinity(reports.get(pair.left.path)!, pair.right, pair);
    if (sourceIds.has(pair.right.id)) addFileAffinity(reports.get(pair.right.path)!, pair.left, pair);
  }
  for (const report of reports.values()) {
    const total = report.internalAffinity + report.sameFolderAffinity + report.externalAffinity;
    report.externalAffinityRatio = ratio(report.externalAffinity, total);
  }
  return [...reports.values()].sort((left, right) => right.externalAffinityRatio - left.externalAffinityRatio
    || right.externalAffinity - left.externalAffinity
    || left.path.localeCompare(right.path));
}

function addFileAffinity(
  report: CohesionFileReport,
  match: IndexedFunction,
  pair: CohesionPair,
): void {
  if (pair.location.category === "same-file") report.internalAffinity += pair.semanticWeight;
  else if (pair.location.category === "same-folder") report.sameFolderAffinity += pair.semanticWeight;
  else {
    report.externalAffinity += pair.semanticWeight;
    updateStrongestExternal(report, match, pair);
  }
}

function updateStrongestExternal(
  report: CohesionFileReport,
  callable: IndexedFunction,
  pair: CohesionPair,
): void {
  if (!report.strongestExternalMatch || pair.cohesionGap > report.strongestExternalMatch.cohesionGap) {
    report.strongestExternalMatch = {
      function: callable,
      similarity: pair.similarity,
      ...(pair.codeSimilarity !== undefined ? { codeSimilarity: pair.codeSimilarity } : {}),
      ...(pair.descriptionSimilarity !== undefined ? { descriptionSimilarity: pair.descriptionSimilarity } : {}),
      ...(pair.fileDescriptionSimilarity !== undefined ? { fileDescriptionSimilarity: pair.fileDescriptionSimilarity } : {}),
      cohesionGap: pair.cohesionGap,
    };
  }
}

function buildGroups(pairs: readonly CohesionPair[]): CohesionGroup[] {
  const functions = new Map<number, IndexedFunction>();
  const neighbors = new Map<number, Set<number>>();
  for (const pair of pairs) {
    functions.set(pair.left.id, pair.left);
    functions.set(pair.right.id, pair.right);
    addNeighbor(neighbors, pair.left.id, pair.right.id);
    addNeighbor(neighbors, pair.right.id, pair.left.id);
  }
  const seen = new Set<number>();
  const groups: CohesionGroup[] = [];
  for (const start of neighbors.keys()) {
    if (seen.has(start)) continue;
    const pending = [start];
    const memberIds = new Set<number>();
    while (pending.length > 0) {
      const id = pending.pop()!;
      if (seen.has(id)) continue;
      seen.add(id);
      memberIds.add(id);
      for (const neighbor of neighbors.get(id) ?? []) pending.push(neighbor);
    }
    const groupPairs = pairs.filter((pair) => memberIds.has(pair.left.id) && memberIds.has(pair.right.id));
    const members = [...memberIds].map((id) => functions.get(id)!).sort(compareFunctions);
    groups.push({
      rank: 0,
      memberCount: members.length,
      fileCount: new Set(members.map((member) => member.path)).size,
      minimumEdgeSimilarity: Math.min(...groupPairs.map((pair) => pair.similarity)),
      maximumEdgeSimilarity: Math.max(...groupPairs.map((pair) => pair.similarity)),
      maximumPhysicalDistance: Math.max(...groupPairs.map((pair) => pair.location.physicalDistance)),
      maximumCohesionGap: Math.max(...groupPairs.map((pair) => pair.cohesionGap)),
      members,
    });
  }
  groups.sort((left, right) => right.maximumCohesionGap - left.maximumCohesionGap
    || right.memberCount - left.memberCount
    || compareFunctions(left.members[0]!, right.members[0]!));
  groups.forEach((group, index) => {
    group.rank = index + 1;
  });
  return groups;
}

function directoryParts(filePath: string): string[] {
  const directory = path.posix.dirname(filePath.replaceAll("\\", "/"));
  return directory === "." ? [] : directory.split("/").filter(Boolean);
}

function isTestPath(filePath: string): boolean {
  const original = filePath.replaceAll("\\", "/");
  const normalized = original.toLowerCase();
  const segments = normalized.split("/");
  const fileName = segments.at(-1) ?? "";
  return segments.some((segment) => segment === "test" || segment === "tests" || segment === "__tests__")
    || /\.(?:test|spec)\.[cm]?[jt]sx?$/.test(fileName)
    || /_test\.go$/.test(fileName)
    || /^(?:test_.+|.+_test)\.(?:py|pyw|rs|c|h)$/.test(fileName)
    || /^(?:Test.+|.+Tests?|.+TestCase)\.java$/.test(original.split("/").at(-1) ?? "");
}

function orderedFunctions(left: IndexedFunction, right: IndexedFunction): [IndexedFunction, IndexedFunction] {
  return compareFunctions(left, right) <= 0 ? [left, right] : [right, left];
}

function compareFunctions(left: IndexedFunction, right: IndexedFunction): number {
  return left.path.localeCompare(right.path)
    || left.startLine - right.startLine
    || left.startColumn - right.startColumn
    || left.qualifiedName.localeCompare(right.qualifiedName)
    || left.id - right.id;
}

function comparePairs(left: CohesionPair, right: CohesionPair): number {
  return right.cohesionGap - left.cohesionGap
    || right.similarity - left.similarity
    || Number(right.reciprocal === true) - Number(left.reciprocal === true)
    || compareFunctions(left.left, right.left)
    || compareFunctions(left.right, right.right);
}

function pairKey(leftId: number, rightId: number): string {
  return leftId < rightId ? `${leftId}:${rightId}` : `${rightId}:${leftId}`;
}

function addNeighbor(neighbors: Map<number, Set<number>>, source: number, target: number): void {
  const values = neighbors.get(source) ?? new Set<number>();
  values.add(target);
  neighbors.set(source, values);
}

function ratio(numerator: number, denominator: number): number {
  return denominator === 0 ? 0 : numerator / denominator;
}

function clamp(value: number): number {
  return Math.max(0, Math.min(1, value));
}
