import { rmSync, symlinkSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { CodeIndex } from "../src/code-index.js";
import type { DescriptionFileInput, DescriptionInput, DescriptionProvider } from "../src/types.js";
import { FakeEmbeddingProvider, temporaryRoot, write } from "./helpers.js";

class KeywordDescriptionProvider implements DescriptionProvider {
  public readonly profile = { provider: "fake", model: "keywords", strategyVersion: "v1" };

  public async describeFile(input: DescriptionFileInput): Promise<string> {
    return `File ${input.path} supports alpha beta workflows.`;
  }

  public async describe(input: DescriptionInput): Promise<string> {
    return `Callable ${input.callable.qualifiedName} serves alpha beta requests.`;
  }
}

describe("describe context", () => {
  it("gathers matching files, callables, descriptions, and full sources", async () => {
    const root = temporaryRoot();
    write(root, "server.ts", `export function alpha() {\n  return "beta";\n}\n`);
    write(root, "other.ts", "export function gamma() {\n  return 1;\n}\n");
    const index = new CodeIndex({
      rootDir: root,
      provider: new FakeEmbeddingProvider(),
      descriptionProvider: new KeywordDescriptionProvider(),
    });
    await index.updateFiles({ upsert: ["server.ts", "other.ts"] });
    await index.useDescriptions();

    const context = await index.describe({ query: "alpha beta", minSimilarity: -1, fullFileThreshold: -1 });

    expect(context.repository).toBe(path.basename(root));
    expect(context.query).toBe("alpha beta");
    expect(context.fullFileThreshold).toBe(-1);
    expect(context.fileContentErrors).toEqual([]);
    const server = context.files.find((file) => file.path === "server.ts");
    expect(server).toMatchObject({ description: "File server.ts supports alpha beta workflows." });
    expect(server!.content).toContain("export function alpha");
    const alpha = context.functions.find((callable) => callable.qualifiedName === "alpha");
    expect(alpha).toMatchObject({
      path: "server.ts",
      kind: "function",
      startLine: 1,
      endLine: 3,
      description: "Callable alpha serves alpha beta requests.",
    });
    expect(alpha!.source).toContain("function alpha");
    expect(context.functions.map((callable) => callable.qualifiedName)).toEqual(["alpha", "gamma"]);
    index.close();
  });

  it("omits whole files at or below the full-file threshold and when contents are disabled", async () => {
    const root = temporaryRoot();
    write(root, "server.ts", "export function alpha() { return \"beta\"; }\n");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["server.ts"] });

    const aboveOne = await index.describe({ query: "alpha beta", minSimilarity: -1, fullFileThreshold: 1 });
    expect(aboveOne.files).toHaveLength(1);
    expect(aboveOne.files[0]!.content).toBeNull();

    const disabled = await index.describe({ query: "alpha beta", minSimilarity: -1, fullFileThreshold: -1, includeFileContents: false });
    expect(disabled.files[0]!.content).toBeNull();
    index.close();
  });

  it("drops every full file source when one qualifying file cannot be read", async () => {
    const root = temporaryRoot();
    write(root, "a.ts", "export function alphaOne() { return \"beta\"; }\n");
    write(root, "b.ts", "export function alphaTwo() { return \"beta\"; }\n");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["a.ts", "b.ts"] });
    rmSync(path.join(root, "b.ts"));

    const context = await index.describe({ query: "alpha beta", minSimilarity: -1, fullFileThreshold: -1 });

    expect(context.files.length).toBeGreaterThan(0);
    expect(context.files.every((file) => file.content === null)).toBe(true);
    expect(context.fileContentErrors).toHaveLength(1);
    expect(context.fileContentErrors[0]).toContain("b.ts");
    index.close();
  });

  it("does not return changed working-tree source as indexed content", async () => {
    const root = temporaryRoot();
    write(root, "server.ts", "export function alpha() { return 1; }\n");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["server.ts"] });
    write(root, "server.ts", "export function unindexed() { return 2; }\n");

    const context = await index.describe({ query: "alpha", minSimilarity: -1, fullFileThreshold: -1 });

    expect(context.files[0]!.content).toBeNull();
    expect(context.fileContentErrors).toHaveLength(1);
    expect(context.fileContentErrors[0]).toContain("Source changed since indexing: server.ts");
    expect(context.functions[0]!.source).toContain("function alpha");
    index.close();
  });

  it("does not follow a symlink that replaces an indexed working-tree file", async () => {
    const root = temporaryRoot();
    write(root, "server.ts", "export function alpha() { return 1; }\n");
    write(root, "outside.ts", "export function secret() { return 2; }\n");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["server.ts"] });
    rmSync(path.join(root, "server.ts"));
    symlinkSync(path.join(root, "outside.ts"), path.join(root, "server.ts"));

    const context = await index.describe({ query: "alpha", minSimilarity: -1, fullFileThreshold: -1 });

    expect(context.files[0]!.content).toBeNull();
    expect(context.fileContentErrors).toHaveLength(1);
    expect(context.fileContentErrors[0]).toContain("no longer a regular file");
    expect(JSON.stringify(context)).not.toContain("function secret");
    index.close();
  });

  it("preserves reranker order and scores while respecting its candidate maximum", async () => {
    const root = temporaryRoot();
    for (const name of ["one", "two", "three"]) {
      write(root, `${name}.ts`, `export function ${name}() { return ${JSON.stringify(name)}; }\n`);
    }
    let candidateCount = 0;
    const index = new CodeIndex({
      rootDir: root,
      provider: {
        profile: { provider: "controlled", model: "equal", dimensions: 2 },
        embedDocuments: async (inputs) => inputs.map(() => [1, 0]),
        embedQuery: async () => [1, 0],
      },
      reranker: {
        profile: { provider: "limited", model: "controlled" },
        maximumCandidateCount: 2,
        rerank: async (_query, documents) => {
          candidateCount = documents.length;
          return [{ index: 1, score: 0.9 }, { index: 0, score: 0.8 }];
        },
      },
    });
    await index.updateFiles({ upsert: ["one.ts", "two.ts", "three.ts"] });

    const context = await index.describe({ query: "numbered function", minSimilarity: -1, includeFileContents: false });

    expect(candidateCount).toBe(2);
    expect(context.functions.map(({ qualifiedName, rerankScore }) => ({ qualifiedName, rerankScore }))).toEqual([
      { qualifiedName: "two", rerankScore: 0.9 },
      { qualifiedName: "one", rerankScore: 0.8 },
    ]);
    expect(context.files.map((file) => file.path)).toEqual(["two.ts", "one.ts"]);
    index.close();
  });

  it("returns an empty context when no callable meets the threshold", async () => {
    const root = temporaryRoot();
    write(root, "server.ts", "export function alpha() { return 1; }\n");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["server.ts"] });

    const context = await index.describe({ query: "alpha", minSimilarity: 2, fullFileThreshold: -1 });

    expect(context.files).toEqual([]);
    expect(context.functions).toEqual([]);
    expect(context.fileContentErrors).toEqual([]);
    index.close();
  });
});
