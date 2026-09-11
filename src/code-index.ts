import { lstat, readFile, readdir } from "node:fs/promises";
import path from "node:path";

import { CodeIndexError, GitDivergenceError } from "./errors.js";
import { GitRepository, type GitChange, type GitTreeEntry } from "./git/repository.js";
import { GitignoreRules } from "./gitignore.js";
import { languageForPath, parseCallables, parseFileCallables } from "./parser/callable-parser.js";
import { SourcePolicy } from "./source-policy.js";
import { IndexDatabase, type PreparedCallable, type PreparedFile, type PreparedSummary } from "./storage/database.js";
import { OpenAISummaryProvider } from "./summaries/openai.js";
import type {
  CodeIndexOptions,
  CrossSearchSourceFilter,
  EmbeddingProfile,
  IndexStatus,
  IndexedFunction,
  IndexingError,
  SimilarityResult,
  SimilaritySearchOptions,
  SummaryProvider,
  SummaryStats,
  UpdateFilesOptions,
  UpdateFromGitOptions,
  UpdateFromWorkingTreeOptions,
  UpdateStats,
} from "./types.js";
import { assertPositiveInteger, chunk, compileNameRegex, normalizeEmbeddingVector, normalizeRelativePath, sha256, throwIfAborted } from "./utils.js";

const DEFAULT_MAX_FILE_SIZE = 1024 * 1024;
const DEFAULT_BATCH_SIZE = 32;

export class CodeIndex {
  public readonly rootDir: string;
  public readonly indexPath: string;
  public readonly provider;
  public readonly summaryProvider: SummaryProvider;
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
    this.summaryProvider = options.summaryProvider ?? new OpenAISummaryProvider({
      ...(this.#database.summaryProfile()?.provider === "openai" ? { model: this.#database.summaryProfile()!.model } : {}),
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
    await this.#attachEmbeddings(prepared, options.signal);
    await gitignore.assertUnchanged(options.signal);
    return this.#database.applyUpdate({
      files: prepared,
      deletePaths: [...deletePaths, ...renameMap.values()],
      expectedGeneration: generation,
    });
  }

  public async updateFromWorkingTree(options: UpdateFromWorkingTreeOptions = {}): Promise<UpdateStats> {
    throwIfAborted(options.signal);
    const generation = this.#database.getGeneration();
    const indexedFiles = this.#database.getFileStates();
    const gitignore = GitignoreRules.workingTree(this.rootDir);
    const sourcePaths = await this.#workingTreeSourcePaths(gitignore, options.signal);
    const prepared: PreparedFile[] = [];
    const skippedPaths = new Set<string>();
    for (const relativePath of sourcePaths) {
      throwIfAborted(options.signal);
      const file = await this.#prepareWorkingFile(relativePath, {
        blobOid: null,
        sourceMode: "working-tree",
        indexedCommit: null,
      });
      if (!file || file.errors.some((error) => error.code === "file-too-large")) skippedPaths.add(relativePath);
      if (file) prepared.push(file);
    }
    await this.#attachEmbeddings(prepared, options.signal);

    await gitignore.assertUnchanged(options.signal);
    const pathsAgain = await this.#workingTreeSourcePaths(gitignore, options.signal);
    if (JSON.stringify(pathsAgain) !== JSON.stringify(sourcePaths)) {
      throw new CodeIndexError("Working-tree files changed while indexing; retry the update.");
    }
    for (const file of prepared) {
      if (file.unavailable) continue;
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

    await gitignore.assertUnchanged(options.signal);
    return this.#database.applyUpdate({
      files: prepared,
      deletePaths: indexedFiles.map((file) => file.path),
      checkpoint: null,
      expectedGeneration: generation,
      completeDiagnosticsScan: true,
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
      if (file.errors.some((error) => error.code === "file-too-large")) skippedWorkingPaths.add(relativePath);
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
    await this.#attachEmbeddings([...prepared, ...workingPrepared], options.signal);
    const noChanges = (
      !reconcileAll
      && !diagnosticsScan
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
        if (file.unavailable) continue;
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
      await gitignore.assertUnchanged(options.signal);
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

  #prepareFile(
    relativePath: string,
    buffer: Buffer,
    provenance: Pick<PreparedFile, "blobOid" | "sourceMode" | "indexedCommit" | "previousPath" | "replacePath">,
  ): PreparedFile {
    const content = buffer.toString("utf8");
    const { callables, errors } = parseFileCallables(relativePath, content, this.#onWarning);
    const language = languageForPath(relativePath) ?? path.extname(relativePath).slice(1);
    return {
      path: relativePath,
      contentHash: sha256(content),
      blobOid: provenance.blobOid,
      sourceMode: provenance.sourceMode,
      indexedCommit: provenance.indexedCommit,
      language,
      byteSize: buffer.byteLength,
      source: content,
      ...(provenance.previousPath ? { previousPath: provenance.previousPath } : {}),
      ...(provenance.replacePath ? { replacePath: provenance.replacePath } : {}),
      callables: callables as PreparedCallable[],
      errors,
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

  async #attachEmbeddings(files: PreparedFile[], signal?: AbortSignal): Promise<void> {
    if (this.#database.summariesEnabled()) {
      if (JSON.stringify(this.#database.summaryProfile()) !== JSON.stringify(this.summaryProvider.profile)) {
        throw new CodeIndexError("Summary provider or model differs from this index; run use-summaries (useSummaries() in the library) with the new provider first.");
      }
      await this.#attachSummaries(files, signal);
    }
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
    throwIfAborted(signal);
  }

  async #attachSummaries(files: PreparedFile[], signal?: AbortSignal): Promise<number> {
    const profile = JSON.stringify(this.summaryProvider.profile);
    const embeddingProfile = JSON.stringify(normalizeProfile(this.provider.profile));
    const prepared = new Map<string, PreparedSummary>();
    for (const file of files) {
      for (const callable of file.callables) {
        throwIfAborted(signal);
        const key = sha256(`${profile}\0${embeddingProfile}\0${file.path}\0${file.contentHash}\0${callable.identityKey}\0${callable.sourceHash}`);
        let purpose = prepared.get(key) ?? this.#database.cachedSummary(key);
        if (!purpose) {
          const summary = await this.summaryProvider.summarize({
            repository: path.basename(this.rootDir),
            callable,
            fileSource: file.source,
          }, signal ? { signal } : undefined);
          if (typeof summary !== "string" || !summary.trim()) throw new CodeIndexError("Summary provider returned an empty summary.");
          purpose = { key, summary: summary.trim() };
          prepared.set(key, purpose);
        }
        callable.purpose = purpose;
      }
    }
    for (const batch of chunk([...prepared.values()], this.#embeddingBatchSize)) {
      throwIfAborted(signal);
      const vectors = await this.provider.embedDocuments(batch.map((value) => value.summary), signal ? { signal } : undefined);
      if (vectors.length !== batch.length) throw new CodeIndexError("Embedding provider returned an unexpected number of summary vectors.");
      vectors.forEach((vector, index) => {
        batch[index]!.vector = normalizeEmbeddingVector(vector, this.provider.profile.dimensions);
      });
    }
    throwIfAborted(signal);
    return prepared.size;
  }

  public async useSummaries(options: { signal?: AbortSignal } = {}): Promise<SummaryStats> {
    throwIfAborted(options.signal);
    const generation = this.#database.getGeneration();
    const functions = this.allFunctions();
    if (this.#database.summariesEnabled()
      && JSON.stringify(this.#database.summaryProfile()) === JSON.stringify(this.summaryProvider.profile)
      && functions.every((callable) => callable.summaryEmbeddingId !== null)) {
      return { summariesCreated: 0, summariesEnabled: true };
    }
    const byPath = groupBy(functions, (callable) => callable.path);
    const files: PreparedFile[] = [];
    for (const file of this.#database.getFileStates()) {
      const callables = byPath.get(file.path);
      if (!callables?.length) continue;
      throwIfAborted(options.signal);
      const buffer = file.sourceMode === "git" && file.blobOid
        ? await this.#git.readBlob(file.blobOid)
        : await readFile(path.join(this.rootDir, file.path));
      const source = buffer.toString("utf8");
      if (sha256(source) !== file.contentHash) {
        throw new CodeIndexError(`Source changed since indexing: ${file.path}; update the index before enabling summaries.`);
      }
      files.push({
        path: file.path, contentHash: file.contentHash, blobOid: file.blobOid,
        sourceMode: file.sourceMode, indexedCommit: null, language: callables[0]!.language,
        byteSize: buffer.byteLength, source,
        errors: [],
        callables: callables.map((callable) => ({ ...callable, embeddingKey: "" })),
      });
    }
    const summariesCreated = await this.#attachSummaries(files, options.signal);
    const ids = new Map(functions.map((callable) => [callable.identityKey, callable.id]));
    this.#database.enableSummaries(files.flatMap((file) => file.callables.map((callable) => ({
      id: ids.get(callable.identityKey)!, purpose: callable.purpose!,
    }))), this.summaryProvider.profile, generation);
    return { summariesCreated, summariesEnabled: true };
  }

  public async searchSummary(options: SimilaritySearchOptions): Promise<SimilarityResult[]> {
    if (!this.#database.summariesEnabled()) throw new CodeIndexError("Summaries are not enabled; run use-summaries first.");
    if (!options.query.trim()) throw new CodeIndexError("query must not be empty.");
    const limit = options.limit ?? 10;
    assertPositiveInteger(limit, "limit");
    compileNameRegex(options.nameRegex);
    throwIfAborted(options.signal);
    const vector = await this.provider.embedQuery(options.query, options.signal ? { signal: options.signal } : undefined);
    throwIfAborted(options.signal);
    return this.#database.searchVector(normalizeEmbeddingVector(vector, this.provider.profile.dimensions), {
      summaries: true, limit, minSimilarity: options.minSimilarity ?? -1,
      ...(options.nameRegex !== undefined ? { nameRegex: options.nameRegex } : {}),
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
    });
  }

  public async similaritySearch(options: SimilaritySearchOptions): Promise<SimilarityResult[]> {
    if (!options.query.trim()) throw new CodeIndexError("query must not be empty.");
    const limit = options.limit ?? 10;
    assertPositiveInteger(limit, "limit");
    compileNameRegex(options.nameRegex);
    throwIfAborted(options.signal);
    const vector = await this.provider.embedQuery(options.query, options.signal ? { signal: options.signal } : undefined);
    return this.searchByVector(vector, {
      limit,
      ...(options.nameRegex !== undefined ? { nameRegex: options.nameRegex } : {}),
      minSimilarity: options.minSimilarity ?? -1,
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
    });
  }

  public similarToFunction(functionId: number, options: {
    includeSummaries?: boolean;
    limit: number;
    minSimilarity: number;
    maxSimilarity?: number;
    excludePaths?: readonly string[];
    minLines?: number;
    nameRegex?: string;
  }): SimilarityResult[] {
    const vector = this.#database.vectorForFunction(functionId);
    return this.#database.searchVector(vector, {
      ...(options.includeSummaries ? { summaryVector: this.#database.vectorForFunction(functionId, "summary") } : {}),
      limit: options.limit,
      minSimilarity: options.minSimilarity,
      ...(options.maxSimilarity !== undefined ? { maxSimilarity: options.maxSimilarity } : {}),
      excludeId: functionId,
      ...(options.excludePaths !== undefined ? { excludePaths: options.excludePaths } : {}),
      ...(options.minLines !== undefined ? { minLines: options.minLines } : {}),
      ...(options.nameRegex !== undefined ? { nameRegex: options.nameRegex } : {}),
    });
  }

  public vectorForFunction(functionId: number, kind: "code" | "summary" = "code"): number[] {
    return this.#database.vectorForFunction(functionId, kind);
  }

  public searchByVector(vector: readonly number[], options: {
    summaryVector?: readonly number[];
    limit: number;
    minSimilarity: number;
    maxSimilarity?: number;
    excludePaths?: readonly string[];
    minLines?: number;
    nameRegex?: string;
  }): SimilarityResult[] {
    return this.#database.searchVector(normalizeEmbeddingVector(vector, this.provider.profile.dimensions), {
      ...options,
      ...(options.summaryVector !== undefined
        ? { summaryVector: normalizeEmbeddingVector(options.summaryVector, this.provider.profile.dimensions) } : {}),
    });
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
