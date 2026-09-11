import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { describe, expect, it } from "vitest";

import { CodeIndex } from "../src/code-index.js";
import { OpenAIDescriptionProvider } from "../src/descriptions/openai.js";
import { resetIndexState } from "../src/storage/database.js";
import type { DescriptionInput, DescriptionProvider, EmbeddingProvider } from "../src/types.js";
import { FakeEmbeddingProvider, commitAll, git, initGit, temporaryRoot, write } from "./helpers.js";

class FakeDescriptionProvider implements DescriptionProvider {
  public readonly profile = { provider: "fake", model: "purpose", strategyVersion: "v1" };
  public inputs: DescriptionInput[] = [];
  public fail = false;

  public async describe(input: DescriptionInput): Promise<string> {
    this.inputs.push(input);
    if (this.fail) throw new Error("Description service unavailable");
    return `Purpose of ${input.callable.qualifiedName} in ${input.callable.path}: support the application workflow.`;
  }
}

describe("purpose descriptions", () => {
  it("is optional, backfills every callable, persists, and reuses unchanged descriptions", async () => {
    const root = temporaryRoot();
    const source = `import { send } from './transport';
export function deliver() { send(); }
export class Client { constructor() {} send() { deliver(); } }
`;
    write(root, "client.ts", source);
    const provider = new FakeEmbeddingProvider();
    const descriptions = new FakeDescriptionProvider();
    const index = new CodeIndex({ rootDir: root, provider, descriptionProvider: descriptions });
    await index.updateFiles({ upsert: ["client.ts"] });
    expect(index.status()).toMatchObject({ descriptionsEnabled: false, descriptionCount: 0 });
    expect(index.allFunctions().every((value) => value.description === null)).toBe(true);
    expect(descriptions.inputs).toHaveLength(0);
    await expect(index.searchDescription({ query: "workflow" })).rejects.toThrow(/descriptions enable/);

    const functionsBefore = index.allFunctions();
    const vectorsBefore = functionsBefore.map((value) => index.vectorForFunction(value.id));
    await expect(index.useDescriptions()).resolves.toEqual({ descriptionsCreated: 3, descriptionsEnabled: true });
    expect(index.status()).toMatchObject({ descriptionCount: 3, descriptionProfile: descriptions.profile });
    expect(descriptions.inputs.every((input) => input.fileSource === source && input.repository === path.basename(root))).toBe(true);
    expect(index.allFunctions().map((value) => value.id)).toEqual(functionsBefore.map((value) => value.id));
    expect(index.allFunctions().map((value) => index.vectorForFunction(value.id))).toEqual(vectorsBefore);
    await expect(index.useDescriptions()).resolves.toEqual({ descriptionsCreated: 0, descriptionsEnabled: true });
    expect(index.disableDescriptions()).toEqual({ descriptionsCreated: 0, descriptionsEnabled: false });
    expect(index.status()).toMatchObject({ descriptionsEnabled: false, descriptionCount: 3 });
    await expect(index.searchDescription({ query: "workflow" })).rejects.toThrow(/descriptions enable/);
    await expect(index.useDescriptions()).resolves.toEqual({ descriptionsCreated: 0, descriptionsEnabled: true });
    index.close();

    const reopened = new CodeIndex({ rootDir: root, provider, descriptionProvider: descriptions });
    await reopened.updateFromWorkingTree();
    expect(descriptions.inputs).toHaveLength(3);
    expect(reopened.status()).toMatchObject({ descriptionsEnabled: true, descriptionCount: 3 });
    const results = await reopened.searchDescription({ query: "workflow" });
    expect(results).toHaveLength(3);
    expect(results.every((value) => value.function.description?.includes("Purpose of"))).toBe(true);
    reopened.close();
  });

  it("refreshes context changes, additions, renames and deletions through Git updates", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, ".gitignore", ".slopdex/\n");
    write(root, "a.ts", "const mode = 'initial'; export function one() { return mode; }\n");
    write(root, "b.ts", "export function two() { return 2; }\n");
    commitAll(root, "initial");
    const descriptions = new FakeDescriptionProvider();
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider(), descriptionProvider: descriptions });
    await index.updateFromGit();
    await index.useDescriptions();
    await index.updateFromGit();
    expect(descriptions.inputs).toHaveLength(2);

    write(root, "a.ts", "const mode = 'changed'; export function one() { return mode; }\n");
    write(root, "c.ts", "export const three = () => 3;\n");
    await index.updateFromGit();
    expect(descriptions.inputs.slice(2).map((value) => value.callable.name).sort()).toEqual(["one", "three"]);
    commitAll(root, "context and new function");
    await index.updateFromGit();
    expect(descriptions.inputs).toHaveLength(4);

    git(root, "mv", "a.ts", "renamed.ts");
    git(root, "rm", "b.ts");
    await index.updateFromGit();
    expect(descriptions.inputs).toHaveLength(5);
    expect(descriptions.inputs[4]!.callable.path).toBe("renamed.ts");
    expect(index.status()).toMatchObject({ functionCount: 2, descriptionCount: 2 });
    const results = await index.searchDescription({ query: "workflow" });
    expect(results.map((value) => value.function.path).sort()).toEqual(["c.ts", "renamed.ts"]);
    index.close();
  });

  it("searches the description vectors independently of the code vectors and applies thresholds", async () => {
    const root = temporaryRoot();
    write(root, "functions.ts", "export function one() { return 1; }\nexport function two() { return 2; }\n");
    const provider: EmbeddingProvider = {
      profile: { provider: "test", model: "independent", dimensions: 2 },
      embedDocuments: async (inputs) => inputs.map((input) => {
        const one = input.includes("one");
        return input.startsWith("Purpose") ? (one ? [0, 1] : [1, 0]) : (one ? [1, 0] : [0, 1]);
      }),
      embedQuery: async () => [1, 0],
    };
    const index = new CodeIndex({ rootDir: root, provider, descriptionProvider: new FakeDescriptionProvider() });
    await index.updateFiles({ upsert: ["functions.ts"] });
    await index.useDescriptions();
    expect((await index.similaritySearch({ query: "purpose", limit: 1 }))[0]!.function.name).toBe("one");
    expect((await index.searchDescription({ query: "purpose", limit: 1 }))[0]!.function.name).toBe("two");
    expect((await index.searchDescription({ query: "purpose", minSimilarity: 0, maxSimilarity: 1 })).map((value) => value.function.name)).toEqual(["one"]);
    expect(await index.searchDescription({ query: "purpose", minSimilarity: 1.1 })).toEqual([]);
    await expect(index.searchDescription({ query: " " })).rejects.toThrow(/empty/);
    await expect(index.searchDescription({ query: "purpose", limit: 0 })).rejects.toThrow(/positive integer/);
    index.close();
  });

  it("rolls back initialization and subsequent updates when generation or embedding fails", async () => {
    const root = temporaryRoot();
    write(root, "a.ts", "export function one() { return 1; }\n");
    const provider = new FakeEmbeddingProvider();
    const descriptions = new FakeDescriptionProvider();
    const index = new CodeIndex({ rootDir: root, provider, descriptionProvider: descriptions });
    await index.updateFiles({ upsert: ["a.ts"] });
    const before = index.status();
    const embed = provider.embedDocuments.bind(provider);
    provider.embedDocuments = async () => [[NaN]];
    await expect(index.useDescriptions()).rejects.toThrow(/vector/);
    expect(index.status()).toEqual(before);
    expect(index.allFunctions()[0]!.description).toBeNull();
    provider.embedDocuments = embed;
    await index.useDescriptions();
    const enabledStatus = index.status();
    const enabledFunctions = index.allFunctions();
    write(root, "a.ts", "export function one() { return 2; }\n");
    descriptions.fail = true;
    await expect(index.updateFiles({ upsert: ["a.ts"] })).rejects.toThrow(/unavailable/);
    expect(index.status()).toEqual(enabledStatus);
    expect(index.allFunctions()).toEqual(enabledFunctions);
    descriptions.fail = false;
    await index.updateFiles({ upsert: ["a.ts"] });
    expect(index.allFunctions()[0]!.source).toContain("return 2");
    index.close();
  });

  it("persists each generated description before a later description request fails", async () => {
    const root = temporaryRoot();
    write(root, "functions.ts", "export function one() { return 1; }\nexport function two() { return 2; }\n");
    class FailingDescriptionProvider extends FakeDescriptionProvider {
      public shouldFail = true;

      public override async describe(input: DescriptionInput): Promise<string> {
        if (this.shouldFail && input.callable.name === "two") {
          this.inputs.push(input);
          throw new Error("description failed");
        }
        return await super.describe(input);
      }
    }
    const descriptions = new FailingDescriptionProvider();
    const provider = new FakeEmbeddingProvider();
    const first = new CodeIndex({ rootDir: root, provider, descriptionProvider: descriptions, embeddingBatchSize: 1 });
    await first.updateFiles({ upsert: ["functions.ts"] });
    await expect(first.useDescriptions()).rejects.toThrow(/description failed/);
    expect(first.status()).toMatchObject({ descriptionsEnabled: false, descriptionCount: 0 });
    first.close();

    descriptions.shouldFail = false;
    const resumed = new CodeIndex({ rootDir: root, provider, descriptionProvider: descriptions, embeddingBatchSize: 1 });
    await expect(resumed.useDescriptions()).resolves.toEqual({ descriptionsCreated: 1, descriptionsEnabled: true });
    expect(descriptions.inputs.filter((input) => input.callable.name === "one")).toHaveLength(1);
    expect(descriptions.inputs.filter((input) => input.callable.name === "two")).toHaveLength(2);
    resumed.close();
  });

  it("persists each completed description vector before a later vector request fails", async () => {
    const root = temporaryRoot();
    write(root, "functions.ts", "export function one() { return 1; }\nexport function two() { return 2; }\n");
    class FailingEmbeddingProvider extends FakeEmbeddingProvider {
      public descriptionInputs: string[] = [];
      public shouldFail = true;

      public override async embedDocuments(inputs: readonly string[]): Promise<number[][]> {
        if (inputs.every((input) => input.startsWith("Purpose of"))) {
          this.descriptionInputs.push(...inputs);
          if (this.shouldFail && this.descriptionInputs.length === 2) throw new Error("description embedding failed");
        }
        return await super.embedDocuments(inputs);
      }
    }
    const provider = new FailingEmbeddingProvider();
    const descriptions = new FakeDescriptionProvider();
    const first = new CodeIndex({ rootDir: root, provider, descriptionProvider: descriptions, embeddingBatchSize: 1 });
    await first.updateFiles({ upsert: ["functions.ts"] });
    await expect(first.useDescriptions()).rejects.toThrow(/description embedding failed/);
    first.close();

    provider.shouldFail = false;
    const resumed = new CodeIndex({ rootDir: root, provider, descriptionProvider: descriptions, embeddingBatchSize: 1 });
    await expect(resumed.useDescriptions()).resolves.toEqual({ descriptionsCreated: 0, descriptionsEnabled: true });
    expect(descriptions.inputs).toHaveLength(2);
    expect(provider.descriptionInputs.filter((input) => input.includes("one"))).toHaveLength(1);
    expect(provider.descriptionInputs.filter((input) => input.includes("two"))).toHaveLength(2);
    resumed.close();
  });

  it("retains description text across a logical rebuild with a new embedding profile", async () => {
    const root = temporaryRoot();
    write(root, "function.ts", "export function one() { return 1; }\n");
    const provider = new FakeEmbeddingProvider();
    const descriptions = new FakeDescriptionProvider();
    const first = new CodeIndex({ rootDir: root, provider, descriptionProvider: descriptions });
    await first.updateFiles({ upsert: ["function.ts"] });
    await first.useDescriptions();
    const indexPath = first.indexPath;
    first.close();

    const replacement: EmbeddingProvider = {
      profile: { ...provider.profile, model: "replacement" },
      embedDocuments: provider.embedDocuments.bind(provider),
      embedQuery: provider.embedQuery.bind(provider),
    };
    resetIndexState(indexPath, root, replacement.profile);
    const rebuilt = new CodeIndex({ rootDir: root, provider: replacement, descriptionProvider: descriptions });
    await rebuilt.useDescriptions();
    await rebuilt.updateFiles({ upsert: ["function.ts"] });
    expect(descriptions.inputs).toHaveLength(1);
    expect(rebuilt.status()).toMatchObject({ functionCount: 1, descriptionCount: 1, descriptionsEnabled: true });
    rebuilt.close();
  });

  it("rejects concurrent changes and aborted initialization without enabling descriptions", async () => {
    const root = temporaryRoot();
    write(root, "a.ts", "export function one() { return 1; }\n");
    const provider = new FakeEmbeddingProvider();
    const descriptions = new FakeDescriptionProvider();
    const index = new CodeIndex({ rootDir: root, provider, descriptionProvider: descriptions });
    await index.updateFiles({ upsert: ["a.ts"] });
    const other = new CodeIndex({ rootDir: root, provider });
    descriptions.describe = async () => {
      await other.updateFiles({ delete: ["a.ts"] });
      return "Purpose";
    };
    await expect(index.useDescriptions()).rejects.toThrow(/Index changed/);
    expect(index.status()).toMatchObject({ descriptionsEnabled: false, descriptionCount: 0 });
    await expect(index.useDescriptions({ signal: AbortSignal.abort() })).rejects.toThrow();
    other.close();
    index.close();
  });

  it("rejects indexes from before the cache schema cutover", async () => {
    const root = temporaryRoot();
    write(root, "a.ts", "export function one() { return 1; }\n");
    const provider = new FakeEmbeddingProvider();
    const index = new CodeIndex({ rootDir: root, provider });
    await index.updateFiles({ upsert: ["a.ts"] });
    index.close();
    const db = new DatabaseSync(path.join(root, ".slopdex/index.sqlite"));
    db.exec("UPDATE metadata SET value = '5' WHERE key = 'schema_version';");
    db.close();
    expect(() => new CodeIndex({ rootDir: root, provider })).toThrow(/Unsupported index schema version 5/);
  });

  it("persists the OpenAI model choice and enables descriptions on empty indexes", async () => {
    const root = temporaryRoot();
    const provider = new FakeEmbeddingProvider();
    const index = new CodeIndex({ rootDir: root, provider, descriptionProvider: new OpenAIDescriptionProvider({ model: "custom-model" }) });
    await index.useDescriptions();
    index.close();
    const reopened = new CodeIndex({ rootDir: root, provider });
    expect(reopened.descriptionProvider.profile.model).toBe("custom-model");
    expect(reopened.status()).toMatchObject({ descriptionsEnabled: true, descriptionCount: 0 });
    reopened.close();
  });
});
