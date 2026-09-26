import type { PreparedCallable } from "../indexing/prepared.js";
import type { IndexedFunction, MarkdownChunk, SourceMode } from "../types.js";

export interface FileRow {
  path: string;
  content_hash: string;
  blob_oid: string | null;
  source_mode: SourceMode;
  indexed_commit: string | null;
  previous_path: string | null;
  language: string;
  byte_size: number;
  file_description_path: string | null;
  file_description_content_hash: string | null;
  file_description: string | null;
  file_description_embedding_id: number | null;
}

export interface FunctionRow {
  id: number;
  path: string;
  language: IndexedFunction["language"];
  kind: IndexedFunction["kind"];
  name: string;
  qualified_name: string;
  signature: string | null;
  identity_key: string;
  start_line: number;
  start_column: number;
  end_line: number;
  end_column: number;
  line_count: number;
  source: string;
  source_hash: string;
  embedding_input: string;
  first_seen_commit: string | null;
  last_seen_commit: string | null;
  source_mode: SourceMode;
  embedding_id: number;
  description: string | null;
  description_embedding_id: number | null;
}

export interface MarkdownChunkRow {
  id: number;
  path: string;
  heading_path: string;
  start_line: number;
  end_line: number;
  content: string;
  source_hash: string;
  source_mode: SourceMode;
  embedding_id: number;
}

export function vectorBuffer(vector: readonly number[]): Uint8Array {
  const values = Float32Array.from(vector);
  return new Uint8Array(values.buffer);
}

export function bufferVector(vector: Uint8Array): number[] {
  return Array.from(new Float32Array(vector.buffer, vector.byteOffset, vector.byteLength / 4));
}

export function toIndexedFunction(row: FunctionRow): IndexedFunction {
  return {
    id: row.id,
    path: row.path,
    language: row.language,
    kind: row.kind,
    name: row.name,
    qualifiedName: row.qualified_name,
    signature: row.signature,
    identityKey: row.identity_key,
    startLine: row.start_line,
    startColumn: row.start_column,
    endLine: row.end_line,
    endColumn: row.end_column,
    lineCount: row.line_count,
    source: row.source,
    sourceHash: row.source_hash,
    embeddingInput: row.embedding_input,
    firstSeenCommit: row.first_seen_commit,
    lastSeenCommit: row.last_seen_commit,
    sourceMode: row.source_mode,
    embeddingId: row.embedding_id,
    description: row.description,
    descriptionEmbeddingId: row.description_embedding_id,
  };
}

export function toMarkdownChunk(row: MarkdownChunkRow): MarkdownChunk {
  return {
    id: row.id,
    path: row.path,
    headingPath: JSON.parse(row.heading_path) as string[],
    startLine: row.start_line,
    endLine: row.end_line,
    content: row.content,
    sourceHash: row.source_hash,
    sourceMode: row.source_mode,
    embeddingId: row.embedding_id,
  };
}

export function changedEntries(values: object, previous: object): Array<[string, string | number | null]> {
  return (Object.entries(values) as Array<[string, string | number | null]>)
    .filter(([column, value]) => (previous as Record<string, unknown>)[column] !== value);
}

export function reconcileFunctions(
  current: readonly PreparedCallable[],
  previous: readonly FunctionRow[],
): Map<PreparedCallable, FunctionRow> {
  const matches = new Map<PreparedCallable, FunctionRow>();
  const keys = new Set([
    ...current.map((callable) => `${callable.qualifiedName}\0${callable.kind}`),
    ...previous.map((callable) => `${callable.qualified_name}\0${callable.kind}`),
  ]);
  for (const key of keys) {
    const currentGroup = current.filter((callable) => `${callable.qualifiedName}\0${callable.kind}` === key);
    const previousGroup = previous.filter((callable) => `${callable.qualified_name}\0${callable.kind}` === key);
    const unmatchedPrevious = new Set(previousGroup);
    const unmatchedCurrent = new Set(currentGroup);
    for (const callable of currentGroup) {
      const exact = [...unmatchedPrevious].find((old) => old.source_hash === callable.sourceHash);
      if (exact) {
        matches.set(callable, exact);
        unmatchedPrevious.delete(exact);
        unmatchedCurrent.delete(callable);
      }
    }
    for (const callable of unmatchedCurrent) {
      const old = unmatchedPrevious.values().next().value as FunctionRow | undefined;
      if (!old) break;
      matches.set(callable, old);
      unmatchedPrevious.delete(old);
    }
  }
  return matches;
}
