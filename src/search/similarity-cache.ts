import type { IndexDatabase } from "../database.js";
import type { IndexProgress, IndexStatus, IndexedFunction, SimilarityResult } from "../types.js";
import { compileNameRegex, throwIfAborted } from "../utils.js";
import { analysisSimilarity } from "./similarity.js";

const DEFAULT_SIMILARITY_CACHE_WIDTH = 50;
const MAX_SIMILARITY_CACHE_WIDTH = 200;

export interface RefreshSimilarityCacheOptions {
  width?: number;
  /** Minimum similarity stored; pairs below it are never cached. Defaults to -1 (cache everything). */
  minSimilarity?: number;
  signal?: AbortSignal;
  /** Overrides the index-level onProgress for the cache-fill bar. */
  onProgress?: (progress: IndexProgress) => void;
}

export interface RefreshSimilarityCacheResult {
  similarityMode: string;
  width: number;
  minSimilarity: number;
  sourcesRefreshed: number;
  pairsStored: number;
  skipped: boolean;
}

export interface SimilarityCacheQuery {
  includeDescriptions?: boolean;
  limit: number;
  minSimilarity: number;
  maxSimilarity?: number;
  excludePaths?: readonly string[];
  minLines?: number;
  nameRegex?: string;
}

export interface SimilarityCacheReaderOptions {
  includeDescriptions?: boolean;
}

export interface SimilarityCacheReader {
  similarToFunction: (functionId: number, query: SimilarityCacheQuery) => SimilarityResult[];
}

export interface SimilarityCacheSource {
  status(): IndexStatus;
  allFunctions(): IndexedFunction[];
  similarToFunction(functionId: number, query: SimilarityCacheQuery): SimilarityResult[];
}

export interface SimilarityCacheInfo {
  cachedSources: number;
  cachedPairs: number;
}

export class SimilarityCache {
  readonly #database: IndexDatabase;
  readonly #source: SimilarityCacheSource;
  readonly #onProgress: ((progress: IndexProgress) => void) | undefined;

  public constructor(
    database: IndexDatabase,
    source: SimilarityCacheSource,
    onProgress: ((progress: IndexProgress) => void) | undefined,
  ) {
    this.#database = database;
    this.#source = source;
    this.#onProgress = onProgress;
  }

  public similarityCacheInfo(): SimilarityCacheInfo {
    return this.#database.similarityCacheInfo();
  }

  public async refreshSimilarityCache(options: RefreshSimilarityCacheOptions = {}): Promise<RefreshSimilarityCacheResult> {
    const width = Math.min(
      MAX_SIMILARITY_CACHE_WIDTH,
      Math.max(1, Math.floor(options.width ?? DEFAULT_SIMILARITY_CACHE_WIDTH)),
    );
    const floor = options.minSimilarity ?? -1;
    const mode = analysisSimilarity(this.#source.status()).similarityMode;
    const includeDescriptions = mode !== "code";
    if (this.#database.isReadOnly) {
      return { similarityMode: mode, width, minSimilarity: floor, sourcesRefreshed: 0, pairsStored: 0, skipped: true };
    }
    throwIfAborted(options.signal);
    const generation = this.#database.getGeneration();
    const triples = this.#database.similarityCacheTriples();
    if (triples.length === 0) {
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
    // Deletions cascade cached pairs away, including when an unrelated addition
    // or edit has already populated the dirty set.
    const counts = this.#database.similarityCacheCounts(mode);
    for (const triple of triples) {
      if ((counts.get(triple.functionId) ?? 0) < (states.get(triple.functionId)?.storedCount ?? 0)) {
        dirty.add(triple.functionId);
      }
    }
    // A previous refresh may have stopped after updating the dirty sources but
    // before reconciling their neighbors. Do not bless the remaining old rows
    // merely because every embedding triple now matches.
    const interrupted = new Set([...states.values()].map((state) => state.generation)).size > 1;
    let sourcesRefreshed = 0;
    let pairsStored = 0;
    if (dirty.size === 0 && !interrupted) {
      // Metadata-only updates can advance the index generation without changing
      // any scores. Keep the valid rows usable without recomputing their pairs.
      for (const [functionId, state] of states) {
        if (state.generation !== generation) {
          throwIfAborted(options.signal);
          this.#database.touchSimilarityCacheState(functionId, mode, { ...state, generation });
        }
      }
      return { similarityMode: mode, width, minSimilarity: floor, sourcesRefreshed, pairsStored, skipped: false };
    }
    const report = options.onProgress ?? this.#onProgress;
    const total = triples.length;
    let completed = 0;
    report?.({ phase: "similarity-cache", completed, total });
    const fullRefresh = states.size === 0 || expanding || interrupted || dirty.size > Math.max(8, Math.ceil(triples.length * 0.25));
    if (fullRefresh) {
      for (const triple of triples) {
        throwIfAborted(options.signal);
        const prior = states.get(triple.functionId);
        const keepWidth = Math.max(width, prior?.cachedWidth ?? width);
        const keepFloor = Math.min(floor, prior?.floor ?? floor);
        const scanned = this.#source.similarToFunction(triple.functionId, {
          ...(includeDescriptions ? { includeDescriptions: true } : {}),
          limit: Math.max(1, Math.min(keepWidth + 1, triples.length - 1)),
          minSimilarity: keepFloor,
        });
        const neighbors = scanned.slice(0, keepWidth);
        this.#database.storeSimilarityNeighbors(triple.functionId, mode, neighbors, {
          codeEmbeddingId: triple.codeEmbeddingId,
          descriptionEmbeddingId: triple.descriptionEmbeddingId,
          fileDescriptionEmbeddingId: triple.fileDescriptionEmbeddingId,
          cachedWidth: keepWidth,
          generation,
          floor: keepFloor,
          complete: scanned.length <= keepWidth,
        });
        sourcesRefreshed += 1;
        pairsStored += neighbors.length;
        completed += 1;
        report?.({ phase: "similarity-cache", completed, total });
      }
      return { similarityMode: mode, width, minSimilarity: floor, sourcesRefreshed, pairsStored, skipped: false };
    }
    const functionsById = new Map(this.#source.allFunctions().map((callable) => [callable.id, callable]));
    const dirtyNeighbors = new Map<number, Map<number, SimilarityResult>>();
    // Dirty scans must cover the widest band any clean row keeps, or merged
    // rows would silently lose pairs below the request floor.
    let scanFloor = floor;
    for (const state of states.values()) scanFloor = Math.min(scanFloor, state.floor);
    for (const dirtyId of dirty) {
      throwIfAborted(options.signal);
      const prior = states.get(dirtyId);
      const keepWidth = Math.max(width, prior?.cachedWidth ?? width);
      const full = this.#source.similarToFunction(dirtyId, {
        ...(includeDescriptions ? { includeDescriptions: true } : {}),
        limit: Math.max(1, triples.length - 1),
        minSimilarity: scanFloor,
      });
      const byTarget = new Map<number, SimilarityResult>();
      for (const match of full) byTarget.set(match.function.id, match);
      dirtyNeighbors.set(dirtyId, byTarget);
      const triple = tripleById.get(dirtyId)!;
      const top = full.slice(0, keepWidth);
      this.#database.storeSimilarityNeighbors(dirtyId, mode, top, {
        codeEmbeddingId: triple.codeEmbeddingId,
        descriptionEmbeddingId: triple.descriptionEmbeddingId,
        fileDescriptionEmbeddingId: triple.fileDescriptionEmbeddingId,
        cachedWidth: keepWidth,
        generation,
        floor: scanFloor,
        complete: full.length <= keepWidth,
      });
      sourcesRefreshed += 1;
      pairsStored += top.length;
      completed += 1;
      report?.({ phase: "similarity-cache", completed, total });
    }
    for (const triple of triples) {
      throwIfAborted(options.signal);
      if (dirty.has(triple.functionId)) continue;
      // Never narrow a row the read-repair path widened: keep the lowest
      // floor served and the widest entry stored, or every refresh demotes
      // repaired rows and every analysis run re-repairs them with full scans.
      const prior = states.get(triple.functionId);
      const keepFloor = Math.min(floor, prior?.floor ?? floor);
      const keepWidth = Math.max(width, prior?.cachedWidth ?? width);
      const cached = this.#database.cachedSimilarityNeighbors(triple.functionId, mode);
      if (!prior?.complete && cached.some((match) => dirty.has(match.function.id))) {
        // A dirty neighbor can fall below the old top-k boundary. The omitted
        // candidates are unknown, so merging dirty scores cannot reconstruct
        // the correct prefix, even if it still contains k rows.
        const scanned = this.#source.similarToFunction(triple.functionId, {
          includeDescriptions,
          limit: Math.min(keepWidth + 1, triples.length - 1),
          minSimilarity: keepFloor,
        });
        const neighbors = scanned.slice(0, keepWidth);
        this.#database.storeSimilarityNeighbors(triple.functionId, mode, neighbors, {
          ...triple,
          cachedWidth: keepWidth,
          generation,
          floor: keepFloor,
          complete: scanned.length <= keepWidth,
        });
        sourcesRefreshed += 1;
        pairsStored += neighbors.length;
        completed += 1;
        report?.({ phase: "similarity-cache", completed, total });
        continue;
      }
      const kept = cached.filter((match) => match.similarity >= keepFloor
        && !dirty.has(match.function.id) && functionsById.has(match.function.id));
      const added: SimilarityResult[] = [];
      for (const [dirtyId, byTarget] of dirtyNeighbors) {
        if (dirtyId === triple.functionId) continue;
        const match = byTarget.get(triple.functionId);
        const dirtyFunction = functionsById.get(dirtyId);
        if (match && match.similarity >= keepFloor && dirtyFunction) {
          const { function: _function, ...scores } = match;
          added.push({ ...scores, function: dirtyFunction });
        }
      }
      const plusOne = [...kept, ...added]
        .sort((left, right) => right.similarity - left.similarity || left.function.id - right.function.id)
        .slice(0, keepWidth + 1);
      const merged = plusOne.slice(0, keepWidth);
      // Completeness is per-row: the merge reuses this row's own pairs and
      // exact dirty scans covering its whole band, so it stays complete
      // exactly when it was complete and nothing was truncated. A global
      // check here would let one incomplete row flip the entire index to
      // incomplete on every refresh, and every later lookup would re-scan.
      const complete = plusOne.length <= keepWidth && (prior?.complete ?? false);
      const unchanged = merged.length === cached.length
        && merged.every((match, index) => match.function.id === cached[index]!.function.id
          && match.similarity === cached[index]!.similarity
          && match.codeSimilarity === cached[index]!.codeSimilarity
          && match.descriptionSimilarity === cached[index]!.descriptionSimilarity
          && match.fileDescriptionSimilarity === cached[index]!.fileDescriptionSimilarity);
      if (unchanged) {
        // Rows are untouched: record the preserved floor and width without
        // rewriting the neighbor rows.
        this.#database.touchSimilarityCacheState(triple.functionId, mode, {
          codeEmbeddingId: triple.codeEmbeddingId,
          descriptionEmbeddingId: triple.descriptionEmbeddingId,
          fileDescriptionEmbeddingId: triple.fileDescriptionEmbeddingId,
          cachedWidth: keepWidth,
          generation,
          floor: keepFloor,
          storedCount: merged.length,
          complete,
        });
      } else {
        this.#database.storeSimilarityNeighbors(triple.functionId, mode, merged, {
          codeEmbeddingId: triple.codeEmbeddingId,
          descriptionEmbeddingId: triple.descriptionEmbeddingId,
          fileDescriptionEmbeddingId: triple.fileDescriptionEmbeddingId,
          cachedWidth: keepWidth,
          generation,
          floor: keepFloor,
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

  public cachedSimilarityReader(options: SimilarityCacheReaderOptions = {}): SimilarityCacheReader {
    const mode = options.includeDescriptions ? "code-description-file-average" : "code";
    const states = this.#database.similarityCacheStates(mode);
    const triples = this.#database.similarityCacheTriples();
    const tripleById = new Map(triples.map((triple) => [triple.functionId, triple]));
    const generation = this.#database.getGeneration();
    const readFiltered = (functionId: number, query: SimilarityCacheQuery): SimilarityResult[] => this.#database.cachedSimilarityNeighbors(functionId, mode, {
      limit: query.limit,
      minSimilarity: query.minSimilarity,
      ...(query.maxSimilarity !== undefined ? { maxSimilarity: query.maxSimilarity } : {}),
      ...(query.minLines !== undefined ? { minLines: query.minLines } : {}),
      ...(query.nameRegex !== undefined ? { nameRegex: query.nameRegex } : {}),
      ...(query.excludePaths !== undefined ? { excludePaths: query.excludePaths } : {}),
    });
    return {
      similarToFunction: (functionId, query) => {
        const live = (): SimilarityResult[] => this.#source.similarToFunction(functionId, query);
        // The reader is fixed to one scoring mode; a mismatched query cannot be
        // served from this snapshot and falls back to a live query.
        if ((query.includeDescriptions === true) !== (mode !== "code")) {
          return live();
        }
        compileNameRegex(query.nameRegex);
        const triple = tripleById.get(functionId);
        if (!triple) {
          return live();
        }
        const state = states.get(functionId);
        const tripleMatches = !!state
          && triple.codeEmbeddingId === state.codeEmbeddingId
          && triple.descriptionEmbeddingId === state.descriptionEmbeddingId
          && triple.fileDescriptionEmbeddingId === state.fileDescriptionEmbeddingId;
        if (state && (!tripleMatches || state.generation !== generation)) {
          return live();
        }
        if (state && state.floor <= query.minSimilarity) {
          const cached = readFiltered(functionId, query);
          if (cached.length >= query.limit) return cached;
          if (state.complete) return cached;
          if (state.storedCount >= MAX_SIMILARITY_CACHE_WIDTH) return live();
        }
        // Repair requires a writable index. Otherwise its computed rows cannot
        // be persisted, and reading the old cache again would lose matches.
        if (this.#database.isReadOnly || this.#database.getGeneration() !== generation) {
          return live();
        }
        // Repair never narrows: entries keep the lowest floor they have served,
        // so alternating thresholds cannot thrash the cache.
        const repairFloor = Math.min(query.minSimilarity, state?.floor ?? query.minSimilarity);
        const width = MAX_SIMILARITY_CACHE_WIDTH;
        const scanned = this.#source.similarToFunction(functionId, {
          ...(mode !== "code" ? { includeDescriptions: true } : {}),
          limit: Math.max(1, Math.min(width + 1, tripleById.size - 1)),
          minSimilarity: repairFloor,
        });
        const neighbors = scanned.slice(0, width);
        const complete = scanned.length <= width;
        this.#database.storeSimilarityNeighbors(functionId, mode, neighbors, {
          codeEmbeddingId: triple.codeEmbeddingId,
          descriptionEmbeddingId: triple.descriptionEmbeddingId,
          fileDescriptionEmbeddingId: triple.fileDescriptionEmbeddingId,
          cachedWidth: width,
          generation,
          floor: repairFloor,
          complete,
        });
        states.set(functionId, {
          codeEmbeddingId: triple.codeEmbeddingId,
          descriptionEmbeddingId: triple.descriptionEmbeddingId,
          fileDescriptionEmbeddingId: triple.fileDescriptionEmbeddingId,
          cachedWidth: width,
          generation,
          floor: repairFloor,
          storedCount: neighbors.length,
          complete,
        });
        const cached = readFiltered(functionId, query);
        if (cached.length >= query.limit) return cached;
        if (complete) return cached;
        return live();
      },
    };
  }
}
