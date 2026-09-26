import { lstat, readFile, readdir } from "node:fs/promises";
import { lstatSync, readFileSync } from "node:fs";
import path from "node:path";

import { CodeIndexError, GitDivergenceError } from "./errors.js";
import { GitRepository, type GitChange, type GitTreeEntry } from "./git/repository.js";
import { GitignoreRules } from "./gitignore.js";
import { CALLABLE_PARSER_CACHE_VERSION, languageForPath, parseCallables, parseFileCallables } from "./parser/callable-parser.js";
import { chunkMarkdown, isMarkdownPath } from "./parser/markdown.js";
import {
  SimilarityCache,
  type RefreshSimilarityCacheOptions,
  type RefreshSimilarityCacheResult,
  type SimilarityCacheInfo,
  type SimilarityCacheQuery,
  type SimilarityCacheReader,
  type SimilarityCacheReaderOptions,
} from "./search/similarity-cache.js";
import { SourcePolicy } from "./source-policy.js";
import { IndexDatabase, type IndexedFileState, type PreparedCallable, type PreparedDescription, type PreparedFile, type PreparedMarkdownChunk } from "./storage/database.js";
import { isDescriptionProviderName, OpenAIDescriptionProvider } from "./descriptions/openai.js";
import type {
  CodeIndexOptions,
  CrossSearchSourceFilter,
  DescribeContext,
  DescribeFile,
  DescribeFunction,
  DescribeOptions,
  EmbeddingProfile,
  IndexProgress,
  IndexProgressPhase,
  IndexStatus,
  IndexedFunction,
  IndexingError,
  MarkdownChunk,
  MarkdownSearchOptions,
  MarkdownSearchResult,
  SearchOptions,
  SearchResult,
  SimilarityResult,
  SimilaritySearchOptions,
  DescriptionProvider,
  DescriptionStats,
  ReindexFilesOptions,
  ReindexFilesStats,
  UpdateFilesOptions,
  UpdateFromGitOptions,
  UpdateFromWorkingTreeOptions,
  UpdateStats,
} from "./types.js";
import {
  assertPositiveInteger,
  chunk,
  compileNameRegex,
  DEFAULT_PARALLELISM,
  forEachConcurrent,
  normalizeEmbeddingVector,
  normalizeRelativePath,
  sha256,
  throwIfAborted,
} from "./utils.js";

const DEFAULT_MAX_FILE_SIZE = 1024 * 1024;
const DEFAULT_BATCH_SIZE = 32;
const RERANK_CANDIDATE_MULTIPLIER = 5;

export class CodeIndex {
  public readonly rootDir: string;
  public readonly indexPath: string;
  public readonly provider;
  public readonly reranker;
  public readonly descriptionProvider: DescriptionProvider;
  readonly #database: IndexDatabase;
  readonly #similarityCache: SimilarityCache;
  readonly #policy: SourcePolicy;
  readonly #maxFileSize: number;
  readonly #embeddingBatchSize: number;
  readonly #parallelism: number;
  readonly #onProgress: ((progress: IndexProgress) => void) | undefined;
  readonly #git: GitRepository;
  readonly #onWarning: (message: string) => void;

  public constructor(options: CodeIndexOptions) {
    this.rootDir = path.resolve(options.rootDir);
    this.indexPath = path.resolve(options.indexPath ?? path.join(this.rootDir, ".slopdex", "index.sqlite"));
    this.provider = options.provider;
    this.reranker = options.reranker;
    if (this.reranker?.candidateCount !== undefined) {
      assertPositiveInteger(this.reranker.candidateCount, "reranker candidate count");
    }
    if (this.reranker?.maximumCandidateCount !== undefined) {
      assertPositiveInteger(this.reranker.maximumCandidateCount, "reranker maximum candidate count");
      if (this.reranker.candidateCount !== undefined && this.reranker.candidateCount > this.reranker.maximumCandidateCount) {
        throw new CodeIndexError("reranker candidate count must not exceed its maximum candidate count.");
      }
    }
    const profile = normalizeProfile(options.provider.profile);
    assertPositiveInteger(profile.dimensions, "embedding dimensions");
    this.#database = new IndexDatabase(this.indexPath, this.rootDir, profile, options.readOnly ?? false);
    this.#parallelism = options.parallelism ?? DEFAULT_PARALLELISM;
    assertPositiveInteger(this.#parallelism, "parallelism");
    this.#onProgress = options.onProgress;
    this.#similarityCache = new SimilarityCache(this.#database, this, this.#onProgress);
    const storedDescriptionProfile = this.#database.descriptionProfile();
    this.descriptionProvider = options.descriptionProvider ?? new OpenAIDescriptionProvider({
      ...(storedDescriptionProfile && isDescriptionProviderName(storedDescriptionProfile.provider)
        ? { provider: storedDescriptionProfile.provider, model: storedDescriptionProfile.model }
        : {}),
      parallelism: this.#parallelism,
      ...(options.verbose ? { verbose: true } : {}),
    });
    this.#policy = new SourcePolicy(options.include, options.exclude);
    this.#maxFileSize = options.maxFileSize ?? DEFAULT_MAX_FILE_SIZE;
    this.#embeddingBatchSize = options.embeddingBatchSize ?? DEFAULT_BATCH_SIZE;
    this.#onWarning = options.onWarning ?? console.warn;
    assertPositiveInteger(this.#maxFileSize, "maxFileSize");
    assertPositiveInteger(this.#embeddingBatchSize, "embeddingBatchSize");
    this.#git = new GitRepository(this.rootDir);
  }

  #progress(phase: IndexProgressPhase, completed: number, total: number): void {
    this.#onProgress?.({ phase, completed, total });
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

  public allMarkdownChunks(): MarkdownChunk[] {
    return this.#database.allMarkdownChunks();
  }

  public indexErrors(): IndexingError[] {
    return this.#database.indexErrors();
  }

  public async updateFiles(options: UpdateFilesOptions): Promise<UpdateStats> {
    throwIfAborted(options.signal);
    const gitignore = GitignoreRules.workingTree(this.rootDir);
    const generation = this.#database.getGeneration();
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
      if (!this.#policy.includes(relativePath) || await gitignore.ignores(relativePath)) {
        throw new CodeIndexError(`Unsupported or excluded source file: ${relativePath}`);
      }
      const renameSource = renameMap.get(relativePath);
      const originalPath = renameSource ? this.#database.previousPath(renameSource) ?? renameSource : undefined;
      const file = await this.#prepareWorkingFile(relativePath, {
        blobOid: null,
        sourceMode: "working-tree",
        indexedCommit: null,
        ...(originalPath ? { previousPath: originalPath } : {}),
        ...(renameSource ? { replacePath: renameSource } : {}),
      }, true);
      if (file) prepared.push(file);
    }
    let embeddingsCreated = await this.#attachEmbeddings(prepared, options.signal);
    for (let attempt = 0; ; attempt += 1) {
      await gitignore.assertUnchanged(options.signal);
      const changed: PreparedFile[] = [];
      for (let index = 0; index < prepared.length; index += 1) {
        const file = prepared[index]!;
        if (!this.#workingFileChangedSynchronously(file)) continue;
        if (attempt >= 2) throw new CodeIndexError(`Source changed repeatedly while indexing: ${file.path}; retry the update.`);
        const refreshed = await this.#prepareWorkingFile(file.path, {
          blobOid: null,
          sourceMode: "working-tree",
          indexedCommit: null,
          ...(file.previousPath ? { previousPath: file.previousPath } : {}),
          ...(file.replacePath ? { replacePath: file.replacePath } : {}),
        }, true);
        if (!refreshed) throw new CodeIndexError(`Source changed while indexing: ${file.path}; retry the update.`);
        prepared[index] = refreshed;
        changed.push(refreshed);
      }
      if (changed.length === 0) break;
      embeddingsCreated += await this.#attachEmbeddings(changed, options.signal);
    }
    return this.#database.applyUpdate({
      files: prepared,
      deletePaths: [...deletePaths, ...renameMap.values()],
      expectedGeneration: generation,
      embeddingsCreated,
    });
  }

  public async updateFromWorkingTree(options: UpdateFromWorkingTreeOptions = {}): Promise<UpdateStats> {
    throwIfAborted(options.signal);
    const generation = this.#database.getGeneration();
    const indexedFiles = this.#database.getFileStates();
    const gitignore = GitignoreRules.workingTree(this.rootDir);
    let sourcePaths = await this.#workingTreeSourcePaths(gitignore, options.signal);
    let prepared = await this.#prepareWorkingPaths(sourcePaths, options.signal);
    let embeddingsCreated = await this.#attachEmbeddings(prepared, options.signal);

    while (true) {
      await gitignore.assertUnchanged(options.signal);
      const pathsAgain = await this.#workingTreeSourcePaths(gitignore, options.signal);
      if (JSON.stringify(pathsAgain) !== JSON.stringify(sourcePaths)) {
        sourcePaths = pathsAgain;
        prepared = await this.#prepareWorkingPaths(sourcePaths, options.signal);
        embeddingsCreated += await this.#attachEmbeddings(prepared, options.signal);
        continue;
      }
      const changed: PreparedFile[] = [];
      for (let index = 0; index < prepared.length; index += 1) {
        const file = prepared[index]!;
        if (!await this.#workingFileChanged(file)) continue;
        const refreshed = await this.#prepareWorkingFile(file.path, {
          blobOid: null,
          sourceMode: "working-tree",
          indexedCommit: null,
        });
        if (!refreshed) {
          sourcePaths = await this.#workingTreeSourcePaths(gitignore, options.signal);
          prepared = await this.#prepareWorkingPaths(sourcePaths, options.signal);
          changed.push(...prepared);
          break;
        }
        prepared[index] = refreshed;
        changed.push(refreshed);
      }
      if (changed.length === 0) break;
      embeddingsCreated += await this.#attachEmbeddings(changed, options.signal);
    }

    await gitignore.assertUnchanged(options.signal);
    if (this.#database.getGeneration() !== generation) {
      throw new CodeIndexError("Index changed while the update was being prepared; retry the update.");
    }
    if (this.#isWorkingTreeClean(indexedFiles, prepared)) {
      return emptyStats(this.#database.getCheckpoint());
    }
    return this.#database.applyUpdate({
      files: prepared,
      deletePaths: indexedFiles.map((file) => file.path),
      checkpoint: null,
      expectedGeneration: generation,
      completeDiagnosticsScan: true,
      embeddingsCreated,
    });
  }

  /**
   * Whether a working-tree refresh changed nothing: same file set, same
   * content and prepared rows, with no checkout or pending scans to clear.
   * Compare effective descriptions and diagnostics too: identical source
   * alone does not imply identical indexing results or size eligibility.
   */
  #isWorkingTreeClean(indexedFiles: IndexedFileState[], prepared: PreparedFile[]): boolean {
    if (this.#database.getCheckpoint() !== null) return false;
    if (this.#database.needsDiagnosticsScan() || this.#database.needsMarkdownScan()) return false;
    if (indexedFiles.length !== prepared.length) return false;
    const indexedByPath = new Map(indexedFiles.map((file) => [file.path, file]));
    return prepared.every((file) => {
      const indexed = indexedByPath.get(file.path);
      return !!indexed
        && indexed.sourceMode === "working-tree"
        && indexed.blobOid === file.blobOid
        && indexed.language === file.language
        && indexed.contentHash === file.contentHash
        && this.#database.matchesPreparedFile(file);
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
    // Only file-too-large state transitions (e.g. maxFileSize config change)
    // re-evaluate an unchanged blob; all other recorded errors never force a
    // retry on their own.
    const tooLargePaths = new Set(this.#database.filesWithFileTooLargeErrors());
    let reconcileAll = checkpoint === null || this.#database.needsMarkdownScan();
    if (checkpoint && !(await this.#git.isAncestor(checkpoint, target))) {
      if (!options.rebuildOnDivergence) throw new GitDivergenceError(checkpoint, target);
      reconcileAll = true;
    }

    const tree = await this.#git.listTree(target);
    const indexedFiles = this.#database.getFileStates();
    const indexedByPath = new Map(indexedFiles.map((file) => [file.path, file]));
    const workingFiles = indexedFiles.filter((file) => file.sourceMode === "working-tree");
    const workingChanges = overlayWorkingTree ? await this.#git.workTreeChanges(target, indexArtifacts) : [];
    const gitignore = overlayWorkingTree ? GitignoreRules.workingTree(this.rootDir) : new GitignoreRules(async (filePath) => {
      const entry = tree.get(filePath);
      return entry ? (await this.#git.readBlob(entry.oid)).toString("utf8") : null;
    });
    const allowedPaths = new Set<string>();
    const candidatePaths = new Set([
      ...tree.keys(), ...workingChanges.map((change) => change.path), ...indexedFiles.map((file) => file.path),
    ]);
    for (const filePath of candidatePaths) {
      throwIfAborted(options.signal);
      if (this.#policy.includes(filePath) && !await gitignore.ignores(filePath)) allowedPaths.add(filePath);
    }
    const visibleWorkingPaths = new Set(
      workingChanges.filter((change) => change.status !== "D").map((change) => change.path),
    );
    const changes = reconcileAll || !checkpoint ? [] : await this.#git.diff(checkpoint, target);
    const upserts = new Map<string, { entry: GitTreeEntry; previousPath?: string }>();
    const deletes = new Set<string>();
    const eligibleEntries = [...tree.values()]
      .filter((entry) => allowedPaths.has(entry.path));
    const eligibleTargetPaths = new Set(eligibleEntries.map((entry) => entry.path));
    if (reconcileAll) {
      const renameCandidates = indexedFiles.filter((file) => (
        file.sourceMode === "git" && file.blobOid && !eligibleTargetPaths.has(file.path)
      ));
      const claimedRenameSources = new Set<string>();
      for (const entry of eligibleEntries) {
        const indexed = indexedByPath.get(entry.path);
        // Recorded indexing errors never force a retry on their own; an
        // unchanged blob is skipped unless its file-too-large state flipped
        // (e.g. maxFileSize config change). Other errors are retried only
        // when the blob changes or via an explicit updateFiles upsert.
        if (indexed?.sourceMode === "git" && indexed.blobOid === entry.oid
          && (entry.size > this.#maxFileSize) === tooLargePaths.has(entry.path)) continue;
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
      this.#classifyGitChanges(changes, tree, upserts, deletes, allowedPaths);
      const indexedPaths = new Set(indexedByPath.keys());
      for (const targetPath of eligibleTargetPaths) {
        const indexed = indexedByPath.get(targetPath);
        const entry = tree.get(targetPath)!;
        const sizeStateFlipped = indexed?.sourceMode === "git" && indexed.blobOid === entry.oid
          && (entry.size > this.#maxFileSize) !== tooLargePaths.has(targetPath);
        if (
          (!indexed || (indexed.sourceMode === "git" && indexed.blobOid !== entry.oid) || sizeStateFlipped)
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
        && allowedPaths.has(previousEntry.path)
        && (entry !== undefined || !visibleWorkingPaths.has(dirtyFile.path))
      ) {
        const currentPlan = entry ? upserts.get(dirtyFile.path) : undefined;
        upserts.delete(dirtyFile.path);
        upserts.delete(previousEntry.path);
        upserts.set(previousEntry.path, { entry: previousEntry, previousPath: dirtyFile.path });
        if (entry && allowedPaths.has(entry.path)) {
          upserts.set(dirtyFile.path, currentPlan ?? { entry });
        }
        continue;
      }
      if (entry && allowedPaths.has(dirtyFile.path)) {
        const planned = upserts.get(dirtyFile.path);
        if (planned?.previousPath && !indexedByPath.has(planned.previousPath)) {
          upserts.set(dirtyFile.path, { entry, previousPath: dirtyFile.path });
        } else if (!planned) {
          upserts.set(dirtyFile.path, { entry });
        }
        continue;
      }
      if (previousEntry && allowedPaths.has(previousEntry.path)) {
        upserts.set(previousEntry.path, { entry: previousEntry, previousPath: dirtyFile.path });
      } else {
        deletes.add(dirtyFile.path);
      }
    }
    for (const [filePath, { entry, previousPath }] of upserts) {
      const indexed = indexedByPath.get(filePath);
      if (!previousPath && indexed?.sourceMode === "git" && indexed.blobOid === entry.oid
        && (entry.size > this.#maxFileSize) === tooLargePaths.has(filePath)) upserts.delete(filePath);
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
      if (!allowedPaths.has(relativePath)) {
        if (replacePath) workingDeletes.add(replacePath);
        workingDeletes.add(relativePath);
        continue;
      }
      const file = await this.#prepareWorkingFile(relativePath, {
        blobOid: null, sourceMode: "working-tree", indexedCommit: null,
      });
      if (!file) {
        if (replacePath) workingDeletes.add(replacePath);
        workingDeletes.add(relativePath);
        skippedWorkingPaths.add(relativePath);
        continue;
      }
      if (!file.unavailable && !replacePath && change.status === "A") {
        const contentHash = file.contentHash;
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
      workingPrepared.push({
        ...file,
        ...(previousPath ? { previousPath } : {}),
        ...(replacePath ? { replacePath } : {}),
      });
    }

    const prepared: PreparedFile[] = [];
    for (const { entry, previousPath } of upserts.values()) {
      throwIfAborted(options.signal);
      const provenance = {
        blobOid: entry.oid,
        sourceMode: "git" as const,
        indexedCommit: target,
        ...(previousPath ? { replacePath: previousPath } : {}),
      };
      if (entry.size > this.#maxFileSize) {
        prepared.push(this.#failedFile(entry.path, "file-too-large", `File exceeds maxFileSize (${this.#maxFileSize} bytes).`, provenance, entry.size));
        continue;
      }
      try {
        const content = await this.#git.readBlob(entry.oid);
        prepared.push(this.#prepareFile(entry.path, content, provenance));
      } catch (error) {
        prepared.push(this.#failedFile(entry.path, "read-error", error instanceof Error ? error.message : String(error), provenance, entry.size));
      }
    }
    let embeddingsCreated = await this.#attachEmbeddings([...prepared, ...workingPrepared], options.signal);
    const noChanges = (
      !reconcileAll
      && checkpoint === target
      && prepared.length === 0
      && deletes.size === 0
      && workingPrepared.length === 0
      && workingDeletes.size === 0
    );

    while (true) {
      const resolvedAgain = await this.#git.resolveCommit(options.target ?? "HEAD");
      if (resolvedAgain !== target) throw new CodeIndexError("Target Git ref changed while indexing; retry the update.");
      if (!overlayWorkingTree) break;
      const headAgain = await this.#git.resolveCommit("HEAD");
      const workingChangesAgain = await this.#git.workTreeChanges(target, indexArtifacts);
      if (headAgain !== head) throw new CodeIndexError("Git HEAD changed while indexing; retry the update.");
      if (JSON.stringify(workingChangesAgain) !== JSON.stringify(workingChanges)) return await this.updateFromGit(options);
      const changed: PreparedFile[] = [];
      for (let index = 0; index < workingPrepared.length; index += 1) {
        const file = workingPrepared[index]!;
        if (!await this.#workingFileChanged(file)) continue;
        const refreshed = await this.#prepareWorkingFile(file.path, {
          blobOid: null,
          sourceMode: "working-tree",
          indexedCommit: null,
          ...(file.previousPath ? { previousPath: file.previousPath } : {}),
          ...(file.replacePath ? { replacePath: file.replacePath } : {}),
        });
        if (!refreshed) return await this.updateFromGit(options);
        workingPrepared[index] = refreshed;
        changed.push(refreshed);
      }
      for (const relativePath of skippedWorkingPaths) {
        const refreshed = await this.#prepareWorkingFile(relativePath, {
          blobOid: null,
          sourceMode: "working-tree",
          indexedCommit: null,
        });
        if (refreshed && !refreshed.unavailable) return await this.updateFromGit(options);
      }
      if (changed.length > 0) {
        embeddingsCreated += await this.#attachEmbeddings(changed, options.signal);
        continue;
      }
      await gitignore.assertUnchanged(options.signal);
      break;
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
      completeDiagnosticsScan: true,
      embeddingsCreated,
    });
  }

  #classifyGitChanges(
    changes: readonly GitChange[],
    tree: ReadonlyMap<string, GitTreeEntry>,
    upserts: Map<string, { entry: GitTreeEntry; previousPath?: string }>,
    deletes: Set<string>,
    allowedPaths: ReadonlySet<string>,
  ): void {
    for (const change of changes) {
      if (change.status === "D") {
        deletes.add(change.path);
        continue;
      }
      if (change.status === "R") {
        deletes.add(change.oldPath);
        const entry = tree.get(change.path);
        if (entry && allowedPaths.has(change.path)) {
          upserts.set(change.path, { entry, previousPath: change.oldPath });
        }
        continue;
      }
      const entry = tree.get(change.path);
      if (entry && allowedPaths.has(change.path)) upserts.set(change.path, { entry });
      else deletes.add(change.path);
    }
  }

  async #workingTreeSourcePaths(gitignore: GitignoreRules, signal?: AbortSignal): Promise<string[]> {
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
          if (this.#policy.traversesDirectory(relativePath) && !await gitignore.ignores(relativePath, true)) directories.push(relativePath);
        } else if (entry.isFile() && !indexArtifacts.has(relativePath) && this.#policy.includes(relativePath) && !await gitignore.ignores(relativePath)) {
          sourcePaths.push(relativePath);
        }
      }
    }
    sourcePaths.sort();
    return sourcePaths;
  }

  async #prepareWorkingPaths(relativePaths: readonly string[], signal?: AbortSignal): Promise<PreparedFile[]> {
    const prepared: PreparedFile[] = [];
    for (const relativePath of relativePaths) {
      throwIfAborted(signal);
      const file = await this.#prepareWorkingFile(relativePath, {
        blobOid: null,
        sourceMode: "working-tree",
        indexedCommit: null,
      });
      if (file) prepared.push(file);
    }
    return prepared;
  }

  async #workingFileChanged(file: PreparedFile): Promise<boolean> {
    try {
      const info = await lstat(path.join(this.rootDir, file.path));
      if (!info.isFile() || info.isSymbolicLink() || info.size !== file.byteSize) return true;
      if (info.size > this.#maxFileSize) return !file.errors.some((error) => error.code === "file-too-large");
      const content = await readFile(path.join(this.rootDir, file.path));
      return file.unavailable === true || sha256(content.toString("utf8")) !== file.contentHash;
    } catch {
      return file.unavailable !== true;
    }
  }

  #workingFileChangedSynchronously(file: PreparedFile): boolean {
    try {
      const absolutePath = path.join(this.rootDir, file.path);
      const info = lstatSync(absolutePath);
      if (!info.isFile() || info.isSymbolicLink() || info.size !== file.byteSize) return true;
      if (info.size > this.#maxFileSize) return !file.errors.some((error) => error.code === "file-too-large");
      const contentHash = sha256(readFileSync(absolutePath, "utf8"));
      return file.unavailable === true || contentHash !== file.contentHash;
    } catch {
      return file.unavailable !== true;
    }
  }

  #prepareFile(
    relativePath: string,
    buffer: Buffer,
    provenance: Pick<PreparedFile, "blobOid" | "sourceMode" | "indexedCommit" | "previousPath" | "replacePath">,
  ): PreparedFile {
    const content = buffer.toString("utf8");
    const contentHash = sha256(content);
    const markdown = isMarkdownPath(relativePath);
    const parseKey = sha256(`${CALLABLE_PARSER_CACHE_VERSION}\0${relativePath}\0${contentHash}`);
    let parsed = markdown ? { callables: [], errors: [] } : this.#database.cachedParse(parseKey);
    if (!parsed) {
      parsed = parseFileCallables(relativePath, content, this.#onWarning);
      if (!parsed.errors.some((error) => error.code === "parse-failed" || error.code === "extraction-error")) {
        this.#database.storeParse(parseKey, parsed);
      }
    } else if (parsed.errors.length > 0) {
      this.#onWarning(`Cannot fully parse ${relativePath}: tree-sitter reported syntax errors; indexing recoverable callables only.`);
    }
    const language = markdown ? "markdown" : languageForPath(relativePath) ?? path.extname(relativePath).slice(1);
    return {
      path: relativePath,
      contentHash,
      blobOid: provenance.blobOid,
      sourceMode: provenance.sourceMode,
      indexedCommit: provenance.indexedCommit,
      language,
      byteSize: buffer.byteLength,
      source: content,
      ...(provenance.previousPath ? { previousPath: provenance.previousPath } : {}),
      ...(provenance.replacePath ? { replacePath: provenance.replacePath } : {}),
      callables: parsed.callables as PreparedCallable[],
      markdownChunks: markdown
        ? chunkMarkdown(content).map((chunk): PreparedMarkdownChunk => ({ ...chunk, embeddingKey: "" }))
        : [],
      errors: parsed.errors,
    };
  }

  async #prepareWorkingFile(
    relativePath: string,
    provenance: Pick<PreparedFile, "blobOid" | "sourceMode" | "indexedCommit" | "previousPath" | "replacePath">,
    strict = false,
  ): Promise<PreparedFile | null> {
    let size = 0;
    try {
      const absolutePath = path.join(this.rootDir, relativePath);
      const info = await lstat(absolutePath);
      if (info.isSymbolicLink() || !info.isFile()) {
        if (strict) throw new CodeIndexError(`Not a regular file or symbolic links are not supported: ${relativePath}`);
        return null;
      }
      size = info.size;
      if (size > this.#maxFileSize) {
        return this.#failedFile(relativePath, "file-too-large", `File exceeds maxFileSize (${this.#maxFileSize} bytes).`, provenance, size);
      }
      return this.#prepareFile(relativePath, await readFile(absolutePath), provenance);
    } catch (error) {
      if (error instanceof CodeIndexError) throw error;
      return this.#failedFile(relativePath, "read-error", error instanceof Error ? error.message : String(error), provenance, size);
    }
  }

  #failedFile(
    relativePath: string, code: "read-error" | "file-too-large", message: string,
    provenance: Pick<PreparedFile, "blobOid" | "sourceMode" | "indexedCommit" | "previousPath" | "replacePath">,
    byteSize: number,
  ): PreparedFile {
    this.#onWarning(`Cannot index ${relativePath}: ${message}`);
    return {
      ...provenance, path: relativePath, contentHash: sha256(""), source: "", byteSize,
      language: isMarkdownPath(relativePath) ? "markdown" : languageForPath(relativePath) ?? "unknown",
      callables: [], markdownChunks: [], unavailable: true,
      errors: [{
        path: relativePath, language: languageForPath(relativePath), scope: "file", code, message,
        qualifiedName: null, startLine: null, startColumn: null, endLine: null, endColumn: null, source: null,
      }],
    };
  }

  async #attachEmbeddings(files: PreparedFile[], signal?: AbortSignal): Promise<number> {
    if (this.#database.descriptionsEnabled()) {
      await this.#attachDescriptions(files.filter((file) => !file.unavailable && file.language !== "markdown"), signal);
    }
    throwIfAborted(signal);
    const profile = JSON.stringify(normalizeProfile(this.provider.profile));
    const unique = new Map<string, Array<PreparedCallable | PreparedMarkdownChunk>>();
    for (const file of files) {
      for (const item of [...file.callables, ...file.markdownChunks]) {
        item.embeddingKey = embeddingKey(profile, "document", item.embeddingInput);
        const values = unique.get(item.embeddingKey) ?? [];
        values.push(item);
        unique.set(item.embeddingKey, values);
      }
    }
    const missing: Array<[string, Array<PreparedCallable | PreparedMarkdownChunk>]> = [];
    for (const [key, items] of unique) {
      const vector = this.#database.cachedEmbedding(key);
      if (vector) {
        for (const item of items) item.vector = vector;
      } else {
        missing.push([key, items]);
      }
    }
    if (missing.length === 0) return 0;
    let completed = 0;
    this.#progress("vectors", 0, missing.length);
    await forEachConcurrent(chunk(missing, this.#embeddingBatchSize), this.#parallelism, async (batch, _index, workerSignal) => {
      const vectors = await this.provider.embedDocuments(
        batch.map(([, items]) => items[0]!.embeddingInput),
        { signal: workerSignal },
      );
      if (vectors.length !== batch.length) throw new CodeIndexError("Embedding provider returned an unexpected number of vectors.");
      vectors.forEach((vector, index) => {
        const converted = normalizeEmbeddingVector(vector, this.provider.profile.dimensions);
        this.#database.storeEmbedding(batch[index]![0], converted);
        for (const item of batch[index]![1]) item.vector = converted;
      });
      completed += batch.length;
      this.#progress("vectors", completed, missing.length);
    }, signal);
    throwIfAborted(signal);
    return missing.length;
  }

  async #attachDescriptions(
    files: PreparedFile[],
    signal?: AbortSignal,
    options: { refreshFileDescriptions?: boolean; forceCallableDescriptions?: boolean; reindexCache?: boolean; skipCallables?: boolean; ignoreLiveDescriptions?: boolean } = {},
  ): Promise<{ descriptionsCreated: number; fileDescriptionsCreated: number }> {
    const profile = descriptionProfileJson(this.descriptionProvider.profile);
    const embeddingProfile = JSON.stringify(normalizeProfile(this.provider.profile));
    const prepared = new Map<string, PreparedDescription>();
    const repository = path.basename(this.rootDir);
    let descriptionsCreated = 0;
    let fileDescriptionsCreated = 0;
    let pending = 0;
    const tasks: DescriptionTask[] = [];
    const strategy = this.descriptionProvider.profile.strategyVersion;
    const useLiveReuse = !options.skipCallables && !options.reindexCache && !options.forceCallableDescriptions && !options.ignoreLiveDescriptions;
    const liveCallableDescriptions = useLiveReuse
      ? this.#database.liveFunctionDescriptions(files.flatMap((file) => file.callables.map((callable) => callable.identityKey)))
      : new Map<string, { description: string | null; sourceHash: string }>();
    const liveFileHashes = useLiveReuse && files.length > 0
      ? this.#database.liveFileContentHashes(files.map((file) => file.replacePath ?? file.previousPath ?? file.path))
      : new Map<string, string>();
    for (const file of files) {
      const lookupPath = file.replacePath ?? file.previousPath ?? file.path;
      const storedFileDescription = this.#database.fileDescription(lookupPath);
      const fileDescriptionKey = sha256(`${profile}\0${repository}\0${file.path}\0${file.contentHash}\0file`);
      const fileAgnosticKey = sha256(`${strategy}\0${repository}\0${file.path}\0${file.contentHash}\0file`);
      const fileCacheKey = options.reindexCache
        ? sha256(`${fileDescriptionKey}\0reindex\0${storedFileDescription?.description ?? ""}`)
        : fileDescriptionKey;
      const refreshFileDescription = options.refreshFileDescriptions || !storedFileDescription;
      const fileDescription = refreshFileDescription
        ? this.#database.cachedDescription(fileCacheKey)
          ?? (!options.reindexCache ? this.#database.cachedDescription(fileAgnosticKey) : undefined)
        : storedFileDescription.description;
      const fileUnchanged = liveFileHashes.get(lookupPath) === file.contentHash;
      const descriptions = (options.skipCallables ? [] : file.callables).map((callable) => {
        const descriptionKey = sha256(`${profile}\0${repository}\0${file.path}\0${file.contentHash}\0${callable.identityKey}\0${callable.sourceHash}`);
        const agnosticKey = sha256(`${strategy}\0${repository}\0${file.path}\0${file.contentHash}\0${callable.identityKey}\0${callable.sourceHash}`);
        const cacheKey = options.reindexCache
          ? sha256(`${descriptionKey}\0reindex\0${this.#database.functionDescription(callable.identityKey) ?? ""}`)
          : descriptionKey;
        if (useLiveReuse && fileUnchanged) {
          const live = liveCallableDescriptions.get(callable.identityKey);
          if (live?.description && live.sourceHash === callable.sourceHash) {
            return {
              callable,
              descriptionKey,
              cacheKey,
              agnosticKey,
              description: live.description,
            };
          }
        }
        const description = options.forceCallableDescriptions || options.reindexCache
          ? this.#database.cachedDescription(cacheKey)
          : this.#database.cachedDescription(descriptionKey)
            ?? this.#database.cachedDescription(agnosticKey);
        return {
          callable,
          descriptionKey,
          cacheKey,
          agnosticKey,
          ...(description ? { description } : {}),
        };
      });
      pending += (refreshFileDescription && !fileDescription ? 1 : 0)
        + descriptions.filter(({ description }) => !description).length;
      tasks.push({
        file,
        fileCacheKey,
        fileAgnosticKey,
        fileDescriptionKey,
        refreshFileDescription,
        ...(fileDescription ? { fileDescription } : {}),
        fileDescriptionPath: refreshFileDescription ? file.path : storedFileDescription.path,
        fileDescriptionContentHash: refreshFileDescription ? file.contentHash : storedFileDescription.contentHash,
        descriptions,
      });
    }
    let completed = 0;
    if (pending > 0) this.#progress("descriptions", 0, pending);
    await forEachConcurrent(tasks, this.#parallelism, async (task, _index, workerSignal) => {
      const { file } = task;
      const session = task.refreshFileDescription || task.descriptions.some(({ description }) => !description)
        ? this.descriptionProvider.startFile?.({ repository, path: file.path, fileSource: file.source })
        : undefined;
      let fileDescription = task.fileDescription;
      let fileDescriptionAddedToSession = false;
      if (task.refreshFileDescription && !fileDescription) {
        const generated = session
          ? await session.describeFile({ signal: workerSignal })
          : await this.descriptionProvider.describeFile(
            { repository, path: file.path, fileSource: file.source },
            { signal: workerSignal },
          );
        if (typeof generated !== "string" || !generated.trim()) {
          throw new CodeIndexError("Description provider does not support file descriptions or returned an empty description.");
        }
        fileDescription = this.#database.storeDescription(task.fileCacheKey, generated.trim());
        this.#database.storeDescription(task.fileAgnosticKey, fileDescription);
        fileDescriptionsCreated += 1;
        fileDescriptionAddedToSession = session !== undefined;
        completed += 1;
        this.#progress("descriptions", completed, pending);
      }
      if (session && fileDescription && !fileDescriptionAddedToSession) session.replayFile(fileDescription);
      if (fileDescription) {
        const value = prepareDescription(prepared, embeddingProfile, fileDescription, this.#database);
        file.fileDescription = {
          path: task.fileDescriptionPath,
          contentHash: task.fileDescriptionContentHash,
          descriptionKey: task.fileDescriptionKey,
          value,
        };
      }
      for (const entry of task.descriptions) {
        throwIfAborted(workerSignal);
        const { callable, descriptionKey, cacheKey, agnosticKey } = entry;
        let { description } = entry;
        if (!description) {
          const generated = session
            ? await session.describe(callable, { signal: workerSignal })
            : await this.descriptionProvider.describe({ repository, callable, fileSource: file.source }, { signal: workerSignal });
          if (typeof generated !== "string" || !generated.trim()) throw new CodeIndexError("Description provider returned an empty description.");
          description = this.#database.storeDescription(cacheKey, generated.trim());
          this.#database.storeDescription(agnosticKey, description);
          descriptionsCreated += 1;
          completed += 1;
          this.#progress("descriptions", completed, pending);
        } else {
          session?.replay(callable, description);
        }
        callable.descriptionKey = descriptionKey;
        callable.description = prepareDescription(prepared, embeddingProfile, description, this.#database);
      }
    }, signal);
    const missing = [...prepared.values()].filter((description) => !description.vector);
    if (missing.length > 0) {
      let embedded = 0;
      this.#progress("description-vectors", 0, missing.length);
      await forEachConcurrent(chunk(missing, this.#embeddingBatchSize), this.#parallelism, async (batch, _index, workerSignal) => {
        const vectors = await this.provider.embedDocuments(batch.map((value) => value.description), { signal: workerSignal });
        if (vectors.length !== batch.length) throw new CodeIndexError("Embedding provider returned an unexpected number of description vectors.");
        vectors.forEach((vector, index) => {
          const converted = normalizeEmbeddingVector(vector, this.provider.profile.dimensions);
          this.#database.storeEmbedding(batch[index]!.key, converted);
          batch[index]!.vector = converted;
        });
        embedded += batch.length;
        this.#progress("description-vectors", embedded, missing.length);
      }, signal);
    }
    throwIfAborted(signal);
    return { descriptionsCreated, fileDescriptionsCreated };
  }

  public async useDescriptions(options: { signal?: AbortSignal } = {}): Promise<DescriptionStats> {
    throwIfAborted(options.signal);
    const generation = this.#database.getGeneration();
    const functions = this.allFunctions();
    const status = this.status();
    const storedProfile = this.#database.descriptionProfile();
    const strategyChanged = (storedProfile?.strategyVersion ?? this.descriptionProvider.profile.strategyVersion)
      !== this.descriptionProvider.profile.strategyVersion;
    const profileChanged = descriptionProfileJson(storedProfile ?? this.descriptionProvider.profile)
      !== descriptionProfileJson(this.descriptionProvider.profile);
    if (this.#database.descriptionsEnabled()
      && !strategyChanged
      && functions.every((callable) => callable.descriptionEmbeddingId !== null)
      && status.fileDescriptionCount === status.describableFileCount) {
      if (profileChanged) this.#database.updateDescriptionProfile(this.descriptionProvider.profile, generation);
      return { descriptionsCreated: 0, fileDescriptionsCreated: 0, descriptionsEnabled: true };
    }
    const files = await this.#descriptionFiles(this.#database.getFileStates().filter((file) => file.describable), functions, options.signal);
    const { descriptionsCreated, fileDescriptionsCreated } = await this.#attachDescriptions(files, options.signal, {
      refreshFileDescriptions: strategyChanged,
      ...(strategyChanged ? { ignoreLiveDescriptions: true } : {}),
    });
    const ids = new Map(functions.map((callable) => [callable.identityKey, callable.id]));
    this.#database.enableDescriptions(files.flatMap((file) => file.callables.map((callable) => ({
      id: ids.get(callable.identityKey)!, description: callable.description!,
    }))), files, this.descriptionProvider.profile, generation);
    return { descriptionsCreated, fileDescriptionsCreated, descriptionsEnabled: true };
  }

  public async reindexFiles(options: ReindexFilesOptions = {}): Promise<ReindexFilesStats> {
    throwIfAborted(options.signal);
    if (!this.#database.descriptionsEnabled()) throw new CodeIndexError("Descriptions are not enabled; run descriptions enable first.");
    const generation = this.#database.getGeneration();
    const states = this.#database.getFileStates()
      .filter((file) => file.describable
        && (file.fileDescriptionPath !== file.path || file.fileDescriptionContentHash !== file.contentHash));
    const files = await this.#descriptionFiles(states, this.allFunctions(), options.signal);
    const created = await this.#attachDescriptions(files, options.signal, {
      refreshFileDescriptions: true,
      ...(options.includeCallables !== undefined ? { forceCallableDescriptions: options.includeCallables } : {}),
      reindexCache: true,
      skipCallables: !options.includeCallables,
    });
    this.#database.updateDescriptions(files, options.includeCallables ?? false, generation, this.descriptionProvider.profile);
    return { filesReindexed: files.length, ...created };
  }

  async #descriptionFiles(
    states: readonly IndexedFileState[],
    functions: readonly IndexedFunction[],
    signal?: AbortSignal,
  ): Promise<PreparedFile[]> {
    const byPath = groupBy(functions, (callable) => callable.path);
    const files: PreparedFile[] = [];
    for (const file of states) {
      throwIfAborted(signal);
      const buffer = file.sourceMode === "git" && file.blobOid
        ? await this.#git.readBlob(file.blobOid)
        : await readFile(path.join(this.rootDir, file.path));
      const source = buffer.toString("utf8");
      if (sha256(source) !== file.contentHash) {
        throw new CodeIndexError(`Source changed since indexing: ${file.path}; update the index first.`);
      }
      const callables = byPath.get(file.path) ?? [];
      files.push({
        path: file.path, contentHash: file.contentHash, blobOid: file.blobOid,
        sourceMode: file.sourceMode, indexedCommit: null, language: file.language,
        byteSize: buffer.byteLength, source,
        errors: [],
        markdownChunks: [],
        callables: callables.map(({ description: _description, descriptionEmbeddingId: _descriptionEmbeddingId, ...callable }) => ({
          ...callable,
          embeddingKey: "",
        })),
      });
    }
    return files;
  }

  public disableDescriptions(): DescriptionStats {
    this.#database.disableDescriptions();
    return { descriptionsCreated: 0, fileDescriptionsCreated: 0, descriptionsEnabled: false };
  }

  public async searchDescription(options: SimilaritySearchOptions): Promise<SimilarityResult[]> {
    if (!this.#database.descriptionsEnabled()) throw new CodeIndexError("Descriptions are not enabled; run descriptions enable first.");
    if (!options.query.trim()) throw new CodeIndexError("query must not be empty.");
    const limit = options.limit;
    if (limit !== undefined) assertPositiveInteger(limit, "limit");
    const candidateLimit = this.#candidateLimit(limit);
    compileNameRegex(options.nameRegex);
    throwIfAborted(options.signal);
    const vector = await this.#queryEmbedding(options.query, options.signal);
    throwIfAborted(options.signal);
    const includeFileDescriptions = this.#descriptionScoringAvailable();
    const results = this.#database.searchVector(vector, {
      descriptions: true,
      ...(candidateLimit === undefined ? {} : { limit: candidateLimit }),
      minSimilarity: options.minSimilarity ?? -1,
      ...(includeFileDescriptions ? { fileDescriptionVector: vector } : {}),
      ...(options.nameRegex !== undefined ? { nameRegex: options.nameRegex } : {}),
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
    });
    return await this.#rerank(options.query, results, limit, options.signal);
  }

  public async searchCode(options: SimilaritySearchOptions): Promise<SimilarityResult[]> {
    if (!options.query.trim()) throw new CodeIndexError("query must not be empty.");
    const limit = options.limit;
    if (limit !== undefined) assertPositiveInteger(limit, "limit");
    const candidateLimit = this.#candidateLimit(limit);
    compileNameRegex(options.nameRegex);
    throwIfAborted(options.signal);
    const vector = await this.#queryEmbedding(options.query, options.signal);
    throwIfAborted(options.signal);
    const results = this.#database.searchVector(vector, {
      ...(candidateLimit === undefined ? {} : { limit: candidateLimit }),
      minSimilarity: options.minSimilarity ?? -1,
      ...(options.nameRegex !== undefined ? { nameRegex: options.nameRegex } : {}),
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
    });
    return await this.#rerank(options.query, results, limit, options.signal);
  }

  public async searchMarkdown(options: MarkdownSearchOptions): Promise<MarkdownSearchResult[]> {
    if (!options.query.trim()) throw new CodeIndexError("query must not be empty.");
    const limit = options.limit;
    if (limit !== undefined) assertPositiveInteger(limit, "limit");
    const candidateLimit = this.#candidateLimit(limit);
    throwIfAborted(options.signal);
    const vector = await this.#queryEmbedding(options.query, options.signal);
    throwIfAborted(options.signal);
    const results = this.#database.searchMarkdown(vector, {
      ...(candidateLimit === undefined ? {} : { limit: candidateLimit }),
      minSimilarity: options.minSimilarity ?? -1,
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
    });
    return await this.#rerankMarkdown(options.query, results, limit, options.signal);
  }

  public async search(options: SearchOptions): Promise<SearchResult[]> {
    if (!options.query.trim()) throw new CodeIndexError("query must not be empty.");
    const limit = options.limit;
    if (limit !== undefined) assertPositiveInteger(limit, "limit");
    compileNameRegex(options.nameRegex);
    const requested = options.indexes === undefined
      ? new Set(["code", "descriptions", "markdown"] as const)
      : new Set(options.indexes);
    if (requested.size === 0) throw new CodeIndexError("at least one search index must be selected.");
    for (const index of requested) {
      if (index !== "code" && index !== "descriptions" && index !== "markdown") {
        throw new CodeIndexError(`Unknown search index: ${String(index)}.`);
      }
    }
    if (options.indexes !== undefined && requested.has("descriptions") && !this.#database.descriptionsEnabled()) {
      throw new CodeIndexError("Descriptions are not enabled; run descriptions enable first.");
    }
    if (!this.#database.descriptionsEnabled()) requested.delete("descriptions");

    const candidateLimit = this.#candidateLimit(limit);
    throwIfAborted(options.signal);
    const vector = await this.#queryEmbedding(options.query, options.signal);
    throwIfAborted(options.signal);
    const functionOptions = {
      ...(candidateLimit === undefined ? {} : { limit: candidateLimit }),
      minSimilarity: options.minSimilarity ?? -1,
      ...(options.nameRegex !== undefined ? { nameRegex: options.nameRegex } : {}),
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
    };
    const includeCode = requested.has("code");
    const includeDescriptions = requested.has("descriptions");
    let functions: SimilarityResult[] = [];
    if (includeCode && includeDescriptions) {
      const complete = this.#descriptionScoringAvailable();
      functions = this.#database.searchVector(vector, {
        ...functionOptions,
        ...(complete ? { descriptionVector: vector, fileDescriptionVector: vector } : {}),
      });
    } else if (includeCode) {
      functions = this.#database.searchVector(vector, functionOptions);
    } else if (includeDescriptions) {
      functions = this.#database.searchVector(vector, {
        ...functionOptions,
        descriptions: true,
        ...(this.#descriptionScoringAvailable() ? { fileDescriptionVector: vector } : {}),
      });
    }
    const markdown = requested.has("markdown")
      ? this.#database.searchMarkdown(vector, {
        ...(candidateLimit === undefined ? {} : { limit: candidateLimit }),
        minSimilarity: options.minSimilarity ?? -1,
        ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
      })
      : [];
    const candidates: SearchResult[] = [
      ...functions.map((result): SearchResult => ({ type: "function", ...result })),
      ...markdown.map((result): SearchResult => ({ type: "markdown", ...result })),
    ].sort((left, right) => right.similarity - left.similarity
      || left.type.localeCompare(right.type)
      || (left.type === "function" ? left.function.id : left.chunk.id)
        - (right.type === "function" ? right.function.id : right.chunk.id));
    const selected = candidateLimit === undefined ? candidates : candidates.slice(0, candidateLimit);
    const documents = selected.map((result) => result.type === "function"
      ? this.#functionRerankDocument(result)
      : `path: ${result.chunk.path}\n${result.chunk.content}`);
    return await this.#rerankCandidates(options.query, selected, documents, limit, options.signal);
  }

  public async similaritySearch(options: SimilaritySearchOptions): Promise<SimilarityResult[]> {
    if (!options.query.trim()) throw new CodeIndexError("query must not be empty.");
    const limit = options.limit;
    if (limit !== undefined) assertPositiveInteger(limit, "limit");
    const candidateLimit = this.#candidateLimit(limit);
    compileNameRegex(options.nameRegex);
    throwIfAborted(options.signal);
    const vector = await this.#queryEmbedding(options.query, options.signal);
    const includeDescriptions = this.#descriptionScoringAvailable();
    const results = this.searchByVector(vector, {
      ...(includeDescriptions ? { descriptionVector: vector, fileDescriptionVector: vector } : {}),
      ...(candidateLimit === undefined ? {} : { limit: candidateLimit }),
      ...(options.nameRegex !== undefined ? { nameRegex: options.nameRegex } : {}),
      minSimilarity: options.minSimilarity ?? -1,
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
    });
    return await this.#rerank(options.query, results, limit, options.signal);
  }

  /**
   * Gather the relevant files, callables, descriptions, and optionally complete
   * file sources for a natural-language request. The result is a discovery
   * context for a description model, not an implementation plan.
   */
  public async describe(options: DescribeOptions): Promise<DescribeContext> {
    if (!options.query.trim()) throw new CodeIndexError("query must not be empty.");
    const minSimilarity = options.minSimilarity ?? -1;
    const fullFileThreshold = options.fullFileThreshold ?? 0.8;
    if (!Number.isFinite(fullFileThreshold)) throw new CodeIndexError("fullFileThreshold must be a finite number.");
    const results = await this.similaritySearch({
      query: options.query,
      ...(options.nameRegex !== undefined ? { nameRegex: options.nameRegex } : {}),
      ...(options.limit === undefined ? {} : { limit: options.limit }),
      minSimilarity,
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
      ...(options.signal ? { signal: options.signal } : {}),
    });
    const grouped = new Map<string, number>();
    for (const result of results) {
      grouped.set(result.function.path, Math.max(grouped.get(result.function.path) ?? -Infinity, result.similarity));
    }
    const files: DescribeFile[] = [...grouped]
      .map(([filePath, similarity]) => ({
        path: filePath,
        similarity,
        description: this.#database.fileDescription(filePath)?.description ?? null,
        content: null,
      }));
    const functions: DescribeFunction[] = results
      .map((result) => ({
        path: result.function.path,
        qualifiedName: result.function.qualifiedName,
        kind: result.function.kind,
        signature: result.function.signature,
        startLine: result.function.startLine,
        endLine: result.function.endLine,
        similarity: result.similarity,
        ...(result.rerankScore !== undefined ? { rerankScore: result.rerankScore } : {}),
        description: result.function.description,
        source: result.function.source,
      }));
    const fileContentErrors: string[] = [];
    if (options.includeFileContents !== false) {
      const pending = files.filter((file) => file.similarity > fullFileThreshold);
      const states = pending.length === 0
        ? new Map<string, IndexedFileState>()
        : new Map(this.#database.getFileStates().map((state) => [state.path, state]));
      const contents = new Map<string, string>();
      for (const file of pending) {
        throwIfAborted(options.signal);
        const state = states.get(file.path);
        try {
          if (!state) throw new CodeIndexError("File is not present in the index.");
          contents.set(file.path, await this.#readIndexedSource(state));
        } catch (error) {
          fileContentErrors.push(
            `cannot include full content of ${file.path}: ${error instanceof Error ? error.message : String(error)}`,
          );
        }
      }
      if (fileContentErrors.length === 0) {
        for (const file of pending) file.content = contents.get(file.path) ?? null;
      }
    }
    return {
      repository: path.basename(this.rootDir),
      query: options.query,
      minSimilarity,
      fullFileThreshold,
      files,
      functions,
      fileContentErrors,
    };
  }

  async #readIndexedSource(state: IndexedFileState): Promise<string> {
    if (state.sourceMode === "working-tree") {
      const sourcePath = path.join(this.rootDir, state.path);
      const file = await lstat(sourcePath);
      if (!file.isFile()) throw new CodeIndexError("Indexed path is no longer a regular file.");
      const source = await readFile(sourcePath, "utf8");
      if (sha256(source) !== state.contentHash) {
        throw new CodeIndexError(`Source changed since indexing: ${state.path}; update the index first.`);
      }
      return source;
    }
    if (!state.blobOid) throw new CodeIndexError("File has no indexed Git blob.");
    return (await this.#git.readBlob(state.blobOid)).toString("utf8");
  }

  #candidateLimit(limit: number | undefined): number | undefined {
    if (!this.reranker) return limit;
    if (limit === undefined) return this.reranker.maximumCandidateCount;
    if (this.reranker.maximumCandidateCount !== undefined && limit > this.reranker.maximumCandidateCount) {
      throw new CodeIndexError(`${this.reranker.profile.provider} reranker supports at most ${this.reranker.maximumCandidateCount} results.`);
    }
    const preferred = this.reranker.candidateCount === undefined
      ? limit * RERANK_CANDIDATE_MULTIPLIER
      : Math.max(limit, this.reranker.candidateCount);
    return this.reranker.maximumCandidateCount === undefined
      ? preferred
      : Math.min(preferred, this.reranker.maximumCandidateCount);
  }

  async #rerank(
    query: string,
    candidates: SimilarityResult[],
    limit: number | undefined,
    signal?: AbortSignal,
  ): Promise<SimilarityResult[]> {
    const documents = candidates.map((candidate) => this.#functionRerankDocument(candidate));
    return await this.#rerankCandidates(query, candidates, documents, limit, signal);
  }

  #functionRerankDocument({ function: callable }: SimilarityResult): string {
    return [
      `path: ${callable.path}`,
      callable.description ? `description:\n${callable.description}` : null,
      callable.embeddingInput,
    ].filter((value): value is string => value !== null).join("\n");
  }

  async #rerankMarkdown(
    query: string,
    candidates: MarkdownSearchResult[],
    limit: number | undefined,
    signal?: AbortSignal,
  ): Promise<MarkdownSearchResult[]> {
    const documents = candidates.map(({ chunk }) => `path: ${chunk.path}\n${chunk.content}`);
    return await this.#rerankCandidates(query, candidates, documents, limit, signal);
  }

  async #rerankCandidates<T extends { rerankScore?: number }>(
    query: string,
    candidates: T[],
    documents: string[],
    limit: number | undefined,
    signal?: AbortSignal,
  ): Promise<Array<T & { rerankScore?: number }>> {
    if (!this.reranker || candidates.length === 0) return limit === undefined ? candidates : candidates.slice(0, limit);
    const rerankLimit = limit === undefined ? candidates.length : Math.min(limit, candidates.length);
    const rankings = await this.reranker.rerank(query, documents, signal ? { limit: rerankLimit, signal } : { limit: rerankLimit });
    throwIfAborted(signal);
    const seen = new Set<number>();
    if (rankings.length !== rerankLimit) {
      throw new CodeIndexError(`${this.reranker.profile.provider} returned invalid reranking results.`);
    }
    for (const { index, score } of rankings) {
      if (!Number.isInteger(index) || index < 0 || index >= candidates.length
        || typeof score !== "number" || !Number.isFinite(score) || seen.has(index)) {
        throw new CodeIndexError(`${this.reranker.profile.provider} returned invalid reranking results.`);
      }
      seen.add(index);
    }
    return rankings.map(({ index, score }) => ({ ...candidates[index]!, rerankScore: score }));
  }

  async #queryEmbedding(query: string, signal?: AbortSignal): Promise<number[]> {
    const profile = JSON.stringify(normalizeProfile(this.provider.profile));
    const key = embeddingKey(profile, "query", query);
    const cached = this.#database.cachedEmbedding(key);
    if (cached) return cached;
    const vector = normalizeEmbeddingVector(
      await this.provider.embedQuery(query, signal ? { signal } : undefined),
      this.provider.profile.dimensions,
    );
    this.#database.storeEmbedding(key, vector);
    return vector;
  }

  public similarToFunction(functionId: number, options: {
    includeDescriptions?: boolean;
    limit: number;
    minSimilarity: number;
    maxSimilarity?: number;
    excludePaths?: readonly string[];
    minLines?: number;
    nameRegex?: string;
  }): SimilarityResult[] {
    const vector = this.#database.vectorForFunction(functionId);
    return this.#database.searchVector(vector, {
      ...(options.includeDescriptions ? { descriptionVector: this.#database.vectorForFunction(functionId, "description") } : {}),
      ...(options.includeDescriptions ? { fileDescriptionVector: this.#database.fileVectorForFunction(functionId) } : {}),
      limit: options.limit,
      minSimilarity: options.minSimilarity,
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
      excludeId: functionId,
      ...(options.excludePaths !== undefined ? { excludePaths: options.excludePaths } : {}),
      ...(options.minLines !== undefined ? { minLines: options.minLines } : {}),
      ...(options.nameRegex !== undefined ? { nameRegex: options.nameRegex } : {}),
    });
  }

  public vectorForFunction(functionId: number, kind: "code" | "description" = "code"): number[] {
    return this.#database.vectorForFunction(functionId, kind);
  }

  public get readOnly(): boolean {
    return this.#database.isReadOnly;
  }

  public similarityCacheInfo(): SimilarityCacheInfo {
    return this.#similarityCache.similarityCacheInfo();
  }

  /**
   * Incrementally refresh the persisted pairwise-similarity cache for same-index
   * analysis. Reuses unchanged scores, rescanning changed sources and truncated
   * neighbor lists whose ranking is affected. Cached bands are only widened;
   * queries outside them fall back to live search. Call before cross-search or
   * cohesion, then read through {@link cachedSimilarToFunction} or a shared
   * {@link cachedSimilarityReader}. Best-effort under concurrent writers: a
   * later refresh repairs rows raced by an overlapping update.
   */
  public async refreshSimilarityCache(options: RefreshSimilarityCacheOptions = {}): Promise<RefreshSimilarityCacheResult> {
    return this.#similarityCache.refreshSimilarityCache(options);
  }

  /**
   * Same-index neighbor lookup backed by {@link refreshSimilarityCache}.
   * Returns cache rows filtered exactly like {@link similarToFunction} and falls
   * back to a live vector query whenever the cache entry is missing, stale,
   * computed at a higher floor than requested, or too truncated to satisfy the
   * filters. A cache entry flagged complete holds every pair above its floor,
   * so short results from it are exact and need no live query.
   *
   * Prefer {@link cachedSimilarityReader} inside per-function loops: it loads
   * the validity snapshot once instead of re-reading it for every function.
   */
  public cachedSimilarToFunction(functionId: number, options: SimilarityCacheQuery): SimilarityResult[] {
    return this.cachedSimilarityReader({
      ...(options.includeDescriptions ? { includeDescriptions: true } : {}),
    }).similarToFunction(functionId, options);
  }

  /**
   * Snapshot the similarity-cache validity state (per-mode states, embedding
   * triples, generation) once for a run of neighbor lookups. The returned
   * lookup behaves exactly like {@link cachedSimilarToFunction} but avoids
   * re-reading whole tables per function; refresh the cache first and create
   * one reader per analysis run. Best-effort under concurrent writers, like the
   * refresh itself: a concurrent mutation mid-run is picked up by the next run.
   *
   * A fallback scan doubles as a read-repair: when the entry is fresh but
   * unusable (missing, recorded at a higher floor, or incomplete), the scan is
   * widened to the full cache width and written back, so later lookups hit.
   * Stale entries are left for the refresh, dense-at-max-width entries cannot
   * be improved, and read-only indexes never write.
   */
  public cachedSimilarityReader(options: SimilarityCacheReaderOptions = {}): SimilarityCacheReader {
    return this.#similarityCache.cachedSimilarityReader(options);
  }

  public vectorForFile(filePath: string): number[] {
    return this.#database.vectorForFile(filePath);
  }

  public searchByVector(vector: readonly number[], options: {
    descriptionVector?: readonly number[];
    fileDescriptionVector?: readonly number[];
    limit?: number;
    minSimilarity: number;
    maxSimilarity?: number;
    excludePaths?: readonly string[];
    minLines?: number;
    nameRegex?: string;
  }): SimilarityResult[] {
    return this.#database.searchVector(normalizeEmbeddingVector(vector, this.provider.profile.dimensions), {
      ...options,
      ...(options.descriptionVector !== undefined
        ? { descriptionVector: normalizeEmbeddingVector(options.descriptionVector, this.provider.profile.dimensions) } : {}),
      ...(options.fileDescriptionVector !== undefined
        ? { fileDescriptionVector: normalizeEmbeddingVector(options.fileDescriptionVector, this.provider.profile.dimensions) } : {}),
    });
  }

  #descriptionScoringAvailable(): boolean {
    const status = this.status();
    return status.descriptionsEnabled
      && status.descriptionCount === status.functionCount
      && status.fileDescriptionCount === status.describableFileCount;
  }

  public async sourceFunctions(filter: CrossSearchSourceFilter): Promise<IndexedFunction[]> {
    const nameRegex = compileNameRegex(filter.nameRegex);
    const sourcePath = filter.path ? normalizeRelativePath(this.rootDir, filter.path) : undefined;
    const uncommitted = filter.type === "uncommitted" || (filter.type === "changed-since" && filter.uncommitted);
    const functions = filter.type === "changed-since"
      ? await this.#functionsChangedSince(filter.commit)
      : this.allFunctions();
    return functions.filter((callable) =>
      (!uncommitted || callable.sourceMode === "working-tree")
      && (!sourcePath || callable.path === sourcePath || callable.path.startsWith(`${sourcePath}/`))
      && (!nameRegex || nameRegex.test(callable.qualifiedName)));
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

interface DescriptionTaskEntry {
  callable: PreparedCallable;
  descriptionKey: string;
  cacheKey: string;
  agnosticKey: string;
  description?: string;
}

interface DescriptionTask {
  file: PreparedFile;
  fileCacheKey: string;
  fileAgnosticKey: string;
  fileDescriptionKey: string;
  refreshFileDescription: boolean;
  fileDescription?: string;
  fileDescriptionPath: string;
  fileDescriptionContentHash: string;
  descriptions: DescriptionTaskEntry[];
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
    strategyVersion: profile.strategyVersion ?? "callable-v2",
  };
}

function descriptionProfileJson(profile: DescriptionProvider["profile"]): string {
  return JSON.stringify({
    provider: profile.provider,
    model: profile.model,
    strategyVersion: profile.strategyVersion,
  });
}

function embeddingKey(profile: string, operation: "document" | "query", input: string): string {
  return sha256(`${profile}\0${operation}\0${input}`);
}

function prepareDescription(
  prepared: Map<string, PreparedDescription>,
  embeddingProfile: string,
  description: string,
  database: IndexDatabase,
): PreparedDescription {
  const key = embeddingKey(embeddingProfile, "document", description);
  let value = prepared.get(key);
  if (!value) {
    value = { key, description };
    const vector = database.cachedEmbedding(key);
    if (vector) value.vector = vector;
    prepared.set(key, value);
  }
  return value;
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
