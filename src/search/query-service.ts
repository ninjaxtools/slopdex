import { lstat, readFile } from "node:fs/promises";
import path from "node:path";

import type { IndexDatabase } from "../storage/database.js";
import { CodeIndexError } from "../errors.js";
import type { IndexedFileState } from "../indexing/prepared.js";
import type { GitRepository } from "../repository.js";
import type {
  DescribeContext,
  DescribeFile,
  DescribeFunction,
  DescribeOptions,
  EmbeddingProvider,
  MarkdownSearchOptions,
  MarkdownSearchResult,
  Reranker,
  SearchOptions,
  SearchResult,
  SimilarityResult,
  SimilaritySearchOptions,
} from "../types.js";
import {
  assertPositiveInteger,
  compileNameRegex,
  embeddingKey,
  normalizeEmbeddingProfile,
  normalizeEmbeddingVector,
  sha256,
  throwIfAborted,
} from "../utils.js";

const RERANK_CANDIDATE_MULTIPLIER = 5;

export interface VectorSearchOptions {
  descriptionVector?: readonly number[];
  fileDescriptionVector?: readonly number[];
  limit?: number;
  minSimilarity: number;
  maxSimilarity?: number;
  excludePaths?: readonly string[];
  minLines?: number;
  nameRegex?: string;
}

export interface FunctionSimilarityOptions {
  includeDescriptions?: boolean;
  limit: number;
  minSimilarity: number;
  maxSimilarity?: number;
  excludePaths?: readonly string[];
  minLines?: number;
  nameRegex?: string;
}

export class QueryService {
  readonly #database: IndexDatabase;
  readonly #provider: EmbeddingProvider;
  readonly #reranker: Reranker | undefined;
  readonly #rootDir: string;
  readonly #git: GitRepository;

  public constructor(
    database: IndexDatabase,
    provider: EmbeddingProvider,
    reranker: Reranker | undefined,
    rootDir: string,
    git: GitRepository,
  ) {
    this.#database = database;
    this.#provider = provider;
    this.#reranker = reranker;
    this.#rootDir = rootDir;
    this.#git = git;
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
      repository: path.basename(this.#rootDir),
      query: options.query,
      minSimilarity,
      fullFileThreshold,
      files,
      functions,
      fileContentErrors,
    };
  }

  public similarToFunction(functionId: number, options: FunctionSimilarityOptions): SimilarityResult[] {
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

  public searchByVector(vector: readonly number[], options: VectorSearchOptions): SimilarityResult[] {
    return this.#database.searchVector(normalizeEmbeddingVector(vector, this.#provider.profile.dimensions), {
      ...options,
      ...(options.descriptionVector !== undefined
        ? { descriptionVector: normalizeEmbeddingVector(options.descriptionVector, this.#provider.profile.dimensions) } : {}),
      ...(options.fileDescriptionVector !== undefined
        ? { fileDescriptionVector: normalizeEmbeddingVector(options.fileDescriptionVector, this.#provider.profile.dimensions) } : {}),
    });
  }

  async #readIndexedSource(state: IndexedFileState): Promise<string> {
    if (state.sourceMode === "working-tree") {
      const sourcePath = path.join(this.#rootDir, state.path);
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
    if (!this.#reranker) return limit;
    if (limit === undefined) return this.#reranker.maximumCandidateCount;
    if (this.#reranker.maximumCandidateCount !== undefined && limit > this.#reranker.maximumCandidateCount) {
      throw new CodeIndexError(`${this.#reranker.profile.provider} reranker supports at most ${this.#reranker.maximumCandidateCount} results.`);
    }
    const preferred = this.#reranker.candidateCount === undefined
      ? limit * RERANK_CANDIDATE_MULTIPLIER
      : Math.max(limit, this.#reranker.candidateCount);
    return this.#reranker.maximumCandidateCount === undefined
      ? preferred
      : Math.min(preferred, this.#reranker.maximumCandidateCount);
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
    if (!this.#reranker || candidates.length === 0) return limit === undefined ? candidates : candidates.slice(0, limit);
    const rerankLimit = limit === undefined ? candidates.length : Math.min(limit, candidates.length);
    const rankings = await this.#reranker.rerank(query, documents, signal ? { limit: rerankLimit, signal } : { limit: rerankLimit });
    throwIfAborted(signal);
    const seen = new Set<number>();
    if (rankings.length !== rerankLimit) {
      throw new CodeIndexError(`${this.#reranker.profile.provider} returned invalid reranking results.`);
    }
    for (const { index, score } of rankings) {
      if (!Number.isInteger(index) || index < 0 || index >= candidates.length
        || typeof score !== "number" || !Number.isFinite(score) || seen.has(index)) {
        throw new CodeIndexError(`${this.#reranker.profile.provider} returned invalid reranking results.`);
      }
      seen.add(index);
    }
    return rankings.map(({ index, score }) => ({ ...candidates[index]!, rerankScore: score }));
  }

  async #queryEmbedding(query: string, signal?: AbortSignal): Promise<number[]> {
    const profile = JSON.stringify(normalizeEmbeddingProfile(this.#provider.profile));
    const key = embeddingKey(profile, "query", query);
    const cached = this.#database.cachedEmbedding(key);
    if (cached) return cached;
    const vector = normalizeEmbeddingVector(
      await this.#provider.embedQuery(query, signal ? { signal } : undefined),
      this.#provider.profile.dimensions,
    );
    this.#database.storeEmbedding(key, vector);
    return vector;
  }

  #descriptionScoringAvailable(): boolean {
    const status = this.#database.status();
    return status.descriptionsEnabled
      && status.descriptionCount === status.functionCount
      && status.fileDescriptionCount === status.describableFileCount;
  }
}
