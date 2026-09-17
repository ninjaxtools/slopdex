import { rmSync } from "node:fs";
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
});
