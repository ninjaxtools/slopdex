import { lstat, readFile } from "node:fs/promises";
import path from "node:path";

import { CodeIndexError, GitDivergenceError } from "./errors.js";
import { GitRepository, type GitChange, type GitTreeEntry } from "./git/repository.js";
import { parseCallables } from "./parser/callable-parser.js";
import { SourcePolicy } from "./source-policy.js";
import { IndexDatabase, type PreparedCallable, type PreparedFile } from "./storage/database.js";
import type {
  CodeIndexOptions,
  CrossSearchSourceFilter,
  EmbeddingProfile,
  IndexStatus,
  IndexedFunction,
  SimilarityResult,
  SimilaritySearchOptions,
  UpdateFilesOptions,
  UpdateFromGitOptions,
  UpdateStats,
} from "./types.js";
import { assertPositiveInteger, chunk, normalizeEmbeddingVector, normalizeRelativePath, sha256, throwIfAborted } from "./utils.js";

const DEFAULT_MAX_FILE_SIZE = 1024 * 1024;
const DEFAULT_BATCH_SIZE = 32;

export class CodeIndex {
  public readonly rootDir: string;
  public readonly indexPath: string;
  public readonly provider;
  readonly #database: IndexDatabase;
  readonly #policy: SourcePolicy;
  readonly #maxFileSize: number;
  readonly #embeddingBatchSize: number;
  readonly #git: GitRepository;
  readonly #onWarning: (message: string) => void;

  public constructor(options: CodeIndexOptions) {
    this.rootDir = path.resolve(options.rootDir);
    this.indexPath = path.resolve(options.indexPath ?? path.join(this.rootDir, ".slopdex", "index.sqlite"));
    this.provider = options.provider;
    const profile = normalizeProfile(options.provider.profile);
    assertPositiveInteger(profile.dimensions, "embedding dimensions");
    this.#database = new IndexDatabase(this.indexPath, this.rootDir, profile, options.readOnly ?? false);
    this.#policy = new SourcePolicy(options.include, options.exclude);
    this.#maxFileSize = options.maxFileSize ?? DEFAULT_MAX_FILE_SIZE;
    this.#embeddingBatchSize = options.embeddingBatchSize ?? DEFAULT_BATCH_SIZE;
    this.#onWarning = options.onWarning ?? console.warn;
    assertPositiveInteger(this.#maxFileSize, "maxFileSize");
    assertPositiveInteger(this.#embeddingBatchSize, "embeddingBatchSize");
    this.#git = new GitRepository(this.rootDir);
  }

  public close(): void {
    this.#database.close();
  }

  public status(): IndexStatus {
    return this.#database.status();
  }

  public allFunctions(): IndexedFunction[] {
    return this.#database.allFunctions();
  }

  public async updateFiles(options: UpdateFilesOptions): Promise<UpdateStats> {
    const normalizedRenames = (options.renames ?? []).map(({ from, to }) => ({
      from: normalizeRelativePath(this.rootDir, from),
      to: normalizeRelativePath(this.rootDir, to),
    }));
    validateRenames(normalizedRenames);
    const upsertPaths = [...new Set((options.upsert ?? []).map((value) => normalizeRelativePath(this.rootDir, value)))];
    const deletePaths = [...new Set((options.delete ?? []).map((value) => normalizeRelativePath(this.rootDir, value)))];
    const renameMap = new Map(
      normalizedRenames.map(({ from, to }) => [to, from]),
    );
    for (const target of renameMap.keys()) {
      if (!upsertPaths.includes(target)) upsertPaths.push(target);
    }

    const prepared: PreparedFile[] = [];
    for (const relativePath of upsertPaths) {
      throwIfAborted(options.signal);
      if (!this.#policy.includes(relativePath)) throw new CodeIndexError(`Unsupported or excluded source file: ${relativePath}`);
      const absolutePath = path.join(this.rootDir, relativePath);
      const info = await lstat(absolutePath);
      if (info.isSymbolicLink()) throw new CodeIndexError(`Symbolic links are not supported: ${relativePath}`);
      if (!info.isFile()) throw new CodeIndexError(`Not a regular file: ${relativePath}`);
      if (info.size > this.#maxFileSize) throw new CodeIndexError(`File exceeds maxFileSize: ${relativePath}`);
      const content = await readFile(absolutePath);
      const renameSource = renameMap.get(relativePath);
      const originalPath = renameSource ? this.#database.previousPath(renameSource) ?? renameSource : undefined;
      prepared.push(this.#prepareFile(relativePath, content, {
        blobOid: null,
        sourceMode: "working-tree",
        indexedCommit: null,
        ...(originalPath ? { previousPath: originalPath } : {}),
        ...(renameSource ? { replacePath: renameSource } : {}),
      }));
    }
    await this.#attachEmbeddings(prepared, options.signal);
    return this.#database.applyUpdate({
      files: prepared,
      deletePaths: [...deletePaths, ...renameMap.values()],
    });
  }

  public async updateFromGit(options: UpdateFromGitOptions = {}): Promise<UpdateStats> {
    throwIfAborted(options.signal);
    await this.#git.assertRepository();
    const relativeIndexPath = path.relative(this.rootDir, this.indexPath).replaceAll(path.sep, "/");
    const indexArtifacts = relativeIndexPath && !relativeIndexPath.startsWith("../")
      ? [relativeIndexPath, `${relativeIndexPath}-shm`, `${relativeIndexPath}-wal`, `${relativeIndexPath}-journal`]
      : [];
    await this.#git.assertCleanWorkingTree(indexArtifacts);
    const target = await this.#git.resolveCommit(options.target ?? "HEAD");
    const checkpoint = this.#database.getCheckpoint();
    let rebuild = checkpoint === null;
    if (checkpoint && !(await this.#git.isAncestor(checkpoint, target))) {
      if (!options.rebuildOnDivergence) throw new GitDivergenceError(checkpoint, target);
      rebuild = true;
    }

    const tree = await this.#git.listTree(target);
    const workingFiles = this.#database.getWorkingTreeFiles();
    const changes = rebuild || !checkpoint ? [] : await this.#git.diff(checkpoint, target);
    const upserts = new Map<string, { entry: GitTreeEntry; previousPath?: string }>();
    const deletes = new Set<string>();
    if (rebuild) {
      for (const existing of this.#database.allFilePaths()) deletes.add(existing);
      for (const entry of tree.values()) {
        if (this.#policy.includes(entry.path) && entry.size <= this.#maxFileSize) upserts.set(entry.path, { entry });
      }
    } else {
      this.#classifyGitChanges(changes, tree, upserts, deletes);
      for (const { path: dirtyPath } of workingFiles) {
        const entry = tree.get(dirtyPath);
        if (entry && this.#policy.includes(dirtyPath) && entry.size <= this.#maxFileSize) upserts.set(dirtyPath, { entry });
        else deletes.add(dirtyPath);
      }
      const eligibleTargetPaths = new Set(
        [...tree.values()]
          .filter((entry) => this.#policy.includes(entry.path) && entry.size <= this.#maxFileSize)
          .map((entry) => entry.path),
      );
      const indexedPaths = new Set(this.#database.allFilePaths());
      for (const targetPath of eligibleTargetPaths) {
        if (!indexedPaths.has(targetPath) && !upserts.has(targetPath)) {
          upserts.set(targetPath, { entry: tree.get(targetPath)! });
        }
      }
      for (const indexedPath of indexedPaths) {
        if (!eligibleTargetPaths.has(indexedPath)) deletes.add(indexedPath);
      }
    }

    if (!rebuild && checkpoint === target && upserts.size === 0 && deletes.size === 0) return emptyStats(checkpoint);

    const prepared: PreparedFile[] = [];
    for (const { entry, previousPath } of upserts.values()) {
      throwIfAborted(options.signal);
      const content = await this.#git.readBlob(entry.oid);
      if (content.byteLength > this.#maxFileSize) {
        deletes.add(entry.path);
        continue;
      }
      prepared.push(this.#prepareFile(entry.path, content, {
        blobOid: entry.oid,
        sourceMode: "git",
        indexedCommit: target,
        ...(previousPath ? { replacePath: previousPath } : {}),
      }));
    }
    await this.#attachEmbeddings(prepared, options.signal);
    const resolvedAgain = await this.#git.resolveCommit(options.target ?? "HEAD");
    if (resolvedAgain !== target) throw new CodeIndexError("Target Git ref changed while indexing; retry the update.");
    return this.#database.applyUpdate({
      files: prepared,
      deletePaths: [...deletes],
      checkpoint: target,
      expectedCheckpoint: checkpoint,
    });
  }

  #classifyGitChanges(
    changes: readonly GitChange[],
    tree: ReadonlyMap<string, GitTreeEntry>,
    upserts: Map<string, { entry: GitTreeEntry; previousPath?: string }>,
    deletes: Set<string>,
  ): void {
    for (const change of changes) {
      if (change.status === "D") {
        deletes.add(change.path);
        continue;
      }
      if (change.status === "R") {
        deletes.add(change.oldPath);
        const entry = tree.get(change.path);
        if (entry && this.#policy.includes(change.path) && entry.size <= this.#maxFileSize) {
          upserts.set(change.path, { entry, previousPath: change.oldPath });
        }
        continue;
      }
      const entry = tree.get(change.path);
      if (entry && this.#policy.includes(change.path) && entry.size <= this.#maxFileSize) upserts.set(change.path, { entry });
      else deletes.add(change.path);
    }
  }

  #prepareFile(
    relativePath: string,
    buffer: Buffer,
    provenance: Pick<PreparedFile, "blobOid" | "sourceMode" | "indexedCommit" | "previousPath" | "replacePath">,
  ): PreparedFile {
    const content = buffer.toString("utf8");
    const callables = parseCallables(relativePath, content, this.#onWarning) as PreparedCallable[];
    const language = callables[0]?.language ?? path.extname(relativePath).slice(1);
    return {
      path: relativePath,
      contentHash: sha256(content),
      blobOid: provenance.blobOid,
      sourceMode: provenance.sourceMode,
      indexedCommit: provenance.indexedCommit,
      language,
      byteSize: buffer.byteLength,
      ...(provenance.previousPath ? { previousPath: provenance.previousPath } : {}),
      ...(provenance.replacePath ? { replacePath: provenance.replacePath } : {}),
      callables,
    };
  }

  async #attachEmbeddings(files: PreparedFile[], signal?: AbortSignal): Promise<void> {
    const profile = JSON.stringify(normalizeProfile(this.provider.profile));
    const unique = new Map<string, PreparedCallable[]>();
    for (const file of files) {
      for (const callable of file.callables) {
        callable.embeddingKey = sha256(`${profile}\0${callable.embeddingInput}`);
        const values = unique.get(callable.embeddingKey) ?? [];
        values.push(callable);
        unique.set(callable.embeddingKey, values);
      }
    }
    const existing = this.#database.getEmbeddingKeys([...unique.keys()]);
    const missing = [...unique.entries()].filter(([key]) => !existing.has(key));
    for (const batch of chunk(missing, this.#embeddingBatchSize)) {
      throwIfAborted(signal);
      const vectors = await this.provider.embedDocuments(batch.map(([, callables]) => callables[0]!.embeddingInput), signal ? { signal } : undefined);
      if (vectors.length !== batch.length) throw new CodeIndexError("Embedding provider returned an unexpected number of vectors.");
      vectors.forEach((vector, index) => {
        const converted = normalizeEmbeddingVector(vector, this.provider.profile.dimensions);
        for (const callable of batch[index]![1]) callable.vector = converted;
      });
    }
  }

  public async similaritySearch(options: SimilaritySearchOptions): Promise<SimilarityResult[]> {
    if (!options.query.trim()) throw new CodeIndexError("query must not be empty.");
    const limit = options.limit ?? 10;
    assertPositiveInteger(limit, "limit");
    throwIfAborted(options.signal);
    const vector = await this.provider.embedQuery(options.query, options.signal ? { signal: options.signal } : undefined);
    return this.searchByVector(vector, {
      limit,
      minSimilarity: options.minSimilarity ?? -1,
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
    });
  }

  public similarToFunction(functionId: number, options: {
    limit: number;
    minSimilarity: number;
    maxSimilarity?: number;
  }): SimilarityResult[] {
    const vector = this.#database.vectorForFunction(functionId);
    return this.#database.searchVector(vector, {
      limit: options.limit,
      minSimilarity: options.minSimilarity,
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
      excludeId: functionId,
    });
  }

  public vectorForFunction(functionId: number): number[] {
    return this.#database.vectorForFunction(functionId);
  }

  public searchByVector(vector: readonly number[], options: {
    limit: number;
    minSimilarity: number;
    maxSimilarity?: number;
  }): SimilarityResult[] {
    return this.#database.searchVector(normalizeEmbeddingVector(vector, this.provider.profile.dimensions), options);
  }

  public async sourceFunctions(filter: CrossSearchSourceFilter): Promise<IndexedFunction[]> {
    const functions = filter.type === "all" ? this.allFunctions() : await this.#functionsAddedSince(filter.commit);
    if (!filter.path) return functions;
    const sourcePath = normalizeRelativePath(this.rootDir, filter.path);
    return functions.filter((callable) => callable.path === sourcePath || callable.path.startsWith(`${sourcePath}/`));
  }

  async #functionsAddedSince(baseRef: string): Promise<IndexedFunction[]> {
    await this.#git.assertRepository();
    const checkpoint = this.#database.getCheckpoint();
    if (!checkpoint) throw new CodeIndexError("added-since requires an index with a Git checkpoint.");
    const base = await this.#git.resolveCommit(baseRef);
    if (!(await this.#git.isAncestor(base, checkpoint))) {
      throw new GitDivergenceError(base, checkpoint);
    }

    const changes = await this.#git.diff(base, checkpoint);
    const baseTree = await this.#git.listTree(base);
    const comparisonPaths = new Map<string, string | null>();
    for (const change of changes) {
      if (change.status === "D") continue;
      if (change.status === "R") comparisonPaths.set(change.path, change.oldPath);
      else if (change.status === "A" || change.status === "C") comparisonPaths.set(change.path, null);
      else comparisonPaths.set(change.path, change.path);
    }
    for (const dirtyFile of this.#database.getWorkingTreeFiles()) {
      if (!comparisonPaths.has(dirtyFile.path)) {
        comparisonPaths.set(dirtyFile.path, dirtyFile.previousPath ?? dirtyFile.path);
      }
    }

    const current = this.#database.functionsForPaths([...comparisonPaths.keys()]);
    const baseFunctionsByCurrentPath = new Map<string, ReturnType<typeof parseCallables>>();
    for (const [currentPath, basePath] of comparisonPaths) {
      if (!basePath) {
        baseFunctionsByCurrentPath.set(currentPath, []);
        continue;
      }
      const entry = baseTree.get(basePath);
      if (!entry) {
        baseFunctionsByCurrentPath.set(currentPath, []);
        continue;
      }
      const content = (await this.#git.readBlob(entry.oid)).toString("utf8");
      baseFunctionsByCurrentPath.set(currentPath, parseCallables(basePath, content, this.#onWarning));
    }
    const currentByPath = groupBy(current, (callable) => callable.path);
    const addedIds = new Set<number>();
    for (const [currentPath, currentFunctions] of currentByPath) {
      for (const id of addedFunctionIds(currentFunctions, baseFunctionsByCurrentPath.get(currentPath) ?? [])) {
        addedIds.add(id);
      }
    }
    return current.filter((callable) => addedIds.has(callable.id));
  }
}

function addedFunctionIds(
  current: readonly IndexedFunction[],
  base: readonly Pick<IndexedFunction, "qualifiedName" | "kind" | "sourceHash">[],
): number[] {
  const currentGroups = groupBy(current, (callable) => `${callable.qualifiedName}\0${callable.kind}`);
  const baseGroups = groupBy(base, (callable) => `${callable.qualifiedName}\0${callable.kind}`);
  const added: number[] = [];
  for (const [key, currentGroup] of currentGroups) {
    const baseGroup = baseGroups.get(key) ?? [];
    const unmatchedBase = new Set(baseGroup.map((_, index) => index));
    const unmatchedCurrent = new Set(currentGroup.map((_, index) => index));
    for (let currentIndex = 0; currentIndex < currentGroup.length; currentIndex += 1) {
      const exactBaseIndex = [...unmatchedBase].find(
        (baseIndex) => baseGroup[baseIndex]!.sourceHash === currentGroup[currentIndex]!.sourceHash,
      );
      if (exactBaseIndex !== undefined) {
        unmatchedBase.delete(exactBaseIndex);
        unmatchedCurrent.delete(currentIndex);
      }
    }
    for (const currentIndex of unmatchedCurrent) {
      const baseIndex = unmatchedBase.values().next().value as number | undefined;
      if (baseIndex === undefined) added.push(currentGroup[currentIndex]!.id);
      else unmatchedBase.delete(baseIndex);
    }
  }
  return added;
}

function normalizeProfile(profile: EmbeddingProfile): Required<EmbeddingProfile> {
  return {
    provider: profile.provider,
    model: profile.model,
    dimensions: profile.dimensions,
    strategyVersion: profile.strategyVersion ?? "callable-v1",
  };
}

function emptyStats(checkpoint: string | null): UpdateStats {
  return {
    filesUpdated: 0,
    filesDeleted: 0,
    functionsAdded: 0,
    functionsUpdated: 0,
    functionsDeleted: 0,
    embeddingsCreated: 0,
    checkpoint,
  };
}

function validateRenames(renames: readonly { from: string; to: string }[]): void {
  const sources = new Set(renames.map((rename) => rename.from));
  const targets = new Set(renames.map((rename) => rename.to));
  if (sources.size !== renames.length || targets.size !== renames.length) {
    throw new CodeIndexError("Rename sources and targets must be unique.");
  }
  if ([...sources].some((source) => targets.has(source))) {
    throw new CodeIndexError("Chained or cyclic renames must be applied in separate updates.");
  }
}

function groupBy<T>(values: readonly T[], keyFor: (value: T) => string): Map<string, T[]> {
  const groups = new Map<string, T[]>();
  for (const value of values) {
    const key = keyFor(value);
    const group = groups.get(key) ?? [];
    group.push(value);
    groups.set(key, group);
  }
  return groups;
}
