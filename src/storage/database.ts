import { mkdirSync } from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";

import * as sqliteVec from "sqlite-vec";

import { CodeIndexError, IncompatibleIndexError } from "../errors.js";
import type {
  EmbeddingProfile,
  IndexStatus,
  IndexedFunction,
  IndexingError,
  IndexingIssue,
  ParsedCallable,
  SourceMode,
  SimilarityResult,
  SummaryProfile,
  UpdateStats,
} from "../types.js";

const SCHEMA_VERSION = "5";

export interface PreparedSummary {
  key: string;
  summary: string;
  vector?: readonly number[];
}

export interface PreparedCallable extends ParsedCallable {
  embeddingKey: string;
  vector?: readonly number[];
  purpose?: PreparedSummary;
}

export interface PreparedFile {
  path: string;
  contentHash: string;
  blobOid: string | null;
  sourceMode: SourceMode;
  indexedCommit: string | null;
  language: string;
  byteSize: number;
  source: string;
  previousPath?: string;
  replacePath?: string;
  callables: PreparedCallable[];
  errors: IndexingIssue[];
  unavailable?: boolean;
}

export interface CachedParse {
  callables: ParsedCallable[];
  errors: IndexingIssue[];
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
  summary: string | null;
  summary_embedding_id: number | null;
}

export class IndexDatabase {
  readonly #db: DatabaseSync;
  readonly #rootDir: string;
  readonly #indexPath: string;
  readonly #profile: Required<EmbeddingProfile>;
  readonly #readOnly: boolean;

  public constructor(indexPath: string, rootDir: string, profile: Required<EmbeddingProfile>, readOnly = false) {
    if (!readOnly) mkdirSync(path.dirname(indexPath), { recursive: true });
    this.#indexPath = indexPath;
    this.#rootDir = rootDir;
    this.#profile = profile;
    this.#readOnly = readOnly;
    this.#db = new DatabaseSync(indexPath, { allowExtension: true, readOnly });
    try {
      sqliteVec.load(this.#db);
      this.#db.enableLoadExtension(false);
      const regexes = new Map<string, RegExp>();
      this.#db.function("slopdex_regexp", { deterministic: true }, (pattern, value) => {
        if (typeof pattern !== "string" || typeof value !== "string") return 0;
        let regex = regexes.get(pattern);
        if (!regex) {
          regex = new RegExp(pattern);
          regexes.set(pattern, regex);
        }
        return regex.test(value) ? 1 : 0;
      });
      if (readOnly) {
        this.#db.exec("PRAGMA foreign_keys=ON");
      } else {
        this.#db.exec("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;");
        this.#initializeSchema();
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

  #initializeSchema(): void {
    const existingVersion = this.#metadataTableExists() ? this.#metadata("schema_version") : null;
    if (existingVersion && existingVersion !== SCHEMA_VERSION) {
      throw new IncompatibleIndexError(`Unsupported index schema version ${existingVersion}.`);
    }
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
      CREATE TABLE IF NOT EXISTS summary_cache (
        summary_key TEXT PRIMARY KEY,
        summary TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS parse_cache (
        parse_key TEXT PRIMARY KEY,
        result TEXT NOT NULL
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
        embedding_id INTEGER NOT NULL REFERENCES embeddings(id),
        summary TEXT,
        summary_embedding_id INTEGER REFERENCES embeddings(id)
      );
      CREATE TABLE IF NOT EXISTS callable_provenance (
        identity_key TEXT NOT NULL,
        source_hash TEXT NOT NULL,
        first_seen_commit TEXT NOT NULL,
        PRIMARY KEY(identity_key, source_hash)
      );
      CREATE INDEX IF NOT EXISTS functions_path ON functions(path);
      CREATE INDEX IF NOT EXISTS functions_embedding ON functions(embedding_id);
      CREATE INDEX IF NOT EXISTS functions_summary_embedding ON functions(summary_embedding_id);
      CREATE TABLE IF NOT EXISTS indexing_errors (
        id INTEGER PRIMARY KEY,
        path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE ON UPDATE CASCADE,
        diagnostic TEXT NOT NULL
      );
      CREATE INDEX IF NOT EXISTS indexing_errors_path ON indexing_errors(path);
      INSERT OR IGNORE INTO callable_provenance(identity_key, source_hash, first_seen_commit)
        SELECT identity_key, source_hash, first_seen_commit FROM functions WHERE first_seen_commit IS NOT NULL;
    `);
  }

  #metadataTableExists(): boolean {
    return Boolean(this.#db.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'metadata'").get());
  }

  #metadata(key: string): string | null {
    if (!this.#metadataTableExists()) return null;
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

  public summariesEnabled(): boolean {
    return this.#metadata("summaries_enabled") === "true";
  }

  public summaryProfile(): SummaryProfile | null {
    const value = this.#metadata("summary_profile");
    return value ? JSON.parse(value) as SummaryProfile : null;
  }

  public cachedEmbedding(key: string): number[] | undefined {
    const row = this.#db.prepare("SELECT vector FROM embeddings WHERE embedding_key = ?").get(key) as
      { vector: Uint8Array } | undefined;
    return row ? bufferVector(row.vector) : undefined;
  }

  public storeEmbedding(key: string, vector: readonly number[]): void {
    if (this.#readOnly) return;
    this.#db.prepare("INSERT OR IGNORE INTO embeddings(embedding_key, vector) VALUES (?, ?)")
      .run(key, vectorBuffer(vector));
  }

  public cachedSummary(key: string): string | undefined {
    const row = this.#db.prepare("SELECT summary FROM summary_cache WHERE summary_key = ?").get(key) as
      { summary: string } | undefined;
    return row?.summary;
  }

  public storeSummary(key: string, summary: string): string {
    if (this.#readOnly) return summary;
    this.#db.prepare("INSERT OR IGNORE INTO summary_cache(summary_key, summary) VALUES (?, ?)").run(key, summary);
    return (this.#db.prepare("SELECT summary FROM summary_cache WHERE summary_key = ?").get(key) as { summary: string }).summary;
  }

  public cachedParse(key: string): CachedParse | undefined {
    const row = this.#db.prepare("SELECT result FROM parse_cache WHERE parse_key = ?").get(key) as
      { result: string } | undefined;
    return row ? JSON.parse(row.result) as CachedParse : undefined;
  }

  public storeParse(key: string, result: CachedParse): void {
    if (this.#readOnly) return;
    this.#db.prepare("INSERT OR IGNORE INTO parse_cache(parse_key, result) VALUES (?, ?)").run(key, JSON.stringify(result));
  }

  #embeddingId(key: string, vector?: readonly number[]): number {
    if (vector) this.storeEmbedding(key, vector);
    const row = this.#db.prepare("SELECT id FROM embeddings WHERE embedding_key = ?").get(key) as
      { id: number } | undefined;
    if (!row) throw new CodeIndexError("Missing cached embedding.");
    return row.id;
  }

  public enableSummaries(values: readonly { id: number; purpose: PreparedSummary }[], profile: SummaryProfile, expectedGeneration: number): void {
    this.#transaction(() => {
      if (this.getGeneration() !== expectedGeneration) throw new CodeIndexError("Index changed while summaries were being prepared; retry the update.");
      for (const { id, purpose } of values) {
        this.#db.prepare("UPDATE functions SET summary = ?, summary_embedding_id = ? WHERE id = ?")
          .run(purpose.summary, this.#embeddingId(purpose.key, purpose.vector), id);
      }
      this.#setMetadata("summaries_enabled", "true");
      this.#setMetadata("summary_profile", JSON.stringify(profile));
      this.#setMetadata("generation", String(expectedGeneration + 1));
    });
  }

  public disableSummaries(): void {
    if (!this.summariesEnabled()) return;
    this.#transaction(() => {
      this.#setMetadata("summaries_enabled", "false");
      this.#setMetadata("generation", String(this.getGeneration() + 1));
    });
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
    completeDiagnosticsScan?: boolean;
    embeddingsCreated?: number;
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
        embeddingsCreated: options.embeddingsCreated ?? 0,
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
      if (options.completeDiagnosticsScan) this.#deleteMetadata("diagnostics_scan_pending");
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
    const insertError = this.#db.prepare("INSERT INTO indexing_errors(path, diagnostic) VALUES (?, ?)");
    for (const error of file.errors) insertError.run(file.path, JSON.stringify(error));

    const usedIds = new Set<number>();
    const orderedCallables = [
      ...file.callables.filter((callable) => oldMatches.has(callable)),
      ...file.callables.filter((callable) => !oldMatches.has(callable)),
    ];
    for (const callable of orderedCallables) {
      if (callable.vector) {
        this.storeEmbedding(callable.embeddingKey, callable.vector);
      }
      const embedding = this.#db.prepare("SELECT id FROM embeddings WHERE embedding_key = ?").get(callable.embeddingKey) as { id: number } | undefined;
      if (!embedding) throw new CodeIndexError(`Missing embedding for ${callable.qualifiedName}.`);

      const old = oldMatches.get(callable);
      if (this.summariesEnabled() && !callable.purpose) throw new CodeIndexError(`Missing summary for ${callable.qualifiedName}.`);
      const summaryId = callable.purpose ? this.#embeddingId(callable.purpose.key, callable.purpose.vector) : null;
      if (old) usedIds.add(old.id);
      const remembered = this.#db.prepare(`
        SELECT first_seen_commit FROM callable_provenance WHERE identity_key = ? AND source_hash = ?
      `).get(callable.identityKey, callable.sourceHash) as { first_seen_commit: string } | undefined;
      const firstSeenCommit = old?.first_seen_commit ?? remembered?.first_seen_commit ?? file.indexedCommit;
      this.#db.prepare(`
        INSERT INTO functions(
          id, path, language, kind, name, qualified_name, signature, identity_key,
          start_line, start_column, end_line, end_column, line_count, source, source_hash,
          embedding_input, first_seen_commit, last_seen_commit, source_mode, embedding_id, summary, summary_embedding_id
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
        callable.purpose?.summary ?? null,
        summaryId,
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
    summaries?: boolean;
    summaryVector?: readonly number[];
    limit: number;
    minSimilarity: number;
    maxSimilarity?: number;
    excludeId?: number;
    excludePaths?: readonly string[];
    minLines?: number;
    nameRegex?: string;
  }): SimilarityResult[] {
    const fused = options.summaryVector !== undefined;
    if (fused && options.summaries) throw new CodeIndexError("Summary-only search cannot also use score fusion.");
    const excludePaths = options.excludePaths ?? [];
    const pathFilter = excludePaths.length > 0
      ? `AND f.path NOT IN (${excludePaths.map(() => "?").join(", ")})`
      : "";
    const rows = this.#db.prepare(`
      WITH scores AS (
        SELECT f.*, 1.0 - vec_distance_cosine(e.vector, ?) AS base_similarity
          ${fused ? ", 1.0 - vec_distance_cosine(s.vector, ?) AS summary_similarity" : ""}
        FROM functions f
        JOIN embeddings e ON e.id = f.${options.summaries ? "summary_embedding_id" : "embedding_id"}
        ${fused ? "JOIN embeddings s ON s.id = f.summary_embedding_id" : ""}
        WHERE (? IS NULL OR f.id != ?)
          ${pathFilter}
          AND f.line_count >= ?
          AND (? IS NULL OR slopdex_regexp(?, f.qualified_name))
      ), ranked AS (
        SELECT *, ${fused ? "0.5 * base_similarity + 0.5 * summary_similarity" : "base_similarity"} AS similarity
        FROM scores
      )
      SELECT * FROM ranked
      WHERE similarity >= ? AND (? IS NULL OR similarity < ?)
      ORDER BY similarity DESC, id ASC
      LIMIT ?
    `).all(
      vectorBuffer(vector),
      ...(options.summaryVector ? [vectorBuffer(options.summaryVector)] : []),
      options.excludeId ?? null,
      options.excludeId ?? null,
      ...excludePaths,
      options.minLines ?? 1,
      options.nameRegex ?? null,
      options.nameRegex ?? null,
      options.minSimilarity,
      options.maxSimilarity ?? null,
      options.maxSimilarity ?? null,
      options.limit,
    ) as unknown as Array<FunctionRow & { similarity: number; base_similarity: number; summary_similarity: number }>;
    return rows.map((row) => ({
      function: toIndexedFunction(row),
      similarity: row.similarity,
      ...(fused ? { codeSimilarity: row.base_similarity, summarySimilarity: row.summary_similarity } : {}),
    }));
  }

  public vectorForFunction(id: number, kind: "code" | "summary" = "code"): number[] {
    const row = this.#db.prepare(`
      SELECT e.vector FROM functions f
      JOIN embeddings e ON e.id = f.${kind === "summary" ? "summary_embedding_id" : "embedding_id"} WHERE f.id = ?
    `).get(id) as { vector: Uint8Array } | undefined;
    if (!row) throw new CodeIndexError(`Function ${id} does not exist or has no ${kind} embedding.`);
    return bufferVector(row.vector);
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
      summariesEnabled: this.summariesEnabled(),
      summaryCount: Number((this.#db.prepare("SELECT COUNT(*) AS count FROM functions WHERE summary_embedding_id IS NOT NULL").get() as { count: number }).count),
      summaryProfile: this.summaryProfile(),
      indexingErrorCount: Number((this.#db.prepare("SELECT COUNT(*) AS count FROM indexing_errors").get() as { count: number }).count),
      failedFileCount: this.filesWithErrors().length,
    };
  }

  public indexErrors(): IndexingError[] {
    return errorsFromDatabase(this.#db);
  }

  public filesWithErrors(): string[] {
    return (this.#db.prepare("SELECT DISTINCT path FROM indexing_errors").all() as Array<{ path: string }>).map((row) => row.path);
  }

  public needsDiagnosticsScan(): boolean {
    return this.#metadata("diagnostics_scan_pending") === "true";
  }
}

export function readIndexErrors(indexPath: string): IndexingError[] {
  const database = new DatabaseSync(indexPath, { readOnly: true });
  try {
    if (!database.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'indexing_errors'").get()) return [];
    return errorsFromDatabase(database);
  } finally {
    database.close();
  }
}

export function resetIndexState(
  indexPath: string,
  rootDir: string,
  profile: EmbeddingProfile,
): void {
  const database = new DatabaseSync(indexPath);
  try {
    database.exec("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;");
    const metadata = new Map((database.prepare("SELECT key, value FROM metadata").all() as Array<{ key: string; value: string }>)
      .map((row) => [row.key, row.value]));
    if (metadata.get("schema_version") !== SCHEMA_VERSION) {
      throw new IncompatibleIndexError(`Unsupported index schema version ${metadata.get("schema_version") ?? "unknown"}.`);
    }
    if (path.resolve(metadata.get("root_dir") ?? "") !== path.resolve(rootDir)) {
      throw new IncompatibleIndexError("Index belongs to a different repository.");
    }
    database.exec("BEGIN IMMEDIATE");
    try {
      database.exec("DELETE FROM files; DELETE FROM callable_provenance; DELETE FROM metadata;");
      const setMetadata = database.prepare("INSERT INTO metadata(key, value) VALUES (?, ?)");
      setMetadata.run("schema_version", SCHEMA_VERSION);
      setMetadata.run("root_dir", path.resolve(rootDir));
      setMetadata.run("embedding_profile", JSON.stringify({
        provider: profile.provider,
        model: profile.model,
        dimensions: profile.dimensions,
        strategyVersion: profile.strategyVersion ?? "callable-v2",
      }));
      setMetadata.run("generation", "0");
      database.exec("COMMIT");
    } catch (error) {
      database.exec("ROLLBACK");
      throw error;
    }
  } finally {
    database.close();
  }
}

export function readIndexErrorCounts(indexPath: string): { errors: number; files: number } {
  const database = new DatabaseSync(indexPath, { readOnly: true });
  try {
    if (!database.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'indexing_errors'").get()) {
      return { errors: 0, files: 0 };
    }
    const counts = database.prepare("SELECT COUNT(*) AS errors, COUNT(DISTINCT path) AS files FROM indexing_errors").get() as { errors: number; files: number };
    return { errors: Number(counts.errors), files: Number(counts.files) };
  } finally {
    database.close();
  }
}

function errorsFromDatabase(database: DatabaseSync): IndexingError[] {
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

function vectorBuffer(vector: readonly number[]): Uint8Array {
  const values = Float32Array.from(vector);
  return new Uint8Array(values.buffer);
}

function bufferVector(vector: Uint8Array): number[] {
  return Array.from(new Float32Array(vector.buffer, vector.byteOffset, vector.byteLength / 4));
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
    summary: row.summary,
    summaryEmbeddingId: row.summary_embedding_id,
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
