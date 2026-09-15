import { DatabaseSync } from "node:sqlite";
import { describe, expect, it } from "vitest";

import { CodeIndex } from "../src/code-index.js";
import { crossSearch } from "../src/search/cross-search.js";
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
      .toEqual({ value: "9" });
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
});
