import type { DatabaseSync } from "node:sqlite";

import type { IndexedFileState } from "../indexing/prepared.js";
import type {
  DescriptionProfile,
  EmbeddingProfile,
  IndexedFunction,
  IndexingError,
  IndexingIssue,
  IndexStatus,
  MarkdownChunk,
  SourceMode,
} from "../types.js";
import { type FunctionRow, type MarkdownChunkRow, toIndexedFunction, toMarkdownChunk } from "./rows.js";

export function getWorkingTreeFiles(database: DatabaseSync): Array<{ path: string; previousPath: string | null }> {
  return database.prepare(`
    SELECT path, previous_path AS previousPath
    FROM files WHERE source_mode = 'working-tree' ORDER BY path
  `).all() as Array<{ path: string; previousPath: string | null }>;
}

export function getFileStates(database: DatabaseSync): IndexedFileState[] {
  return database.prepare(`
    SELECT path, content_hash AS contentHash, blob_oid AS blobOid,
      source_mode AS sourceMode, previous_path AS previousPath, language,
      language != 'markdown' AND NOT EXISTS (
        SELECT 1 FROM indexing_errors ie
        WHERE ie.path = files.path
          AND json_extract(ie.diagnostic, '$.code') IN ('read-error', 'file-too-large')
      ) AS describable,
      file_description AS fileDescription,
      file_description_path AS fileDescriptionPath,
      file_description_content_hash AS fileDescriptionContentHash
    FROM files ORDER BY path
  `).all() as unknown as IndexedFileState[];
}

export function fileDescription(
  database: DatabaseSync,
  filePath: string,
): { description: string; path: string; contentHash: string } | undefined {
  return database.prepare(`
    SELECT file_description AS description, file_description_path AS path,
      file_description_content_hash AS contentHash
    FROM files WHERE path = ? AND file_description IS NOT NULL
  `).get(filePath) as { description: string; path: string; contentHash: string } | undefined;
}

export function functionDescription(database: DatabaseSync, identityKey: string): string | undefined {
  const row = database.prepare("SELECT description FROM functions WHERE identity_key = ?")
    .get(identityKey) as { description: string | null } | undefined;
  return row?.description ?? undefined;
}

export function rowsForPath(database: DatabaseSync, filePath: string): FunctionRow[] {
  return database.prepare("SELECT * FROM functions WHERE path = ? ORDER BY start_line, start_column, id")
    .all(filePath) as unknown as FunctionRow[];
}

export function allFunctions(database: DatabaseSync): IndexedFunction[] {
  return (database.prepare("SELECT * FROM functions ORDER BY path, start_line, start_column, id").all() as unknown as FunctionRow[])
    .map(toIndexedFunction);
}

export function allMarkdownChunks(database: DatabaseSync): MarkdownChunk[] {
  return (database.prepare(`
    SELECT m.*, f.source_mode FROM markdown_chunks m
    JOIN files f ON f.path = m.path
    ORDER BY m.path, m.start_line, m.id
  `).all() as unknown as MarkdownChunkRow[]).map(toMarkdownChunk);
}

export function allFilePaths(database: DatabaseSync): string[] {
  return (database.prepare("SELECT path FROM files ORDER BY path").all() as Array<{ path: string }>).map((row) => row.path);
}

export function previousPath(database: DatabaseSync, filePath: string): string | null {
  const row = database.prepare("SELECT previous_path FROM files WHERE path = ?").get(filePath) as
    | { previous_path: string | null }
    | undefined;
  return row?.previous_path ?? null;
}

export function functionsForPaths(database: DatabaseSync, paths: readonly string[]): IndexedFunction[] {
  const results: IndexedFunction[] = [];
  for (const filePath of paths) results.push(...rowsForPath(database, filePath).map(toIndexedFunction));
  return results;
}

export function status(database: DatabaseSync, context: {
  rootDir: string;
  indexPath: string;
  profile: Required<EmbeddingProfile>;
  getGeneration: () => number;
  getCheckpoint: () => string | null;
  descriptionsEnabled: () => boolean;
  descriptionProfile: () => DescriptionProfile | null;
  filesWithErrors: () => string[];
}): IndexStatus {
  const functionCount = Number((database.prepare("SELECT COUNT(*) AS count FROM functions").get() as { count: number }).count);
  const fileCount = Number((database.prepare("SELECT COUNT(*) AS count FROM files").get() as { count: number }).count);
  const describableFileCount = Number((database.prepare(`
    SELECT COUNT(*) AS count FROM files f
    WHERE f.language != 'markdown' AND NOT EXISTS (
      SELECT 1 FROM indexing_errors ie
      WHERE ie.path = f.path
        AND json_extract(ie.diagnostic, '$.code') IN ('read-error', 'file-too-large')
    )
  `).get() as { count: number }).count);
  return {
    rootDir: context.rootDir,
    indexPath: context.indexPath,
    functionCount,
    markdownChunkCount: Number((database.prepare("SELECT COUNT(*) AS count FROM markdown_chunks").get() as { count: number }).count),
    fileCount,
    generation: context.getGeneration(),
    gitCheckpoint: context.getCheckpoint(),
    embeddingProfile: context.profile,
    descriptionsEnabled: context.descriptionsEnabled(),
    descriptionCount: Number((database.prepare("SELECT COUNT(*) AS count FROM functions WHERE description_embedding_id IS NOT NULL").get() as { count: number }).count),
    fileDescriptionCount: Number((database.prepare("SELECT COUNT(*) AS count FROM files WHERE language != 'markdown' AND file_description_embedding_id IS NOT NULL").get() as { count: number }).count),
    describableFileCount,
    staleFileDescriptionCount: Number((database.prepare(`
      SELECT COUNT(*) AS count FROM files f
      WHERE f.language != 'markdown'
        AND (file_description_path IS NULL OR file_description_path != path
        OR file_description_content_hash IS NULL OR file_description_content_hash != content_hash)
        AND NOT EXISTS (
          SELECT 1 FROM indexing_errors ie
          WHERE ie.path = f.path
            AND json_extract(ie.diagnostic, '$.code') IN ('read-error', 'file-too-large')
        )
    `).get() as { count: number }).count),
    descriptionProfile: context.descriptionProfile(),
    indexingErrorCount: Number((database.prepare("SELECT COUNT(*) AS count FROM indexing_errors").get() as { count: number }).count),
    failedFileCount: context.filesWithErrors().length,
  };
}

export function errorsFromDatabase(database: DatabaseSync): IndexingError[] {
  const rows = database.prepare(`
    SELECT e.id, e.diagnostic, f.source_mode, f.indexed_commit
    FROM indexing_errors e JOIN files f ON f.path = e.path
    ORDER BY e.path, e.id
  `).all() as Array<{ id: number; diagnostic: string; source_mode: SourceMode; indexed_commit: string | null }>;
  return rows.map((row) => ({
    ...JSON.parse(row.diagnostic) as IndexingIssue,
    id: row.id, sourceMode: row.source_mode, indexedCommit: row.indexed_commit,
  }));
}

export function filesWithErrors(database: DatabaseSync): string[] {
  return (database.prepare("SELECT DISTINCT path FROM indexing_errors").all() as Array<{ path: string }>).map((row) => row.path);
}

export function filesWithFileTooLargeErrors(database: DatabaseSync): string[] {
  return (database.prepare(
    "SELECT DISTINCT path FROM indexing_errors WHERE json_extract(diagnostic, '$.code') = 'file-too-large'",
  ).all() as Array<{ path: string }>).map((row) => row.path);
}
