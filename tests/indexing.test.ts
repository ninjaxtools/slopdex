import { chmodSync, renameSync, rmSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";

import Parser from "tree-sitter";
import { describe, expect, it, vi } from "vitest";

import { CodeIndex } from "../src/code-index.js";
import type { IndexProgress } from "../src/types.js";
import { FakeEmbeddingProvider, commitAll, git, initGit, temporaryRoot, write } from "./helpers.js";

class CountingEmbeddingProvider extends FakeEmbeddingProvider {
  public documentCount = 0;

  public override async embedDocuments(inputs: readonly string[]): Promise<number[][]> {
    this.documentCount += inputs.length;
    return await super.embedDocuments(inputs);
  }
}

describe("explicit indexing", () => {
  it("updates, reconciles, and deletes specific files", async () => {
    const root = temporaryRoot();
    write(root, "src/math.ts", `
export function add(a: number, b: number) { return a + b; }
export const subtract = (a: number, b: number) => a - b;
`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });

    const initial = await index.updateFiles({ upsert: ["src/math.ts"] });
    expect(initial).toMatchObject({ filesUpdated: 1, functionsAdded: 2, embeddingsCreated: 2 });
    const addId = index.allFunctions().find((item) => item.name === "add")!.id;

    write(root, "src/math.ts", `
export function add(a: number, b: number) { return Number(a) + Number(b); }
export function multiply(a: number, b: number) { return a * b; }
`);
    const updated = await index.updateFiles({ upsert: ["src/math.ts"] });
    expect(updated).toMatchObject({ functionsAdded: 1, functionsUpdated: 1, functionsDeleted: 1 });
    expect(index.allFunctions().find((item) => item.name === "add")!.id).toBe(addId);

    const deleted = await index.updateFiles({ delete: ["src/math.ts"] });
    expect(deleted).toMatchObject({ filesDeleted: 1, functionsDeleted: 2 });
    expect(index.status().functionCount).toBe(0);
    index.close();
  });

  it("deduplicates identical embedding inputs across files", async () => {
    const root = temporaryRoot();
    write(root, "one.ts", `export function same() { return 1; }\n`);
    write(root, "two.ts", `export function same() { return 1; }\n`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    const stats = await index.updateFiles({ upsert: ["one.ts", "two.ts"] });
    expect(stats).toMatchObject({ functionsAdded: 2, embeddingsCreated: 1 });
    expect(index.status().functionCount).toBe(2);
    index.close();
  });

  it("embeds vector batches concurrently up to the configured parallelism", async () => {
    const root = temporaryRoot();
    const paths: string[] = [];
    for (let index = 0; index < 6; index += 1) {
      paths.push(`file${index}.ts`);
      write(root, `file${index}.ts`, `export function fn${index}() { return ${index}; }\n`);
    }
    let active = 0;
    let maximum = 0;
    class TrackingProvider extends FakeEmbeddingProvider {
      public override async embedDocuments(inputs: readonly string[]): Promise<number[][]> {
        active += 1;
        maximum = Math.max(maximum, active);
        await new Promise((resolve) => setTimeout(resolve, 2));
        active -= 1;
        return await super.embedDocuments(inputs);
      }
    }
    const progress: IndexProgress[] = [];
    const index = new CodeIndex({
      rootDir: root,
      provider: new TrackingProvider(),
      embeddingBatchSize: 1,
      parallelism: 3,
      onProgress: (value) => progress.push(value),
    });

    await index.updateFiles({ upsert: paths });

    expect(maximum).toBe(3);
    expect(progress[0]).toEqual({ phase: "vectors", completed: 0, total: 6 });
    expect(progress.at(-1)).toEqual({ phase: "vectors", completed: 6, total: 6 });
    index.close();
  });

  it("rejects invalid parallelism", () => {
    const root = temporaryRoot();
    expect(() => new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider(), parallelism: 0 }))
      .toThrow(/parallelism must be a positive integer/);
  });

  it("durably caches parsing and each completed embedding before a failed initial update", async () => {
    const root = temporaryRoot();
    write(root, "functions.ts", "export function one() { return 1; }\nexport function two() { return 2; }\n");
    class FailingProvider extends FakeEmbeddingProvider {
      public inputs: string[] = [];
      public fail = true;

      public override async embedDocuments(inputs: readonly string[]): Promise<number[][]> {
        this.inputs.push(...inputs);
        if (this.fail && inputs.some((input) => input.includes("symbol: two"))) throw new Error("embedding failed");
        return await super.embedDocuments(inputs);
      }
    }
    const provider = new FailingProvider();
    const first = new CodeIndex({ rootDir: root, provider, embeddingBatchSize: 1 });
    await expect(first.updateFiles({ upsert: ["functions.ts"] })).rejects.toThrow(/embedding failed/);
    expect(first.status()).toMatchObject({ fileCount: 0, functionCount: 0, generation: 0 });
    first.close();

    provider.fail = false;
    const parse = vi.spyOn(Parser.prototype, "parse").mockImplementation(() => { throw new Error("parse should be cached"); });
    const resumed = new CodeIndex({ rootDir: root, provider, embeddingBatchSize: 1 });
    await expect(resumed.updateFiles({ upsert: ["functions.ts"] })).resolves.toMatchObject({
      functionsAdded: 2,
      embeddingsCreated: 1,
    });
    expect(provider.inputs.filter((input) => input.includes("symbol: one"))).toHaveLength(1);
    expect(provider.inputs.filter((input) => input.includes("symbol: two"))).toHaveLength(2);
    expect(parse).not.toHaveBeenCalled();
    parse.mockRestore();
    resumed.close();
  });

  it("re-reads explicitly updated source that changes during embedding", async () => {
    const root = temporaryRoot();
    write(root, "value.ts", "export function value() { return 1; }\n");
    let changed = false;
    class ChangingProvider extends FakeEmbeddingProvider {
      public override async embedDocuments(inputs: readonly string[]): Promise<number[][]> {
        if (!changed) {
          changed = true;
          write(root, "value.ts", "export function value() { return 2; }\n");
        }
        return await super.embedDocuments(inputs);
      }
    }
    const index = new CodeIndex({ rootDir: root, provider: new ChangingProvider() });

    await index.updateFiles({ upsert: ["value.ts"] });

    expect(index.allFunctions()[0]!.source).toContain("return 2");
    index.close();
  });

  it("takes one stable snapshot across a multi-file explicit update", async () => {
    const root = temporaryRoot();
    write(root, ".gitignore", "");
    write(root, "first.ts", "export function first() { return 1; }\n");
    write(root, "second.ts", "export function second() { return 2; }\n");
    let scheduled = false;
    class DelayedChangeProvider extends FakeEmbeddingProvider {
      public override async embedDocuments(inputs: readonly string[]): Promise<number[][]> {
        if (!scheduled) {
          scheduled = true;
          setTimeout(() => write(root, "first.ts", "export function first() { return 3; }\n"), 0);
        }
        return await super.embedDocuments(inputs);
      }
    }
    const index = new CodeIndex({ rootDir: root, provider: new DelayedChangeProvider() });

    await index.updateFiles({ upsert: ["first.ts", "second.ts"] });

    expect(index.allFunctions().find((callable) => callable.name === "first")!.source).toContain("return 3");
    index.close();
  });

  it.skipIf(process.getuid?.() === 0)("persists explicit read errors without aborting healthy files", async () => {
    const root = temporaryRoot();
    write(root, "locked.ts", "export function locked() { return 1; }\n");
    write(root, "healthy.ts", "export function healthy() { return 2; }\n");
    const lockedPath = `${root}/locked.ts`;
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    chmodSync(lockedPath, 0);

    try {
      await index.updateFiles({ upsert: ["locked.ts", "healthy.ts"] });

      expect(index.allFunctions().map((callable) => callable.name)).toEqual(["healthy"]);
      expect(index.indexErrors()[0]).toMatchObject({ path: "locked.ts", code: "read-error" });
    } finally {
      chmodSync(lockedPath, 0o644);
      index.close();
    }
  });
});

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

describe("Git indexing", () => {
  it("reconciles unchanged committed files against a lower maxFileSize", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "large.ts", `export function large() { return 1; }\n//${"x".repeat(256)}\n`);
    commitAll(root, "base");
    const initial = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider(), maxFileSize: 1024 });
    await initial.updateFromGit();
    initial.close();

    const restricted = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider(), maxFileSize: 64 });
    await restricted.updateFromGit();

    expect(restricted.status()).toMatchObject({ functionCount: 0, indexingErrorCount: 1 });
    expect(restricted.indexErrors()[0]).toMatchObject({ path: "large.ts", code: "file-too-large" });
    restricted.close();
  });

  it("can update committed files without indexing working-tree changes", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "value.ts", `export function committed() { return 1; }\n`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    write(root, "value.ts", `export function dirty() { return 2; }\n`);
    write(root, "untracked.ts", `export function untracked() { return 3; }\n`);
    await index.updateFromGit({ includeWorkingTree: false });

    expect(index.status().gitCheckpoint).toBe(base);
    expect(index.allFunctions().map((item) => [item.name, item.sourceMode])).toEqual([["committed", "git"]]);
    index.close();
  });

  it("removes an existing working-tree overlay when it is disabled", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "value.ts", `export function committed() { return 1; }\n`);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    write(root, "value.ts", `export function dirty() { return 2; }\n`);
    write(root, "untracked.ts", `export function untracked() { return 3; }\n`);
    await index.updateFromGit();

    await index.updateFromGit({ includeWorkingTree: false });

    expect(index.allFunctions().map((item) => [item.name, item.sourceMode])).toEqual([["committed", "git"]]);
    index.close();
  });

  it("preserves identity when a working-tree rename is committed before overlays are disabled", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "old.ts", `export function stable() { return 1; }\n`);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    const originalId = index.allFunctions()[0]!.id;

    git(root, "mv", "old.ts", "new.ts");
    await index.updateFromGit();
    commitAll(root, "rename");
    await index.updateFromGit({ includeWorkingTree: false });

    expect(index.allFunctions()[0]).toMatchObject({ id: originalId, path: "new.ts", sourceMode: "git" });
    index.close();
  });

  it("advances the checkpoint without re-indexing unchanged committed files", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "stable.ts", `export function stable() { return 1; }\n`);
    const base = commitAll(root, "base");
    const provider = new CountingEmbeddingProvider();
    const index = new CodeIndex({ rootDir: root, provider });
    await index.updateFromGit();
    const stable = index.allFunctions()[0]!;

    write(root, "README.txt", "Documentation only\n");
    const target = commitAll(root, "documentation");
    const stats = await index.updateFromGit();

    expect(stats).toMatchObject({ filesUpdated: 0, filesDeleted: 0, checkpoint: target });
    expect(provider.documentCount).toBe(1);
    expect(index.allFunctions()[0]).toMatchObject({ id: stable.id, lastSeenCommit: base, sourceMode: "git" });
    index.close();
  });

  it("does not re-index a committed file when only its executable bit changes", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "script.ts", `export function run() { return 1; }\n`);
    commitAll(root, "base");
    const provider = new CountingEmbeddingProvider();
    const index = new CodeIndex({ rootDir: root, provider });
    await index.updateFromGit();

    chmodSync(`${root}/script.ts`, 0o755);
    const target = commitAll(root, "make executable");
    const stats = await index.updateFromGit();

    expect(stats).toMatchObject({ filesUpdated: 0, checkpoint: target });
    expect(provider.documentCount).toBe(1);
    index.close();
  });

  it("re-indexes dirty files on every refresh while reusing function embeddings", async () => {
    const root = temporaryRoot();
    initGit(root);
    const committedSource = `export function value() { return 1; }\n`;
    write(root, "value.ts", committedSource);
    commitAll(root, "base");
    const provider = new CountingEmbeddingProvider();
    const index = new CodeIndex({ rootDir: root, provider });
    await index.updateFromGit();

    write(root, "value.ts", `export function value() { return 2; }\n`);
    const firstDirty = await index.updateFromGit();
    const secondDirty = await index.updateFromGit();
    expect(firstDirty.filesUpdated).toBe(1);
    expect(secondDirty.filesUpdated).toBe(1);
    expect(index.allFunctions()[0]!.sourceMode).toBe("working-tree");
    expect(provider.documentCount).toBe(2);

    write(root, "value.ts", committedSource);
    const restored = await index.updateFromGit();
    expect(restored.filesUpdated).toBe(1);
    expect(index.allFunctions()[0]!.sourceMode).toBe("git");
    expect(provider.documentCount).toBe(2);
    index.close();
  });

  it("preserves function identity when refreshing an uncommitted rename repeatedly", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "old.ts", `export function stable() { return 1; }\n`);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    const originalId = index.allFunctions()[0]!.id;

    git(root, "mv", "old.ts", "new.ts");
    await index.updateFromGit();
    const firstRefreshId = index.allFunctions()[0]!.id;
    await index.updateFromGit();

    expect(firstRefreshId).toBe(originalId);
    expect(index.allFunctions()[0]).toMatchObject({ id: originalId, path: "new.ts", sourceMode: "working-tree" });
    index.close();
  });

  it("preserves function identity when an uncommitted rename is undone", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "old.ts", `export function stable() { return 1; }\n`);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    const originalId = index.allFunctions()[0]!.id;

    git(root, "mv", "old.ts", "new.ts");
    await index.updateFromGit();
    git(root, "mv", "new.ts", "old.ts");
    await index.updateFromGit();

    expect(index.allFunctions()[0]).toMatchObject({ id: originalId, path: "old.ts", sourceMode: "git" });
    index.close();
  });

  it("preserves the source identity when a tracked rename replaces an indexed untracked file", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "old.ts", `export function tracked() { return 1; }\n`);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    const trackedId = index.allFunctions()[0]!.id;

    write(root, "new.ts", `export function transient() { return 2; }\n`);
    await index.updateFromGit();
    rmSync(`${root}/new.ts`);
    git(root, "mv", "old.ts", "new.ts");
    await index.updateFromGit();

    expect(index.allFunctions()).toHaveLength(1);
    expect(index.allFunctions()[0]).toMatchObject({ id: trackedId, name: "tracked", path: "new.ts" });
    index.close();
  });

  it("preserves the source identity when a committed rename replaces an indexed untracked file", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "old.ts", `export function tracked() { return 1; }\n`);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    const trackedId = index.allFunctions()[0]!.id;

    write(root, "new.ts", `export function transient() { return 2; }\n`);
    await index.updateFromGit();
    rmSync(`${root}/new.ts`);
    git(root, "mv", "old.ts", "new.ts");
    commitAll(root, "rename");
    const stats = await index.updateFromGit();

    expect(stats.filesDeleted).toBe(1);
    expect(index.allFunctions()).toHaveLength(1);
    expect(index.allFunctions()[0]).toMatchObject({ id: trackedId, name: "tracked", path: "new.ts", sourceMode: "git" });
    index.close();
  });

  it("restores rename identity when both the old and dirty destination paths are committed", async () => {
    const root = temporaryRoot();
    initGit(root);
    const originalSource = `export function original() { return 1; }\n`;
    write(root, "old.ts", originalSource);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    const originalId = index.allFunctions()[0]!.id;

    git(root, "mv", "old.ts", "new.ts");
    await index.updateFromGit();
    write(root, "old.ts", originalSource);
    write(root, "new.ts", `export function independent() { return 2; }\n`);
    commitAll(root, "restore old and add new");
    await index.updateFromGit();

    expect(index.allFunctions().find((item) => item.name === "original")).toMatchObject({
      id: originalId,
      path: "old.ts",
      sourceMode: "git",
    });
    expect(index.allFunctions().find((item) => item.name === "independent")!.id).not.toBe(originalId);
    index.close();
  });

  it("restores rename identity before overlaying a dirty independent destination", async () => {
    const root = temporaryRoot();
    initGit(root);
    const originalSource = `export function original() { return 1; }\n`;
    write(root, "old.ts", originalSource);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    const originalId = index.allFunctions()[0]!.id;

    git(root, "mv", "old.ts", "new.ts");
    await index.updateFromGit();
    write(root, "old.ts", originalSource);
    write(root, "new.ts", `export function independent() { return 2; }\n`);
    commitAll(root, "restore old and add new");
    write(root, "new.ts", `export function dirtyIndependent() { return 3; }\n`);
    await index.updateFromGit();

    expect(index.allFunctions().find((item) => item.name === "original")).toMatchObject({
      id: originalId,
      path: "old.ts",
      sourceMode: "git",
    });
    expect(index.allFunctions().find((item) => item.name === "dirtyIndependent")).toMatchObject({
      path: "new.ts",
      sourceMode: "working-tree",
    });
    index.close();
  });

  it("preserves function identity for an untracked file rename", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "tracked.ts", `export function tracked() { return 1; }\n`);
    commitAll(root, "base");
    write(root, "a-untracked.ts", `export function moving() { return 2; }\n`);
    write(root, "z-untracked.ts", `export function staying() { return 3; }\n`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    const movingId = index.allFunctions().find((item) => item.name === "moving")!.id;

    renameSync(`${root}/a-untracked.ts`, `${root}/b-untracked.ts`);
    await index.updateFromGit();

    expect(index.allFunctions().find((item) => item.name === "moving")).toMatchObject({
      id: movingId,
      path: "b-untracked.ts",
      sourceMode: "working-tree",
    });
    index.close();
  });

  it("does not guess identity for an ambiguous untracked rename", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "tracked.ts", `export function tracked() { return 1; }\n`);
    commitAll(root, "base");
    const duplicateSource = `export function duplicate() { return 2; }\n`;
    write(root, "a.ts", duplicateSource);
    write(root, "b.ts", duplicateSource);
    write(root, "z.ts", `export function staying() { return 3; }\n`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    const duplicateIds = index.allFunctions()
      .filter((item) => item.name === "duplicate")
      .map((item) => item.id);

    rmSync(`${root}/a.ts`);
    renameSync(`${root}/b.ts`, `${root}/c.ts`);
    await index.updateFromGit();

    const renamedId = index.allFunctions().find((item) => item.path === "c.ts")!.id;
    expect(duplicateIds).not.toContain(renamedId);
    index.close();
  });

  it("reconciles divergent branches without re-indexing shared blobs", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "stable.ts", `export function stable() { return 1; }\n`);
    const base = commitAll(root, "base");
    write(root, "main.txt", "main\n");
    commitAll(root, "main");
    const provider = new CountingEmbeddingProvider();
    const index = new CodeIndex({ rootDir: root, provider });
    await index.updateFromGit();

    git(root, "checkout", "-q", "-b", "other", base);
    write(root, "other.txt", "other\n");
    const target = commitAll(root, "other");
    const stats = await index.updateFromGit({ rebuildOnDivergence: true });

    expect(stats).toMatchObject({ filesUpdated: 0, checkpoint: target });
    expect(provider.documentCount).toBe(1);
    index.close();
  });

  it("rejects a prepared update if another writer changes the index", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "value.ts", `export function committed() { return 1; }\n`);
    write(root, "other.ts", `export function other() { return 2; }\n`);
    const base = commitAll(root, "base");
    const initial = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await initial.updateFromGit();
    initial.close();

    write(root, "value.ts", `export function dirty() { return 3; }\n`);
    class ConcurrentProvider extends FakeEmbeddingProvider {
      public override async embedDocuments(inputs: readonly string[]): Promise<number[][]> {
        const concurrent = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
        await concurrent.updateFiles({ upsert: ["other.ts"] });
        concurrent.close();
        return await super.embedDocuments(inputs);
      }
    }
    const index = new CodeIndex({ rootDir: root, provider: new ConcurrentProvider() });

    await expect(index.updateFromGit()).rejects.toThrow(/Index changed while the update was being prepared/);
    expect(index.status().gitCheckpoint).toBe(base);
    expect(index.allFunctions().map((item) => [item.name, item.sourceMode])).toEqual([
      ["other", "working-tree"],
      ["committed", "git"],
    ]);
    index.close();
  });

  it.each(["unstaged", "staged", "untracked"] as const)("overlays %s changes without advancing past HEAD", async (change) => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "valid.ts", `export function valid() { return true; }\n`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    if (change === "untracked") {
      write(root, "new.ts", `export function added() { return true; }\n`);
    } else {
      write(root, "valid.ts", `export function changed() { return true; }\n`);
      if (change === "staged") git(root, "add", "valid.ts");
    }

    await index.updateFromGit();
    expect(index.status().gitCheckpoint).toBe(base);
    const functions = index.allFunctions();
    expect(functions.map((item) => item.name)).toEqual(change === "untracked" ? ["added", "valid"] : ["changed"]);
    expect(functions.find((item) => item.name === (change === "untracked" ? "added" : "changed"))!.sourceMode).toBe("working-tree");
    index.close();
  });

  it("filters changed-since and uncommitted functions from the working-tree overlay", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "tracked.ts", `
export function stable() { return 1; }
export function modified() { return 2; }
`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    write(root, "tracked.ts", `
export function stable() { return 1; }
export function modified() { return 3; }
`);
    write(root, "untracked.ts", `export function untracked() { return 4; }\n`);
    await index.updateFromGit();

    const changed = await index.sourceFunctions({ type: "changed-since", commit: base });
    expect(changed.map((item) => item.name)).toEqual(["modified", "untracked"]);
    const uncommitted = await index.sourceFunctions({ type: "uncommitted" });
    expect(uncommitted.map((item) => item.name)).toEqual(["stable", "modified", "untracked"]);
    index.close();
  });

  it("indexes net changes, records a checkpoint, and identifies changed functions", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "src/service.ts", `export function existing() { return "old"; }\n`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });

    await index.updateFromGit();
    const existingId = index.allFunctions()[0]!.id;
    expect(index.status().gitCheckpoint).toBe(base);

    write(root, "src/service.ts", `
export function existing() { return "changed"; }
export function added() { return "new"; }
`);
    const target = commitAll(root, "add function");
    const stats = await index.updateFromGit();
    expect(stats.checkpoint).toBe(target);
    expect(index.allFunctions().find((item) => item.name === "existing")!.id).toBe(existingId);
    const changed = await index.sourceFunctions({ type: "changed-since", commit: base });
    expect(changed.map((item) => item.name)).toEqual(["existing", "added"]);
    expect(changed.find((item) => item.name === "added")!.firstSeenCommit).toBe(target);
    index.close();
  });

  it("keeps an explicit historical target separate from the current checkout", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "value.ts", `export function historical() { return 1; }\n`);
    const historical = commitAll(root, "historical");
    write(root, "value.ts", `export function current() { return 2; }\n`);
    commitAll(root, "current");
    write(root, "value.ts", `export function dirty() { return 3; }\n`);
    write(root, "untracked.ts", `export function untracked() { return 4; }\n`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });

    await index.updateFromGit({ target: historical });

    expect(index.status().gitCheckpoint).toBe(historical);
    expect(index.allFunctions().map((item) => [item.name, item.sourceMode])).toEqual([["historical", "git"]]);
    index.close();
  });

  it("re-reads a working-tree file that changes while embeddings are prepared", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "value.ts", `export function committed() { return 1; }\n`);
    const base = commitAll(root, "base");
    const initial = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await initial.updateFromGit();
    initial.close();

    write(root, "value.ts", `export function dirty() { return 2; }\n`);
    class MutatingProvider extends FakeEmbeddingProvider {
      public override async embedDocuments(inputs: readonly string[]): Promise<number[][]> {
        write(root, "value.ts", `export function changedAgain() { return 3; }\n`);
        return await super.embedDocuments(inputs);
      }
    }
    const index = new CodeIndex({ rootDir: root, provider: new MutatingProvider() });

    await index.updateFromGit();
    expect(index.status().gitCheckpoint).toBe(base);
    expect(index.allFunctions().map((item) => item.name)).toEqual(["changedAgain"]);
    index.close();
  });

  it("preserves function identity across a Git file rename", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "old.ts", `export function stable() { return 1; }\n`);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    const original = index.allFunctions()[0]!;

    git(root, "mv", "old.ts", "new.ts");
    commitAll(root, "rename");
    await index.updateFromGit();
    const renamed = index.allFunctions()[0]!;
    expect(renamed.path).toBe("new.ts");
    expect(renamed.id).toBe(original.id);
    expect(renamed.firstSeenCommit).toBe(original.firstSeenCommit);
    index.close();
  });

  it("removes files deleted by a Git commit", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "keep.ts", `export function keep() { return 1; }\n`);
    write(root, "remove.ts", `export function remove() { return 2; }\n`);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    rmSync(`${root}/remove.ts`);
    const target = commitAll(root, "remove file");
    const stats = await index.updateFromGit();

    expect(stats).toMatchObject({ filesDeleted: 1, functionsDeleted: 1, checkpoint: target });
    expect(index.allFunctions().map((item) => item.name)).toEqual(["keep"]);
    index.close();
  });

  it("warns and advances the checkpoint when parsing is incomplete", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "valid.ts", `export function valid() { return true; }\n`);
    const base = commitAll(root, "base");
    const warnings: string[] = [];
    const index = new CodeIndex({
      rootDir: root,
      provider: new FakeEmbeddingProvider(),
      onWarning: (message) => warnings.push(message),
    });
    await index.updateFromGit();

    write(root, "valid.ts", "export function broken( {");
    const broken = commitAll(root, "broken");
    await index.updateFromGit();
    expect(index.status().gitCheckpoint).toBe(broken);
    expect(index.status().gitCheckpoint).not.toBe(base);
    expect(index.allFunctions()).toEqual([]);
    expect(warnings).toEqual([
      "Cannot fully parse valid.ts: tree-sitter reported syntax errors; indexing recoverable callables only.",
    ]);
    index.close();
  });

  it("reconciles a working-tree update after it is committed", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "value.ts", `export function committed() { return 1; }\n`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    write(root, "value.ts", `export function dirty() { return 2; }\n`);
    await index.updateFiles({ upsert: ["value.ts"] });
    expect(index.allFunctions()[0]!.name).toBe("dirty");

    await index.updateFromGit();
    expect(index.status().gitCheckpoint).toBe(base);
    expect(index.allFunctions()[0]!.sourceMode).toBe("working-tree");

    commitAll(root, "update value");
    await index.updateFromGit();
    expect(index.allFunctions()[0]!.name).toBe("dirty");
    expect(index.allFunctions()[0]!.sourceMode).toBe("git");
    index.close();
  });

  it("retains first-seen provenance across temporary working-tree replacements", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "value.ts", `export function original() { return 1; }\n`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    write(root, "value.ts", `export function temporary() { return 2; }\n`);
    write(root, "unrelated.ts", `export function unrelated() { return 3; }\n`);
    git(root, "add", "unrelated.ts");
    git(root, "commit", "-q", "-m", "unrelated");
    const target = git(root, "rev-parse", "HEAD");
    await index.updateFromGit();

    write(root, "value.ts", `export function original() { return 1; }\n`);
    await index.updateFromGit();

    const original = index.allFunctions().find((item) => item.name === "original")!;
    expect(index.status().gitCheckpoint).toBe(target);
    expect(original.firstSeenCommit).toBe(base);
    index.close();
  });

  it("removes transient working-tree files before applying a new commit and current overlay", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "committed.ts", `export function committed() { return 1; }\n`);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    write(root, "transient.ts", `export function transient() { return 2; }\n`);
    await index.updateFiles({ upsert: ["transient.ts"] });
    rmSync(`${root}/transient.ts`);
    write(root, "committed.ts", `export function committedAtTarget() { return 3; }\n`);
    const target = commitAll(root, "next commit");
    write(root, "dirty.ts", `export function dirty() { return 4; }\n`);

    await index.updateFromGit();

    expect(index.status().gitCheckpoint).toBe(target);
    expect(index.allFunctions().map((item) => [item.name, item.sourceMode])).toEqual([
      ["committedAtTarget", "git"],
      ["dirty", "working-tree"],
    ]);
    index.close();
  });

  it("restores an explicitly deleted committed file", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "kept.ts", `export function kept() { return 1; }\n`);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    await index.updateFiles({ delete: ["kept.ts"] });
    expect(index.status().functionCount).toBe(0);

    await index.updateFromGit();
    expect(index.allFunctions().map((item) => item.name)).toEqual(["kept"]);
    index.close();
  });

  it("reconciles include and exclude policy at an unchanged checkpoint", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "keep.ts", `export function keep() { return 1; }\n`);
    write(root, "skip.ts", `export function skip() { return 2; }\n`);
    commitAll(root, "base");
    const provider = new FakeEmbeddingProvider();
    const first = new CodeIndex({ rootDir: root, provider });
    await first.updateFromGit();
    first.close();

    const second = new CodeIndex({ rootDir: root, provider, exclude: ["skip.ts"] });
    await second.updateFromGit();
    expect(second.allFunctions().map((item) => item.name)).toEqual(["keep"]);
    second.close();
  });

  it("classifies an explicit working-tree rename as changed", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "old.ts", `export function stable() { return 1; }\n`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    git(root, "mv", "old.ts", "new.ts");
    await index.updateFiles({ renames: [{ from: "old.ts", to: "new.ts" }] });

    const changed = await index.sourceFunctions({ type: "changed-since", commit: base });
    expect(changed.map((item) => item.name)).toEqual(["stable"]);
    index.close();
  });

  it("composes sequential working-tree rename provenance", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "old.ts", `export function stable() { return 1; }\n`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    git(root, "mv", "old.ts", "mid.ts");
    await index.updateFiles({ renames: [{ from: "old.ts", to: "mid.ts" }] });
    git(root, "mv", "mid.ts", "new.ts");
    await index.updateFiles({ renames: [{ from: "mid.ts", to: "new.ts" }] });
    write(root, "new.ts", `export function stable() { return Number(1); }\n`);
    await index.updateFiles({ upsert: ["new.ts"] });

    expect((await index.sourceFunctions({ type: "changed-since", commit: base })).map((item) => item.name)).toEqual(["stable"]);
    index.close();
  });

  it("matches duplicate names by source before declaration order", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "duplicates.js", `function same() { return "old"; }\n`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    write(root, "duplicates.js", `
function same() { return "new"; }
function same() { return "old"; }
`);
    const target = commitAll(root, "insert duplicate");
    await index.updateFromGit();
    const changed = await index.sourceFunctions({ type: "changed-since", commit: base });
    expect(changed).toHaveLength(1);
    expect(changed[0]!.source).toContain('return "new"');
    expect(changed[0]!.firstSeenCommit).toBe(target);
    const old = index.allFunctions().find((item) => item.source.includes('return "old"'))!;
    expect(old.firstSeenCommit).toBe(base);
    index.close();
  });

  it("rejects cyclic explicit rename batches", async () => {
    const root = temporaryRoot();
    write(root, "a.ts", `export function a() { return 1; }\n`);
    write(root, "b.ts", `export function b() { return 2; }\n`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["a.ts", "b.ts"] });
    await expect(index.updateFiles({
      renames: [
        { from: "a.ts", to: "b.ts" },
        { from: "b.ts", to: "a.ts" },
      ],
    })).rejects.toThrow(/cyclic/);
    expect(index.status().functionCount).toBe(2);
    index.close();
  });
});
