import { readdir } from "node:fs/promises";
import path from "node:path";

import { IndexDatabase } from "../storage/database.js";
import { CodeIndexError, GitDivergenceError } from "../errors.js";
import { GitignoreRules } from "../gitignore.js";
import { GitRepository, type GitChange, type GitTreeEntry } from "../repository.js";
import { SourcePolicy } from "../source-policy.js";
import type {
  UpdateFilesOptions,
  UpdateFromGitOptions,
  UpdateFromWorkingTreeOptions,
  UpdateStats,
} from "../types.js";
import { normalizeRelativePath, throwIfAborted } from "../utils.js";
import { EmbeddingIndexer } from "./embedding-indexer.js";
import { FilePreparer } from "./file-preparer.js";
import type { IndexedFileState, PreparedFile } from "./prepared.js";

export class IndexUpdater {
  readonly #indexPath: string;
  readonly #database: IndexDatabase;
  readonly #git: GitRepository;
  readonly #policy: SourcePolicy;
  readonly #filePreparer: FilePreparer;
  readonly #embeddingIndexer: EmbeddingIndexer;

  public constructor(
    public readonly rootDir: string,
    indexPath: string,
    database: IndexDatabase,
    git: GitRepository,
    policy: SourcePolicy,
    filePreparer: FilePreparer,
    embeddingIndexer: EmbeddingIndexer,
  ) {
    this.#indexPath = indexPath;
    this.#database = database;
    this.#git = git;
    this.#policy = policy;
    this.#filePreparer = filePreparer;
    this.#embeddingIndexer = embeddingIndexer;
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
      const file = await this.#filePreparer.prepareWorkingFile(relativePath, {
        blobOid: null,
        sourceMode: "working-tree",
        indexedCommit: null,
        ...(originalPath ? { previousPath: originalPath } : {}),
        ...(renameSource ? { replacePath: renameSource } : {}),
      }, true);
      if (file) prepared.push(file);
    }
    let embeddingsCreated = await this.#embeddingIndexer.attachEmbeddings(prepared, options.signal);
    for (let attempt = 0; ; attempt += 1) {
      await gitignore.assertUnchanged(options.signal);
      const changed: PreparedFile[] = [];
      for (let index = 0; index < prepared.length; index += 1) {
        const file = prepared[index]!;
        if (!this.#filePreparer.workingFileChangedSynchronously(file)) continue;
        if (attempt >= 2) throw new CodeIndexError(`Source changed repeatedly while indexing: ${file.path}; retry the update.`);
        const refreshed = await this.#filePreparer.prepareWorkingFile(file.path, {
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
      embeddingsCreated += await this.#embeddingIndexer.attachEmbeddings(changed, options.signal);
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
    let embeddingsCreated = await this.#embeddingIndexer.attachEmbeddings(prepared, options.signal);

    while (true) {
      await gitignore.assertUnchanged(options.signal);
      const pathsAgain = await this.#workingTreeSourcePaths(gitignore, options.signal);
      if (JSON.stringify(pathsAgain) !== JSON.stringify(sourcePaths)) {
        sourcePaths = pathsAgain;
        prepared = await this.#prepareWorkingPaths(sourcePaths, options.signal);
        embeddingsCreated += await this.#embeddingIndexer.attachEmbeddings(prepared, options.signal);
        continue;
      }
      const changed: PreparedFile[] = [];
      for (let index = 0; index < prepared.length; index += 1) {
        const file = prepared[index]!;
        if (!await this.#filePreparer.workingFileChanged(file)) continue;
        const refreshed = await this.#filePreparer.prepareWorkingFile(file.path, {
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
      embeddingsCreated += await this.#embeddingIndexer.attachEmbeddings(changed, options.signal);
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
    const indexArtifacts = this.#indexArtifacts();
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
          && (entry.size > this.#filePreparer.maxFileSize) === tooLargePaths.has(entry.path)) continue;
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
          && (entry.size > this.#filePreparer.maxFileSize) !== tooLargePaths.has(targetPath);
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
        && (entry.size > this.#filePreparer.maxFileSize) === tooLargePaths.has(filePath)) upserts.delete(filePath);
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
      const file = await this.#filePreparer.prepareWorkingFile(relativePath, {
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
      if (entry.size > this.#filePreparer.maxFileSize) {
        prepared.push(this.#filePreparer.failedFile(entry.path, "file-too-large", `File exceeds maxFileSize (${this.#filePreparer.maxFileSize} bytes).`, provenance, entry.size));
        continue;
      }
      try {
        const content = await this.#git.readBlob(entry.oid);
        prepared.push(this.#filePreparer.prepareFile(entry.path, content, provenance));
      } catch (error) {
        prepared.push(this.#filePreparer.failedFile(entry.path, "read-error", error instanceof Error ? error.message : String(error), provenance, entry.size));
      }
    }
    let embeddingsCreated = await this.#embeddingIndexer.attachEmbeddings([...prepared, ...workingPrepared], options.signal);
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
        if (!await this.#filePreparer.workingFileChanged(file)) continue;
        const refreshed = await this.#filePreparer.prepareWorkingFile(file.path, {
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
        const refreshed = await this.#filePreparer.prepareWorkingFile(relativePath, {
          blobOid: null,
          sourceMode: "working-tree",
          indexedCommit: null,
        });
        if (refreshed && !refreshed.unavailable) return await this.updateFromGit(options);
      }
      if (changed.length > 0) {
        embeddingsCreated += await this.#embeddingIndexer.attachEmbeddings(changed, options.signal);
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
    const indexArtifacts = new Set(this.#indexArtifacts());
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
      const file = await this.#filePreparer.prepareWorkingFile(relativePath, {
        blobOid: null,
        sourceMode: "working-tree",
        indexedCommit: null,
      });
      if (file) prepared.push(file);
    }
    return prepared;
  }

  #indexArtifacts(): string[] {
    const relativeIndexPath = path.relative(this.rootDir, this.#indexPath).replaceAll(path.sep, "/");
    return relativeIndexPath && !relativeIndexPath.startsWith("../")
      ? [relativeIndexPath, `${relativeIndexPath}-shm`, `${relativeIndexPath}-wal`, `${relativeIndexPath}-journal`]
      : [];
  }
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
