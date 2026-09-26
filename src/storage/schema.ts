import type { DatabaseSync } from "node:sqlite";

import { IncompatibleIndexError } from "../errors.js";
import type { EmbeddingProfile } from "../types.js";

export const SCHEMA_VERSION = "11";
export const VEC0_MAX_DIMENSIONS = 8192;

export function metadataTableExists(database: DatabaseSync): boolean {
  return Boolean(database.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'metadata'").get());
}

export function readMetadata(database: DatabaseSync, key: string, checkTable = true): string | null {
  if (checkTable && !metadataTableExists(database)) return null;
  const row = database.prepare("SELECT value FROM metadata WHERE key = ?").get(key) as { value: string } | undefined;
  return row?.value ?? null;
}

export function setMetadata(database: DatabaseSync, key: string, value: string): void {
  database.prepare(`
    INSERT INTO metadata(key, value) VALUES (?, ?)
    ON CONFLICT(key) DO UPDATE SET value = excluded.value
  `).run(key, value);
}

export function deleteMetadata(database: DatabaseSync, key: string): void {
  database.prepare("DELETE FROM metadata WHERE key = ?").run(key);
}

export function initializeSchema(
  database: DatabaseSync,
  profile: Required<EmbeddingProfile>,
  transaction: (operation: () => void) => void,
): void {
  const existingVersion = metadataTableExists(database) ? readMetadata(database, "schema_version") : null;
  if (existingVersion && existingVersion !== "6" && existingVersion !== "7" && existingVersion !== "8" && existingVersion !== "9" && existingVersion !== "10" && existingVersion !== SCHEMA_VERSION) {
    throw new IncompatibleIndexError(`Unsupported index schema version ${existingVersion}.`);
  }
  database.exec(`
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
    transaction(() => {
      if (existingVersion === "6") {
        database.exec(`
          ALTER TABLE files ADD COLUMN file_description_path TEXT;
          ALTER TABLE files ADD COLUMN file_description_content_hash TEXT;
          ALTER TABLE files ADD COLUMN file_description TEXT;
          ALTER TABLE files ADD COLUMN file_description_embedding_id INTEGER REFERENCES embeddings(id);
        `);
      }
      const storedProfile = readMetadata(database, "embedding_profile");
      const dimensions = storedProfile
        ? Number((JSON.parse(storedProfile) as EmbeddingProfile).dimensions)
        : profile.dimensions;
      if (!Number.isInteger(dimensions) || dimensions <= 0) throw new IncompatibleIndexError("Index has an invalid embedding profile.");
      if (dimensions <= VEC0_MAX_DIMENSIONS) database.exec(functionVectorsSchema(dimensions));
      setMetadata(database, "schema_version", SCHEMA_VERSION);
      if (existingVersion) setMetadata(database, "markdown_scan_pending", "true");
    });
  } else if (existingVersion === "8") {
    transaction(() => {
      database.exec(similarityCacheSchema());
      setMetadata(database, "schema_version", SCHEMA_VERSION);
      setMetadata(database, "markdown_scan_pending", "true");
    });
  } else if (existingVersion === "9") {
    transaction(() => {
      database.exec(`
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
      setMetadata(database, "schema_version", SCHEMA_VERSION);
      setMetadata(database, "markdown_scan_pending", "true");
    });
  } else if (existingVersion === "10") {
    transaction(() => {
      setMetadata(database, "schema_version", SCHEMA_VERSION);
      setMetadata(database, "markdown_scan_pending", "true");
    });
  }
  database.exec("CREATE INDEX IF NOT EXISTS files_description_embedding ON files(file_description_embedding_id);");
  database.exec(similarityCacheSchema());
}

export function similarityCacheSchema(): string {
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

export function functionVectorsSchema(dimensions: number): string {
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
