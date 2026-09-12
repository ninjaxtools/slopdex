import { realpath, stat } from "node:fs/promises";
import path from "node:path";

import { IncompatibleIndexError } from "../errors.js";
import type { CrossSearchOptions, CrossSearchResult } from "../types.js";
import { assertPositiveInteger, compileNameRegex, throwIfAborted } from "../utils.js";
import { analysisSimilarity } from "./similarity.js";

export async function* crossSearch(options: CrossSearchOptions): AsyncGenerator<CrossSearchResult> {
  const target = options.target ?? options.source;
  const sourceStatus = options.source.status();
  const targetStatus = target.status();
  const sourceProfile = JSON.stringify(sourceStatus.embeddingProfile);
  const targetProfile = JSON.stringify(targetStatus.embeddingProfile);
  if (sourceProfile !== targetProfile) {
    throw new IncompatibleIndexError("Cross-search requires identical embedding profiles.");
  }
  const scoring = {
    ...analysisSimilarity(sourceStatus, targetStatus),
    sourceDescriptionProfile: sourceStatus.descriptionProfile,
    targetDescriptionProfile: targetStatus.descriptionProfile,
  };
  const includeDescriptions = scoring.similarityMode === "code-description-file-average";

  const limit = options.limitPerFunction ?? 5;
  assertPositiveInteger(limit, "limitPerFunction");
  const minLines = options.minLines ?? 2;
  assertPositiveInteger(minLines, "minLines");
  const nameRegex = compileNameRegex(options.nameRegex);
  const sourceFunctions = (await options.source.sourceFunctions(options.sourceFilter ?? { type: "all" }))
    .filter((callable) => callable.lineCount >= minLines && (!nameRegex || nameRegex.test(callable.qualifiedName)));
  const sameIndex = target.indexPath === options.source.indexPath
    || await fileIdentity(target.indexPath) === await fileIdentity(options.source.indexPath);
  const sourceRoot = options.crossFileOnly ? await canonicalRoot(options.source.rootDir) : undefined;
  const targetRoot = options.crossFileOnly
    ? sameIndex ? sourceRoot! : await canonicalRoot(target.rootDir)
    : undefined;
  const canonicalFiles = new Map<string, Promise<string>>();
  const canonicalFile = (root: string, filePath: string): Promise<string> => {
    const absolutePath = path.resolve(root, filePath);
    let result = canonicalFiles.get(absolutePath);
    if (!result) {
      result = fileIdentity(absolutePath);
      canonicalFiles.set(absolutePath, result);
    }
    return result;
  };
  const targetPathsByCanonicalFile = new Map<string, string[]>();
  if (targetRoot) {
    const targetPaths = [...new Set(target.allFunctions().map((callable) => callable.path))];
    await Promise.all(targetPaths.map(async (targetPath) => {
      const canonicalPath = await canonicalFile(targetRoot, targetPath);
      const paths = targetPathsByCanonicalFile.get(canonicalPath) ?? [];
      paths.push(targetPath);
      targetPathsByCanonicalFile.set(canonicalPath, paths);
    }));
  }
  const seenPairs = new Set<string>();
  for (let index = 0; index < sourceFunctions.length; index += 1) {
    throwIfAborted(options.signal);
    const source = sourceFunctions[index]!;
    const vector = options.source.vectorForFunction(source.id);
    const canonicalSourceFile = sourceRoot ? await canonicalFile(sourceRoot, source.path) : undefined;
    const excludedTargetPaths = canonicalSourceFile ? targetPathsByCanonicalFile.get(canonicalSourceFile) : undefined;
    const excludePaths = excludedTargetPaths ? { excludePaths: excludedTargetPaths } : {};
    const candidates = sameIndex
      ? options.source.similarToFunction(source.id, {
        includeDescriptions,
        limit,
        minSimilarity: options.minSimilarity ?? -1,
        ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
        minLines,
        ...(options.nameRegex !== undefined ? { nameRegex: options.nameRegex } : {}),
        ...excludePaths,
      })
      : target.searchByVector(vector, {
        ...(includeDescriptions ? {
          descriptionVector: options.source.vectorForFunction(source.id, "description"),
          fileDescriptionVector: options.source.vectorForFile(source.path),
        } : {}),
        limit,
        minSimilarity: options.minSimilarity ?? -1,
        ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
        minLines,
        ...(options.nameRegex !== undefined ? { nameRegex: options.nameRegex } : {}),
        ...excludePaths,
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
    if (matches.length > 0) yield { source, matches, scoring };
    options.onProgress?.({ completed: index + 1, total: sourceFunctions.length });
  }
}

async function canonicalRoot(root: string): Promise<string> {
  try {
    return await realpath(root);
  } catch {
    return path.resolve(root);
  }
}

async function fileIdentity(filePath: string): Promise<string> {
  try {
    const metadata = await stat(filePath);
    return `inode:${metadata.dev}:${metadata.ino}`;
  } catch {
    return `path:${path.resolve(filePath)}`;
  }
}
