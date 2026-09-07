import { mkdirSync } from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";

import * as sqliteVec from "sqlite-vec";

import { CodeIndexError, IncompatibleIndexError } from "../errors.js";
import type {
  EmbeddingProfile,
  IndexStatus,
  IndexedFunction,
  ParsedCallable,
  SourceMode,
  UpdateStats,
} from "../types.js";

const SCHEMA_VERSION = "2";

export interface PreparedCallable extends ParsedCallable {
  embeddingKey: string;
  vector?: readonly number[];
}

export interface PreparedFile {
  path: string;
  contentHash: string;
  blobOid: string | null;
  sourceMode: SourceMode;
  indexedCommit: string | null;
  language: string;
  byteSize: number;
  previousPath?: string;
  replacePath?: string;
  callables: PreparedCallable[];
}

export interface IndexedFileState {
  path: string;
  contentHash: string;
  blobOid: string | null;
  sourceMode: SourceMode;
  previousPath: string | null;
}

interface FunctionRow {
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
}

export class IndexDatabase {
  readonly #db: DatabaseSync;
  readonly #rootDir: string;
  readonly #indexPath: string;
  readonly #profile: Required<EmbeddingProfile>;

  public constructor(indexPath: string, rootDir: string, profile: Required<EmbeddingProfile>, readOnly = false) {
    if (!readOnly) mkdirSync(path.dirname(indexPath), { recursive: true });
    this.#indexPath = indexPath;
    this.#rootDir = rootDir;
    this.#profile = profile;
    this.#db = new DatabaseSync(indexPath, { allowExtension: true, readOnly });
    try {
      sqliteVec.load(this.#db);
      this.#db.enableLoadExtension(false);
      if (readOnly) {
        this.#db.exec("PRAGMA foreign_keys=ON");
      } else {
        this.#db.exec("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;");
        this.#migrate();
      }
      this.#validateMetadata(readOnly);
    } catch (error) {
      this.#db.close();
      throw error;
    }
  }

  public close(): void {
    this.#db.close();
  }

  #migrate(): void {
    this.#db.exec(`
      CREATE TABLE IF NOT EXISTS metadata (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS files (
        path TEXT PRIMARY KEY,
        content_hash TEXT NOT NULL,
        blob_oid TEXT,
        source_mode TEXT NOT NULL CHECK(source_mode IN ('git', 'working-tree')),
        indexed_commit TEXT,
        previous_path TEXT,
        language TEXT NOT NULL,
        byte_size INTEGER NOT NULL
      );
      CREATE TABLE IF NOT EXISTS embeddings (
        id INTEGER PRIMARY KEY,
        embedding_key TEXT NOT NULL UNIQUE,
        vector BLOB NOT NULL
      );
      CREATE TABLE IF NOT EXISTS functions (
        id INTEGER PRIMARY KEY,
        path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE ON UPDATE CASCADE,
        language TEXT NOT NULL,
        kind TEXT NOT NULL,
        name TEXT NOT NULL,
        qualified_name TEXT NOT NULL,
        signature TEXT,
        identity_key TEXT NOT NULL UNIQUE,
        start_line INTEGER NOT NULL,
        start_column INTEGER NOT NULL,
        end_line INTEGER NOT NULL,
        end_column INTEGER NOT NULL,
        line_count INTEGER NOT NULL,
        source TEXT NOT NULL,
        source_hash TEXT NOT NULL,
        embedding_input TEXT NOT NULL,
        first_seen_commit TEXT,
        last_seen_commit TEXT,
        source_mode TEXT NOT NULL CHECK(source_mode IN ('git', 'working-tree')),
        embedding_id INTEGER NOT NULL REFERENCES embeddings(id)
      );
      CREATE TABLE IF NOT EXISTS callable_provenance (
        identity_key TEXT NOT NULL,
        source_hash TEXT NOT NULL,
        first_seen_commit TEXT NOT NULL,
        PRIMARY KEY(identity_key, source_hash)
      );
      CREATE INDEX IF NOT EXISTS functions_path ON functions(path);
      CREATE INDEX IF NOT EXISTS functions_embedding ON functions(embedding_id);
      INSERT OR IGNORE INTO callable_provenance(identity_key, source_hash, first_seen_commit)
        SELECT identity_key, source_hash, first_seen_commit FROM functions WHERE first_seen_commit IS NOT NULL;
    `);
    const fileColumns = this.#db.prepare("PRAGMA table_info(files)").all() as Array<{ name: string }>;
    if (!fileColumns.some((column) => column.name === "previous_path")) {
      this.#db.exec("ALTER TABLE files ADD COLUMN previous_path TEXT");
    }
    const functionColumns = this.#db.prepare("PRAGMA table_info(functions)").all() as Array<{ name: string }>;
    const needsLineCount = !functionColumns.some((column) => column.name === "line_count");
    const schemaVersion = this.#metadata("schema_version");
    if (needsLineCount || schemaVersion === "1") {
      this.#transaction(() => {
        if (needsLineCount) this.#db.exec("ALTER TABLE functions ADD COLUMN line_count INTEGER NOT NULL DEFAULT 1");
        this.#db.exec("UPDATE functions SET line_count = end_line - start_line + 1");
        if (schemaVersion === "1") this.#setMetadata("schema_version", SCHEMA_VERSION);
      });
    }
  }

  #metadata(key: string): string | null {
    const row = this.#db.prepare("SELECT value FROM metadata WHERE key = ?").get(key) as { value: string } | undefined;
    return row?.value ?? null;
  }

  #setMetadata(key: string, value: string): void {
    this.#db.prepare(`
      INSERT INTO metadata(key, value) VALUES (?, ?)
      ON CONFLICT(key) DO UPDATE SET value = excluded.value
    `).run(key, value);
  }

  #deleteMetadata(key: string): void {
    this.#db.prepare("DELETE FROM metadata WHERE key = ?").run(key);
  }

  #validateMetadata(readOnly: boolean): void {
    const schemaVersion = this.#metadata("schema_version");
    if (readOnly && !schemaVersion) throw new IncompatibleIndexError("Target file is not a slopdex index.");
    if (schemaVersion && schemaVersion !== SCHEMA_VERSION) {
      throw new IncompatibleIndexError(`Unsupported index schema version ${schemaVersion}.`);
    }
    const storedRoot = this.#metadata("root_dir");
    if (readOnly && !storedRoot) throw new IncompatibleIndexError("Target index has no repository identity.");
    if (storedRoot && path.resolve(storedRoot) !== this.#rootDir) {
      throw new IncompatibleIndexError(`Index belongs to ${storedRoot}, not ${this.#rootDir}.`);
    }
    const profileJson = JSON.stringify(this.#profile);
    const storedProfile = this.#metadata("embedding_profile");
    if (readOnly && !storedProfile) throw new IncompatibleIndexError("Target index has no embedding profile.");
    if (storedProfile && storedProfile !== profileJson) {
      throw new IncompatibleIndexError("Embedding provider, model, dimensions, or strategy differs from this index.");
    }
    if (readOnly) return;
    this.#transaction(() => {
      this.#setMetadata("schema_version", SCHEMA_VERSION);
      this.#setMetadata("root_dir", this.#rootDir);
      this.#setMetadata("embedding_profile", profileJson);
      if (!this.#metadata("generation")) this.#setMetadata("generation", "0");
    });
  }

  #transaction<T>(operation: () => T): T {
    this.#db.exec("BEGIN IMMEDIATE");
    try {
      const result = operation();
      this.#db.exec("COMMIT");
      return result;
    } catch (error) {
      this.#db.exec("ROLLBACK");
      throw error;
    }
  }

  public getCheckpoint(): string | null {
    return this.#metadata("git_checkpoint");
  }

  public getGeneration(): number {
    return Number(this.#metadata("generation") ?? 0);
  }

  public getWorkingTreeFiles(): Array<{ path: string; previousPath: string | null }> {
    return this.#db.prepare(`
      SELECT path, previous_path AS previousPath
      FROM files WHERE source_mode = 'working-tree' ORDER BY path
    `).all() as Array<{ path: string; previousPath: string | null }>;
  }

  public getFileStates(): IndexedFileState[] {
    return this.#db.prepare(`
      SELECT path, content_hash AS contentHash, blob_oid AS blobOid,
        source_mode AS sourceMode, previous_path AS previousPath
      FROM files ORDER BY path
    `).all() as unknown as IndexedFileState[];
  }

  public getEmbeddingKeys(keys: readonly string[]): Set<string> {
    const found = new Set<string>();
    const statement = this.#db.prepare("SELECT 1 FROM embeddings WHERE embedding_key = ?");
    for (const key of keys) {
      if (statement.get(key)) found.add(key);
    }
    return found;
  }

  public applyUpdate(options: {
    files: readonly PreparedFile[];
    deletePaths: readonly string[];
    workingTree?: {
      files: readonly PreparedFile[];
      deletePaths: readonly string[];
    };
    checkpoint?: string | null;
    expectedCheckpoint?: string | null;
    expectedGeneration?: number;
  }): UpdateStats {
    return this.#transaction(() => {
      if (options.expectedCheckpoint !== undefined && this.getCheckpoint() !== options.expectedCheckpoint) {
        throw new CodeIndexError("Git checkpoint changed while the update was being prepared.");
      }
      if (options.expectedGeneration !== undefined && this.getGeneration() !== options.expectedGeneration) {
        throw new CodeIndexError("Index changed while the update was being prepared; retry the update.");
      }

      const stats: UpdateStats = {
        filesUpdated: 0,
        filesDeleted: 0,
        functionsAdded: 0,
        functionsUpdated: 0,
        functionsDeleted: 0,
        embeddingsCreated: 0,
        checkpoint: options.checkpoint !== undefined ? options.checkpoint : this.getCheckpoint(),
      };

      const deleteFile = this.#db.prepare("DELETE FROM files WHERE path = ?");
      const countFunctions = this.#db.prepare("SELECT COUNT(*) AS count FROM functions WHERE path = ?");
      const applyStage = (files: readonly PreparedFile[], deletePaths: readonly string[]): void => {
        stats.filesUpdated += files.length;
        const uniqueDeletes = new Set(deletePaths);
        for (const file of files) {
          uniqueDeletes.delete(file.path);
          if (file.previousPath) uniqueDeletes.delete(file.previousPath);
          if (file.replacePath) uniqueDeletes.delete(file.replacePath);
        }
        for (const filePath of uniqueDeletes) {
          const count = Number((countFunctions.get(filePath) as { count: number }).count);
          const result = deleteFile.run(filePath);
          if (result.changes > 0) {
            stats.filesDeleted += 1;
            stats.functionsDeleted += count;
          }
        }

        for (const file of files) this.#replaceFile(file, stats);
      };

      applyStage(options.files, options.deletePaths);
      if (options.workingTree) applyStage(options.workingTree.files, options.workingTree.deletePaths);

      const generation = this.getGeneration() + 1;
      this.#setMetadata("generation", String(generation));
      if (options.checkpoint === null) this.#deleteMetadata("git_checkpoint");
      else if (options.checkpoint !== undefined) this.#setMetadata("git_checkpoint", options.checkpoint);
      return stats;
    });
  }

  #replaceFile(file: PreparedFile, stats: UpdateStats): void {
    const sourcePath = file.replacePath ?? file.previousPath ?? file.path;
    const oldRows = this.#rowsForPath(sourcePath);
    const existingFile = this.#db.prepare("SELECT previous_path FROM files WHERE path = ?").get(sourcePath) as
      | { previous_path: string | null }
      | undefined;
    const oldMatches = reconcileFunctions(file.callables, oldRows);

    if (sourcePath !== file.path) {
      const displacedRows = this.#rowsForPath(file.path);
      const displaced = this.#db.prepare("DELETE FROM files WHERE path = ?").run(file.path);
      if (displaced.changes > 0) {
        stats.filesDeleted += 1;
        stats.functionsDeleted += displacedRows.length;
      }
      this.#db.prepare("DELETE FROM files WHERE path = ?").run(sourcePath);
    } else {
      this.#db.prepare("DELETE FROM files WHERE path = ?").run(file.path);
    }
    this.#db.prepare(`
      INSERT INTO files(path, content_hash, blob_oid, source_mode, indexed_commit, previous_path, language, byte_size)
      VALUES (?, ?, ?, ?, ?, ?, ?, ?)
    `).run(
      file.path,
      file.contentHash,
      file.blobOid,
      file.sourceMode,
      file.indexedCommit,
      file.sourceMode === "working-tree" ? file.previousPath ?? existingFile?.previous_path ?? null : null,
      file.language,
      file.byteSize,
    );

    const usedIds = new Set<number>();
    const orderedCallables = [
      ...file.callables.filter((callable) => oldMatches.has(callable)),
      ...file.callables.filter((callable) => !oldMatches.has(callable)),
    ];
    for (const callable of orderedCallables) {
      if (callable.vector) {
        const result = this.#db.prepare("INSERT OR IGNORE INTO embeddings(embedding_key, vector) VALUES (?, ?)")
          .run(callable.embeddingKey, vectorBuffer(callable.vector));
        stats.embeddingsCreated += Number(result.changes);
      }
      const embedding = this.#db.prepare("SELECT id FROM embeddings WHERE embedding_key = ?").get(callable.embeddingKey) as { id: number } | undefined;
      if (!embedding) throw new CodeIndexError(`Missing embedding for ${callable.qualifiedName}.`);

      const old = oldMatches.get(callable);
      if (old) usedIds.add(old.id);
      const remembered = this.#db.prepare(`
        SELECT first_seen_commit FROM callable_provenance WHERE identity_key = ? AND source_hash = ?
      `).get(callable.identityKey, callable.sourceHash) as { first_seen_commit: string } | undefined;
      const firstSeenCommit = old?.first_seen_commit ?? remembered?.first_seen_commit ?? file.indexedCommit;
      this.#db.prepare(`
        INSERT INTO functions(
          id, path, language, kind, name, qualified_name, signature, identity_key,
          start_line, start_column, end_line, end_column, line_count, source, source_hash,
          embedding_input, first_seen_commit, last_seen_commit, source_mode, embedding_id
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
      `).run(
        old?.id ?? null,
        file.path,
        callable.language,
        callable.kind,
        callable.name,
        callable.qualifiedName,
        callable.signature,
        callable.identityKey,
        callable.startLine,
        callable.startColumn,
        callable.endLine,
        callable.endColumn,
        callable.lineCount,
        callable.source,
        callable.sourceHash,
        callable.embeddingInput,
        firstSeenCommit,
        file.indexedCommit,
        file.sourceMode,
        embedding.id,
      );
      if (old) stats.functionsUpdated += 1;
      else stats.functionsAdded += 1;
      if (firstSeenCommit) {
        this.#db.prepare(`
          INSERT OR IGNORE INTO callable_provenance(identity_key, source_hash, first_seen_commit) VALUES (?, ?, ?)
        `).run(callable.identityKey, callable.sourceHash, firstSeenCommit);
      }
    }
    stats.functionsDeleted += oldRows.length - usedIds.size;
  }

  #rowsForPath(filePath: string): FunctionRow[] {
    return this.#db.prepare("SELECT * FROM functions WHERE path = ? ORDER BY start_line, start_column, id").all(filePath) as unknown as FunctionRow[];
  }

  public allFunctions(): IndexedFunction[] {
    return (this.#db.prepare("SELECT * FROM functions ORDER BY path, start_line, start_column, id").all() as unknown as FunctionRow[])
      .map(toIndexedFunction);
  }

  public allFilePaths(): string[] {
    return (this.#db.prepare("SELECT path FROM files ORDER BY path").all() as Array<{ path: string }>).map((row) => row.path);
  }

  public previousPath(filePath: string): string | null {
    const row = this.#db.prepare("SELECT previous_path FROM files WHERE path = ?").get(filePath) as
      | { previous_path: string | null }
      | undefined;
    return row?.previous_path ?? null;
  }

  public functionsForPaths(paths: readonly string[]): IndexedFunction[] {
    const results: IndexedFunction[] = [];
    for (const filePath of paths) results.push(...this.#rowsForPath(filePath).map(toIndexedFunction));
    return results;
  }

  public searchVector(vector: readonly number[], options: {
    limit: number;
    minSimilarity: number;
    maxSimilarity?: number;
    excludeId?: number;
    excludePaths?: readonly string[];
    minLines?: number;
  }): Array<{ function: IndexedFunction; similarity: number }> {
    const excludePaths = options.excludePaths ?? [];
    const pathFilter = excludePaths.length > 0
      ? `AND f.path NOT IN (${excludePaths.map(() => "?").join(", ")})`
      : "";
    const rows = this.#db.prepare(`
      SELECT f.*, 1.0 - vec_distance_cosine(e.vector, ?) AS similarity
      FROM functions f
      JOIN embeddings e ON e.id = f.embedding_id
      WHERE (? IS NULL OR f.id != ?)
        ${pathFilter}
        AND f.line_count >= ?
        AND (1.0 - vec_distance_cosine(e.vector, ?)) >= ?
        AND (? IS NULL OR (1.0 - vec_distance_cosine(e.vector, ?)) <= ?)
      ORDER BY similarity DESC, f.id ASC
      LIMIT ?
    `).all(
      vectorBuffer(vector),
      options.excludeId ?? null,
      options.excludeId ?? null,
      ...excludePaths,
      options.minLines ?? 1,
      vectorBuffer(vector),
      options.minSimilarity,
      options.maxSimilarity ?? null,
      vectorBuffer(vector),
      options.maxSimilarity ?? null,
      options.limit,
    ) as unknown as Array<FunctionRow & { similarity: number }>;
    return rows.map((row) => ({ function: toIndexedFunction(row), similarity: row.similarity }));
  }

  public vectorForFunction(id: number): number[] {
    const row = this.#db.prepare(`
      SELECT e.vector FROM functions f JOIN embeddings e ON e.id = f.embedding_id WHERE f.id = ?
    `).get(id) as { vector: Uint8Array } | undefined;
    if (!row) throw new CodeIndexError(`Function ${id} does not exist.`);
    return Array.from(new Float32Array(row.vector.buffer, row.vector.byteOffset, row.vector.byteLength / 4));
  }

  public status(): IndexStatus {
    const functionCount = Number((this.#db.prepare("SELECT COUNT(*) AS count FROM functions").get() as { count: number }).count);
    const fileCount = Number((this.#db.prepare("SELECT COUNT(*) AS count FROM files").get() as { count: number }).count);
    return {
      rootDir: this.#rootDir,
      indexPath: this.#indexPath,
      functionCount,
      fileCount,
      generation: this.getGeneration(),
      gitCheckpoint: this.getCheckpoint(),
      embeddingProfile: this.#profile,
    };
  }
}

function vectorBuffer(vector: readonly number[]): Uint8Array {
  const values = Float32Array.from(vector);
  return new Uint8Array(values.buffer);
}

function toIndexedFunction(row: FunctionRow): IndexedFunction {
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
  };
}

function reconcileFunctions(
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
