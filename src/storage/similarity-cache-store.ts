import type { DatabaseSync } from "node:sqlite";

import type { SimilarityResult } from "../types.js";
import { type FunctionRow, toIndexedFunction } from "./rows.js";

export interface SimilarityCacheState {
  codeEmbeddingId: number;
  descriptionEmbeddingId: number | null;
  fileDescriptionEmbeddingId: number | null;
  cachedWidth: number;
  generation: number;
  floor: number;
  storedCount: number;
  complete: boolean;
}

export interface SimilarityCacheFilter {
  limit: number;
  minSimilarity: number;
  maxSimilarity?: number;
  minLines?: number;
  nameRegex?: string;
  excludePaths?: readonly string[];
}

export function similarityCacheTriples(database: DatabaseSync): Array<{
  functionId: number;
  codeEmbeddingId: number;
  descriptionEmbeddingId: number | null;
  fileDescriptionEmbeddingId: number | null;
}> {
  return database.prepare(`
    SELECT f.id AS functionId, f.embedding_id AS codeEmbeddingId,
      f.description_embedding_id AS descriptionEmbeddingId,
      files.file_description_embedding_id AS fileDescriptionEmbeddingId
    FROM functions f JOIN files ON files.path = f.path
    ORDER BY f.id
  `).all() as Array<{
    functionId: number;
    codeEmbeddingId: number;
    descriptionEmbeddingId: number | null;
    fileDescriptionEmbeddingId: number | null;
  }>;
}

export function similarityCacheStates(database: DatabaseSync, mode: string): Map<number, SimilarityCacheState> {
  const rows = database.prepare(`
    SELECT function_id AS functionId, code_embedding_id AS codeEmbeddingId,
      description_embedding_id AS descriptionEmbeddingId,
      file_description_embedding_id AS fileDescriptionEmbeddingId,
      cached_width AS cachedWidth, generation, floor,
      stored_count AS storedCount, complete
    FROM similarity_cache_state WHERE similarity_mode = ?
  `).all(mode) as Array<{
    functionId: number;
    codeEmbeddingId: number;
    descriptionEmbeddingId: number | null;
    fileDescriptionEmbeddingId: number | null;
    cachedWidth: number;
    generation: number;
    floor: number;
    storedCount: number;
    complete: number;
  }>;
  return new Map(rows.map((row) => [row.functionId, {
    codeEmbeddingId: row.codeEmbeddingId,
    descriptionEmbeddingId: row.descriptionEmbeddingId,
    fileDescriptionEmbeddingId: row.fileDescriptionEmbeddingId,
    cachedWidth: row.cachedWidth,
    generation: row.generation,
    floor: row.floor,
    storedCount: Number(row.storedCount),
    complete: row.complete !== 0,
  }]));
}

export function cachedSimilarityNeighbors(
  database: DatabaseSync,
  sourceId: number,
  mode: string,
  filter?: SimilarityCacheFilter,
): SimilarityResult[] {
  const excludePaths = filter?.excludePaths ?? [];
  const pathFilter = excludePaths.length > 0
    ? `AND f.path NOT IN (${excludePaths.map(() => "?").join(", ")})`
    : "";
  const rows = database.prepare(`
    SELECT f.*, c.similarity, c.similarity AS base_similarity,
      c.code_similarity AS code_similarity,
      c.description_similarity AS description_similarity,
      c.file_description_similarity AS file_description_similarity
    FROM similarity_cache c JOIN functions f ON f.id = c.target_id
    WHERE c.source_id = ? AND c.similarity_mode = ?
      AND c.similarity >= ?
      AND (? IS NULL OR c.similarity < ?)
      AND f.line_count >= ?
      AND (? IS NULL OR slopdex_regexp(?, f.qualified_name))
      ${pathFilter}
    ORDER BY c.similarity DESC, f.id ASC
    ${filter ? "LIMIT ?" : ""}
  `).all(
    sourceId,
    mode,
    filter?.minSimilarity ?? -1,
    filter?.maxSimilarity ?? null,
    filter?.maxSimilarity ?? null,
    filter?.minLines ?? 1,
    filter?.nameRegex ?? null,
    filter?.nameRegex ?? null,
    ...excludePaths,
    ...(filter ? [filter.limit] : []),
  ) as unknown as Array<FunctionRow & {
    similarity: number;
    base_similarity: number;
    code_similarity: number | null;
    description_similarity: number | null;
    file_description_similarity: number | null;
  }>;
  const fused = mode !== "code";
  return rows.map((row) => ({
    function: toIndexedFunction(row),
    similarity: row.similarity,
    ...(fused && row.code_similarity !== null ? { codeSimilarity: row.code_similarity } : {}),
    ...(fused && row.description_similarity !== null ? { descriptionSimilarity: row.description_similarity } : {}),
    ...(fused && row.file_description_similarity !== null ? { fileDescriptionSimilarity: row.file_description_similarity } : {}),
  }));
}

export function storeSimilarityNeighbors(
  database: DatabaseSync,
  sourceId: number,
  mode: string,
  neighbors: readonly SimilarityResult[],
  state: Omit<SimilarityCacheState, "storedCount">,
): void {
  database.prepare("DELETE FROM similarity_cache WHERE source_id = ? AND similarity_mode = ?").run(sourceId, mode);
  const insert = database.prepare(`
    INSERT INTO similarity_cache(
      source_id, target_id, similarity_mode, similarity,
      code_similarity, description_similarity, file_description_similarity
    ) VALUES (?, ?, ?, ?, ?, ?, ?)
  `);
  for (const neighbor of neighbors) {
    insert.run(
      sourceId,
      neighbor.function.id,
      mode,
      neighbor.similarity,
      neighbor.codeSimilarity ?? null,
      neighbor.descriptionSimilarity ?? null,
      neighbor.fileDescriptionSimilarity ?? null,
    );
  }
  database.prepare(`
    INSERT INTO similarity_cache_state(
      function_id, code_embedding_id, description_embedding_id,
      file_description_embedding_id, similarity_mode, cached_width, generation,
      floor, stored_count, complete
    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
    ON CONFLICT(function_id, similarity_mode) DO UPDATE SET
      code_embedding_id = excluded.code_embedding_id,
      description_embedding_id = excluded.description_embedding_id,
      file_description_embedding_id = excluded.file_description_embedding_id,
      cached_width = excluded.cached_width,
      generation = excluded.generation,
      floor = excluded.floor,
      stored_count = excluded.stored_count,
      complete = excluded.complete
  `).run(
    sourceId,
    state.codeEmbeddingId,
    state.descriptionEmbeddingId,
    state.fileDescriptionEmbeddingId,
    mode,
    state.cachedWidth,
    state.generation,
    state.floor,
    neighbors.length,
    state.complete ? 1 : 0,
  );
}

export function similarityCacheInfo(database: DatabaseSync): { cachedSources: number; cachedPairs: number } {
  const sources = database.prepare("SELECT COUNT(*) AS count FROM similarity_cache_state").get() as { count: number };
  const pairs = database.prepare("SELECT COUNT(*) AS count FROM similarity_cache").get() as { count: number };
  return { cachedSources: Number(sources.count), cachedPairs: Number(pairs.count) };
}

export function similarityCacheCounts(database: DatabaseSync, mode: string): Map<number, number> {
  const rows = database.prepare(`
    SELECT source_id AS sourceId, COUNT(*) AS count
    FROM similarity_cache WHERE similarity_mode = ? GROUP BY source_id
  `).all(mode) as Array<{ sourceId: number; count: number }>;
  return new Map(rows.map((row) => [row.sourceId, Number(row.count)]));
}

export function touchSimilarityCacheState(
  database: DatabaseSync,
  functionId: number,
  mode: string,
  state: SimilarityCacheState,
): void {
  database.prepare(`
    INSERT INTO similarity_cache_state(
      function_id, code_embedding_id, description_embedding_id,
      file_description_embedding_id, similarity_mode, cached_width, generation,
      floor, stored_count, complete
    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
    ON CONFLICT(function_id, similarity_mode) DO UPDATE SET
      code_embedding_id = excluded.code_embedding_id,
      description_embedding_id = excluded.description_embedding_id,
      file_description_embedding_id = excluded.file_description_embedding_id,
      cached_width = excluded.cached_width,
      generation = excluded.generation,
      floor = excluded.floor,
      stored_count = excluded.stored_count,
      complete = excluded.complete
  `).run(
    functionId,
    state.codeEmbeddingId,
    state.descriptionEmbeddingId,
    state.fileDescriptionEmbeddingId,
    mode,
    state.cachedWidth,
    state.generation,
    state.floor,
    state.storedCount,
    state.complete ? 1 : 0,
  );
}
