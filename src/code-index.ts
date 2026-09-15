import { lstat, readFile, readdir } from "node:fs/promises";
import { lstatSync, readFileSync } from "node:fs";
import path from "node:path";

import { CodeIndexError, GitDivergenceError } from "./errors.js";
import { GitRepository, type GitChange, type GitTreeEntry } from "./git/repository.js";
import { GitignoreRules } from "./gitignore.js";
import { CALLABLE_PARSER_CACHE_VERSION, languageForPath, parseCallables, parseFileCallables } from "./parser/callable-parser.js";
import { analysisSimilarity } from "./search/similarity.js";
import { SourcePolicy } from "./source-policy.js";
import { IndexDatabase, type IndexedFileState, type PreparedCallable, type PreparedDescription, type PreparedFile } from "./storage/database.js";
import { isDescriptionProviderName, OpenAIDescriptionProvider } from "./descriptions/openai.js";
import type {
  CodeIndexOptions,
  CrossSearchSourceFilter,
  EmbeddingProfile,
  IndexProgress,
  IndexProgressPhase,
  IndexStatus,
  IndexedFunction,
  IndexingError,
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
const DEFAULT_SIMILARITY_CACHE_WIDTH = 50;
const MAX_SIMILARITY_CACHE_WIDTH = 200;

export class CodeIndex {
  public readonly rootDir: string;
  public readonly indexPath: string;
  public readonly provider;
  public readonly reranker;
  public readonly descriptionProvider: DescriptionProvider;
  readonly #database: IndexDatabase;
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
    return this.#database.applyUpdate({
      files: prepared,
      deletePaths: indexedFiles.map((file) => file.path),
      checkpoint: null,
      expectedGeneration: generation,
      completeDiagnosticsScan: true,
      embeddingsCreated,
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
    const diagnosticsScan = this.#database.needsDiagnosticsScan();
    const retryPaths = new Set(this.#database.filesWithErrors());
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
        if (indexed?.sourceMode === "git" && indexed.blobOid === entry.oid && entry.size <= this.#maxFileSize
          && !diagnosticsScan && !retryPaths.has(entry.path)) continue;
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
        if (
          (diagnosticsScan || retryPaths.has(targetPath) || entry.size > this.#maxFileSize
            || !indexed || (indexed.sourceMode === "git" && indexed.blobOid !== entry.oid))
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
      if (!previousPath && indexed?.sourceMode === "git" && indexed.blobOid === entry.oid && entry.size <= this.#maxFileSize
        && !diagnosticsScan && !retryPaths.has(filePath)) upserts.delete(filePath);
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
      && !diagnosticsScan
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
    const parseKey = sha256(`${CALLABLE_PARSER_CACHE_VERSION}\0${relativePath}\0${contentHash}`);
    let parsed = this.#database.cachedParse(parseKey);
    if (!parsed) {
      parsed = parseFileCallables(relativePath, content, this.#onWarning);
      if (!parsed.errors.some((error) => error.code === "parse-failed" || error.code === "extraction-error")) {
        this.#database.storeParse(parseKey, parsed);
      }
    } else if (parsed.errors.length > 0) {
      this.#onWarning(`Cannot fully parse ${relativePath}: tree-sitter reported syntax errors; indexing recoverable callables only.`);
    }
    const language = languageForPath(relativePath) ?? path.extname(relativePath).slice(1);
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
      language: languageForPath(relativePath) ?? "unknown", callables: [], unavailable: true,
      errors: [{
        path: relativePath, language: languageForPath(relativePath), scope: "file", code, message,
        qualifiedName: null, startLine: null, startColumn: null, endLine: null, endColumn: null, source: null,
      }],
    };
  }

  async #attachEmbeddings(files: PreparedFile[], signal?: AbortSignal): Promise<number> {
    if (this.#database.descriptionsEnabled()) {
      await this.#attachDescriptions(files.filter((file) => !file.unavailable), signal);
    }
    throwIfAborted(signal);
    const profile = JSON.stringify(normalizeProfile(this.provider.profile));
    const unique = new Map<string, PreparedCallable[]>();
    for (const file of files) {
      for (const callable of file.callables) {
        callable.embeddingKey = embeddingKey(profile, "document", callable.embeddingInput);
        const values = unique.get(callable.embeddingKey) ?? [];
        values.push(callable);
        unique.set(callable.embeddingKey, values);
      }
    }
    const missing: Array<[string, PreparedCallable[]]> = [];
    for (const [key, callables] of unique) {
      const vector = this.#database.cachedEmbedding(key);
      if (vector) {
        for (const callable of callables) callable.vector = vector;
      } else {
        missing.push([key, callables]);
      }
    }
    if (missing.length === 0) return 0;
    let completed = 0;
    this.#progress("vectors", 0, missing.length);
    await forEachConcurrent(chunk(missing, this.#embeddingBatchSize), this.#parallelism, async (batch, _index, workerSignal) => {
      const vectors = await this.provider.embedDocuments(
        batch.map(([, callables]) => callables[0]!.embeddingInput),
        { signal: workerSignal },
      );
      if (vectors.length !== batch.length) throw new CodeIndexError("Embedding provider returned an unexpected number of vectors.");
      vectors.forEach((vector, index) => {
        const converted = normalizeEmbeddingVector(vector, this.provider.profile.dimensions);
        this.#database.storeEmbedding(batch[index]![0], converted);
        for (const callable of batch[index]![1]) callable.vector = converted;
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
    const limit = options.limit ?? 10;
    assertPositiveInteger(limit, "limit");
    const candidateLimit = this.#candidateLimit(limit);
    compileNameRegex(options.nameRegex);
    throwIfAborted(options.signal);
    const vector = await this.#queryEmbedding(options.query, options.signal);
    throwIfAborted(options.signal);
    const includeFileDescriptions = this.#descriptionScoringAvailable();
    const results = this.#database.searchVector(vector, {
      descriptions: true, limit: candidateLimit, minSimilarity: options.minSimilarity ?? -1,
      ...(includeFileDescriptions ? { fileDescriptionVector: vector } : {}),
      ...(options.nameRegex !== undefined ? { nameRegex: options.nameRegex } : {}),
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
    });
    return await this.#rerank(options.query, results, limit, options.signal);
  }

  public async similaritySearch(options: SimilaritySearchOptions): Promise<SimilarityResult[]> {
    if (!options.query.trim()) throw new CodeIndexError("query must not be empty.");
    const limit = options.limit ?? 10;
    assertPositiveInteger(limit, "limit");
    const candidateLimit = this.#candidateLimit(limit);
    compileNameRegex(options.nameRegex);
    throwIfAborted(options.signal);
    const vector = await this.#queryEmbedding(options.query, options.signal);
    const includeDescriptions = this.#descriptionScoringAvailable();
    const results = this.searchByVector(vector, {
      ...(includeDescriptions ? { descriptionVector: vector, fileDescriptionVector: vector } : {}),
      limit: candidateLimit,
      ...(options.nameRegex !== undefined ? { nameRegex: options.nameRegex } : {}),
      minSimilarity: options.minSimilarity ?? -1,
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
    });
    return await this.#rerank(options.query, results, limit, options.signal);
  }

  #candidateLimit(limit: number): number {
    if (!this.reranker) return limit;
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
    limit: number,
    signal?: AbortSignal,
  ): Promise<SimilarityResult[]> {
    if (!this.reranker || candidates.length === 0) return candidates.slice(0, limit);
    const documents = candidates.map(({ function: callable }) => [
      `path: ${callable.path}`,
      callable.description ? `description:\n${callable.description}` : null,
      callable.embeddingInput,
    ].filter((value): value is string => value !== null).join("\n"));
    const rerankLimit = Math.min(limit, candidates.length);
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

  public similarityCacheInfo(): { cachedSources: number; cachedPairs: number } {
    return this.#database.similarityCacheInfo();
  }

  /**
   * Incrementally refresh the persisted pairwise-similarity cache for same-index
   * analysis. Only pairs scoring at or above `minSimilarity` (the caller's
   * effective threshold) are stored; queries below the cached floor fall back to
   * live vector search. Only functions whose embeddings changed (plus a merge
   * pass over the remaining functions when the changed set is small) are
   * re-queried; unchanged pairs are reused. Lowering the floor below what is
   * cached re-queries every function, since the missing band cannot be rebuilt
   * incrementally; raising it reuses the cache for free. A per-function state
   * row records the embedding triple, floor, row count, and a completeness flag,
   * so a sparse row set is never mistaken for an unfinished computation. Call
   * before cross-search/cohesion, then read through
   * {@link cachedSimilarToFunction}. Best-effort under concurrent writers: a
   * later refresh repairs any rows raced by an overlapping update.
   */
  public async refreshSimilarityCache(options: {
    width?: number;
    /** Minimum similarity stored; pairs below it are never cached. Defaults to -1 (cache everything). */
    minSimilarity?: number;
    signal?: AbortSignal;
    /** Overrides the index-level onProgress for the cache-fill bar. */
    onProgress?: (progress: IndexProgress) => void;
  } = {}): Promise<{
    similarityMode: string;
    width: number;
    minSimilarity: number;
    sourcesRefreshed: number;
    pairsStored: number;
    skipped: boolean;
  }> {
    const width = Math.min(
      MAX_SIMILARITY_CACHE_WIDTH,
      Math.max(1, Math.floor(options.width ?? DEFAULT_SIMILARITY_CACHE_WIDTH)),
    );
    const floor = options.minSimilarity ?? -1;
    const mode = analysisSimilarity(this.status()).similarityMode;
    const includeDescriptions = mode !== "code";
    if (this.#database.isReadOnly) {
      return { similarityMode: mode, width, minSimilarity: floor, sourcesRefreshed: 0, pairsStored: 0, skipped: true };
    }
    throwIfAborted(options.signal);
    const generation = this.#database.getGeneration();
    const triples = this.#database.similarityCacheTriples();
    if (triples.length <= 1) {
      return { similarityMode: mode, width, minSimilarity: floor, sourcesRefreshed: 0, pairsStored: 0, skipped: false };
    }
    const states = this.#database.similarityCacheStates(mode);
    const tripleById = new Map(triples.map((triple) => [triple.functionId, triple]));
    const dirty = new Set<number>();
    let expanding = false;
    for (const triple of triples) {
      throwIfAborted(options.signal);
      const state = states.get(triple.functionId);
      if (!state
        || state.codeEmbeddingId !== triple.codeEmbeddingId
        || state.descriptionEmbeddingId !== triple.descriptionEmbeddingId
        || state.fileDescriptionEmbeddingId !== triple.fileDescriptionEmbeddingId
        || (state.cachedWidth < width && !state.complete)) {
        dirty.add(triple.functionId);
      } else if (state.floor > floor) {
        expanding = true;
        dirty.add(triple.functionId);
      }
    }
    if (dirty.size === 0 && !expanding) {
      const counts = this.#database.similarityCacheCounts(mode);
      for (const triple of triples) {
        if ((counts.get(triple.functionId) ?? 0) < (states.get(triple.functionId)?.storedCount ?? 0)) {
          dirty.add(triple.functionId);
        }
      }
    }
    let sourcesRefreshed = 0;
    let pairsStored = 0;
    if (dirty.size === 0) {
      return { similarityMode: mode, width, minSimilarity: floor, sourcesRefreshed, pairsStored, skipped: false };
    }
    const report = options.onProgress ?? this.#onProgress;
    const total = triples.length;
    let completed = 0;
    report?.({ phase: "similarity-cache", completed, total });
    const fullRefresh = states.size === 0 || expanding || dirty.size > Math.max(8, Math.ceil(triples.length * 0.25));
    if (fullRefresh) {
      for (const triple of triples) {
        throwIfAborted(options.signal);
        const scanned = this.similarToFunction(triple.functionId, {
          ...(includeDescriptions ? { includeDescriptions: true } : {}),
          limit: Math.min(width + 1, triples.length - 1),
          minSimilarity: floor,
        });
        const neighbors = scanned.slice(0, width);
        this.#database.storeSimilarityNeighbors(triple.functionId, mode, neighbors, {
          codeEmbeddingId: triple.codeEmbeddingId,
          descriptionEmbeddingId: triple.descriptionEmbeddingId,
          fileDescriptionEmbeddingId: triple.fileDescriptionEmbeddingId,
          cachedWidth: width,
          generation,
          floor,
          complete: scanned.length <= width,
        });
        sourcesRefreshed += 1;
        pairsStored += neighbors.length;
        completed += 1;
        report?.({ phase: "similarity-cache", completed, total });
      }
      return { similarityMode: mode, width, minSimilarity: floor, sourcesRefreshed, pairsStored, skipped: false };
    }
    const functionsById = new Map(this.allFunctions().map((callable) => [callable.id, callable]));
    const lean = new Map<number, Map<number, SimilarityResult>>();
    for (const dirtyId of dirty) {
      throwIfAborted(options.signal);
      const full = this.similarToFunction(dirtyId, {
        ...(includeDescriptions ? { includeDescriptions: true } : {}),
        limit: triples.length - 1,
        minSimilarity: floor,
      });
      const byTarget = new Map<number, SimilarityResult>();
      for (const match of full) byTarget.set(match.function.id, match);
      lean.set(dirtyId, byTarget);
      const triple = tripleById.get(dirtyId)!;
      const top = full.slice(0, width);
      this.#database.storeSimilarityNeighbors(dirtyId, mode, top, {
        codeEmbeddingId: triple.codeEmbeddingId,
        descriptionEmbeddingId: triple.descriptionEmbeddingId,
        fileDescriptionEmbeddingId: triple.fileDescriptionEmbeddingId,
        cachedWidth: width,
        generation,
        floor,
        complete: full.length <= width,
      });
      sourcesRefreshed += 1;
      pairsStored += top.length;
      completed += 1;
      report?.({ phase: "similarity-cache", completed, total });
    }
    for (const triple of triples) {
      throwIfAborted(options.signal);
      if (dirty.has(triple.functionId)) continue;
      const cached = this.#database.cachedSimilarityNeighbors(triple.functionId, mode);
      const kept = cached.filter((match) => match.similarity >= floor
        && !dirty.has(match.function.id) && functionsById.has(match.function.id));
      const added: SimilarityResult[] = [];
      for (const [dirtyId, byTarget] of lean) {
        if (dirtyId === triple.functionId) continue;
        const match = byTarget.get(triple.functionId);
        const dirtyFunction = functionsById.get(dirtyId);
        if (match && dirtyFunction) {
          const { function: _function, ...scores } = match;
          added.push({ ...scores, function: dirtyFunction });
        }
      }
      const plusOne = [...kept, ...added]
        .sort((left, right) => right.similarity - left.similarity || left.function.id - right.function.id)
        .slice(0, width + 1);
      const merged = plusOne.slice(0, width);
      const contributorsComplete = triples.every((candidate) =>
        dirty.has(candidate.functionId) || states.get(candidate.functionId)?.complete === true);
      const complete = plusOne.length <= width && contributorsComplete;
      const unchanged = merged.length === cached.length
        && merged.every((match, index) => match.function.id === cached[index]!.function.id
          && match.similarity === cached[index]!.similarity);
      if (unchanged) {
        // Rows are untouched, but the merge already applied the new floor and
        // width: record them without rewriting the neighbor rows. When narrowing
        // dropped no rows, the previous (lower) floor still holds.
        const narrowed = cached.some((match) => match.similarity < floor);
        this.#database.touchSimilarityCacheState(triple.functionId, mode, {
          codeEmbeddingId: triple.codeEmbeddingId,
          descriptionEmbeddingId: triple.descriptionEmbeddingId,
          fileDescriptionEmbeddingId: triple.fileDescriptionEmbeddingId,
          cachedWidth: width,
          generation,
          floor: narrowed ? floor : (states.get(triple.functionId)?.floor ?? floor),
          storedCount: merged.length,
          complete,
        });
      } else {
        this.#database.storeSimilarityNeighbors(triple.functionId, mode, merged, {
          codeEmbeddingId: triple.codeEmbeddingId,
          descriptionEmbeddingId: triple.descriptionEmbeddingId,
          fileDescriptionEmbeddingId: triple.fileDescriptionEmbeddingId,
          cachedWidth: width,
          generation,
          floor,
          complete,
        });
        pairsStored += merged.length;
      }
      sourcesRefreshed += 1;
      completed += 1;
      report?.({ phase: "similarity-cache", completed, total });
    }
    return { similarityMode: mode, width, minSimilarity: floor, sourcesRefreshed, pairsStored, skipped: false };
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
  public cachedSimilarToFunction(functionId: number, options: {
    includeDescriptions?: boolean;
    limit: number;
    minSimilarity: number;
    maxSimilarity?: number;
    excludePaths?: readonly string[];
    minLines?: number;
    nameRegex?: string;
  }): SimilarityResult[] {
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
   */
  public cachedSimilarityReader(options: {
    includeDescriptions?: boolean;
  } = {}): {
    similarToFunction: (functionId: number, query: {
      includeDescriptions?: boolean;
      limit: number;
      minSimilarity: number;
      maxSimilarity?: number;
      excludePaths?: readonly string[];
      minLines?: number;
      nameRegex?: string;
    }) => SimilarityResult[];
  } {
    const mode = options.includeDescriptions ? "code-description-file-average" : "code";
    const states = this.#database.similarityCacheStates(mode);
    const triples = this.#database.similarityCacheTriples();
    const tripleById = new Map(triples.map((triple) => [triple.functionId, triple]));
    const generation = this.#database.getGeneration();
    return {
      similarToFunction: (functionId, query) => {
        const live = (): SimilarityResult[] => this.similarToFunction(functionId, query);
        // The reader is fixed to one scoring mode; a mismatched query cannot be
        // served from this snapshot and falls back to a live query.
        if ((query.includeDescriptions === true) !== (mode !== "code")) {
          return live();
        }
        const state = states.get(functionId);
        if (!state || state.generation !== generation) {
          return live();
        }
        const triple = tripleById.get(functionId);
        if (!triple
          || triple.codeEmbeddingId !== state.codeEmbeddingId
          || triple.descriptionEmbeddingId !== state.descriptionEmbeddingId
          || triple.fileDescriptionEmbeddingId !== state.fileDescriptionEmbeddingId) {
          return live();
        }
        if (state.floor > query.minSimilarity) {
          return live();
        }
        compileNameRegex(query.nameRegex);
        const cached = this.#database.cachedSimilarityNeighbors(functionId, mode, {
          limit: query.limit,
          minSimilarity: query.minSimilarity,
          ...(query.maxSimilarity !== undefined ? { maxSimilarity: query.maxSimilarity } : {}),
          ...(query.minLines !== undefined ? { minLines: query.minLines } : {}),
          ...(query.nameRegex !== undefined ? { nameRegex: query.nameRegex } : {}),
          ...(query.excludePaths !== undefined ? { excludePaths: query.excludePaths } : {}),
        });
        if (cached.length >= query.limit) return cached;
        if (state.complete) return cached;
        return live();
      },
    };
  }

  public vectorForFile(filePath: string): number[] {
    return this.#database.vectorForFile(filePath);
  }

  public searchByVector(vector: readonly number[], options: {
    descriptionVector?: readonly number[];
    fileDescriptionVector?: readonly number[];
    limit: number;
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
