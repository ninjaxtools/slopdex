import { chmodSync } from "node:fs";

import Parser from "tree-sitter";
import { describe, expect, it, vi } from "vitest";

import { CodeIndex } from "../../src/code-index.js";
import type { IndexProgress } from "../../src/types.js";
import { FakeEmbeddingProvider, temporaryRoot, write } from "../helpers.js";

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
