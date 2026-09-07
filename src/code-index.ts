import { lstat, readFile, readdir } from "node:fs/promises";
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
  UpdateFromWorkingTreeOptions,
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

  public async updateFromWorkingTree(options: UpdateFromWorkingTreeOptions = {}): Promise<UpdateStats> {
    throwIfAborted(options.signal);
    const generation = this.#database.getGeneration();
    const indexedFiles = this.#database.getFileStates();
    const sourcePaths = await this.#workingTreeSourcePaths(options.signal);
    const prepared: PreparedFile[] = [];
    const skippedPaths = new Set<string>();
    for (const relativePath of sourcePaths) {
      throwIfAborted(options.signal);
      const absolutePath = path.join(this.rootDir, relativePath);
      const info = await lstat(absolutePath);
      if (!info.isFile() || info.isSymbolicLink() || info.size > this.#maxFileSize) {
        skippedPaths.add(relativePath);
        continue;
      }
      const content = await readFile(absolutePath);
      prepared.push(this.#prepareFile(relativePath, content, {
        blobOid: null,
        sourceMode: "working-tree",
        indexedCommit: null,
      }));
    }
    await this.#attachEmbeddings(prepared, options.signal);

    const pathsAgain = await this.#workingTreeSourcePaths(options.signal);
    if (JSON.stringify(pathsAgain) !== JSON.stringify(sourcePaths)) {
      throw new CodeIndexError("Working-tree files changed while indexing; retry the update.");
    }
    for (const file of prepared) {
      try {
        const info = await lstat(path.join(this.rootDir, file.path));
        const content = await readFile(path.join(this.rootDir, file.path));
        if (!info.isFile() || info.isSymbolicLink() || info.size > this.#maxFileSize || sha256(content.toString("utf8")) !== file.contentHash) {
          throw new CodeIndexError(`Working-tree file changed while indexing: ${file.path}`);
        }
      } catch (error) {
        if (error instanceof CodeIndexError) throw error;
        throw new CodeIndexError(`Working-tree file changed while indexing: ${file.path}`, { cause: error });
      }
    }
    for (const relativePath of skippedPaths) {
      const info = await lstat(path.join(this.rootDir, relativePath));
      if (info.isFile() && !info.isSymbolicLink() && info.size <= this.#maxFileSize) {
        throw new CodeIndexError(`Working-tree file changed while indexing: ${relativePath}`);
      }
    }

    return this.#database.applyUpdate({
      files: prepared,
      deletePaths: indexedFiles.map((file) => file.path),
      checkpoint: null,
      expectedGeneration: generation,
    });
  }

  public async updateFromGit(options: UpdateFromGitOptions = {}): Promise<UpdateStats> {
    throwIfAborted(options.signal);
    await this.#git.assertRepository();
    const relativeIndexPath = path.relative(this.rootDir, this.indexPath).replaceAll(path.sep, "/");
    const indexArtifacts = relativeIndexPath && !relativeIndexPath.startsWith("../")
      ? [relativeIndexPath, `${relativeIndexPath}-shm`, `${relativeIndexPath}-wal`, `${relativeIndexPath}-journal`]
      : [];
    const target = await this.#git.resolveCommit(options.target ?? "HEAD");
    const head = await this.#git.resolveCommit("HEAD");
    const overlayWorkingTree = target === head && options.includeWorkingTree !== false;
    const checkpoint = this.#database.getCheckpoint();
    const generation = this.#database.getGeneration();
    let reconcileAll = checkpoint === null;
    if (checkpoint && !(await this.#git.isAncestor(checkpoint, target))) {
      if (!options.rebuildOnDivergence) throw new GitDivergenceError(checkpoint, target);
      reconcileAll = true;
    }

    const tree = await this.#git.listTree(target);
    const indexedFiles = this.#database.getFileStates();
    const indexedByPath = new Map(indexedFiles.map((file) => [file.path, file]));
    const workingFiles = indexedFiles.filter((file) => file.sourceMode === "working-tree");
    const workingChanges = overlayWorkingTree ? await this.#git.workTreeChanges(target, indexArtifacts) : [];
    const visibleWorkingPaths = new Set(
      workingChanges.filter((change) => change.status !== "D").map((change) => change.path),
    );
    const changes = reconcileAll || !checkpoint ? [] : await this.#git.diff(checkpoint, target);
    const upserts = new Map<string, { entry: GitTreeEntry; previousPath?: string }>();
    const deletes = new Set<string>();
    const eligibleEntries = [...tree.values()]
      .filter((entry) => this.#policy.includes(entry.path) && entry.size <= this.#maxFileSize);
    const eligibleTargetPaths = new Set(eligibleEntries.map((entry) => entry.path));
    if (reconcileAll) {
      const renameCandidates = indexedFiles.filter((file) => (
        file.sourceMode === "git" && file.blobOid && !eligibleTargetPaths.has(file.path)
      ));
      const claimedRenameSources = new Set<string>();
      for (const entry of eligibleEntries) {
        const indexed = indexedByPath.get(entry.path);
        if (indexed?.sourceMode === "git" && indexed.blobOid === entry.oid) continue;
        const matches = renameCandidates.filter((candidate) => (
          candidate.blobOid === entry.oid && !claimedRenameSources.has(candidate.path)
        ));
        const previousPath = matches.length === 1 ? matches[0]!.path : undefined;
        if (previousPath) claimedRenameSources.add(previousPath);
        upserts.set(entry.path, { entry, ...(previousPath ? { previousPath } : {}) });
      }
      for (const indexed of indexedFiles) {
        if (!eligibleTargetPaths.has(indexed.path)) deletes.add(indexed.path);
      }
    } else {
      this.#classifyGitChanges(changes, tree, upserts, deletes);
      const indexedPaths = new Set(indexedByPath.keys());
      for (const targetPath of eligibleTargetPaths) {
        const indexed = indexedByPath.get(targetPath);
        const entry = tree.get(targetPath)!;
        if (
          (!indexed || (indexed.sourceMode === "git" && indexed.blobOid !== entry.oid))
          && !upserts.has(targetPath)
        ) {
          upserts.set(targetPath, { entry });
        }
      }
      for (const indexedPath of indexedPaths) {
        if (!eligibleTargetPaths.has(indexedPath)) deletes.add(indexedPath);
      }
    }
    for (const dirtyFile of workingFiles) {
      const entry = tree.get(dirtyFile.path);
      const previousEntry = dirtyFile.previousPath ? tree.get(dirtyFile.previousPath) : undefined;
      if (
        previousEntry
        && this.#policy.includes(previousEntry.path)
        && previousEntry.size <= this.#maxFileSize
        && (entry !== undefined || !visibleWorkingPaths.has(dirtyFile.path))
      ) {
        const currentPlan = entry ? upserts.get(dirtyFile.path) : undefined;
        upserts.delete(dirtyFile.path);
        upserts.delete(previousEntry.path);
        upserts.set(previousEntry.path, { entry: previousEntry, previousPath: dirtyFile.path });
        if (entry && this.#policy.includes(entry.path) && entry.size <= this.#maxFileSize) {
          upserts.set(dirtyFile.path, currentPlan ?? { entry });
        }
        continue;
      }
      if (entry && this.#policy.includes(dirtyFile.path) && entry.size <= this.#maxFileSize) {
        const planned = upserts.get(dirtyFile.path);
        if (planned?.previousPath && !indexedByPath.has(planned.previousPath)) {
          upserts.set(dirtyFile.path, { entry, previousPath: dirtyFile.path });
        } else if (!planned) {
          upserts.set(dirtyFile.path, { entry });
        }
        continue;
      }
      if (previousEntry && this.#policy.includes(previousEntry.path) && previousEntry.size <= this.#maxFileSize) {
        upserts.set(previousEntry.path, { entry: previousEntry, previousPath: dirtyFile.path });
      } else {
        deletes.add(dirtyFile.path);
      }
    }
    for (const [filePath, { entry, previousPath }] of upserts) {
      const indexed = indexedByPath.get(filePath);
      if (!previousPath && indexed?.sourceMode === "git" && indexed.blobOid === entry.oid) upserts.delete(filePath);
    }

    const workingPrepared: PreparedFile[] = [];
    const workingDeletes = new Set<string>();
    const skippedWorkingPaths = new Set<string>();
    for (const change of workingChanges) {
      throwIfAborted(options.signal);
      if (change.status === "D") {
        upserts.delete(change.path);
        workingDeletes.add(change.path);
        continue;
      }

      let previousPath: string | undefined;
      let replacePath: string | undefined;
      if (change.status === "R") {
        const plannedBase = upserts.get(change.oldPath);
        const existingAtTarget = indexedByPath.get(change.oldPath);
        const existingAtFinal = indexedByPath.get(change.path);
        const continuingRename = existingAtFinal?.sourceMode === "working-tree"
          && existingAtFinal.previousPath === change.oldPath;
        replacePath = continuingRename
          ? change.path
          : existingAtTarget
            ? change.oldPath
            : existingAtFinal
              ? change.path
              : plannedBase?.previousPath && indexedByPath.has(plannedBase.previousPath)
                ? plannedBase.previousPath
                : undefined;
        previousPath = existingAtFinal?.previousPath ?? change.oldPath;
        upserts.delete(change.oldPath);
        upserts.delete(change.path);
        if (replacePath) deletes.delete(replacePath);
        workingDeletes.add(change.oldPath);
      } else {
        const planned = upserts.get(change.path);
        const existing = indexedByPath.get(change.path);
        replacePath = existing
          ? change.path
          : planned?.previousPath && indexedByPath.has(planned.previousPath)
            ? planned.previousPath
            : undefined;
        previousPath = existing?.previousPath ?? planned?.previousPath;
        upserts.delete(change.path);
        if (replacePath) deletes.delete(replacePath);
      }
      const relativePath = change.path;
      if (!this.#policy.includes(relativePath)) {
        if (replacePath) workingDeletes.add(replacePath);
        workingDeletes.add(relativePath);
        continue;
      }
      const absolutePath = path.join(this.rootDir, relativePath);
      const info = await lstat(absolutePath);
      if (!info.isFile() || info.isSymbolicLink() || info.size > this.#maxFileSize) {
        if (replacePath) workingDeletes.add(replacePath);
        workingDeletes.add(relativePath);
        skippedWorkingPaths.add(relativePath);
        continue;
      }
      const content = await readFile(absolutePath);
      if (!replacePath && change.status === "A") {
        const contentHash = sha256(content.toString("utf8"));
        const renameCandidates = workingFiles.filter((file) => (
          file.path !== relativePath
          && !visibleWorkingPaths.has(file.path)
          && deletes.has(file.path)
          && file.contentHash === contentHash
        ));
        const renamed = renameCandidates.length === 1 ? renameCandidates[0] : undefined;
        if (renamed) {
          replacePath = renamed.path;
          previousPath = renamed.previousPath ?? renamed.path;
          deletes.delete(renamed.path);
        }
      }
      workingPrepared.push(this.#prepareFile(relativePath, content, {
        blobOid: null,
        sourceMode: "working-tree",
        indexedCommit: null,
        ...(previousPath ? { previousPath } : {}),
        ...(replacePath ? { replacePath } : {}),
      }));
    }

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
    await this.#attachEmbeddings([...prepared, ...workingPrepared], options.signal);
    const noChanges = (
      !reconcileAll
      && checkpoint === target
      && prepared.length === 0
      && deletes.size === 0
      && workingPrepared.length === 0
      && workingDeletes.size === 0
    );

    const resolvedAgain = await this.#git.resolveCommit(options.target ?? "HEAD");
    if (resolvedAgain !== target) throw new CodeIndexError("Target Git ref changed while indexing; retry the update.");
    if (overlayWorkingTree) {
      const headAgain = await this.#git.resolveCommit("HEAD");
      const workingChangesAgain = await this.#git.workTreeChanges(target, indexArtifacts);
      if (headAgain !== head || JSON.stringify(workingChangesAgain) !== JSON.stringify(workingChanges)) {
        throw new CodeIndexError("Git HEAD or working-tree changes changed while indexing; retry the update.");
      }
      for (const file of workingPrepared) {
        const info = await lstat(path.join(this.rootDir, file.path));
        const content = await readFile(path.join(this.rootDir, file.path));
        if (!info.isFile() || info.isSymbolicLink() || info.size > this.#maxFileSize || sha256(content.toString("utf8")) !== file.contentHash) {
          throw new CodeIndexError(`Working-tree file changed while indexing: ${file.path}`);
        }
      }
      for (const relativePath of skippedWorkingPaths) {
        const info = await lstat(path.join(this.rootDir, relativePath));
        if (info.isFile() && !info.isSymbolicLink() && info.size <= this.#maxFileSize) {
          throw new CodeIndexError(`Working-tree file changed while indexing: ${relativePath}`);
        }
      }
    }
    if (noChanges) {
      if (this.#database.getCheckpoint() !== checkpoint || this.#database.getGeneration() !== generation) {
        throw new CodeIndexError("Index changed while the update was being prepared; retry the update.");
      }
      return emptyStats(checkpoint);
    }
    return this.#database.applyUpdate({
      files: prepared,
      deletePaths: [...deletes],
      workingTree: {
        files: workingPrepared,
        deletePaths: [...workingDeletes],
      },
      checkpoint: target,
      expectedCheckpoint: checkpoint,
      expectedGeneration: generation,
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

  async #workingTreeSourcePaths(signal?: AbortSignal): Promise<string[]> {
    const relativeIndexPath = path.relative(this.rootDir, this.indexPath).replaceAll(path.sep, "/");
    const indexArtifacts = new Set(relativeIndexPath && !relativeIndexPath.startsWith("../")
      ? [relativeIndexPath, `${relativeIndexPath}-shm`, `${relativeIndexPath}-wal`, `${relativeIndexPath}-journal`]
      : []);
    const directories = [""];
    const sourcePaths: string[] = [];
    while (directories.length > 0) {
      throwIfAborted(signal);
      const relativeDirectory = directories.shift()!;
      const entries = await readdir(path.join(this.rootDir, relativeDirectory), { withFileTypes: true });
      entries.sort((left, right) => left.name.localeCompare(right.name));
      for (const entry of entries) {
        const relativePath = relativeDirectory
          ? `${relativeDirectory.replaceAll(path.sep, "/")}/${entry.name}`
          : entry.name;
        if (entry.isDirectory()) {
          if (this.#policy.traversesDirectory(relativePath)) directories.push(relativePath);
        } else if (entry.isFile() && !indexArtifacts.has(relativePath) && this.#policy.includes(relativePath)) {
          sourcePaths.push(relativePath);
        }
      }
    }
    sourcePaths.sort();
    return sourcePaths;
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
    excludePaths?: readonly string[];
    minLines?: number;
  }): SimilarityResult[] {
    const vector = this.#database.vectorForFunction(functionId);
    return this.#database.searchVector(vector, {
      limit: options.limit,
      minSimilarity: options.minSimilarity,
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
      excludeId: functionId,
      ...(options.excludePaths !== undefined ? { excludePaths: options.excludePaths } : {}),
      ...(options.minLines !== undefined ? { minLines: options.minLines } : {}),
    });
  }

  public vectorForFunction(functionId: number): number[] {
    return this.#database.vectorForFunction(functionId);
  }

  public searchByVector(vector: readonly number[], options: {
    limit: number;
    minSimilarity: number;
    maxSimilarity?: number;
    excludePaths?: readonly string[];
    minLines?: number;
  }): SimilarityResult[] {
    return this.#database.searchVector(normalizeEmbeddingVector(vector, this.provider.profile.dimensions), options);
  }

  public async sourceFunctions(filter: CrossSearchSourceFilter): Promise<IndexedFunction[]> {
    const functions = filter.type === "all"
      ? this.allFunctions()
      : filter.type === "uncommitted"
        ? this.allFunctions().filter((callable) => callable.sourceMode === "working-tree")
        : await this.#functionsChangedSince(filter.commit);
    if (!filter.path) return functions;
    const sourcePath = normalizeRelativePath(this.rootDir, filter.path);
    return functions.filter((callable) => callable.path === sourcePath || callable.path.startsWith(`${sourcePath}/`));
  }

  async #functionsChangedSince(baseRef: string): Promise<IndexedFunction[]> {
    await this.#git.assertRepository();
    const checkpoint = this.#database.getCheckpoint();
    if (!checkpoint) throw new CodeIndexError("changed-since requires an index with a Git checkpoint.");
    const base = await this.#git.resolveCommit(baseRef);
    if (!(await this.#git.isAncestor(base, checkpoint))) {
      throw new GitDivergenceError(base, checkpoint);
    }

    const changes = await this.#git.diff(base, checkpoint);
    const baseTree = await this.#git.listTree(base);
    const comparisonPaths = new Map<string, { basePath: string | null; pathChanged: boolean }>();
    for (const change of changes) {
      if (change.status === "D") continue;
      if (change.status === "R") comparisonPaths.set(change.path, { basePath: change.oldPath, pathChanged: true });
      else if (change.status === "A" || change.status === "C") comparisonPaths.set(change.path, { basePath: null, pathChanged: false });
      else comparisonPaths.set(change.path, { basePath: change.path, pathChanged: false });
    }
    for (const dirtyFile of this.#database.getWorkingTreeFiles()) {
      const basePath = dirtyFile.previousPath ?? comparisonPaths.get(dirtyFile.path)?.basePath ?? dirtyFile.path;
      comparisonPaths.set(dirtyFile.path, { basePath, pathChanged: basePath !== dirtyFile.path });
    }

    const current = this.#database.functionsForPaths([...comparisonPaths.keys()]);
    const baseFunctionsByCurrentPath = new Map<string, ReturnType<typeof parseCallables>>();
    for (const [currentPath, { basePath }] of comparisonPaths) {
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
    const changedIds = new Set<number>();
    for (const [currentPath, currentFunctions] of currentByPath) {
      const comparison = comparisonPaths.get(currentPath)!;
      for (const id of changedFunctionIds(
        currentFunctions,
        baseFunctionsByCurrentPath.get(currentPath) ?? [],
        comparison.pathChanged,
      )) {
        changedIds.add(id);
      }
    }
    return current.filter((callable) => changedIds.has(callable.id));
  }
}

function changedFunctionIds(
  current: readonly IndexedFunction[],
  base: readonly Pick<IndexedFunction, "qualifiedName" | "kind" | "sourceHash">[],
  pathChanged: boolean,
): number[] {
  if (pathChanged) return current.map((callable) => callable.id);
  const currentGroups = groupBy(current, (callable) => `${callable.qualifiedName}\0${callable.kind}`);
  const baseGroups = groupBy(base, (callable) => `${callable.qualifiedName}\0${callable.kind}`);
  const changed: number[] = [];
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
    for (const currentIndex of unmatchedCurrent) changed.push(currentGroup[currentIndex]!.id);
  }
  return changed;
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
