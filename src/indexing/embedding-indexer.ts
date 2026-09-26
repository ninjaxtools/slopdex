import { IndexDatabase } from "../storage/database.js";
import { CodeIndexError } from "../errors.js";
import type { EmbeddingProvider, IndexProgress } from "../types.js";
import {
  chunk,
  embeddingKey,
  forEachConcurrent,
  normalizeEmbeddingProfile,
  normalizeEmbeddingVector,
  throwIfAborted,
} from "../utils.js";
import type { PreparedCallable, PreparedFile, PreparedMarkdownChunk } from "./prepared.js";

type DescriptionAttacher = (files: PreparedFile[], signal?: AbortSignal) => Promise<unknown>;

export class EmbeddingIndexer {
  readonly #database: IndexDatabase;
  readonly #provider: EmbeddingProvider;
  readonly #embeddingBatchSize: number;
  readonly #parallelism: number;
  readonly #onProgress: ((progress: IndexProgress) => void) | undefined;
  readonly #attachDescriptions: DescriptionAttacher;

  public constructor(
    database: IndexDatabase,
    provider: EmbeddingProvider,
    embeddingBatchSize: number,
    parallelism: number,
    onProgress: ((progress: IndexProgress) => void) | undefined,
    attachDescriptions: DescriptionAttacher,
  ) {
    this.#database = database;
    this.#provider = provider;
    this.#embeddingBatchSize = embeddingBatchSize;
    this.#parallelism = parallelism;
    this.#onProgress = onProgress;
    this.#attachDescriptions = attachDescriptions;
  }

  public async attachEmbeddings(files: PreparedFile[], signal?: AbortSignal): Promise<number> {
    if (this.#database.descriptionsEnabled()) {
      await this.#attachDescriptions(files.filter((file) => !file.unavailable && file.language !== "markdown"), signal);
    }
    throwIfAborted(signal);
    const profile = JSON.stringify(normalizeEmbeddingProfile(this.#provider.profile));
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
      const vectors = await this.#provider.embedDocuments(
        batch.map(([, items]) => items[0]!.embeddingInput),
        { signal: workerSignal },
      );
      if (vectors.length !== batch.length) throw new CodeIndexError("Embedding provider returned an unexpected number of vectors.");
      vectors.forEach((vector, index) => {
        const converted = normalizeEmbeddingVector(vector, this.#provider.profile.dimensions);
        this.#database.storeEmbedding(batch[index]![0], converted);
        for (const item of batch[index]![1]) item.vector = converted;
      });
      completed += batch.length;
      this.#progress("vectors", completed, missing.length);
    }, signal);
    throwIfAborted(signal);
    return missing.length;
  }

  #progress(phase: IndexProgress["phase"], completed: number, total: number): void {
    this.#onProgress?.({ phase, completed, total });
  }
}
