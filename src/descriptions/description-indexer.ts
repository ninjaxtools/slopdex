import { readFile } from "node:fs/promises";
import path from "node:path";

import { IndexDatabase } from "../storage/database.js";
import { CodeIndexError } from "../errors.js";
import type { GitRepository } from "../repository.js";
import type {
  DescriptionProvider,
  DescriptionStats,
  EmbeddingProvider,
  IndexedFunction,
  IndexProgress,
  ReindexFilesOptions,
  ReindexFilesStats,
} from "../types.js";
import {
  chunk,
  forEachConcurrent,
  groupBy,
  normalizeEmbeddingProfile,
  normalizeEmbeddingVector,
  sha256,
  throwIfAborted,
} from "../utils.js";
import type { IndexedFileState, PreparedCallable, PreparedDescription, PreparedFile } from "../indexing/prepared.js";

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

interface AttachDescriptionOptions {
  refreshFileDescriptions?: boolean;
  forceCallableDescriptions?: boolean;
  reindexCache?: boolean;
  skipCallables?: boolean;
  ignoreLiveDescriptions?: boolean;
}

export class DescriptionIndexer {
  readonly #database: IndexDatabase;
  readonly #git: GitRepository;
  readonly #embeddingProvider: EmbeddingProvider;
  readonly #descriptionProvider: DescriptionProvider;
  readonly #rootDir: string;
  readonly #embeddingBatchSize: number;
  readonly #parallelism: number;
  readonly #onProgress: ((progress: IndexProgress) => void) | undefined;

  public constructor(
    database: IndexDatabase,
    git: GitRepository,
    embeddingProvider: EmbeddingProvider,
    descriptionProvider: DescriptionProvider,
    rootDir: string,
    embeddingBatchSize: number,
    parallelism: number,
    onProgress: ((progress: IndexProgress) => void) | undefined,
  ) {
    this.#database = database;
    this.#git = git;
    this.#embeddingProvider = embeddingProvider;
    this.#descriptionProvider = descriptionProvider;
    this.#rootDir = rootDir;
    this.#embeddingBatchSize = embeddingBatchSize;
    this.#parallelism = parallelism;
    this.#onProgress = onProgress;
  }

  public async attachDescriptions(
    files: PreparedFile[],
    signal?: AbortSignal,
    options: AttachDescriptionOptions = {},
  ): Promise<{ descriptionsCreated: number; fileDescriptionsCreated: number }> {
    const profile = descriptionProfileJson(this.#descriptionProvider.profile);
    const embeddingProfile = JSON.stringify(normalizeEmbeddingProfile(this.#embeddingProvider.profile));
    const prepared = new Map<string, PreparedDescription>();
    const repository = path.basename(this.#rootDir);
    let descriptionsCreated = 0;
    let fileDescriptionsCreated = 0;
    let pending = 0;
    const tasks: DescriptionTask[] = [];
    const strategy = this.#descriptionProvider.profile.strategyVersion;
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
        ? this.#descriptionProvider.startFile?.({ repository, path: file.path, fileSource: file.source })
        : undefined;
      let fileDescription = task.fileDescription;
      let fileDescriptionAddedToSession = false;
      if (task.refreshFileDescription && !fileDescription) {
        const generated = session
          ? await session.describeFile({ signal: workerSignal })
          : await this.#descriptionProvider.describeFile(
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
            : await this.#descriptionProvider.describe({ repository, callable, fileSource: file.source }, { signal: workerSignal });
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
        const vectors = await this.#embeddingProvider.embedDocuments(batch.map((value) => value.description), { signal: workerSignal });
        if (vectors.length !== batch.length) throw new CodeIndexError("Embedding provider returned an unexpected number of description vectors.");
        vectors.forEach((vector, index) => {
          const converted = normalizeEmbeddingVector(vector, this.#embeddingProvider.profile.dimensions);
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
    const functions = this.#database.allFunctions();
    const status = this.#database.status();
    const storedProfile = this.#database.descriptionProfile();
    const strategyChanged = (storedProfile?.strategyVersion ?? this.#descriptionProvider.profile.strategyVersion)
      !== this.#descriptionProvider.profile.strategyVersion;
    const profileChanged = descriptionProfileJson(storedProfile ?? this.#descriptionProvider.profile)
      !== descriptionProfileJson(this.#descriptionProvider.profile);
    if (this.#database.descriptionsEnabled()
      && !strategyChanged
      && functions.every((callable) => callable.descriptionEmbeddingId !== null)
      && status.fileDescriptionCount === status.describableFileCount) {
      if (profileChanged) this.#database.updateDescriptionProfile(this.#descriptionProvider.profile, generation);
      return { descriptionsCreated: 0, fileDescriptionsCreated: 0, descriptionsEnabled: true };
    }
    const files = await this.#descriptionFiles(this.#database.getFileStates().filter((file) => file.describable), functions, options.signal);
    const { descriptionsCreated, fileDescriptionsCreated } = await this.attachDescriptions(files, options.signal, {
      refreshFileDescriptions: strategyChanged,
      ...(strategyChanged ? { ignoreLiveDescriptions: true } : {}),
    });
    const ids = new Map(functions.map((callable) => [callable.identityKey, callable.id]));
    this.#database.enableDescriptions(files.flatMap((file) => file.callables.map((callable) => ({
      id: ids.get(callable.identityKey)!, description: callable.description!,
    }))), files, this.#descriptionProvider.profile, generation);
    return { descriptionsCreated, fileDescriptionsCreated, descriptionsEnabled: true };
  }

  public async reindexFiles(options: ReindexFilesOptions = {}): Promise<ReindexFilesStats> {
    throwIfAborted(options.signal);
    if (!this.#database.descriptionsEnabled()) throw new CodeIndexError("Descriptions are not enabled; run descriptions enable first.");
    const generation = this.#database.getGeneration();
    const states = this.#database.getFileStates()
      .filter((file) => file.describable
        && (file.fileDescriptionPath !== file.path || file.fileDescriptionContentHash !== file.contentHash));
    const files = await this.#descriptionFiles(states, this.#database.allFunctions(), options.signal);
    const created = await this.attachDescriptions(files, options.signal, {
      refreshFileDescriptions: true,
      ...(options.includeCallables !== undefined ? { forceCallableDescriptions: options.includeCallables } : {}),
      reindexCache: true,
      skipCallables: !options.includeCallables,
    });
    this.#database.updateDescriptions(files, options.includeCallables ?? false, generation, this.#descriptionProvider.profile);
    return { filesReindexed: files.length, ...created };
  }

  public disableDescriptions(): DescriptionStats {
    this.#database.disableDescriptions();
    return { descriptionsCreated: 0, fileDescriptionsCreated: 0, descriptionsEnabled: false };
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
        : await readFile(path.join(this.#rootDir, file.path));
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

  #progress(phase: IndexProgress["phase"], completed: number, total: number): void {
    this.#onProgress?.({ phase, completed, total });
  }
}

function descriptionProfileJson(profile: DescriptionProvider["profile"]): string {
  return JSON.stringify({
    provider: profile.provider,
    model: profile.model,
    strategyVersion: profile.strategyVersion,
  });
}

function prepareDescription(
  prepared: Map<string, PreparedDescription>,
  embeddingProfile: string,
  description: string,
  database: IndexDatabase,
): PreparedDescription {
  const key = sha256(`${embeddingProfile}\0document\0${description}`);
  const existing = prepared.get(key);
  if (existing) return existing;
  const value: PreparedDescription = { key, description };
  const vector = database.cachedEmbedding(key);
  if (vector) value.vector = vector;
  prepared.set(key, value);
  return value;
}
