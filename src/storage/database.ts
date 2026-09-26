import { mkdirSync } from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";

import * as sqliteVec from "sqlite-vec";

import { CodeIndexError, IncompatibleIndexError } from "../errors.js";
import type {
  CachedParse,
  IndexedFileState,
  PreparedCallable,
  PreparedDescription,
  PreparedFile,
} from "../indexing/prepared.js";
import * as artifactCache from "./artifact-cache.js";
import * as indexInspection from "./index-inspection.js";
import {
  changedEntries,
  type FileRow,
  type FunctionRow,
  reconcileFunctions,
} from "./rows.js";
import {
  deleteMetadata,
  functionVectorsSchema,
  initializeSchema,
  metadataTableExists,
  readMetadata,
  SCHEMA_VERSION,
  setMetadata,
  VEC0_MAX_DIMENSIONS,
} from "./schema.js";
import * as similarityCacheStore from "./similarity-cache-store.js";
import * as vectorSearch from "./vector-search.js";
import type {
  EmbeddingProfile,
  IndexStatus,
  IndexedFunction,
  IndexingError,
  MarkdownChunk,
  MarkdownSearchResult,
  SimilarityResult,
  DescriptionProfile,
  UpdateStats,
} from "../types.js";

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
    initializeSchema(this.#db, this.#profile, (operation) => this.#transaction(operation));
  }

  #metadataTableExists(): boolean {
    return metadataTableExists(this.#db);
  }

  #metadata(key: string): string | null {
    if (!this.#metadataTableExists()) return null;
    return readMetadata(this.#db, key, false);
  }

  #setMetadata(key: string, value: string): void {
    setMetadata(this.#db, key, value);
  }

  #deleteMetadata(key: string): void {
    deleteMetadata(this.#db, key);
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
    return artifactCache.cachedEmbedding(this.#db, key);
  }

  public storeEmbedding(key: string, vector: readonly number[]): void {
    artifactCache.storeEmbedding(this.#db, this.#readOnly, key, vector);
  }

  public cachedDescription(key: string): string | undefined {
    return artifactCache.cachedDescription(this.#db, key);
  }

  public liveFunctionDescriptions(identityKeys: readonly string[]): Map<string, { description: string | null; sourceHash: string }> {
    return artifactCache.liveFunctionDescriptions(this.#db, identityKeys);
  }

  public liveFileContentHashes(filePaths: readonly string[]): Map<string, string> {
    return artifactCache.liveFileContentHashes(this.#db, filePaths);
  }

  public updateDescriptionProfile(profile: DescriptionProfile, expectedGeneration: number): void {
    this.#transaction(() => {
      if (this.getGeneration() !== expectedGeneration) throw new CodeIndexError("Index changed while descriptions were being prepared; retry the update.");
      this.#setMetadata("description_profile", JSON.stringify(profile));
      this.#setMetadata("generation", String(expectedGeneration + 1));
    });
  }

  public storeDescription(key: string, description: string): string {
    return artifactCache.storeDescription(this.#db, this.#readOnly, key, description);
  }

  public cachedParse(key: string): CachedParse | undefined {
    return artifactCache.cachedParse(this.#db, key);
  }

  public storeParse(key: string, result: CachedParse): void {
    artifactCache.storeParse(this.#db, this.#readOnly, key, result);
  }

  #embeddingId(key: string, vector?: readonly number[]): number {
    return artifactCache.embeddingId(this.#db, this.#readOnly, key, vector);
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
    artifactCache.replaceCachedDescription(this.#db, key, description);
  }

  public disableDescriptions(): void {
    if (!this.descriptionsEnabled()) return;
    this.#transaction(() => {
      this.#setMetadata("descriptions_enabled", "false");
      this.#setMetadata("generation", String(this.getGeneration() + 1));
    });
  }

  public getWorkingTreeFiles(): Array<{ path: string; previousPath: string | null }> {
    return indexInspection.getWorkingTreeFiles(this.#db);
  }

  public getFileStates(): IndexedFileState[] {
    return indexInspection.getFileStates(this.#db);
  }

  public fileDescription(filePath: string): { description: string; path: string; contentHash: string } | undefined {
    return indexInspection.fileDescription(this.#db, filePath);
  }

  public functionDescription(identityKey: string): string | undefined {
    return indexInspection.functionDescription(this.#db, identityKey);
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
    return indexInspection.rowsForPath(this.#db, filePath);
  }

  public allFunctions(): IndexedFunction[] {
    return indexInspection.allFunctions(this.#db);
  }

  public allMarkdownChunks(): MarkdownChunk[] {
    return indexInspection.allMarkdownChunks(this.#db);
  }

  public allFilePaths(): string[] {
    return indexInspection.allFilePaths(this.#db);
  }

  public previousPath(filePath: string): string | null {
    return indexInspection.previousPath(this.#db, filePath);
  }

  public functionsForPaths(paths: readonly string[]): IndexedFunction[] {
    return indexInspection.functionsForPaths(this.#db, paths);
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
    return vectorSearch.searchVector(this.#db, this.#profile.dimensions, vector, options);
  }

  public searchMarkdown(vector: readonly number[], options: {
    limit?: number;
    minSimilarity: number;
    maxSimilarity?: number;
  }): MarkdownSearchResult[] {
    return vectorSearch.searchMarkdown(this.#db, vector, options);
  }

  public vectorForFunction(id: number, kind: "code" | "description" = "code"): number[] {
    return vectorSearch.vectorForFunction(this.#db, id, kind);
  }

  public vectorForFile(filePath: string): number[] {
    return vectorSearch.vectorForFile(this.#db, filePath);
  }

  public fileVectorForFunction(id: number): number[] {
    return vectorSearch.fileVectorForFunction(this.#db, id);
  }

  public similarityCacheTriples(): Array<{
    functionId: number;
    codeEmbeddingId: number;
    descriptionEmbeddingId: number | null;
    fileDescriptionEmbeddingId: number | null;
  }> {
    return similarityCacheStore.similarityCacheTriples(this.#db);
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
    return similarityCacheStore.similarityCacheStates(this.#db, mode);
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
    return similarityCacheStore.cachedSimilarityNeighbors(this.#db, sourceId, mode, filter);
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
      similarityCacheStore.storeSimilarityNeighbors(this.#db, sourceId, mode, neighbors, state);
    });
  }

  public similarityCacheInfo(): { cachedSources: number; cachedPairs: number } {
    return similarityCacheStore.similarityCacheInfo(this.#db);
  }

  public similarityCacheCounts(mode: string): Map<number, number> {
    return similarityCacheStore.similarityCacheCounts(this.#db, mode);
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
    similarityCacheStore.touchSimilarityCacheState(this.#db, functionId, mode, state);
  }

  public status(): IndexStatus {
    return indexInspection.status(this.#db, {
      rootDir: this.#rootDir,
      indexPath: this.#indexPath,
      profile: this.#profile,
      getGeneration: () => this.getGeneration(),
      getCheckpoint: () => this.getCheckpoint(),
      descriptionsEnabled: () => this.descriptionsEnabled(),
      descriptionProfile: () => this.descriptionProfile(),
      filesWithErrors: () => this.filesWithErrors(),
    });
  }

  public indexErrors(): IndexingError[] {
    return indexInspection.errorsFromDatabase(this.#db);
  }

  public filesWithErrors(): string[] {
    return indexInspection.filesWithErrors(this.#db);
  }

  public filesWithFileTooLargeErrors(): string[] {
    return indexInspection.filesWithFileTooLargeErrors(this.#db);
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
    return indexInspection.errorsFromDatabase(database);
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
