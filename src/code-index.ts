import path from "node:path";

import { CodeIndexError } from "./errors.js";
import { GitRepository } from "./repository.js";
import { QueryService } from "./search/query-service.js";
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
import { IndexDatabase } from "./storage/database.js";
import { DescriptionIndexer } from "./descriptions/description-indexer.js";
import { EmbeddingIndexer } from "./indexing/embedding-indexer.js";
import { FilePreparer } from "./indexing/file-preparer.js";
import { IndexUpdater } from "./indexing/index-updater.js";
import { SourceSelector } from "./indexing/source-selector.js";
import { isDescriptionProviderName } from "./descriptions/provider-registry.js";
import { OpenAIDescriptionProvider } from "./descriptions/openai.js";
import type {
  CodeIndexOptions,
  CrossSearchSourceFilter,
  DescribeContext,
  DescribeOptions,
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
  DEFAULT_PARALLELISM,
  normalizeEmbeddingProfile,
} from "./utils.js";

const DEFAULT_MAX_FILE_SIZE = 1024 * 1024;
const DEFAULT_BATCH_SIZE = 32;

export class CodeIndex {
  public readonly rootDir: string;
  public readonly indexPath: string;
  public readonly provider;
  public readonly reranker;
  public readonly descriptionProvider: DescriptionProvider;
  readonly #database: IndexDatabase;
  readonly #queryService: QueryService;
  readonly #similarityCache: SimilarityCache;
  readonly #sourceSelector: SourceSelector;
  readonly #descriptionIndexer: DescriptionIndexer;
  readonly #indexUpdater: IndexUpdater;

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
    const profile = normalizeEmbeddingProfile(options.provider.profile);
    assertPositiveInteger(profile.dimensions, "embedding dimensions");
    this.#database = new IndexDatabase(this.indexPath, this.rootDir, profile, options.readOnly ?? false);
    const parallelism = options.parallelism ?? DEFAULT_PARALLELISM;
    assertPositiveInteger(parallelism, "parallelism");
    this.#similarityCache = new SimilarityCache(this.#database, this, options.onProgress);
    const storedDescriptionProfile = this.#database.descriptionProfile();
    this.descriptionProvider = options.descriptionProvider ?? new OpenAIDescriptionProvider({
      ...(storedDescriptionProfile && isDescriptionProviderName(storedDescriptionProfile.provider)
        ? { provider: storedDescriptionProfile.provider, model: storedDescriptionProfile.model }
        : {}),
      parallelism,
      ...(options.verbose ? { verbose: true } : {}),
    });
    const policy = new SourcePolicy(options.include, options.exclude);
    const maxFileSize = options.maxFileSize ?? DEFAULT_MAX_FILE_SIZE;
    const embeddingBatchSize = options.embeddingBatchSize ?? DEFAULT_BATCH_SIZE;
    const onWarning = options.onWarning ?? console.warn;
    assertPositiveInteger(maxFileSize, "maxFileSize");
    assertPositiveInteger(embeddingBatchSize, "embeddingBatchSize");
    const git = new GitRepository(this.rootDir);
    const filePreparer = new FilePreparer(this.rootDir, maxFileSize, this.#database, onWarning);
    this.#descriptionIndexer = new DescriptionIndexer(
      this.#database,
      git,
      this.provider,
      this.descriptionProvider,
      this.rootDir,
      embeddingBatchSize,
      parallelism,
      options.onProgress,
    );
    const embeddingIndexer = new EmbeddingIndexer(
      this.#database,
      this.provider,
      embeddingBatchSize,
      parallelism,
      options.onProgress,
      (files, signal) => this.#descriptionIndexer.attachDescriptions(files, signal),
    );
    this.#indexUpdater = new IndexUpdater(
      this.rootDir,
      this.indexPath,
      this.#database,
      git,
      policy,
      filePreparer,
      embeddingIndexer,
    );
    this.#queryService = new QueryService(
      this.#database,
      this.provider,
      this.reranker,
      this.rootDir,
      git,
    );
    this.#sourceSelector = new SourceSelector(this.#database, git, this.rootDir, onWarning);
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
    return this.#indexUpdater.updateFiles(options);
  }

  public async updateFromWorkingTree(options: UpdateFromWorkingTreeOptions = {}): Promise<UpdateStats> {
    return this.#indexUpdater.updateFromWorkingTree(options);
  }

  public async updateFromGit(options: UpdateFromGitOptions = {}): Promise<UpdateStats> {
    return this.#indexUpdater.updateFromGit(options);
  }

  public async useDescriptions(options: { signal?: AbortSignal } = {}): Promise<DescriptionStats> {
    return this.#descriptionIndexer.useDescriptions(options);
  }

  public async reindexFiles(options: ReindexFilesOptions = {}): Promise<ReindexFilesStats> {
    return this.#descriptionIndexer.reindexFiles(options);
  }

  public disableDescriptions(): DescriptionStats {
    return this.#descriptionIndexer.disableDescriptions();
  }

  public async searchDescription(options: SimilaritySearchOptions): Promise<SimilarityResult[]> {
    return this.#queryService.searchDescription(options);
  }

  public async searchCode(options: SimilaritySearchOptions): Promise<SimilarityResult[]> {
    return this.#queryService.searchCode(options);
  }

  public async searchMarkdown(options: MarkdownSearchOptions): Promise<MarkdownSearchResult[]> {
    return this.#queryService.searchMarkdown(options);
  }

  public async search(options: SearchOptions): Promise<SearchResult[]> {
    return this.#queryService.search(options);
  }

  public async similaritySearch(options: SimilaritySearchOptions): Promise<SimilarityResult[]> {
    return this.#queryService.similaritySearch(options);
  }

  /**
   * Gather the relevant files, callables, descriptions, and optionally complete
   * file sources for a natural-language request. The result is a discovery
   * context for a description model, not an implementation plan.
   */
  public async describe(options: DescribeOptions): Promise<DescribeContext> {
    return this.#queryService.describe(options);
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
    return this.#queryService.similarToFunction(functionId, options);
  }

  public vectorForFunction(functionId: number, kind: "code" | "description" = "code"): number[] {
    return this.#queryService.vectorForFunction(functionId, kind);
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
    return this.#queryService.vectorForFile(filePath);
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
    return this.#queryService.searchByVector(vector, options);
  }

  public async sourceFunctions(filter: CrossSearchSourceFilter): Promise<IndexedFunction[]> {
    return this.#sourceSelector.sourceFunctions(filter);
  }
}
