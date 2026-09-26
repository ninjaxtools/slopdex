import type { DatabaseSync } from "node:sqlite";

import { CodeIndexError } from "../errors.js";
import type { CachedParse } from "../indexing/prepared.js";
import { bufferVector, vectorBuffer } from "./rows.js";

export function cachedEmbedding(database: DatabaseSync, key: string): number[] | undefined {
  const row = database.prepare("SELECT vector FROM embeddings WHERE embedding_key = ?").get(key) as
    { vector: Uint8Array } | undefined;
  return row ? bufferVector(row.vector) : undefined;
}

export function storeEmbedding(database: DatabaseSync, readOnly: boolean, key: string, vector: readonly number[]): void {
  if (readOnly) return;
  database.prepare("INSERT OR IGNORE INTO embeddings(embedding_key, vector) VALUES (?, ?)")
    .run(key, vectorBuffer(vector));
}

export function cachedDescription(database: DatabaseSync, key: string): string | undefined {
  const row = database.prepare("SELECT description FROM description_cache WHERE description_key = ?").get(key) as
    { description: string } | undefined;
  return row?.description;
}

export function liveFunctionDescriptions(
  database: DatabaseSync,
  identityKeys: readonly string[],
): Map<string, { description: string | null; sourceHash: string }> {
  const result = new Map<string, { description: string | null; sourceHash: string }>();
  for (let index = 0; index < identityKeys.length; index += 500) {
    const batch = identityKeys.slice(index, index + 500);
    if (batch.length === 0) continue;
    const rows = database.prepare(`
      SELECT identity_key AS identityKey, description, source_hash AS sourceHash
      FROM functions WHERE identity_key IN (${batch.map(() => "?").join(", ")})
    `).all(...batch) as Array<{ identityKey: string; description: string | null; sourceHash: string }>;
    for (const row of rows) result.set(row.identityKey, { description: row.description, sourceHash: row.sourceHash });
  }
  return result;
}

export function liveFileContentHashes(database: DatabaseSync, filePaths: readonly string[]): Map<string, string> {
  const result = new Map<string, string>();
  for (let index = 0; index < filePaths.length; index += 500) {
    const batch = filePaths.slice(index, index + 500);
    if (batch.length === 0) continue;
    const rows = database.prepare(`
      SELECT path, content_hash AS contentHash FROM files WHERE path IN (${batch.map(() => "?").join(", ")})
    `).all(...batch) as Array<{ path: string; contentHash: string }>;
    for (const row of rows) result.set(row.path, row.contentHash);
  }
  return result;
}

export function storeDescription(database: DatabaseSync, readOnly: boolean, key: string, description: string): string {
  if (readOnly) return description;
  database.prepare("INSERT OR IGNORE INTO description_cache(description_key, description) VALUES (?, ?)").run(key, description);
  return (database.prepare("SELECT description FROM description_cache WHERE description_key = ?").get(key) as { description: string }).description;
}

export function cachedParse(database: DatabaseSync, key: string): CachedParse | undefined {
  const row = database.prepare("SELECT result FROM parse_cache WHERE parse_key = ?").get(key) as
    { result: string } | undefined;
  return row ? JSON.parse(row.result) as CachedParse : undefined;
}

export function storeParse(database: DatabaseSync, readOnly: boolean, key: string, result: CachedParse): void {
  if (readOnly) return;
  database.prepare("INSERT OR IGNORE INTO parse_cache(parse_key, result) VALUES (?, ?)").run(key, JSON.stringify(result));
}

export function embeddingId(
  database: DatabaseSync,
  readOnly: boolean,
  key: string,
  vector?: readonly number[],
): number {
  if (vector) storeEmbedding(database, readOnly, key, vector);
  const row = database.prepare("SELECT id FROM embeddings WHERE embedding_key = ?").get(key) as
    { id: number } | undefined;
  if (!row) throw new CodeIndexError("Missing cached embedding.");
  return row.id;
}

export function replaceCachedDescription(database: DatabaseSync, key: string, description: string): void {
  database.prepare(`
    INSERT INTO description_cache(description_key, description) VALUES (?, ?)
    ON CONFLICT(description_key) DO UPDATE SET description = excluded.description
  `).run(key, description);
}
