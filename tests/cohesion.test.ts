import { describe, expect, it } from "vitest";

import { analyzeCohesion, cohesionLocation } from "../src/analysis/cohesion.js";
import { CodeIndex } from "../src/code-index.js";
import { formatCohesionSummary } from "../src/format.js";
import type { EmbeddingProvider } from "../src/types.js";
import { temporaryRoot, write } from "./helpers.js";

const equalProvider: EmbeddingProvider = {
  profile: { provider: "controlled", model: "equal", dimensions: 2, strategyVersion: "callable-v1" },
  embedDocuments: async (inputs) => inputs.map(() => [1, 0]),
  embedQuery: async () => [1, 0],
};

describe("cohesion location", () => {
  it("measures file boundaries and directory-tree hops", () => {
    expect(cohesionLocation("src/auth.ts", "src/auth.ts")).toMatchObject({
      category: "same-file",
      physicalDistance: 0,
      folderHops: 0,
      commonAncestor: "src",
    });
    expect(cohesionLocation("src/auth.ts", "src/session.ts")).toMatchObject({
      category: "same-folder",
      physicalDistance: 1,
      folderHops: 0,
      commonAncestor: "src",
    });
    expect(cohesionLocation("src/auth/login.ts", "src/users/profile.ts")).toMatchObject({
      category: "different-folder",
      physicalDistance: 3,
      folderHops: 2,
      commonAncestor: "src",
    });
  });

  it("identifies source and test mirror pairs", () => {
    expect(cohesionLocation("src/auth.ts", "tests/auth.test.ts").sourceTestPair).toBe(true);
    expect(cohesionLocation("tests/auth.test.ts", "tests/session.test.ts").sourceTestPair).toBe(false);
  });

  it.each([
    ["auth.py", "test_auth.py"], ["auth.py", "auth_test.py"],
    ["auth.go", "auth_test.go"], ["Auth.java", "AuthTest.java"],
    ["Auth.java", "TestAuth.java"], ["auth.c", "test_auth.c"], ["auth.rs", "auth_test.rs"],
  ])("recognizes language-specific test names for %s", (source, test) => {
    expect(cohesionLocation(`src/${source}`, `src/${test}`).sourceTestPair).toBe(true);
    expect(cohesionLocation(`src/${test}`, `tests/${test}`).sourceTestPair).toBe(false);
  });

  it("does not treat Java names ending in lowercase test as test classes", () => {
    expect(cohesionLocation("src/Contest.java", "src/Contestant.java").sourceTestPair).toBe(false);
  });
});

describe("cohesion analysis", () => {
  it("ranks remote semantic edges and computes normalized metrics", async () => {
    const root = temporaryRoot();
    write(root, "src/a.ts", `
export function alpha() { return 1; }
export function beta() { return 2; }
`);
    write(root, "src/b.ts", "export function gamma() { return 3; }\n");
    write(root, "packages/feature/c.ts", "export function delta() { return 4; }\n");
    const index = new CodeIndex({ rootDir: root, provider: equalProvider });
    await index.updateFiles({ upsert: ["src/a.ts", "src/b.ts", "packages/feature/c.ts"] });

    const report = await analyzeCohesion({
      source: index,
      neighbors: 10,
      limit: 2,
      minSimilarity: 0.8,
      minLines: 1,
    });

    expect(report.schemaVersion).toBe(3);
    expect(report.metrics).toMatchObject({
      functionsAnalyzed: 4,
      candidateFunctions: 4,
      semanticEdges: 6,
    });
    expect(report.metrics.sameFileRatio).toBeCloseTo(1 / 6);
    expect(report.metrics.sameFolderRatio).toBeCloseTo(2 / 6);
    expect(report.metrics.remoteRatio).toBeCloseTo(3 / 6);
    expect(report.pairs).toHaveLength(2);
    expect(report.pairs.every((pair) => pair.reciprocal)).toBe(true);
    expect(report.pairs.every((pair) => pair.location.physicalDistance === 4)).toBe(true);
    expect(report.pairs[0]!.cohesionGap).toBeCloseTo(1 - Math.exp(-2));
    expect(report.files[0]).toMatchObject({
      path: "packages/feature/c.ts",
      externalAffinityRatio: 1,
    });
    expect(report.groups).toHaveLength(1);
    expect(report.groups[0]).toMatchObject({ memberCount: 3, fileCount: 2, maximumPhysicalDistance: 4 });
    expect(formatCohesionSummary(report)).toContain("Cohesion: 4 functions analyzed, 6 semantic edges");
    index.close();
  });

  it("restricts source functions while comparing them with the whole index", async () => {
    const root = temporaryRoot();
    write(root, "selected.ts", "export function selected() { return 1; }\n");
    write(root, "one.ts", "export function one() { return 1; }\n");
    write(root, "nested/two.ts", "export function two() { return 1; }\n");
    const index = new CodeIndex({ rootDir: root, provider: equalProvider });
    await index.updateFiles({ upsert: ["selected.ts", "one.ts", "nested/two.ts"] });

    const report = await analyzeCohesion({
      source: index,
      sourceFilter: { type: "all", path: "selected.ts" },
      neighbors: 10,
      minSimilarity: 0.8,
      minLines: 1,
    });

    expect(report.metrics).toMatchObject({ functionsAnalyzed: 1, candidateFunctions: 3, semanticEdges: 2 });
    expect(report.metrics.scope).toBe("selected-sources");
    expect(report.files.map((file) => file.path)).toEqual(["selected.ts"]);
    expect(report.pairs.every((pair) => pair.left.path === "selected.ts" || pair.right.path === "selected.ts")).toBe(true);
    expect(report.pairs.every((pair) => pair.reciprocal === null)).toBe(true);
    index.close();
  });

  it("ranks cohesion gap ahead of reciprocal-neighbor confidence", async () => {
    const root = temporaryRoot();
    write(root, "src/a.ts", "export function alpha() { return 1; }\n");
    write(root, "packages/b.ts", `
export function bravo() { return 2; }
export function charlie() { return 3; }
`);
    write(root, "local/d.ts", "export function delta() { return 4; }\n");
    write(root, "local/e.ts", "export function echo() { return 5; }\n");
    const vectors: Record<string, number[]> = {
      alpha: [1, 0],
      bravo: [0.9, Math.sqrt(0.19)],
      charlie: [0.85, Math.sqrt(0.2775)],
      delta: [0, 1],
      echo: [Math.sqrt(0.19), 0.9],
    };
    const provider: EmbeddingProvider = {
      profile: { provider: "controlled", model: "ranking", dimensions: 2, strategyVersion: "callable-v1" },
      embedDocuments: async (inputs) => inputs.map((input) => {
        const name = Object.keys(vectors).find((candidate) => input.includes(`symbol: ${candidate}`))!;
        return vectors[name]!;
      }),
      embedQuery: async () => [1, 0],
    };
    const index = new CodeIndex({ rootDir: root, provider });
    await index.updateFiles({ upsert: ["src/a.ts", "packages/b.ts", "local/d.ts", "local/e.ts"] });

    const report = await analyzeCohesion({
      source: index,
      neighbors: 1,
      minSimilarity: 0.8,
      minLines: 1,
    });

    expect(report.pairs[0]).toMatchObject({
      reciprocal: false,
      left: { qualifiedName: "bravo" },
      right: { qualifiedName: "alpha" },
    });
    expect(report.pairs[0]!.cohesionGap).toBeGreaterThan(report.pairs.find((pair) => (
      pair.left.qualifiedName === "delta" || pair.right.qualifiedName === "delta"
    ))!.cohesionGap);
    index.close();
  });

  it("validates scoring parameters", async () => {
    const root = temporaryRoot();
    const index = new CodeIndex({ rootDir: root, provider: equalProvider });
    await expect(analyzeCohesion({ source: index, minSimilarity: 1 })).rejects.toThrow(/less than 1/);
    await expect(analyzeCohesion({ source: index, neighbors: 0 })).rejects.toThrow(/positive integer/);
    index.close();
  });
});
