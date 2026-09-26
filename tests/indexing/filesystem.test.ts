import { rmSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";

import Parser from "tree-sitter";
import { describe, expect, it, vi } from "vitest";

import { CodeIndex } from "../../src/code-index.js";
import { FakeEmbeddingProvider, commitAll, initGit, temporaryRoot, write } from "../helpers.js";
import { CountingEmbeddingProvider } from "./helpers.js";

describe("filesystem indexing", () => {
  it("preserves unchanged Markdown chunk rows across filesystem refreshes and explicit upserts", async () => {
    const root = temporaryRoot();
    write(root, "guide.md", "# Guide\n\nOriginal instructions.\n\n## Details\n\nMore information.\n");
    write(root, "reference.md", "# Reference\n\nStable reference documentation.\n");
    write(root, "value.ts", "export function value() { return 1; }\n");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromWorkingTree();
    const before = index.allMarkdownChunks();
    const guideBefore = before.filter((chunk) => chunk.path === "guide.md");
    expect(guideBefore).toHaveLength(2);
    const db = new DatabaseSync(index.indexPath);
    try {
      db.exec(`
        CREATE TABLE markdown_writes (operation TEXT, path TEXT);
        CREATE TRIGGER observe_chunk_delete AFTER DELETE ON markdown_chunks BEGIN
          INSERT INTO markdown_writes VALUES ('delete', old.path);
        END;
        CREATE TRIGGER observe_chunk_insert AFTER INSERT ON markdown_chunks BEGIN
          INSERT INTO markdown_writes VALUES ('insert', new.path);
        END;
      `);

      write(root, "value.ts", "export function value() { return 2; }\n");
      expect(await index.updateFromWorkingTree()).toMatchObject({ filesUpdated: 3, functionsUpdated: 1 });
      expect(index.allFunctions()[0]!.source).toContain("return 2");
      expect(index.allMarkdownChunks()).toEqual(before);
      expect(db.prepare("SELECT * FROM markdown_writes").all()).toEqual([]);

      expect(await index.updateFiles({ upsert: ["guide.md", "reference.md"] }))
        .toMatchObject({ filesUpdated: 2, embeddingsCreated: 0 });
      expect(index.allMarkdownChunks()).toEqual(before);
      expect(db.prepare("SELECT * FROM markdown_writes").all()).toEqual([]);

      const updatedGuide = "# Updated guide\n\nReplacement instructions.\n";
      write(root, "guide.md", updatedGuide);
      await index.updateFromWorkingTree();
      const after = index.allMarkdownChunks();
      const guideAfter = after.filter((chunk) => chunk.path === "guide.md");
      expect(guideAfter).toHaveLength(1);
      expect(guideAfter[0]).toMatchObject({
        headingPath: ["Updated guide"], content: updatedGuide.trim(), startLine: 1, endLine: 3,
      });
      expect(guideBefore.map((chunk) => chunk.sourceHash)).not.toContain(guideAfter[0]!.sourceHash);
      expect(guideBefore.map((chunk) => chunk.embeddingId)).not.toContain(guideAfter[0]!.embeddingId);
      expect(after.filter((chunk) => chunk.path === "reference.md"))
        .toEqual(before.filter((chunk) => chunk.path === "reference.md"));
      expect(db.prepare("SELECT * FROM markdown_writes ORDER BY operation").all()).toEqual([
        { operation: "delete", path: "guide.md" },
        { operation: "delete", path: "guide.md" },
        { operation: "insert", path: "guide.md" },
      ]);

      write(root, "guide.md", "");
      await index.updateFiles({ upsert: ["guide.md"] });
      expect(index.allMarkdownChunks()).toEqual(before.filter((chunk) => chunk.path === "reference.md"));
      expect((await index.updateFromWorkingTree()).filesUpdated).toBe(0);
    } finally {
      db.close();
      index.close();
    }
  });

  it("updates only changed rows and columns while retaining similarity caches", async () => {
    const root = temporaryRoot();
    const source = "export function stable() { return 1; }\nexport function changed() { return 2; }\n";
    write(root, "a.ts", source);
    write(root, "b.ts", "export function elsewhere() { return 3; }\n");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromWorkingTree();
    await index.refreshSimilarityCache({ width: 10 });
    const before = index.allFunctions();
    const db = new DatabaseSync(index.indexPath);
    try {
      const pairs = db.prepare("SELECT * FROM similarity_cache ORDER BY source_id, target_id").all();
      const states = db.prepare("SELECT * FROM similarity_cache_state ORDER BY function_id").all();
      expect(pairs.length).toBeGreaterThan(0);
      db.exec(`
        CREATE TABLE reconciliation_writes (operation TEXT, subject TEXT);
        CREATE TRIGGER observe_file_delete AFTER DELETE ON files BEGIN
          INSERT INTO reconciliation_writes VALUES ('delete-file', old.path);
        END;
        CREATE TRIGGER observe_file_update AFTER UPDATE ON files BEGIN
          INSERT INTO reconciliation_writes VALUES ('update-file', new.path);
        END;
        CREATE TRIGGER observe_function_delete AFTER DELETE ON functions BEGIN
          INSERT INTO reconciliation_writes VALUES ('delete-function', old.name);
        END;
        CREATE TRIGGER observe_function_update AFTER UPDATE ON functions BEGIN
          INSERT INTO reconciliation_writes VALUES ('update-function', new.name);
        END;
        CREATE TRIGGER observe_vector_update AFTER UPDATE OF embedding_id, line_count, path ON functions BEGIN
          INSERT INTO reconciliation_writes VALUES ('update-vector', new.name);
        END;
      `);

      write(root, "a.ts", source.replace("return 2", "return 22"));
      const generation = index.status().generation;
      expect(await index.updateFromWorkingTree()).toMatchObject({
        filesUpdated: 2, functionsUpdated: 3, functionsAdded: 0, functionsDeleted: 0, embeddingsCreated: 1,
      });
      expect(index.status().generation).toBe(generation + 1);
      expect(index.allFunctions().filter((value) => value.name !== "changed"))
        .toEqual(before.filter((value) => value.name !== "changed"));
      const changed = index.allFunctions().find((value) => value.name === "changed")!;
      expect(changed.id).toBe(before.find((value) => value.name === "changed")!.id);
      expect(changed.embeddingId).not.toBe(before.find((value) => value.name === "changed")!.embeddingId);
      // Even stale triples survive for incremental cache maintenance to reconcile.
      expect(db.prepare("SELECT * FROM similarity_cache ORDER BY source_id, target_id").all()).toEqual(pairs);
      expect(db.prepare("SELECT * FROM similarity_cache_state ORDER BY function_id").all()).toEqual(states);
      expect(db.prepare("SELECT * FROM reconciliation_writes ORDER BY operation").all()).toEqual([
        { operation: "update-file", subject: "a.ts" },
        { operation: "update-function", subject: "changed" },
        { operation: "update-vector", subject: "changed" },
      ]);

      db.exec("DELETE FROM reconciliation_writes");
      write(root, "a.ts", "// move both declarations\n" + source.replace("return 2", "return 22"));
      await index.updateFromWorkingTree();
      expect(db.prepare("SELECT * FROM reconciliation_writes WHERE operation = 'update-vector'").all()).toEqual([]);
      expect(index.allFunctions().find((value) => value.name === "stable")!.startLine).toBe(2);
      expect(db.prepare("SELECT * FROM similarity_cache ORDER BY source_id, target_id").all()).toEqual(pairs);

      db.exec("DELETE FROM reconciliation_writes");
      write(root, "a.ts", "// move both declarations\nexport function stable() { return 1; }\n");
      expect(await index.updateFromWorkingTree()).toMatchObject({ functionsDeleted: 1, functionsAdded: 0 });
      expect(db.prepare("SELECT * FROM reconciliation_writes WHERE operation LIKE 'delete-%'").all())
        .toEqual([{ operation: "delete-function", subject: "changed" }]);
      expect(db.prepare("SELECT * FROM similarity_cache ORDER BY source_id, target_id").all())
        .toEqual(pairs.filter((pair) => pair.source_id !== changed.id && pair.target_id !== changed.id));
    } finally {
      db.close();
      index.close();
    }
  });

  it("transactionally preserves rows and cache when duplicate declarations exchange identity keys", async () => {
    const root = temporaryRoot();
    const first = 'function same() { return "first"; }\n';
    const second = 'function same() { return "second"; }\n';
    write(root, "duplicates.js", first + second);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromWorkingTree();
    await index.refreshSimilarityCache({ width: 10 });
    const before = index.allFunctions();
    const cache = index.similarityCacheInfo();
    write(root, "duplicates.js", second + first);
    const db = new DatabaseSync(index.indexPath);
    const files = db.prepare("SELECT * FROM files").all();
    const generation = index.status().generation;
    db.exec(`
      CREATE TRIGGER fail_reconciliation BEFORE UPDATE OF start_line ON functions BEGIN
        SELECT RAISE(ABORT, 'injected reconciliation failure');
      END;
    `);
    try {
      await expect(index.updateFromWorkingTree()).rejects.toThrow("injected reconciliation failure");
      expect(index.allFunctions()).toEqual(before);
      expect(index.status().generation).toBe(generation);
      expect(index.similarityCacheInfo()).toEqual(cache);
      expect(db.prepare("SELECT * FROM files").all()).toEqual(files);
    } finally {
      db.exec("DROP TRIGGER fail_reconciliation");
      db.close();
    }
    expect(await index.updateFromWorkingTree()).toMatchObject({ functionsUpdated: 2, functionsAdded: 0, functionsDeleted: 0 });
    const after = index.allFunctions();
    expect(after.map((value) => value.id)).toEqual(before.map((value) => value.id).reverse());
    expect(after.map((value) => value.identityKey)).toEqual(before.map((value) => value.identityKey));
    expect(index.similarityCacheInfo()).toEqual(cache);
    expect((await index.updateFromWorkingTree()).filesUpdated).toBe(0);
    index.close();
  });

  it("applies diagnostic and byte-size changes even when unavailable content hashes match", async () => {
    const root = temporaryRoot();
    write(root, "large.ts", " ".repeat(100));
    const open = (maxFileSize: number) => new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider(), maxFileSize, onWarning: () => {} });
    let index = open(10);
    await index.updateFromWorkingTree();
    const generation = index.status().generation;
    index.close();
    index = open(20);
    expect((await index.updateFromWorkingTree()).filesUpdated).toBe(1);
    expect(index.status().generation).toBe(generation + 1);
    expect(index.indexErrors()[0]!.message).toContain("20 bytes");
    expect((await index.updateFromWorkingTree()).filesUpdated).toBe(0);
    write(root, "large.ts", " ".repeat(200));
    expect((await index.updateFromWorkingTree()).filesUpdated).toBe(1);
    const db = new DatabaseSync(index.indexPath);
    expect(db.prepare("SELECT byte_size FROM files WHERE path = 'large.ts'").get()).toEqual({ byte_size: 200 });
    db.close();
    // Both the unavailable placeholder and an eligible empty file hash to "".
    write(root, "large.ts", "");
    expect((await index.updateFromWorkingTree()).filesUpdated).toBe(1);
    expect(index.indexErrors()).toEqual([]);
    expect((await index.updateFromWorkingTree()).filesUpdated).toBe(0);
    index.close();
  });

  it("persists recovered diagnostics on an otherwise unchanged filesystem refresh", async () => {
    const root = temporaryRoot();
    write(root, "empty.ts", "// no callables\n");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider(), onWarning: () => {} });
    const parse = vi.spyOn(Parser.prototype, "parse").mockImplementationOnce(() => { throw new Error("transient parser failure"); });
    try {
      await index.updateFromWorkingTree();
    } finally {
      parse.mockRestore();
    }
    expect(index.indexErrors()).toHaveLength(1);
    expect(index.allFunctions()).toEqual([]);
    const generation = index.status().generation;
    expect((await index.updateFromWorkingTree()).filesUpdated).toBe(1);
    expect(index.indexErrors()).toEqual([]);
    expect(index.status().generation).toBe(generation + 1);
    expect((await index.updateFromWorkingTree()).filesUpdated).toBe(0);
    index.close();
  });

  it("skips the write when nothing changed and reuses embeddings", async () => {
    const root = temporaryRoot();
    write(root, "src/value.ts", `export function value() { return 1; }\n`);
    const provider = new CountingEmbeddingProvider();
    const index = new CodeIndex({ rootDir: root, provider });

    const first = await index.updateFromWorkingTree();
    const generation = index.status().generation;
    const second = await index.updateFromWorkingTree();

    expect(first.filesUpdated).toBe(1);
    expect(second).toMatchObject({
      filesUpdated: 0,
      filesDeleted: 0,
      functionsAdded: 0,
      functionsUpdated: 0,
      functionsDeleted: 0,
      embeddingsCreated: 0,
    });
    expect(index.status().generation).toBe(generation);
    expect(provider.documentCount).toBe(1);
    expect(index.status().gitCheckpoint).toBeNull();
    expect(index.allFunctions()[0]!.sourceMode).toBe("working-tree");
    index.close();
  });

  it("reconciles added and deleted files without Git", async () => {
    const root = temporaryRoot();
    write(root, "old.ts", `export function oldValue() { return 1; }\n`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromWorkingTree();

    rmSync(`${root}/old.ts`);
    write(root, "new.ts", `export function newValue() { return 2; }\n`);
    const stats = await index.updateFromWorkingTree();

    expect(stats).toMatchObject({ filesUpdated: 1, filesDeleted: 1, checkpoint: null });
    expect(index.allFunctions().map((item) => item.name)).toEqual(["newValue"]);
    index.close();
  });

  it("clears a previous Git checkpoint after a filesystem refresh", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "value.ts", `export function value() { return 1; }\n`);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    expect(index.status().gitCheckpoint).not.toBeNull();

    rmSync(`${root}/.git`, { recursive: true });
    await index.updateFromWorkingTree();

    expect(index.status().gitCheckpoint).toBeNull();
    index.close();
  });

  it("re-reads an oversized source file that becomes eligible while indexing", async () => {
    const root = temporaryRoot();
    write(root, "large.ts", " ".repeat(256));
    write(root, "trigger.ts", `export function trigger() { return 1; }\n`);
    class ShrinkingProvider extends FakeEmbeddingProvider {
      public override async embedDocuments(inputs: readonly string[]): Promise<number[][]> {
        write(root, "large.ts", `export function appeared() { return 2; }\n`);
        return await super.embedDocuments(inputs);
      }
    }
    const index = new CodeIndex({ rootDir: root, provider: new ShrinkingProvider(), maxFileSize: 128 });

    await index.updateFromWorkingTree();
    expect(index.allFunctions().map((item) => item.name)).toEqual(["appeared", "trigger"]);
    index.close();
  });
});
