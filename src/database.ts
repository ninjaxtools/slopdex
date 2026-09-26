import { mkdirSync } from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";

import * as sqliteVec from "sqlite-vec";

import { CodeIndexError, IncompatibleIndexError } from "./errors.js";
import type {
  EmbeddingProfile,
  IndexStatus,
  IndexedFunction,
  IndexingError,
  IndexingIssue,
  MarkdownChunk,
  MarkdownSearchResult,
  ParsedCallable,
  SourceMode,
  SimilarityResult,
  DescriptionProfile,
  UpdateStats,
} from "./types.js";

const SCHEMA_VERSION = "11";
const KNN_TIE_OVERFETCH = 32;
const VEC0_MAX_K = 4096;
const VEC0_MAX_DIMENSIONS = 8192;

export interface PreparedDescription {
  key: string;
  description: string;
  vector?: readonly number[];
}

interface PreparedFileDescription {
  path: string;
  contentHash: string;
  descriptionKey: string;
  value: PreparedDescription;
}

export interface PreparedCallable extends ParsedCallable {
  embeddingKey: string;
  vector?: readonly number[];
  descriptionKey?: string;
  description?: PreparedDescription;
}

export interface PreparedMarkdownChunk {
  headingPath: string[];
  startLine: number;
  endLine: number;
  content: string;
  sourceHash: string;
  embeddingInput: string;
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
  source: string;
  previousPath?: string;
  replacePath?: string;
  callables: PreparedCallable[];
  markdownChunks: PreparedMarkdownChunk[];
  fileDescription?: PreparedFileDescription;
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
  language: string;
  describable: boolean;
  fileDescription: string | null;
  fileDescriptionPath: string | null;
  fileDescriptionContentHash: string | null;
}

interface FileRow {
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
  description: string | null;
  description_embedding_id: number | null;
}

interface MarkdownChunkRow {
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

  public get isReadOnly(): boolean {
    return this.#readOnly;
  }

  #initializeSchema(): void {
    const existingVersion = this.#metadataTableExists() ? this.#metadata("schema_version") : null;
    if (existingVersion && existingVersion !== "6" && existingVersion !== "7" && existingVersion !== "8" && existingVersion !== "9" && existingVersion !== "10" && existingVersion !== SCHEMA_VERSION) {
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
        byte_size INTEGER NOT NULL,
        file_description_path TEXT,
        file_description_content_hash TEXT,
        file_description TEXT,
        file_description_embedding_id INTEGER REFERENCES embeddings(id)
      );
      CREATE TABLE IF NOT EXISTS embeddings (
        id INTEGER PRIMARY KEY,
        embedding_key TEXT NOT NULL UNIQUE,
        vector BLOB NOT NULL
      );
      CREATE TABLE IF NOT EXISTS description_cache (
        description_key TEXT PRIMARY KEY,
        description TEXT NOT NULL
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
        description TEXT,
        description_embedding_id INTEGER REFERENCES embeddings(id)
      );
      CREATE TABLE IF NOT EXISTS callable_provenance (
        identity_key TEXT NOT NULL,
        source_hash TEXT NOT NULL,
        first_seen_commit TEXT NOT NULL,
        PRIMARY KEY(identity_key, source_hash)
      );
      CREATE INDEX IF NOT EXISTS functions_path ON functions(path);
      CREATE INDEX IF NOT EXISTS functions_embedding ON functions(embedding_id);
      CREATE INDEX IF NOT EXISTS functions_description_embedding ON functions(description_embedding_id);
      CREATE TABLE IF NOT EXISTS markdown_chunks (
        id INTEGER PRIMARY KEY,
        path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE ON UPDATE CASCADE,
        heading_path TEXT NOT NULL,
        start_line INTEGER NOT NULL,
        end_line INTEGER NOT NULL,
        content TEXT NOT NULL,
        source_hash TEXT NOT NULL,
        embedding_id INTEGER NOT NULL REFERENCES embeddings(id)
      );
      CREATE INDEX IF NOT EXISTS markdown_chunks_path ON markdown_chunks(path);
      CREATE INDEX IF NOT EXISTS markdown_chunks_embedding ON markdown_chunks(embedding_id);
      CREATE TABLE IF NOT EXISTS indexing_errors (
        id INTEGER PRIMARY KEY,
        path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE ON UPDATE CASCADE,
        diagnostic TEXT NOT NULL
      );
      CREATE INDEX IF NOT EXISTS indexing_errors_path ON indexing_errors(path);
      INSERT OR IGNORE INTO callable_provenance(identity_key, source_hash, first_seen_commit)
        SELECT identity_key, source_hash, first_seen_commit FROM functions WHERE first_seen_commit IS NOT NULL;
    `);
    if (existingVersion === "6" || existingVersion === "7" || existingVersion === null) {
      this.#db.exec("BEGIN IMMEDIATE");
      try {
        if (existingVersion === "6") {
          this.#db.exec(`
            ALTER TABLE files ADD COLUMN file_description_path TEXT;
            ALTER TABLE files ADD COLUMN file_description_content_hash TEXT;
            ALTER TABLE files ADD COLUMN file_description TEXT;
            ALTER TABLE files ADD COLUMN file_description_embedding_id INTEGER REFERENCES embeddings(id);
          `);
        }
        const storedProfile = this.#metadata("embedding_profile");
        const dimensions = storedProfile
          ? Number((JSON.parse(storedProfile) as EmbeddingProfile).dimensions)
          : this.#profile.dimensions;
        if (!Number.isInteger(dimensions) || dimensions <= 0) throw new IncompatibleIndexError("Index has an invalid embedding profile.");
        if (dimensions <= VEC0_MAX_DIMENSIONS) this.#db.exec(functionVectorsSchema(dimensions));
        this.#setMetadata("schema_version", SCHEMA_VERSION);
        if (existingVersion) this.#setMetadata("markdown_scan_pending", "true");
        this.#db.exec("COMMIT");
      } catch (error) {
        this.#db.exec("ROLLBACK");
        throw error;
      }
    } else if (existingVersion === "8") {
      this.#db.exec("BEGIN IMMEDIATE");
      try {
        this.#db.exec(similarityCacheSchema());
        this.#setMetadata("schema_version", SCHEMA_VERSION);
        this.#setMetadata("markdown_scan_pending", "true");
        this.#db.exec("COMMIT");
      } catch (error) {
        this.#db.exec("ROLLBACK");
        throw error;
      }
    } else if (existingVersion === "9") {
      this.#db.exec("BEGIN IMMEDIATE");
      try {
        this.#db.exec(`
          ALTER TABLE similarity_cache_state ADD COLUMN floor REAL NOT NULL DEFAULT -1;
          ALTER TABLE similarity_cache_state ADD COLUMN stored_count INTEGER NOT NULL DEFAULT 0;
          ALTER TABLE similarity_cache_state ADD COLUMN complete INTEGER NOT NULL DEFAULT 0;
          UPDATE similarity_cache_state SET stored_count = (
            SELECT COUNT(*) FROM similarity_cache c
            WHERE c.source_id = similarity_cache_state.function_id
              AND c.similarity_mode = similarity_cache_state.similarity_mode
          );
          UPDATE similarity_cache_state SET complete = (stored_count < cached_width);
        `);
        this.#setMetadata("schema_version", SCHEMA_VERSION);
        this.#setMetadata("markdown_scan_pending", "true");
        this.#db.exec("COMMIT");
      } catch (error) {
        this.#db.exec("ROLLBACK");
        throw error;
      }
    } else if (existingVersion === "10") {
      this.#transaction(() => {
        this.#setMetadata("schema_version", SCHEMA_VERSION);
        this.#setMetadata("markdown_scan_pending", "true");
      });
    }
    this.#db.exec("CREATE INDEX IF NOT EXISTS files_description_embedding ON files(file_description_embedding_id);");
    this.#db.exec(similarityCacheSchema());
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

  public descriptionsEnabled(): boolean {
    return this.#metadata("descriptions_enabled") === "true";
  }

  public descriptionProfile(): DescriptionProfile | null {
    const value = this.#metadata("description_profile");
    return value ? JSON.parse(value) as DescriptionProfile : null;
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

  public cachedDescription(key: string): string | undefined {
    const row = this.#db.prepare("SELECT description FROM description_cache WHERE description_key = ?").get(key) as
      { description: string } | undefined;
    return row?.description;
  }

  public liveFunctionDescriptions(identityKeys: readonly string[]): Map<string, { description: string | null; sourceHash: string }> {
    const result = new Map<string, { description: string | null; sourceHash: string }>();
    for (let index = 0; index < identityKeys.length; index += 500) {
      const batch = identityKeys.slice(index, index + 500);
      if (batch.length === 0) continue;
      const rows = this.#db.prepare(`
        SELECT identity_key AS identityKey, description, source_hash AS sourceHash
        FROM functions WHERE identity_key IN (${batch.map(() => "?").join(", ")})
      `).all(...batch) as Array<{ identityKey: string; description: string | null; sourceHash: string }>;
      for (const row of rows) result.set(row.identityKey, { description: row.description, sourceHash: row.sourceHash });
    }
    return result;
  }

  public liveFileContentHashes(filePaths: readonly string[]): Map<string, string> {
    const result = new Map<string, string>();
    for (let index = 0; index < filePaths.length; index += 500) {
      const batch = filePaths.slice(index, index + 500);
      if (batch.length === 0) continue;
      const rows = this.#db.prepare(`
        SELECT path, content_hash AS contentHash FROM files WHERE path IN (${batch.map(() => "?").join(", ")})
      `).all(...batch) as Array<{ path: string; contentHash: string }>;
      for (const row of rows) result.set(row.path, row.contentHash);
    }
    return result;
  }

  public updateDescriptionProfile(profile: DescriptionProfile, expectedGeneration: number): void {
    this.#transaction(() => {
      if (this.getGeneration() !== expectedGeneration) throw new CodeIndexError("Index changed while descriptions were being prepared; retry the update.");
      this.#setMetadata("description_profile", JSON.stringify(profile));
      this.#setMetadata("generation", String(expectedGeneration + 1));
    });
  }

  public storeDescription(key: string, description: string): string {
    if (this.#readOnly) return description;
    this.#db.prepare("INSERT OR IGNORE INTO description_cache(description_key, description) VALUES (?, ?)").run(key, description);
    return (this.#db.prepare("SELECT description FROM description_cache WHERE description_key = ?").get(key) as { description: string }).description;
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

  public enableDescriptions(
    values: readonly { id: number; description: PreparedDescription }[],
    files: readonly PreparedFile[],
    profile: DescriptionProfile,
    expectedGeneration: number,
  ): void {
    this.#transaction(() => {
      if (this.getGeneration() !== expectedGeneration) throw new CodeIndexError("Index changed while descriptions were being prepared; retry the update.");
      for (const file of files) this.#updateFileDescription(file);
      for (const { id, description } of values) {
        this.#db.prepare("UPDATE functions SET description = ?, description_embedding_id = ? WHERE id = ?")
          .run(description.description, this.#embeddingId(description.key, description.vector), id);
      }
      this.#setMetadata("descriptions_enabled", "true");
      this.#setMetadata("description_profile", JSON.stringify(profile));
      this.#setMetadata("generation", String(expectedGeneration + 1));
    });
  }

  public updateDescriptions(
    files: readonly PreparedFile[],
    includeCallables: boolean,
    expectedGeneration: number,
    profile?: DescriptionProfile,
  ): void {
    this.#transaction(() => {
      if (this.getGeneration() !== expectedGeneration) throw new CodeIndexError("Index changed while descriptions were being prepared; retry the update.");
      if (profile) this.#setMetadata("description_profile", JSON.stringify(profile));
      if (files.length === 0) return;
      for (const file of files) {
        this.#updateFileDescription(file);
        this.#replaceCachedDescription(file.fileDescription!.descriptionKey, file.fileDescription!.value.description);
        if (!includeCallables) continue;
        for (const callable of file.callables) {
          if (!callable.description) throw new CodeIndexError(`Missing description for ${callable.qualifiedName}.`);
          if (!callable.descriptionKey) throw new CodeIndexError(`Missing description cache key for ${callable.qualifiedName}.`);
          this.#db.prepare("UPDATE functions SET description = ?, description_embedding_id = ? WHERE identity_key = ?")
            .run(
              callable.description.description,
              this.#embeddingId(callable.description.key, callable.description.vector),
              callable.identityKey,
            );
          this.#replaceCachedDescription(callable.descriptionKey, callable.description.description);
        }
      }
      this.#setMetadata("generation", String(expectedGeneration + 1));
    });
  }

  #updateFileDescription(file: PreparedFile): void {
    if (!file.fileDescription) throw new CodeIndexError(`Missing file description for ${file.path}.`);
    this.#db.prepare(`
      UPDATE files
      SET file_description_path = ?, file_description_content_hash = ?,
        file_description = ?, file_description_embedding_id = ?
      WHERE path = ?
    `).run(
      file.fileDescription.path,
      file.fileDescription.contentHash,
      file.fileDescription.value.description,
      this.#embeddingId(file.fileDescription.value.key, file.fileDescription.value.vector),
      file.path,
    );
  }

  #replaceCachedDescription(key: string, description: string): void {
    this.#db.prepare(`
      INSERT INTO description_cache(description_key, description) VALUES (?, ?)
      ON CONFLICT(description_key) DO UPDATE SET description = excluded.description
    `).run(key, description);
  }

  public disableDescriptions(): void {
    if (!this.descriptionsEnabled()) return;
    this.#transaction(() => {
      this.#setMetadata("descriptions_enabled", "false");
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

  public fileDescription(filePath: string): { description: string; path: string; contentHash: string } | undefined {
    const row = this.#db.prepare(`
      SELECT file_description AS description, file_description_path AS path,
        file_description_content_hash AS contentHash
      FROM files WHERE path = ? AND file_description IS NOT NULL
    `).get(filePath) as { description: string; path: string; contentHash: string } | undefined;
    return row;
  }

  public functionDescription(identityKey: string): string | undefined {
    const row = this.#db.prepare("SELECT description FROM functions WHERE identity_key = ?")
      .get(identityKey) as { description: string | null } | undefined;
    return row?.description ?? undefined;
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
      if (options.completeDiagnosticsScan) this.#deleteMetadata("markdown_scan_pending");
      if (options.checkpoint === null) this.#deleteMetadata("git_checkpoint");
      else if (options.checkpoint !== undefined) this.#setMetadata("git_checkpoint", options.checkpoint);
      return stats;
    });
  }

  #replaceFile(file: PreparedFile, stats: UpdateStats): void {
    const sourcePath = file.replacePath ?? file.previousPath ?? file.path;
    const oldRows = this.#rowsForPath(sourcePath);
    const existingFile = this.#db.prepare("SELECT * FROM files WHERE path = ?").get(sourcePath) as unknown as FileRow | undefined;
    const oldMatches = reconcileFunctions(file.callables, oldRows);
    const renamed = sourcePath !== file.path;

    if (renamed) {
      const displacedRows = this.#rowsForPath(file.path);
      const displaced = this.#db.prepare("DELETE FROM files WHERE path = ?").run(file.path);
      if (displaced.changes > 0) {
        stats.filesDeleted += 1;
        stats.functionsDeleted += displacedRows.length;
      }
      this.#db.prepare("DELETE FROM files WHERE path = ?").run(sourcePath);
    }
    if (file.fileDescription?.value.vector) this.storeEmbedding(file.fileDescription.value.key, file.fileDescription.value.vector);
    this.#writeRow("files", "path", this.#fileValues(file, existingFile), renamed ? undefined : existingFile);
    if (!this.#errorsMatch(file)) {
      this.#db.prepare("DELETE FROM indexing_errors WHERE path = ?").run(file.path);
      const insertError = this.#db.prepare("INSERT INTO indexing_errors(path, diagnostic) VALUES (?, ?)");
      for (const error of file.errors) insertError.run(file.path, JSON.stringify(error));
    }

    if (!this.#markdownChunksMatch(file)) {
      this.#db.prepare("DELETE FROM markdown_chunks WHERE path = ?").run(file.path);
      const insertMarkdownChunk = this.#db.prepare(`
        INSERT INTO markdown_chunks(path, heading_path, start_line, end_line, content, source_hash, embedding_id)
        VALUES (?, ?, ?, ?, ?, ?, ?)
      `);
      for (const chunk of file.markdownChunks) {
        if (chunk.vector) this.storeEmbedding(chunk.embeddingKey, chunk.vector);
        const embedding = this.#db.prepare("SELECT id FROM embeddings WHERE embedding_key = ?").get(chunk.embeddingKey) as { id: number } | undefined;
        if (!embedding) throw new CodeIndexError(`Missing embedding for markdown chunk in ${file.path}.`);
        insertMarkdownChunk.run(
          file.path,
          JSON.stringify(chunk.headingPath),
          chunk.startLine,
          chunk.endLine,
          chunk.content,
          chunk.sourceHash,
          embedding.id,
        );
      }
    }

    const usedIds = new Set([...oldMatches.values()].map((old) => old.id));
    if (!renamed) {
      for (const old of oldRows) {
        if (!usedIds.has(old.id)) this.#db.prepare("DELETE FROM functions WHERE id = ?").run(old.id);
      }
      // Duplicate declarations can exchange occurrence-based identity keys.
      // Vacate only changing keys before assigning their final values, retaining
      // the function IDs and their dependent cache rows throughout the transaction.
      for (const [callable, old] of oldMatches) {
        if (old.identity_key === callable.identityKey) continue;
        const temporaryKey = `reconcile:${old.id}`;
        this.#db.prepare("UPDATE functions SET identity_key = ? WHERE id = ?").run(temporaryKey, old.id);
        old.identity_key = temporaryKey;
      }
    }
    const orderedCallables = [
      ...file.callables.filter((callable) => oldMatches.has(callable)),
      ...file.callables.filter((callable) => !oldMatches.has(callable)),
    ];
    for (const callable of orderedCallables) {
      if (callable.vector) this.storeEmbedding(callable.embeddingKey, callable.vector);
      if (callable.description?.vector) this.storeEmbedding(callable.description.key, callable.description.vector);
      const old = oldMatches.get(callable);
      if (this.descriptionsEnabled() && !callable.description) throw new CodeIndexError(`Missing description for ${callable.qualifiedName}.`);
      const values = this.#functionValues(file, callable, old);
      this.#writeRow("functions", "id", { id: old?.id ?? null, ...values }, renamed ? undefined : old);
      if (old) stats.functionsUpdated += 1;
      else stats.functionsAdded += 1;
      if (values.first_seen_commit) {
        this.#db.prepare(`
          INSERT OR IGNORE INTO callable_provenance(identity_key, source_hash, first_seen_commit) VALUES (?, ?, ?)
        `).run(callable.identityKey, callable.sourceHash, values.first_seen_commit);
      }
    }
    stats.functionsDeleted += oldRows.length - usedIds.size;
  }

  #fileValues(file: PreparedFile, existing?: FileRow): FileRow {
    const preserveDescription = !file.unavailable && file.language !== "markdown";
    return {
      path: file.path,
      content_hash: file.contentHash,
      blob_oid: file.blobOid,
      source_mode: file.sourceMode,
      indexed_commit: file.indexedCommit,
      previous_path: file.sourceMode === "working-tree" ? file.previousPath ?? existing?.previous_path ?? null : null,
      language: file.language,
      byte_size: file.byteSize,
      file_description_path: file.fileDescription?.path ?? (preserveDescription ? existing?.file_description_path : null) ?? null,
      file_description_content_hash: file.fileDescription?.contentHash ?? (preserveDescription ? existing?.file_description_content_hash : null) ?? null,
      file_description: file.fileDescription?.value.description ?? (preserveDescription ? existing?.file_description : null) ?? null,
      file_description_embedding_id: file.fileDescription
        ? this.#embeddingId(file.fileDescription.value.key)
        : preserveDescription ? existing?.file_description_embedding_id ?? null : null,
    };
  }

  #functionValues(file: PreparedFile, callable: PreparedCallable, old?: FunctionRow): Omit<FunctionRow, "id"> {
    const remembered = old?.first_seen_commit ? undefined : this.#db.prepare(`
      SELECT first_seen_commit FROM callable_provenance WHERE identity_key = ? AND source_hash = ?
    `).get(callable.identityKey, callable.sourceHash) as { first_seen_commit: string } | undefined;
    return {
      path: file.path,
      language: callable.language,
      kind: callable.kind,
      name: callable.name,
      qualified_name: callable.qualifiedName,
      signature: callable.signature,
      identity_key: callable.identityKey,
      start_line: callable.startLine,
      start_column: callable.startColumn,
      end_line: callable.endLine,
      end_column: callable.endColumn,
      line_count: callable.lineCount,
      source: callable.source,
      source_hash: callable.sourceHash,
      embedding_input: callable.embeddingInput,
      first_seen_commit: old?.first_seen_commit ?? remembered?.first_seen_commit ?? file.indexedCommit,
      last_seen_commit: file.indexedCommit,
      source_mode: file.sourceMode,
      embedding_id: this.#embeddingId(callable.embeddingKey),
      description: callable.description?.description ?? null,
      description_embedding_id: callable.description ? this.#embeddingId(callable.description.key) : null,
    };
  }

  #writeRow(table: "files" | "functions", key: "path" | "id", values: object, previous?: object): void {
    const entries = Object.entries(values) as Array<[string, string | number | null]>;
    if (!previous) {
      this.#db.prepare(`INSERT INTO ${table} (${entries.map(([column]) => column).join(", ")}) VALUES (${entries.map(() => "?").join(", ")})`)
        .run(...entries.map(([, value]) => value));
      return;
    }
    const changes = changedEntries(values, previous);
    if (changes.length === 0) return;
    this.#db.prepare(`UPDATE ${table} SET ${changes.map(([column]) => `${column} = ?`).join(", ")} WHERE ${key} = ?`)
      .run(...changes.map(([, value]) => value), (previous as Record<string, string | number>)[key]!);
  }

  #errorsMatch(file: PreparedFile): boolean {
    const errors = this.#db.prepare("SELECT diagnostic FROM indexing_errors WHERE path = ? ORDER BY id")
      .all(file.path) as Array<{ diagnostic: string }>;
    return JSON.stringify(errors.map((row) => row.diagnostic)) === JSON.stringify(file.errors.map((error) => JSON.stringify(error)));
  }

  public matchesPreparedFile(file: PreparedFile): boolean {
    const existing = this.#db.prepare("SELECT * FROM files WHERE path = ?").get(file.path) as unknown as FileRow | undefined;
    if (!existing || changedEntries(this.#fileValues(file, existing), existing).length > 0 || !this.#errorsMatch(file)) return false;
    const rows = this.#rowsForPath(file.path);
    if (rows.length !== file.callables.length) return false;
    const byIdentity = new Map(rows.map((row) => [row.identity_key, row]));
    for (const callable of file.callables) {
      const old = byIdentity.get(callable.identityKey);
      if (!old || changedEntries(this.#functionValues(file, callable, old), old).length > 0) return false;
    }
    return this.#markdownChunksMatch(file);
  }

  #markdownChunksMatch(file: PreparedFile): boolean {
    const chunks = this.#db.prepare(`
      SELECT m.*, e.embedding_key FROM markdown_chunks m
      JOIN embeddings e ON e.id = m.embedding_id WHERE m.path = ? ORDER BY m.id
    `).all(file.path);
    return chunks.length === file.markdownChunks.length && file.markdownChunks.every((chunk, index) => changedEntries({
      heading_path: JSON.stringify(chunk.headingPath), start_line: chunk.startLine, end_line: chunk.endLine,
      content: chunk.content, source_hash: chunk.sourceHash, embedding_key: chunk.embeddingKey,
    }, chunks[index]!).length === 0);
  }

  #rowsForPath(filePath: string): FunctionRow[] {
    return this.#db.prepare("SELECT * FROM functions WHERE path = ? ORDER BY start_line, start_column, id").all(filePath) as unknown as FunctionRow[];
  }

  public allFunctions(): IndexedFunction[] {
    return (this.#db.prepare("SELECT * FROM functions ORDER BY path, start_line, start_column, id").all() as unknown as FunctionRow[])
      .map(toIndexedFunction);
  }

  public allMarkdownChunks(): MarkdownChunk[] {
    return (this.#db.prepare(`
      SELECT m.*, f.source_mode FROM markdown_chunks m
      JOIN files f ON f.path = m.path
      ORDER BY m.path, m.start_line, m.id
    `).all() as unknown as MarkdownChunkRow[]).map(toMarkdownChunk);
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
  }): SimilarityResult[] {
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
      && this.#profile.dimensions <= VEC0_MAX_DIMENSIONS) {
      const excludeIdFilter = options.excludeId === undefined ? "" : "AND function_id_filter != ?";
      const excludePathFilter = excludePaths.length === 0 ? "" : "AND path != ?";
      const candidateLimit = Math.min((options.limit as number) + KNN_TIE_OVERFETCH, VEC0_MAX_K);
      const rows = this.#db.prepare(`
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
    const rows = this.#db.prepare(`
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

  public searchMarkdown(vector: readonly number[], options: {
    limit?: number;
    minSimilarity: number;
    maxSimilarity?: number;
  }): MarkdownSearchResult[] {
    const rows = this.#db.prepare(`
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

  public vectorForFunction(id: number, kind: "code" | "description" = "code"): number[] {
    const row = this.#db.prepare(`
      SELECT e.vector FROM functions f
      JOIN embeddings e ON e.id = f.${kind === "description" ? "description_embedding_id" : "embedding_id"} WHERE f.id = ?
    `).get(id) as { vector: Uint8Array } | undefined;
    if (!row) throw new CodeIndexError(`Function ${id} does not exist or has no ${kind} embedding.`);
    return bufferVector(row.vector);
  }

  public vectorForFile(filePath: string): number[] {
    const row = this.#db.prepare(`
      SELECT e.vector FROM files f
      JOIN embeddings e ON e.id = f.file_description_embedding_id WHERE f.path = ?
    `).get(filePath) as { vector: Uint8Array } | undefined;
    if (!row) throw new CodeIndexError(`File ${filePath} does not exist or has no description embedding.`);
    return bufferVector(row.vector);
  }

  public fileVectorForFunction(id: number): number[] {
    const row = this.#db.prepare(`
      SELECT e.vector FROM functions fn
      JOIN files f ON f.path = fn.path
      JOIN embeddings e ON e.id = f.file_description_embedding_id
      WHERE fn.id = ?
    `).get(id) as { vector: Uint8Array } | undefined;
    if (!row) throw new CodeIndexError(`Function ${id} does not exist or its file has no description embedding.`);
    return bufferVector(row.vector);
  }

  public similarityCacheTriples(): Array<{
    functionId: number;
    codeEmbeddingId: number;
    descriptionEmbeddingId: number | null;
    fileDescriptionEmbeddingId: number | null;
  }> {
    return this.#db.prepare(`
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

  public similarityCacheStates(mode: string): Map<number, {
    codeEmbeddingId: number;
    descriptionEmbeddingId: number | null;
    fileDescriptionEmbeddingId: number | null;
    cachedWidth: number;
    generation: number;
    floor: number;
    storedCount: number;
    complete: boolean;
  }> {
    const rows = this.#db.prepare(`
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

  public cachedSimilarityNeighbors(
    sourceId: number,
    mode: string,
    filter?: {
      limit: number;
      minSimilarity: number;
      maxSimilarity?: number;
      minLines?: number;
      nameRegex?: string;
      excludePaths?: readonly string[];
    },
  ): Array<SimilarityResult> {
    const excludePaths = filter?.excludePaths ?? [];
    const pathFilter = excludePaths.length > 0
      ? `AND f.path NOT IN (${excludePaths.map(() => "?").join(", ")})`
      : "";
    const rows = this.#db.prepare(`
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

  public storeSimilarityNeighbors(
    sourceId: number,
    mode: string,
    neighbors: readonly SimilarityResult[],
    state: {
      codeEmbeddingId: number;
      descriptionEmbeddingId: number | null;
      fileDescriptionEmbeddingId: number | null;
      cachedWidth: number;
      generation: number;
      floor: number;
      complete: boolean;
    },
  ): void {
    if (this.#readOnly) return;
    this.#transaction(() => {
      this.#db.prepare("DELETE FROM similarity_cache WHERE source_id = ? AND similarity_mode = ?").run(sourceId, mode);
      const insert = this.#db.prepare(`
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
      this.#db.prepare(`
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
    });
  }

  public similarityCacheInfo(): { cachedSources: number; cachedPairs: number } {
    const sources = this.#db.prepare("SELECT COUNT(*) AS count FROM similarity_cache_state").get() as { count: number };
    const pairs = this.#db.prepare("SELECT COUNT(*) AS count FROM similarity_cache").get() as { count: number };
    return { cachedSources: Number(sources.count), cachedPairs: Number(pairs.count) };
  }

  public similarityCacheCounts(mode: string): Map<number, number> {
    const rows = this.#db.prepare(`
      SELECT source_id AS sourceId, COUNT(*) AS count
      FROM similarity_cache WHERE similarity_mode = ? GROUP BY source_id
    `).all(mode) as Array<{ sourceId: number; count: number }>;
    return new Map(rows.map((row) => [row.sourceId, Number(row.count)]));
  }

  public touchSimilarityCacheState(
    functionId: number,
    mode: string,
    state: {
      codeEmbeddingId: number;
      descriptionEmbeddingId: number | null;
      fileDescriptionEmbeddingId: number | null;
      cachedWidth: number;
      generation: number;
      floor: number;
      storedCount: number;
      complete: boolean;
    },
  ): void {
    if (this.#readOnly) return;
    this.#db.prepare(`
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

  public status(): IndexStatus {
    const functionCount = Number((this.#db.prepare("SELECT COUNT(*) AS count FROM functions").get() as { count: number }).count);
    const fileCount = Number((this.#db.prepare("SELECT COUNT(*) AS count FROM files").get() as { count: number }).count);
    const describableFileCount = Number((this.#db.prepare(`
      SELECT COUNT(*) AS count FROM files f
      WHERE f.language != 'markdown' AND NOT EXISTS (
        SELECT 1 FROM indexing_errors ie
        WHERE ie.path = f.path
          AND json_extract(ie.diagnostic, '$.code') IN ('read-error', 'file-too-large')
      )
    `).get() as { count: number }).count);
    return {
      rootDir: this.#rootDir,
      indexPath: this.#indexPath,
      functionCount,
      markdownChunkCount: Number((this.#db.prepare("SELECT COUNT(*) AS count FROM markdown_chunks").get() as { count: number }).count),
      fileCount,
      generation: this.getGeneration(),
      gitCheckpoint: this.getCheckpoint(),
      embeddingProfile: this.#profile,
      descriptionsEnabled: this.descriptionsEnabled(),
      descriptionCount: Number((this.#db.prepare("SELECT COUNT(*) AS count FROM functions WHERE description_embedding_id IS NOT NULL").get() as { count: number }).count),
      fileDescriptionCount: Number((this.#db.prepare("SELECT COUNT(*) AS count FROM files WHERE language != 'markdown' AND file_description_embedding_id IS NOT NULL").get() as { count: number }).count),
      describableFileCount,
      staleFileDescriptionCount: Number((this.#db.prepare(`
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
      descriptionProfile: this.descriptionProfile(),
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

  public filesWithFileTooLargeErrors(): string[] {
    return (this.#db.prepare(
      "SELECT DISTINCT path FROM indexing_errors WHERE json_extract(diagnostic, '$.code') = 'file-too-large'",
    ).all() as Array<{ path: string }>).map((row) => row.path);
  }

  public needsDiagnosticsScan(): boolean {
    return this.#metadata("diagnostics_scan_pending") === "true";
  }

  public needsMarkdownScan(): boolean {
    return this.#metadata("markdown_scan_pending") === "true";
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
  const database = new DatabaseSync(indexPath, { allowExtension: true });
  try {
    sqliteVec.load(database);
    database.enableLoadExtension(false);
    database.exec("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;");
    const metadata = new Map((database.prepare("SELECT key, value FROM metadata").all() as Array<{ key: string; value: string }>)
      .map((row) => [row.key, row.value]));
    if (metadata.get("schema_version") !== SCHEMA_VERSION) {
      throw new IncompatibleIndexError(`Unsupported index schema version ${metadata.get("schema_version") ?? "unknown"}.`);
    }
    if (path.resolve(metadata.get("root_dir") ?? "") !== path.resolve(rootDir)) {
      throw new IncompatibleIndexError("Index belongs to a different repository.");
    }
    if (!Number.isInteger(profile.dimensions) || profile.dimensions <= 0) {
      throw new CodeIndexError("Embedding dimensions must be a positive integer.");
    }
    database.exec("BEGIN IMMEDIATE");
    try {
      database.exec(`
        DELETE FROM files;
        DROP TRIGGER IF EXISTS functions_vector_insert;
        DROP TRIGGER IF EXISTS functions_vector_delete;
        DROP TRIGGER IF EXISTS functions_vector_update;
        DROP TABLE IF EXISTS function_vectors;
        DELETE FROM callable_provenance;
        DELETE FROM metadata;
      `);
      if (profile.dimensions <= VEC0_MAX_DIMENSIONS) database.exec(functionVectorsSchema(profile.dimensions));
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

function similarityCacheSchema(): string {
  return `
    CREATE TABLE IF NOT EXISTS similarity_cache (
      source_id INTEGER NOT NULL REFERENCES functions(id) ON DELETE CASCADE,
      target_id INTEGER NOT NULL REFERENCES functions(id) ON DELETE CASCADE,
      similarity_mode TEXT NOT NULL,
      similarity REAL NOT NULL,
      code_similarity REAL,
      description_similarity REAL,
      file_description_similarity REAL,
      PRIMARY KEY (source_id, target_id, similarity_mode)
    );
    CREATE INDEX IF NOT EXISTS similarity_cache_source ON similarity_cache(source_id, similarity_mode, similarity DESC);
    CREATE INDEX IF NOT EXISTS similarity_cache_target ON similarity_cache(target_id, similarity_mode);
    CREATE TABLE IF NOT EXISTS similarity_cache_state (
      function_id INTEGER NOT NULL REFERENCES functions(id) ON DELETE CASCADE,
      similarity_mode TEXT NOT NULL,
      code_embedding_id INTEGER NOT NULL,
      description_embedding_id INTEGER,
      file_description_embedding_id INTEGER,
      cached_width INTEGER NOT NULL,
      generation INTEGER NOT NULL,
      floor REAL NOT NULL DEFAULT -1,
      stored_count INTEGER NOT NULL DEFAULT 0,
      complete INTEGER NOT NULL DEFAULT 0,
      PRIMARY KEY (function_id, similarity_mode)
    );
  `;
}

function functionVectorsSchema(dimensions: number): string {
  return `
    CREATE VIRTUAL TABLE function_vectors USING vec0(
      function_id INTEGER PRIMARY KEY,
      embedding FLOAT[${dimensions}] distance_metric=cosine,
      line_count INTEGER,
      path TEXT,
      function_id_filter INTEGER
    );
    INSERT INTO function_vectors(function_id, embedding, line_count, path, function_id_filter)
      SELECT f.id, e.vector, f.line_count, f.path, f.id
      FROM functions f JOIN embeddings e ON e.id = f.embedding_id;
    CREATE TRIGGER functions_vector_insert AFTER INSERT ON functions BEGIN
      INSERT INTO function_vectors(function_id, embedding, line_count, path, function_id_filter)
        SELECT new.id, e.vector, new.line_count, new.path, new.id FROM embeddings e WHERE e.id = new.embedding_id;
    END;
    CREATE TRIGGER functions_vector_delete AFTER DELETE ON functions BEGIN
      DELETE FROM function_vectors WHERE function_id = old.id;
    END;
    CREATE TRIGGER functions_vector_update AFTER UPDATE OF embedding_id, line_count, path ON functions BEGIN
      DELETE FROM function_vectors WHERE function_id = old.id;
      INSERT INTO function_vectors(function_id, embedding, line_count, path, function_id_filter)
        SELECT new.id, e.vector, new.line_count, new.path, new.id FROM embeddings e WHERE e.id = new.embedding_id;
    END;
  `;
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
    description: row.description,
    descriptionEmbeddingId: row.description_embedding_id,
  };
}

function toMarkdownChunk(row: MarkdownChunkRow): MarkdownChunk {
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

function changedEntries(values: object, previous: object): Array<[string, string | number | null]> {
  return (Object.entries(values) as Array<[string, string | number | null]>)
    .filter(([column, value]) => (previous as Record<string, unknown>)[column] !== value);
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
