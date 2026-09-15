import { DatabaseSync } from "node:sqlite";
import { describe, expect, it } from "vitest";

import * as sqliteVec from "sqlite-vec";

import { CodeIndex } from "../src/code-index.js";
import { crossSearch } from "../src/search/cross-search.js";
import type { IndexProgress } from "../src/types.js";
import { FakeEmbeddingProvider, temporaryRoot, write } from "./helpers.js";

function seed(root: string): void {
  write(root, "alpha.ts", `
export function authenticateSession(session: string) { return session === "valid"; }
export function validateSession(session: string) { return Boolean(session); }
export function addNumbers(a: number, b: number) { return a + b; }
`);
  write(root, "beta.ts", `
export function authenticateUser(user: string) { return user.trim(); }
export function multiplyNumbers(a: number, b: number) { return a * b; }
`);
}

async function crossSearchSnapshot(source: CodeIndex) {
  const results = [];
  for await (const result of crossSearch({
    source,
    limitPerFunction: 3,
    includeSymmetricDuplicates: true,
    minLines: 1,
  })) results.push(result);
  return results.map((result) => ({
    source: result.source.id,
    matches: result.matches.map((match) => [match.function.id, match.similarity] as const),
  }));
}

function indexedNames(snapshot: Awaited<ReturnType<typeof crossSearchSnapshot>>): number[] {
  return snapshot.map((row) => row.source);
}

describe("similarity cache", () => {
  it("populates on cross-search and matches live results exactly", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });

    expect(index.similarityCacheInfo()).toEqual({ cachedSources: 0, cachedPairs: 0 });
    const first = await crossSearchSnapshot(index);
    const info = index.similarityCacheInfo();
    expect(info.cachedSources).toBe(index.status().functionCount);
    expect(info.cachedPairs).toBeGreaterThan(0);

    for (const callable of index.allFunctions()) {
      const live = index.similarToFunction(callable.id, { limit: 3, minSimilarity: -1 });
      const cached = index.cachedSimilarToFunction(callable.id, { limit: 3, minSimilarity: -1 });
      expect(cached).toEqual(live);
    }

    const second = await crossSearchSnapshot(index);
    expect(second).toEqual(first);
    index.close();
  });

  it("is a no-op refresh when nothing changed", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });

    const first = await index.refreshSimilarityCache({ width: 10 });
    expect(first.skipped).toBe(false);
    expect(first.sourcesRefreshed).toBe(index.status().functionCount);
    const second = await index.refreshSimilarityCache({ width: 10 });
    expect(second.sourcesRefreshed).toBe(0);
    index.close();
  });

  it("incrementally updates changed functions and stays exact", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    await crossSearchSnapshot(index);

    write(root, "beta.ts", `
export function authenticateUser(user: string) { return Boolean(user); }
export function subtractNumbers(a: number, b: number) { return a - b; }
`);
    await index.updateFiles({ upsert: ["beta.ts"] });
    const refresh = await index.refreshSimilarityCache({ width: 10 });
    expect(refresh.sourcesRefreshed).toBeGreaterThan(0);
    expect(refresh.sourcesRefreshed).toBeLessThanOrEqual(index.status().functionCount);

    for (const callable of index.allFunctions()) {
      const live = index.similarToFunction(callable.id, { limit: 4, minSimilarity: -1 });
      const cached = index.cachedSimilarToFunction(callable.id, { limit: 4, minSimilarity: -1 });
      expect(cached).toEqual(live);
    }
    index.close();
  });

  it("drops deleted functions from the cache", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    await crossSearchSnapshot(index);

    await index.updateFiles({ delete: ["beta.ts"] });
    await index.refreshSimilarityCache({ width: 10 });
    const remaining = new Set(index.allFunctions().map((callable) => callable.id));
    for (const callable of index.allFunctions()) {
      const cached = index.cachedSimilarToFunction(callable.id, { limit: 10, minSimilarity: -1 });
      expect(cached.every((match) => remaining.has(match.function.id))).toBe(true);
      expect(cached).toEqual(index.similarToFunction(callable.id, { limit: 10, minSimilarity: -1 }));
    }
    index.close();
  });

  it("applies filters through the cache exactly", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    await index.refreshSimilarityCache({ width: 10 });

    const options = [
      { limit: 2, minSimilarity: 0.5 },
      { limit: 3, minSimilarity: -1, maxSimilarity: 0.99 },
      { limit: 3, minSimilarity: -1, excludePaths: ["alpha.ts"] },
      { limit: 3, minSimilarity: -1, nameRegex: "Numbers$" },
      { limit: 3, minSimilarity: -1, minLines: 1 },
    ];
    for (const callable of index.allFunctions()) {
      for (const option of options) {
        expect(index.cachedSimilarToFunction(callable.id, option))
          .toEqual(index.similarToFunction(callable.id, option));
      }
    }
    index.close();
  });

  it("falls back to live search when the cache is narrower than requested", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    await index.refreshSimilarityCache({ width: 1 });

    for (const callable of index.allFunctions()) {
      expect(index.cachedSimilarToFunction(callable.id, { limit: 4, minSimilarity: -1 }))
        .toEqual(index.similarToFunction(callable.id, { limit: 4, minSimilarity: -1 }));
    }
    index.close();
  });

  it("migrates schema 8 indexes by adding similarity tables", async () => {
    const root = temporaryRoot();
    seed(root);
    const provider = new FakeEmbeddingProvider();
    const original = new CodeIndex({ rootDir: root, provider });
    await original.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    const indexPath = original.indexPath;
    original.close();

    const downgrade = new DatabaseSync(indexPath);
    downgrade.exec(`
      DROP TABLE similarity_cache;
      DROP TABLE similarity_cache_state;
      UPDATE metadata SET value = '8' WHERE key = 'schema_version';
    `);
    downgrade.close();

    const migrated = new CodeIndex({ rootDir: root, provider });
    expect(indexedNames(await crossSearchSnapshot(migrated)).length).toBeGreaterThan(0);
    const migratedDb = new DatabaseSync(indexPath, { readOnly: true });
    expect(migratedDb.prepare("SELECT value FROM metadata WHERE key = 'schema_version'").get())
      .toEqual({ value: "10" });
    migratedDb.close();
    migrated.close();
  });

  it("keeps read-only cross-search working without cache writes", async () => {
    const root = temporaryRoot();
    seed(root);
    const provider = new FakeEmbeddingProvider();
    const writable = new CodeIndex({ rootDir: root, provider });
    await writable.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    writable.close();

    const index = new CodeIndex({ rootDir: root, provider, readOnly: true });
    const results = [];
    for await (const result of crossSearch({ source: index, minLines: 1 })) results.push(result);
    expect(results.length).toBeGreaterThan(0);
    index.close();
  });

  it("reports similarity-cache fill progress like vector generation", async () => {
    const root = temporaryRoot();
    seed(root);
    const events: IndexProgress[] = [];
    const index = new CodeIndex({
      rootDir: root,
      provider: new FakeEmbeddingProvider(),
      onProgress: (value) => events.push(value),
    });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    events.length = 0;

    const functionCount = index.status().functionCount;
    expect(functionCount).toBeGreaterThan(1);
    await index.refreshSimilarityCache({ width: 10 });
    const fill = events.filter((event) => event.phase === "similarity-cache");
    expect(fill[0]).toEqual({ phase: "similarity-cache", completed: 0, total: functionCount });
    expect(fill.at(-1)).toEqual({ phase: "similarity-cache", completed: functionCount, total: functionCount });
    expect(fill.every((event, position) => position === 0 || event.completed >= fill[position - 1]!.completed)).toBe(true);

    events.length = 0;
    await index.refreshSimilarityCache({ width: 10 });
    expect(events.filter((event) => event.phase === "similarity-cache")).toEqual([]);
    index.close();
  });

  it("forwards cache-fill progress to cross-search callers", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });

    const events: IndexProgress[] = [];
    for await (const _result of crossSearch({
      source: index,
      limitPerFunction: 3,
      includeSymmetricDuplicates: true,
      minLines: 1,
      onCacheProgress: (value) => events.push(value),
    })) { /* drain */ }
    expect(events.length).toBeGreaterThan(0);
    expect(events[0]).toMatchObject({ phase: "similarity-cache", completed: 0 });
    expect(events.at(-1)!.completed).toBe(events.at(-1)!.total);
    index.close();
  });

  it("stores only pairs at or above the refresh floor", async () => {
    const readSimilarities = (indexPath: string): number[] => {
      const db = new DatabaseSync(indexPath, { readOnly: true });
      try {
        return (db.prepare("SELECT similarity FROM similarity_cache").all() as Array<{ similarity: number }>)
          .map((row) => row.similarity);
      } finally {
        db.close();
      }
    };
    const readFloors = (indexPath: string): number[] => {
      const db = new DatabaseSync(indexPath, { readOnly: true });
      try {
        return (db.prepare("SELECT DISTINCT floor FROM similarity_cache_state").all() as Array<{ floor: number }>)
          .map((row) => row.floor);
      } finally {
        db.close();
      }
    };

    const fullRoot = temporaryRoot();
    seed(fullRoot);
    const full = new CodeIndex({ rootDir: fullRoot, provider: new FakeEmbeddingProvider() });
    await full.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    await full.refreshSimilarityCache({ width: 10, minSimilarity: -1 });
    const allPairs = readSimilarities(full.indexPath);
    expect(Math.min(...allPairs)).toBeLessThan(0.7);
    full.close();

    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    await index.refreshSimilarityCache({ width: 10, minSimilarity: 0.7 });
    const floored = readSimilarities(index.indexPath);
    expect(floored.length).toBeGreaterThan(0);
    expect(floored.length).toBeLessThan(allPairs.length);
    expect(Math.min(...floored)).toBeGreaterThanOrEqual(0.7 - 1e-9);
    expect(readFloors(index.indexPath)).toEqual([0.7]);
    index.close();
  });

  it("stays exact above the floor and falls back below it", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    await index.refreshSimilarityCache({ width: 10, minSimilarity: 0.7 });

    for (const callable of index.allFunctions()) {
      for (const minSimilarity of [0.9, 0.7, 0.3, 0.1, -1]) {
        for (const limit of [1, 3, 10]) {
          expect(index.cachedSimilarToFunction(callable.id, { limit, minSimilarity }))
            .toEqual(index.similarToFunction(callable.id, { limit, minSimilarity }));
        }
      }
      expect(index.cachedSimilarToFunction(callable.id, { limit: 3, minSimilarity: 0.7, excludePaths: ["beta.ts"] }))
        .toEqual(index.similarToFunction(callable.id, { limit: 3, minSimilarity: 0.7, excludePaths: ["beta.ts"] }));
      expect(index.cachedSimilarToFunction(callable.id, { limit: 3, minSimilarity: 0.1, maxSimilarity: 0.8 }))
        .toEqual(index.similarToFunction(callable.id, { limit: 3, minSimilarity: 0.1, maxSimilarity: 0.8 }));
    }
    index.close();
  });

  it("reuses the cache when narrowing and recomputes when expanding", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    const indexPath = index.indexPath;
    const floors = (): number[] => {
      const db = new DatabaseSync(indexPath, { readOnly: true });
      try {
        return (db.prepare("SELECT DISTINCT floor FROM similarity_cache_state").all() as Array<{ floor: number }>)
          .map((row) => row.floor);
      } finally {
        db.close();
      }
    };

    await index.refreshSimilarityCache({ width: 10, minSimilarity: 0.1 });
    const narrow = await index.refreshSimilarityCache({ width: 10, minSimilarity: 0.8 });
    expect(narrow.sourcesRefreshed).toBe(0);
    expect(floors()).toEqual([0.1]);

    const expand = await index.refreshSimilarityCache({ width: 10, minSimilarity: 0.05 });
    expect(expand.sourcesRefreshed).toBe(index.status().functionCount);
    expect(floors()).toEqual([0.05]);
    for (const callable of index.allFunctions()) {
      for (const minSimilarity of [0.8, 0.3, -1]) {
        expect(index.cachedSimilarToFunction(callable.id, { limit: 4, minSimilarity }))
          .toEqual(index.similarToFunction(callable.id, { limit: 4, minSimilarity }));
      }
    }
    index.close();
  });

  it("keeps a floored cache exact across edits and deletions", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    await index.refreshSimilarityCache({ width: 10, minSimilarity: 0.7 });

    write(root, "beta.ts", `
export function authenticateUser(user: string) { return Boolean(user); }
export function subtractNumbers(a: number, b: number) { return a - b; }
`);
    await index.updateFiles({ upsert: ["beta.ts"] });
    await index.refreshSimilarityCache({ width: 10, minSimilarity: 0.7 });
    await index.updateFiles({ delete: ["beta.ts"] });
    await index.refreshSimilarityCache({ width: 10, minSimilarity: 0.7 });

    const remaining = new Set(index.allFunctions().map((callable) => callable.id));
    for (const callable of index.allFunctions()) {
      for (const minSimilarity of [0.7, 0.1, -1]) {
        const cached = index.cachedSimilarToFunction(callable.id, { limit: 10, minSimilarity });
        expect(cached.every((match) => remaining.has(match.function.id))).toBe(true);
        expect(cached).toEqual(index.similarToFunction(callable.id, { limit: 10, minSimilarity }));
      }
    }
    index.close();
  });

  it("runs thresholded cross-search identically from cache and live", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });

    const snapshot = async (): Promise<unknown> => {
      const results = [];
      for await (const result of crossSearch({
        source: index,
        limitPerFunction: 3,
        includeSymmetricDuplicates: true,
        minSimilarity: 0.7,
        minLines: 1,
      })) results.push(result);
      return results.map((result) => ({
        source: result.source.id,
        matches: result.matches.map((match) => [match.function.id, match.similarity] as const),
      }));
    };
    const floored = await snapshot();
    expect(index.similarityCacheInfo().cachedPairs).toBeGreaterThan(0);
    await index.refreshSimilarityCache({ minSimilarity: -1 });
    expect(await snapshot()).toEqual(floored);
    index.close();
  });

  it("migrates schema 9 indexes by backfilling cache state", async () => {
    const root = temporaryRoot();
    seed(root);
    const provider = new FakeEmbeddingProvider();
    const original = new CodeIndex({ rootDir: root, provider });
    await original.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    await original.refreshSimilarityCache({ width: 10 });
    const indexPath = original.indexPath;
    original.close();

    const downgrade = new DatabaseSync(indexPath, { allowExtension: true });
    sqliteVec.load(downgrade);
    downgrade.exec(`
      ALTER TABLE similarity_cache_state DROP COLUMN floor;
      ALTER TABLE similarity_cache_state DROP COLUMN stored_count;
      ALTER TABLE similarity_cache_state DROP COLUMN complete;
      UPDATE metadata SET value = '9' WHERE key = 'schema_version';
    `);
    downgrade.close();

    const migrated = new CodeIndex({ rootDir: root, provider });
    const migratedDb = new DatabaseSync(indexPath, { readOnly: true });
    expect(migratedDb.prepare("SELECT value FROM metadata WHERE key = 'schema_version'").get())
      .toEqual({ value: "10" });
    const states = migratedDb.prepare("SELECT floor, stored_count, complete FROM similarity_cache_state").all() as
      Array<{ floor: number; stored_count: number; complete: number }>;
    expect(states.length).toBeGreaterThan(0);
    expect(states.every((row) => row.floor === -1 && row.stored_count > 0)).toBe(true);
    migratedDb.close();
    for (const callable of migrated.allFunctions()) {
      for (const minSimilarity of [0.5, -1]) {
        expect(migrated.cachedSimilarToFunction(callable.id, { limit: 4, minSimilarity }))
          .toEqual(migrated.similarToFunction(callable.id, { limit: 4, minSimilarity }));
      }
    }
    migrated.close();
  });
});
