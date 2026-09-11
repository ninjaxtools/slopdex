import { chmodSync, renameSync, rmSync } from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";

import Parser from "tree-sitter";
import { afterEach, describe, expect, it, onTestFinished, vi } from "vitest";

import { CodeIndex } from "../src/code-index.js";
import { parseFileCallables } from "../src/parser/callable-parser.js";
import { readIndexErrors } from "../src/storage/database.js";
import { FakeEmbeddingProvider, commitAll, initGit, temporaryRoot, write } from "./helpers.js";

afterEach(() => vi.restoreAllMocks());

const brokenClass = `class Store {
  broken(value: ) { return value; }
  good() { return 1; }
}\n`;
const fixedClass = brokenClass.replace("value: )", "value: string)");

function openIndex(root: string, maxFileSize?: number): CodeIndex {
  const index = new CodeIndex({
    rootDir: root, provider: new FakeEmbeddingProvider(), onWarning: () => {},
    ...(maxFileSize === undefined ? {} : { maxFileSize }),
    descriptionProvider: {
      profile: { provider: "test", model: "purpose", strategyVersion: "v1" },
      describe: async ({ callable }) => `Purpose of ${callable.name}`,
    },
  });
  onTestFinished(() => index.close());
  return index;
}

describe("persistent indexing diagnostics", () => {
  it("stores malformed function references, indexes healthy siblings, and clears errors after fixes, renames, and deletion", async () => {
    const root = temporaryRoot();
    write(root, "store.ts", brokenClass);
    write(root, "healthy.ts", "function healthy() {}\n");
    const index = openIndex(root);
    await index.updateFromWorkingTree();
    expect(index.allFunctions().map((item) => item.qualifiedName)).toEqual(["healthy", "Store.good"]);
    expect(index.indexErrors()).toMatchObject([{
      path: "store.ts", scope: "function", qualifiedName: "Store.broken", code: "parse-error", language: "typescript",
      startLine: 2, startColumn: 3, endLine: 2, source: "broken(value: ) { return value; }",
      sourceMode: "working-tree", indexedCommit: null,
    }]);
    expect(readIndexErrors(index.indexPath)).toEqual(index.indexErrors());
    expect(index.status()).toMatchObject({ fileCount: 2, functionCount: 2, indexingErrorCount: 1, failedFileCount: 1 });
    await index.useDescriptions();
    expect(index.status().descriptionCount).toBe(2);
    await index.updateFromWorkingTree();
    expect(index.indexErrors()).toHaveLength(1);
    write(root, "store.ts", fixedClass);
    await index.updateFiles({ upsert: ["store.ts"] });
    expect(index.indexErrors()).toEqual([]);
    expect(index.status()).toMatchObject({ functionCount: 3, descriptionCount: 3, indexingErrorCount: 0, failedFileCount: 0 });
    write(root, "store.ts", brokenClass);
    await index.updateFiles({ upsert: ["store.ts"] });
    renameSync(path.join(root, "store.ts"), path.join(root, "renamed.ts"));
    await index.updateFiles({ renames: [{ from: "store.ts", to: "renamed.ts" }] });
    expect(index.indexErrors().map((error) => error.path)).toEqual(["renamed.ts"]);
    await index.updateFiles({ delete: ["renamed.ts"] });
    expect(index.indexErrors()).toEqual([]);
  });

  it("recovers references from unparsed regions and does not report supported TypeScript recovery as failure", () => {
    for (const [file, source] of [["bad.ts", "function good() {} function broken( {"], ["bad.py", "def good():\n    return 1\ndef broken(\n"], ["bad.rs", "fn good() {} fn broken( {"]]) {
      const result = parseFileCallables(file!, source!, () => {});
      expect(result.callables.map((callable) => callable.name)).toEqual(["good"]);
      expect(result.errors.some((error) => error.scope === "file" && error.source?.includes("broken"))).toBe(true);
      expect(result.errors.some((error) => error.qualifiedName === "broken")).toBe(true);
    }
    expect(parseFileCallables("modern.ts", 'export type * from "./types.js"; function good() {}', () => {}).errors).toEqual([]);
  });

  it("continues after a parser exception and retries failed Git files even when the blob is unchanged", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "a.ts", "function first() {}\n");
    write(root, "b.ts", "function second() {}\n");
    commitAll(root, "sources");
    const index = openIndex(root);
    const parse = vi.spyOn(Parser.prototype, "parse").mockImplementationOnce(() => { throw new Error("parser failure"); });
    await index.updateFromGit();
    parse.mockRestore();
    expect(index.allFunctions().map((item) => item.name)).toEqual(["second"]);
    const secondId = index.allFunctions()[0]!.id;
    expect(index.indexErrors()).toMatchObject([{ path: "a.ts", code: "parse-failed", source: "function first() {}\n" }]);
    const update = await index.updateFromGit();
    expect(update.filesUpdated).toBe(1);
    expect(index.allFunctions().map((item) => item.name)).toEqual(["first", "second"]);
    expect(index.allFunctions().find((item) => item.name === "second")!.id).toBe(secondId);
    expect(index.indexErrors()).toEqual([]);
  });

  it("reconciles historical diagnostics, working-tree fixes, and gitignore exclusions", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "store.ts", brokenClass);
    const base = commitAll(root, "broken");
    write(root, "store.ts", fixedClass);
    commitAll(root, "fixed");
    const index = openIndex(root);
    await index.updateFromGit({ target: base });
    expect(index.indexErrors()[0]).toMatchObject({ indexedCommit: base, sourceMode: "git" });
    await index.updateFromGit({ includeWorkingTree: false });
    expect(index.indexErrors()).toEqual([]);
    write(root, "store.ts", brokenClass);
    await index.updateFromGit();
    expect(index.indexErrors()[0]).toMatchObject({ indexedCommit: null, sourceMode: "working-tree" });
    write(root, ".gitignore", "store.ts\n");
    await index.updateFromGit();
    expect(index.status()).toMatchObject({ fileCount: 0, indexingErrorCount: 0, failedFileCount: 0 });
    rmSync(path.join(root, ".gitignore"));
    await index.updateFromGit();
    expect(index.indexErrors()).toHaveLength(1);
    rmSync(path.join(root, "store.ts"));
    await index.updateFromGit();
    expect(index.indexErrors()).toEqual([]);
  });

  it.each(["working-tree", "git"] as const)("records oversized %s files and indexes other files", async (mode) => {
    const root = temporaryRoot();
    write(root, "large.ts", "// long comment\n".repeat(100) + "function large() {}\n");
    write(root, "small.ts", "function small() {}\n");
    if (mode === "git") { initGit(root); commitAll(root, "files"); }
    const index = openIndex(root, 100);
    if (mode === "git") await index.updateFromGit({ includeWorkingTree: false });
    else await index.updateFromWorkingTree();
    expect(index.status()).toMatchObject({ fileCount: 2, functionCount: 1, indexingErrorCount: 1 });
    expect(index.indexErrors()[0]).toMatchObject({ path: "large.ts", code: "file-too-large", scope: "file", source: null });
    write(root, "large.ts", "function large() {}\n");
    await index.updateFiles({ upsert: ["large.ts"] });
    expect(index.indexErrors()).toEqual([]);
    expect(index.allFunctions()).toHaveLength(2);
  });

  it("persists file-read errors without aborting other explicitly requested files", async () => {
    const root = temporaryRoot();
    write(root, "good.ts", "function good() {}\n");
    const index = openIndex(root);
    await index.updateFiles({ upsert: ["missing.ts", "good.ts"] });
    expect(index.allFunctions().map((item) => item.name)).toEqual(["good"]);
    expect(index.indexErrors()[0]).toMatchObject({ path: "missing.ts", code: "read-error", scope: "file" });
    write(root, "missing.ts", "function found() {}\n");
    await index.updateFiles({ upsert: ["missing.ts"] });
    expect(index.indexErrors()).toEqual([]);
  });

  it.skipIf(process.getuid?.() === 0)("continues filesystem indexing after a permission error and retries after access is restored", async () => {
    const root = temporaryRoot();
    write(root, "locked.ts", "function locked() {}\n");
    write(root, "good.ts", "function good() {}\n");
    const locked = path.join(root, "locked.ts");
    const index = openIndex(root);
    chmodSync(locked, 0);
    try {
      await index.updateFromWorkingTree();
      expect(index.allFunctions().map((item) => item.name)).toEqual(["good"]);
      expect(index.indexErrors()[0]).toMatchObject({ path: "locked.ts", code: "read-error" });
    } finally {
      chmodSync(locked, 0o644);
    }
    await index.updateFromWorkingTree();
    expect(index.indexErrors()).toEqual([]);
    expect(index.allFunctions()).toHaveLength(2);
  });

  it("rejects indexes from before the cache schema cutover", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "store.ts", brokenClass);
    commitAll(root, "broken source");
    const original = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider(), onWarning: () => {} });
    await original.updateFromGit();
    const indexPath = original.indexPath;
    original.close();
    const db = new DatabaseSync(indexPath);
    db.exec("UPDATE metadata SET value = '4' WHERE key = 'schema_version';");
    db.close();
    expect(() => openIndex(root)).toThrow(/Unsupported index schema version 4/);
  });
});
