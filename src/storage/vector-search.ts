import type { DatabaseSync } from "node:sqlite";

import { CodeIndexError } from "../errors.js";
import type { MarkdownSearchResult, SimilarityResult } from "../types.js";
import { VEC0_MAX_DIMENSIONS } from "./schema.js";
import {
  bufferVector,
  type FunctionRow,
  type MarkdownChunkRow,
  toIndexedFunction,
  toMarkdownChunk,
  vectorBuffer,
} from "./rows.js";

const KNN_TIE_OVERFETCH = 32;
const VEC0_MAX_K = 4096;

export interface VectorSearchOptions {
  descriptions?: boolean;
  descriptionVector?: readonly number[];
  fileDescriptionVector?: readonly number[];
  limit?: number;
  minSimilarity: number;
  maxSimilarity?: number;
  excludeId?: number;
  excludePaths?: readonly string[];
  minLines?: number;
  nameRegex?: string;
}

export function searchVector(
  database: DatabaseSync,
  dimensions: number,
  vector: readonly number[],
  options: VectorSearchOptions,
): SimilarityResult[] {
  const functionDescription = options.descriptionVector !== undefined;
  const fileDescription = options.fileDescriptionVector !== undefined;
  const fused = functionDescription || fileDescription;
  if (functionDescription && options.descriptions) throw new CodeIndexError("Description-only search cannot also supply a separate function-description vector.");
  const scoreCount = 1 + Number(functionDescription) + Number(fileDescription);
  const scoreExpression = ["base_similarity"]
    .concat(functionDescription ? ["description_similarity"] : [])
    .concat(fileDescription ? ["file_description_similarity"] : [])
    .join(" + ");
  const excludePaths = options.excludePaths ?? [];
  if (options.limit !== undefined
    && !options.descriptions && !fused && options.maxSimilarity === undefined
    && options.nameRegex === undefined && excludePaths.length <= 1 && options.limit <= VEC0_MAX_K
    && dimensions <= VEC0_MAX_DIMENSIONS) {
    const excludeIdFilter = options.excludeId === undefined ? "" : "AND function_id_filter != ?";
    const excludePathFilter = excludePaths.length === 0 ? "" : "AND path != ?";
    const candidateLimit = Math.min((options.limit as number) + KNN_TIE_OVERFETCH, VEC0_MAX_K);
    const rows = database.prepare(`
      WITH nearest AS MATERIALIZED (
        SELECT function_id, distance
        FROM function_vectors
        WHERE embedding MATCH ? AND k = ?
          AND line_count >= ?
          ${excludeIdFilter}
          ${excludePathFilter}
      )
      SELECT f.*, 1.0 - nearest.distance AS similarity, 1.0 - nearest.distance AS base_similarity
      FROM nearest JOIN functions f ON f.id = nearest.function_id
      WHERE 1.0 - nearest.distance >= ?
      ORDER BY similarity DESC, f.id ASC
    `).all(
      vectorBuffer(vector),
      candidateLimit,
      options.minLines ?? 1,
      ...(options.excludeId === undefined ? [] : [options.excludeId]),
      ...excludePaths,
      options.minSimilarity,
    ) as unknown as Array<FunctionRow & { similarity: number; base_similarity: number }>;
    const limit = options.limit as number;
    const boundary = rows[limit - 1];
    const last = rows.at(-1);
    const ambiguousTie = rows.length === candidateLimit && boundary && last
      && Math.abs(boundary.similarity - last.similarity) <= Number.EPSILON;
    if (!ambiguousTie) {
      return rows.slice(0, limit)
        .map((row) => ({ function: toIndexedFunction(row), similarity: row.similarity }));
    }
  }
  const pathFilter = excludePaths.length > 0
    ? `AND f.path NOT IN (${excludePaths.map(() => "?").join(", ")})`
    : "";
  const rows = database.prepare(`
    WITH scores AS (
      SELECT f.id AS function_id, 1.0 - vec_distance_cosine(e.vector, ?) AS base_similarity
        ${functionDescription ? ", 1.0 - vec_distance_cosine(d.vector, ?) AS description_similarity" : ""}
        ${fileDescription ? ", 1.0 - vec_distance_cosine(fd.vector, ?) AS file_description_similarity" : ""}
      FROM functions f
      JOIN embeddings e ON e.id = f.${options.descriptions ? "description_embedding_id" : "embedding_id"}
      ${functionDescription ? "JOIN embeddings d ON d.id = f.description_embedding_id" : ""}
      ${fileDescription ? "JOIN files described_file ON described_file.path = f.path JOIN embeddings fd ON fd.id = described_file.file_description_embedding_id" : ""}
      WHERE (? IS NULL OR f.id != ?)
        ${pathFilter}
        AND f.line_count >= ?
        AND (? IS NULL OR slopdex_regexp(?, f.qualified_name))
    ), ranked AS (
      SELECT *, ${fused ? `(${scoreExpression}) / ${scoreCount}.0` : "base_similarity"} AS similarity
      FROM scores
    ), selected AS (
      SELECT * FROM ranked
      WHERE similarity >= ? AND (? IS NULL OR similarity < ?)
      ORDER BY similarity DESC, function_id ASC
      LIMIT ?
    )
    SELECT f.*, selected.similarity, selected.base_similarity
      ${functionDescription ? ", selected.description_similarity" : ""}
      ${fileDescription ? ", selected.file_description_similarity" : ""}
    FROM selected JOIN functions f ON f.id = selected.function_id
    ORDER BY selected.similarity DESC, f.id ASC
  `).all(
    vectorBuffer(vector),
    ...(options.descriptionVector ? [vectorBuffer(options.descriptionVector)] : []),
    ...(options.fileDescriptionVector ? [vectorBuffer(options.fileDescriptionVector)] : []),
    options.excludeId ?? null,
    options.excludeId ?? null,
    ...excludePaths,
    options.minLines ?? 1,
    options.nameRegex ?? null,
    options.nameRegex ?? null,
    options.minSimilarity,
    options.maxSimilarity ?? null,
    options.maxSimilarity ?? null,
    options.limit ?? -1,
  ) as unknown as Array<FunctionRow & {
    similarity: number;
    base_similarity: number;
    description_similarity?: number;
    file_description_similarity?: number;
  }>;
  return rows.map((row) => ({
    function: toIndexedFunction(row),
    similarity: row.similarity,
    ...(fused && !options.descriptions ? { codeSimilarity: row.base_similarity } : {}),
    ...(fused ? { descriptionSimilarity: options.descriptions ? row.base_similarity : row.description_similarity } : {}),
    ...(fileDescription ? { fileDescriptionSimilarity: row.file_description_similarity } : {}),
  }));
}

export function searchMarkdown(
  database: DatabaseSync,
  vector: readonly number[],
  options: { limit?: number; minSimilarity: number; maxSimilarity?: number },
): MarkdownSearchResult[] {
  const rows = database.prepare(`
    WITH scored AS (
      SELECT m.*, f.source_mode, 1.0 - vec_distance_cosine(e.vector, ?) AS similarity
      FROM markdown_chunks m
      JOIN files f ON f.path = m.path
      JOIN embeddings e ON e.id = m.embedding_id
    )
    SELECT * FROM scored
    WHERE similarity >= ? AND (? IS NULL OR similarity < ?)
    ORDER BY similarity DESC, id ASC
    LIMIT ?
  `).all(
    vectorBuffer(vector),
    options.minSimilarity,
    options.maxSimilarity ?? null,
    options.maxSimilarity ?? null,
    options.limit ?? -1,
  ) as unknown as Array<MarkdownChunkRow & { similarity: number }>;
  return rows.map((row) => ({ chunk: toMarkdownChunk(row), similarity: row.similarity }));
}

export function vectorForFunction(database: DatabaseSync, id: number, kind: "code" | "description" = "code"): number[] {
  const row = database.prepare(`
    SELECT e.vector FROM functions f
    JOIN embeddings e ON e.id = f.${kind === "description" ? "description_embedding_id" : "embedding_id"} WHERE f.id = ?
  `).get(id) as { vector: Uint8Array } | undefined;
  if (!row) throw new CodeIndexError(`Function ${id} does not exist or has no ${kind} embedding.`);
  return bufferVector(row.vector);
}

export function vectorForFile(database: DatabaseSync, filePath: string): number[] {
  const row = database.prepare(`
    SELECT e.vector FROM files f
    JOIN embeddings e ON e.id = f.file_description_embedding_id WHERE f.path = ?
  `).get(filePath) as { vector: Uint8Array } | undefined;
  if (!row) throw new CodeIndexError(`File ${filePath} does not exist or has no description embedding.`);
  return bufferVector(row.vector);
}

export function fileVectorForFunction(database: DatabaseSync, id: number): number[] {
  const row = database.prepare(`
    SELECT e.vector FROM functions fn
    JOIN files f ON f.path = fn.path
    JOIN embeddings e ON e.id = f.file_description_embedding_id
    WHERE fn.id = ?
  `).get(id) as { vector: Uint8Array } | undefined;
  if (!row) throw new CodeIndexError(`Function ${id} does not exist or its file has no description embedding.`);
  return bufferVector(row.vector);
}
