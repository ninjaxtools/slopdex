import { lstat, readFile, readdir } from "node:fs/promises";
import path from "node:path";

import { CodeIndexError, GitDivergenceError } from "./errors.js";
import { GitRepository, type GitChange, type GitTreeEntry } from "./git/repository.js";
import { GitignoreRules } from "./gitignore.js";
import { CALLABLE_PARSER_CACHE_VERSION, languageForPath, parseCallables, parseFileCallables } from "./parser/callable-parser.js";
import { SourcePolicy } from "./source-policy.js";
import { IndexDatabase, type IndexedFileState, type PreparedCallable, type PreparedDescription, type PreparedFile } from "./storage/database.js";
import { isDescriptionProviderName, OpenAIDescriptionProvider } from "./descriptions/openai.js";
import type {
  CodeIndexOptions,
  CrossSearchSourceFilter,
  EmbeddingProfile,
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
import { assertPositiveInteger, chunk, compileNameRegex, normalizeEmbeddingVector, normalizeRelativePath, sha256, throwIfAborted } from "./utils.js";

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
  readonly #policy: SourcePolicy;
  readonly #maxFileSize: number;
  readonly #embeddingBatchSize: number;
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
    const storedDescriptionProfile = this.#database.descriptionProfile();
    this.descriptionProvider = options.descriptionProvider ?? new OpenAIDescriptionProvider({
      ...(storedDescriptionProfile && isDescriptionProviderName(storedDescriptionProfile.provider)
        ? { provider: storedDescriptionProfile.provider, model: storedDescriptionProfile.model }
        : {}),
    });
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
    const embeddingsCreated = await this.#attachEmbeddings(prepared, options.signal);
    await gitignore.assertUnchanged(options.signal);
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
        if (indexed?.sourceMode === "git" && indexed.blobOid === entry.oid && !diagnosticsScan && !retryPaths.has(entry.path)) continue;
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
          (diagnosticsScan || retryPaths.has(targetPath) || !indexed || (indexed.sourceMode === "git" && indexed.blobOid !== entry.oid))
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
      if (!previousPath && indexed?.sourceMode === "git" && indexed.blobOid === entry.oid && !diagnosticsScan && !retryPaths.has(filePath)) upserts.delete(filePath);
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
      if (descriptionProfileJson(this.#database.descriptionProfile()!) !== descriptionProfileJson(this.descriptionProvider.profile)) {
        throw new CodeIndexError("Description provider or model differs from this index; run descriptions enable (useDescriptions() in the library) with the new provider first.");
      }
      await this.#attachDescriptions(files.filter((file) => !file.unavailable), signal);
    }
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
    for (const batch of chunk(missing, this.#embeddingBatchSize)) {
      throwIfAborted(signal);
      const vectors = await this.provider.embedDocuments(batch.map(([, callables]) => callables[0]!.embeddingInput), signal ? { signal } : undefined);
      if (vectors.length !== batch.length) throw new CodeIndexError("Embedding provider returned an unexpected number of vectors.");
      vectors.forEach((vector, index) => {
        const converted = normalizeEmbeddingVector(vector, this.provider.profile.dimensions);
        this.#database.storeEmbedding(batch[index]![0], converted);
        for (const callable of batch[index]![1]) callable.vector = converted;
      });
    }
    throwIfAborted(signal);
    return missing.length;
  }

  async #attachDescriptions(
    files: PreparedFile[],
    signal?: AbortSignal,
    options: { refreshFileDescriptions?: boolean; forceCallableDescriptions?: boolean; reindexCache?: boolean; skipCallables?: boolean } = {},
  ): Promise<{ descriptionsCreated: number; fileDescriptionsCreated: number }> {
    const profile = descriptionProfileJson(this.descriptionProvider.profile);
    const embeddingProfile = JSON.stringify(normalizeProfile(this.provider.profile));
    const prepared = new Map<string, PreparedDescription>();
    let descriptionsCreated = 0;
    let fileDescriptionsCreated = 0;
    const repository = path.basename(this.rootDir);
    for (const file of files) {
      const storedFileDescription = this.#database.fileDescription(file.replacePath ?? file.previousPath ?? file.path);
      const fileDescriptionKey = sha256(`${profile}\0${repository}\0${file.path}\0${file.contentHash}\0file`);
      const fileCacheKey = options.reindexCache
        ? sha256(`${fileDescriptionKey}\0reindex\0${storedFileDescription?.description ?? ""}`)
        : fileDescriptionKey;
      const refreshFileDescription = options.refreshFileDescriptions || !storedFileDescription;
      let fileDescription = refreshFileDescription
        ? this.#database.cachedDescription(fileCacheKey)
        : storedFileDescription.description;
      let fileDescriptionAddedToSession = false;
      const descriptions = (options.skipCallables ? [] : file.callables).map((callable) => {
        const descriptionKey = sha256(`${profile}\0${repository}\0${file.path}\0${file.contentHash}\0${callable.identityKey}\0${callable.sourceHash}`);
        const cacheKey = options.reindexCache
          ? sha256(`${descriptionKey}\0reindex\0${this.#database.functionDescription(callable.identityKey) ?? ""}`)
          : descriptionKey;
        return {
          callable,
          descriptionKey,
          cacheKey,
          description: options.forceCallableDescriptions
            ? this.#database.cachedDescription(cacheKey)
            : this.#database.cachedDescription(descriptionKey),
        };
      });
      const session = refreshFileDescription || descriptions.some(({ description }) => !description)
        ? this.descriptionProvider.startFile?.({ repository, path: file.path, fileSource: file.source })
        : undefined;
      if (refreshFileDescription && !fileDescription) {
        const generated = session
          ? await session.describeFile(signal ? { signal } : undefined)
          : await this.descriptionProvider.describeFile(
            { repository, path: file.path, fileSource: file.source },
            signal ? { signal } : undefined,
          );
        if (typeof generated !== "string" || !generated.trim()) {
          throw new CodeIndexError("Description provider does not support file descriptions or returned an empty description.");
        }
        fileDescription = this.#database.storeDescription(fileCacheKey, generated.trim());
        fileDescriptionsCreated += 1;
        fileDescriptionAddedToSession = session !== undefined;
      }
      if (session && fileDescription && !fileDescriptionAddedToSession) session.replayFile(fileDescription);
      if (fileDescription) {
        const value = prepareDescription(prepared, embeddingProfile, fileDescription, this.#database);
        file.fileDescription = {
          path: refreshFileDescription ? file.path : storedFileDescription.path,
          contentHash: refreshFileDescription ? file.contentHash : storedFileDescription.contentHash,
          descriptionKey: fileDescriptionKey,
          value,
        };
      }
      for (const entry of descriptions) {
        throwIfAborted(signal);
        const { callable, descriptionKey, cacheKey } = entry;
        let { description } = entry;
        if (!description) {
          const generated = session
            ? await session.describe(callable, signal ? { signal } : undefined)
            : await this.descriptionProvider.describe({ repository, callable, fileSource: file.source }, signal ? { signal } : undefined);
          if (typeof generated !== "string" || !generated.trim()) throw new CodeIndexError("Description provider returned an empty description.");
          description = this.#database.storeDescription(cacheKey, generated.trim());
          descriptionsCreated += 1;
        } else {
          session?.replay(callable, description);
        }
        callable.descriptionKey = descriptionKey;
        callable.description = prepareDescription(prepared, embeddingProfile, description, this.#database);
      }
    }
    const missing = [...prepared.values()].filter((description) => !description.vector);
    for (const batch of chunk(missing, this.#embeddingBatchSize)) {
      throwIfAborted(signal);
      const vectors = await this.provider.embedDocuments(batch.map((value) => value.description), signal ? { signal } : undefined);
      if (vectors.length !== batch.length) throw new CodeIndexError("Embedding provider returned an unexpected number of description vectors.");
      vectors.forEach((vector, index) => {
        const converted = normalizeEmbeddingVector(vector, this.provider.profile.dimensions);
        this.#database.storeEmbedding(batch[index]!.key, converted);
        batch[index]!.vector = converted;
      });
    }
    throwIfAborted(signal);
    return { descriptionsCreated, fileDescriptionsCreated };
  }

  public async useDescriptions(options: { signal?: AbortSignal } = {}): Promise<DescriptionStats> {
    throwIfAborted(options.signal);
    const generation = this.#database.getGeneration();
    const functions = this.allFunctions();
    const status = this.status();
    const profileChanged = descriptionProfileJson(this.#database.descriptionProfile() ?? this.descriptionProvider.profile)
      !== descriptionProfileJson(this.descriptionProvider.profile);
    if (this.#database.descriptionsEnabled()
      && !profileChanged
      && functions.every((callable) => callable.descriptionEmbeddingId !== null)
      && status.fileDescriptionCount === status.describableFileCount) {
      return { descriptionsCreated: 0, fileDescriptionsCreated: 0, descriptionsEnabled: true };
    }
    const files = await this.#descriptionFiles(this.#database.getFileStates().filter((file) => file.describable), functions, options.signal);
    const { descriptionsCreated, fileDescriptionsCreated } = await this.#attachDescriptions(files, options.signal, {
      refreshFileDescriptions: profileChanged,
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
    if (descriptionProfileJson(this.#database.descriptionProfile()!) !== descriptionProfileJson(this.descriptionProvider.profile)) {
      throw new CodeIndexError("Description provider or model differs from this index; run descriptions enable first.");
    }
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
    this.#database.updateDescriptions(files, options.includeCallables ?? false, generation);
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
    return this.reranker.candidateCount === undefined
      ? limit * RERANK_CANDIDATE_MULTIPLIER
      : Math.max(limit, this.reranker.candidateCount);
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
