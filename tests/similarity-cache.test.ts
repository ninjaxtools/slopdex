import { DatabaseSync } from "node:sqlite";
import { describe, expect, it } from "vitest";

import * as sqliteVec from "sqlite-vec";

import { CodeIndex } from "../src/code-index.js";
import { crossSearch } from "../src/search/cross-search.js";
import { similarityCacheFloor, SIMILARITY_CACHE_FLOOR_ANCHOR } from "../src/search/similarity.js";
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

  it("survives a no-change working-tree refresh without rebuilding", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromWorkingTree();
    await index.refreshSimilarityCache({ width: 10 });

    const refresh = await index.updateFromWorkingTree();
    expect(refresh).toMatchObject({ filesUpdated: 0, filesDeleted: 0 });
    const generation = index.status().generation;
    const second = await index.refreshSimilarityCache({ width: 10 });
    expect(second.sourcesRefreshed).toBe(0);
    expect(index.status().generation).toBe(generation);
    expect(index.similarityCacheInfo().cachedSources).toBe(index.status().functionCount);
    index.close();
  });

  it("never narrows repaired rows on incremental refresh", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    await index.refreshSimilarityCache({ width: 1, minSimilarity: -1 });

    // Widen one entry through read-repair.
    const firstId = index.allFunctions()[0]!.id;
    index.cachedSimilarToFunction(firstId, { limit: 3, minSimilarity: -1 });
    const widths = (): number[] => {
      const db = new DatabaseSync(index.indexPath, { readOnly: true });
      try {
        return (db.prepare("SELECT DISTINCT cached_width AS width FROM similarity_cache_state").all() as Array<{ width: number }>)
          .map((row) => row.width);
      } finally {
        db.close();
      }
    };
    expect(widths()).toContain(200);

    // Dirty an unrelated file, then refresh at the narrow width again.
    write(root, "beta.ts", `
export function authenticateUser(user: string) { return Boolean(user); }
export function multiplyNumbers(a: number, b: number) { return a * b * 2; }
`);
    await index.updateFiles({ upsert: ["beta.ts"] });
    await index.refreshSimilarityCache({ width: 1, minSimilarity: -1 });
    expect(widths()).toContain(200);
    const states = (): Array<{ function_id: number; complete: number }> => {
      const db = new DatabaseSync(index.indexPath, { readOnly: true });
      try {
        return db.prepare("SELECT function_id, complete FROM similarity_cache_state").all() as
          Array<{ function_id: number; complete: number }>;
      } finally {
        db.close();
      }
    };
    // The repaired row stays complete even though other rows are incomplete.
    expect(states().find((row) => row.function_id === firstId)).toMatchObject({ complete: 1 });

    // Steady state is a no-op again.
    const settled = await index.refreshSimilarityCache({ width: 1, minSimilarity: -1 });
    expect(settled.sourcesRefreshed).toBe(0);
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
      .toEqual({ value: "11" });
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
      .toEqual({ value: "11" });
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

  it("serves identical results through a snapshot reader", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });

    // No refresh yet: the reader falls back to live queries exactly.
    const empty = index.cachedSimilarityReader();
    for (const callable of index.allFunctions()) {
      expect(empty.similarToFunction(callable.id, { limit: 3, minSimilarity: 0.3 }))
        .toEqual(index.similarToFunction(callable.id, { limit: 3, minSimilarity: 0.3 }));
    }

    await index.refreshSimilarityCache({ width: 10, minSimilarity: 0.5 });
    const reader = index.cachedSimilarityReader();
    const queries = [
      { limit: 1, minSimilarity: 0.9 },
      { limit: 3, minSimilarity: 0.5 },
      { limit: 10, minSimilarity: 0.5 },
      { limit: 3, minSimilarity: -1 },
      { limit: 3, minSimilarity: 0.5, maxSimilarity: 0.8 },
      { limit: 3, minSimilarity: 0.5, excludePaths: ["beta.ts"] },
      { limit: 3, minSimilarity: 0.5, nameRegex: "Numbers$" },
      { limit: 3, minSimilarity: 0.1, minLines: 1 },
    ];
    for (const callable of index.allFunctions()) {
      for (const query of queries) {
        expect(reader.similarToFunction(callable.id, query))
          .toEqual(index.cachedSimilarToFunction(callable.id, query));
        expect(reader.similarToFunction(callable.id, query))
          .toEqual(index.similarToFunction(callable.id, query));
      }
    }

    // A scoring-mode mismatch cannot be served from this snapshot.
    const firstId = index.allFunctions()[0]!.id;
    expect(() => reader.similarToFunction(firstId, { limit: 1, minSimilarity: -1, includeDescriptions: true }))
      .toThrow(/description/);
    expect(() => reader.similarToFunction(firstId, { limit: 1, minSimilarity: 0.5, nameRegex: "[" }))
      .toThrow(/Invalid name regex/);
    index.close();
  });

  it("heals poisoned completeness flags through read-repair", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    await index.refreshSimilarityCache({ width: 10, minSimilarity: 0.9 });
    const indexPath = index.indexPath;
    const dumpStates = (): Array<{ function_id: number; floor: number; stored_count: number; complete: number; cached_width: number }> => {
      const db = new DatabaseSync(indexPath, { readOnly: true });
      try {
        return db.prepare("SELECT function_id, floor, stored_count, complete, cached_width FROM similarity_cache_state ORDER BY function_id").all() as
          Array<{ function_id: number; floor: number; stored_count: number; complete: number; cached_width: number }>;
      } finally {
        db.close();
      }
    };
    expect(dumpStates().every((row) => row.complete === 1)).toBe(true);

    // Simulate stale flags (e.g. carried over by a migration): sparse but
    // correct rows marked incomplete, forcing live fallbacks on every run.
    const poison = new DatabaseSync(indexPath);
    poison.exec("UPDATE similarity_cache_state SET complete = 0");
    poison.close();
    expect(dumpStates().every((row) => row.complete === 0)).toBe(true);

    const reader = index.cachedSimilarityReader();
    for (const callable of index.allFunctions()) {
      for (const query of [
        { limit: 1, minSimilarity: 0.9 },
        { limit: 3, minSimilarity: 0.9 },
        { limit: 3, minSimilarity: 0.95 },
        { limit: 3, minSimilarity: 0.9, excludePaths: ["beta.ts"] },
      ]) {
        expect(reader.similarToFunction(callable.id, query))
          .toEqual(index.similarToFunction(callable.id, query));
      }
    }
    const healed = dumpStates();
    expect(healed.every((row) => row.complete === 1 && row.floor === 0.9 && row.cached_width === 200)).toBe(true);

    // A second identical run performs no further rewrites.
    for (const callable of index.allFunctions()) {
      reader.similarToFunction(callable.id, { limit: 3, minSimilarity: 0.9 });
    }
    expect(dumpStates()).toEqual(healed);
    index.close();
  });

  it("repairs missing entries without waiting for a refresh", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });

    const reader = index.cachedSimilarityReader();
    for (const callable of index.allFunctions()) {
      expect(reader.similarToFunction(callable.id, { limit: 3, minSimilarity: 0.5 }))
        .toEqual(index.similarToFunction(callable.id, { limit: 3, minSimilarity: 0.5 }));
    }
    expect(index.similarityCacheInfo().cachedSources).toBe(index.status().functionCount);
    const db = new DatabaseSync(index.indexPath, { readOnly: true });
    try {
      const floors = (db.prepare("SELECT DISTINCT floor FROM similarity_cache_state").all() as Array<{ floor: number }>)
        .map((row) => row.floor);
      expect(floors).toEqual([0.5]);
    } finally {
      db.close();
    }
    index.close();
  });

  it("does not thrash the cache on alternating thresholds", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    await index.refreshSimilarityCache({ width: 10, minSimilarity: 0.9 });
    const indexPath = index.indexPath;
    const dumpStates = (): unknown => {
      const db = new DatabaseSync(indexPath, { readOnly: true });
      try {
        return db.prepare("SELECT function_id, floor, stored_count, complete, cached_width FROM similarity_cache_state ORDER BY function_id").all();
      } finally {
        db.close();
      }
    };

    const reader = index.cachedSimilarityReader();
    const firstId = index.allFunctions()[0]!.id;
    expect(reader.similarToFunction(firstId, { limit: 4, minSimilarity: 0.3 }))
      .toEqual(index.similarToFunction(firstId, { limit: 4, minSimilarity: 0.3 }));
    // Expansion repair lowers the floor once instead of recomputing per query.
    const lowered = dumpStates() as Array<{ floor: number }>;
    expect(lowered.find((row) => row.floor === 0.3)).toBeDefined();

    // Higher thresholds reuse the widened entry without rewriting it.
    expect(reader.similarToFunction(firstId, { limit: 4, minSimilarity: 0.9 }))
      .toEqual(index.similarToFunction(firstId, { limit: 4, minSimilarity: 0.9 }));
    const settled = dumpStates();
    expect(reader.similarToFunction(firstId, { limit: 4, minSimilarity: 0.3 }))
      .toEqual(index.similarToFunction(firstId, { limit: 4, minSimilarity: 0.3 }));
    expect(reader.similarToFunction(firstId, { limit: 4, minSimilarity: 0.9 }))
      .toEqual(index.similarToFunction(firstId, { limit: 4, minSimilarity: 0.9 }));
    expect(dumpStates()).toEqual(settled);
    index.close();
  });

  it("leaves dense-at-max-width entries to live queries", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    await index.refreshSimilarityCache({ width: 10, minSimilarity: -1 });
    const indexPath = index.indexPath;

    // Fabricate a maxed-out truncated entry: repair cannot improve it, so reads
    // must fall back without rewriting.
    const fabricate = new DatabaseSync(indexPath);
    fabricate.exec("UPDATE similarity_cache_state SET stored_count = 200, complete = 0, cached_width = 200, floor = -1");
    fabricate.close();

    const reader = index.cachedSimilarityReader();
    for (const callable of index.allFunctions()) {
      expect(reader.similarToFunction(callable.id, { limit: 10, minSimilarity: -1 }))
        .toEqual(index.similarToFunction(callable.id, { limit: 10, minSimilarity: -1 }));
    }
    const db = new DatabaseSync(indexPath, { readOnly: true });
    try {
      const states = db.prepare("SELECT stored_count, complete, cached_width, floor FROM similarity_cache_state").all() as
        Array<{ stored_count: number; complete: number; cached_width: number; floor: number }>;
      expect(states.every((row) =>
        row.stored_count === 200 && row.complete === 0 && row.cached_width === 200 && row.floor === -1)).toBe(true);
    } finally {
      db.close();
    }
    index.close();
  });

  it("anchors refresh floors so threshold sweeps share one cache band", () => {
    expect(SIMILARITY_CACHE_FLOOR_ANCHOR).toBe(0.3);
    expect(similarityCacheFloor(undefined)).toBe(-1);
    expect(similarityCacheFloor(0.9)).toBe(0.3);
    expect(similarityCacheFloor(0.3)).toBe(0.3);
    expect(similarityCacheFloor(0.1)).toBe(0.1);
  });

  it("builds one shared band for cross-searches above the anchor", async () => {
    const root = temporaryRoot();
    seed(root);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["alpha.ts", "beta.ts"] });
    const floors = (): number[] => {
      const db = new DatabaseSync(index.indexPath, { readOnly: true });
      try {
        return (db.prepare("SELECT DISTINCT floor FROM similarity_cache_state").all() as Array<{ floor: number }>)
          .map((row) => row.floor);
      } finally {
        db.close();
      }
    };
    const snapshot = async (minSimilarity: number): Promise<unknown> => {
      const results = [];
      for await (const result of crossSearch({
        source: index,
        limitPerFunction: 3,
        includeSymmetricDuplicates: true,
        minSimilarity,
        minLines: 1,
      })) results.push(result);
      return results.map((result) => ({
        source: result.source.id,
        matches: result.matches.map((match) => [match.function.id, match.similarity] as const),
      }));
    };

    const high = await snapshot(0.9);
    expect(floors()).toEqual([0.3]);
    const mid = await snapshot(0.5);
    // No expansion rebuild: the band is still the anchored one.
    expect(floors()).toEqual([0.3]);
    expect(high).toBeDefined();
    expect(mid).toBeDefined();
    for (const minSimilarity of [0.9, 0.5]) {
      for (const callable of index.allFunctions()) {
        expect(index.cachedSimilarToFunction(callable.id, { limit: 3, minSimilarity }))
          .toEqual(index.similarToFunction(callable.id, { limit: 3, minSimilarity }));
      }
    }
    index.close();
  });
});
