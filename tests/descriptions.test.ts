import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { describe, expect, it } from "vitest";

import { CodeIndex } from "../src/code-index.js";
import { OpenAIDescriptionProvider } from "../src/descriptions/openai.js";
import { resetIndexState } from "../src/storage/database.js";
import type { DescriptionFileInput, DescriptionInput, DescriptionProvider, EmbeddingProvider } from "../src/types.js";
import { FakeEmbeddingProvider, commitAll, git, initGit, temporaryRoot, write } from "./helpers.js";

class FakeDescriptionProvider implements DescriptionProvider {
  public readonly profile = { provider: "fake", model: "purpose", strategyVersion: "v1" };
  public inputs: DescriptionInput[] = [];
  public fileInputs: DescriptionFileInput[] = [];
  public fail = false;

  public async describeFile(input: DescriptionFileInput): Promise<string> {
    this.fileInputs.push(input);
    if (this.fail) throw new Error("Description service unavailable");
    return `Purpose of ${input.path}: support the application workflow.`;
  }

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
    await expect(index.useDescriptions()).resolves.toEqual({ descriptionsCreated: 3, fileDescriptionsCreated: 1, descriptionsEnabled: true });
    expect(index.status()).toMatchObject({ descriptionCount: 3, fileDescriptionCount: 1, descriptionProfile: descriptions.profile });
    expect(descriptions.fileInputs).toHaveLength(1);
    expect(descriptions.inputs.every((input) => input.fileSource === source && input.repository === path.basename(root))).toBe(true);
    expect(index.allFunctions().map((value) => value.id)).toEqual(functionsBefore.map((value) => value.id));
    expect(index.allFunctions().map((value) => index.vectorForFunction(value.id))).toEqual(vectorsBefore);
    await expect(index.useDescriptions()).resolves.toEqual({ descriptionsCreated: 0, fileDescriptionsCreated: 0, descriptionsEnabled: true });
    expect(index.disableDescriptions()).toEqual({ descriptionsCreated: 0, fileDescriptionsCreated: 0, descriptionsEnabled: false });
    expect(index.status()).toMatchObject({ descriptionsEnabled: false, descriptionCount: 3 });
    await expect(index.searchDescription({ query: "workflow" })).rejects.toThrow(/descriptions enable/);
    await expect(index.useDescriptions()).resolves.toEqual({ descriptionsCreated: 0, fileDescriptionsCreated: 0, descriptionsEnabled: true });
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

  it("keeps changed file descriptions stale until explicitly reindexed", async () => {
    const root = temporaryRoot();
    write(root, "functions.ts", "export function one() { return 1; }\nexport function two() { return 2; }\n");
    const descriptions = new FakeDescriptionProvider();
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider(), descriptionProvider: descriptions });
    await index.updateFiles({ upsert: ["functions.ts"] });
    await index.useDescriptions();
    expect(descriptions.fileInputs).toHaveLength(1);
    expect(descriptions.inputs).toHaveLength(2);

    write(root, "functions.ts", "export function one() { return 3; }\nexport function two() { return 4; }\n");
    await index.updateFiles({ upsert: ["functions.ts"] });
    expect(index.status()).toMatchObject({ staleFileDescriptionCount: 1, fileDescriptionCount: 1 });
    expect(descriptions.fileInputs).toHaveLength(1);
    expect(descriptions.inputs).toHaveLength(4);

    await expect(index.reindexFiles()).resolves.toEqual({
      filesReindexed: 1, fileDescriptionsCreated: 1, descriptionsCreated: 0,
    });
    expect(index.status().staleFileDescriptionCount).toBe(0);
    expect(descriptions.fileInputs).toHaveLength(2);
    expect(descriptions.inputs).toHaveLength(4);
    await expect(index.reindexFiles()).resolves.toEqual({
      filesReindexed: 0, fileDescriptionsCreated: 0, descriptionsCreated: 0,
    });

    write(root, "functions.ts", "export function one() { return 5; }\nexport function two() { return 6; }\n");
    await index.updateFiles({ upsert: ["functions.ts"] });
    await expect(index.reindexFiles({ includeCallables: true })).resolves.toEqual({
      filesReindexed: 1, fileDescriptionsCreated: 1, descriptionsCreated: 2,
    });
    expect(descriptions.fileInputs).toHaveLength(3);
    expect(descriptions.inputs).toHaveLength(8);
    index.close();
  });

  it("resumes a file-description reindex from its durable generation cache", async () => {
    const root = temporaryRoot();
    write(root, "function.ts", "export function one() { return 1; }\n");
    class VersionedDescriptions extends FakeDescriptionProvider {
      public override async describeFile(input: DescriptionFileInput): Promise<string> {
        this.fileInputs.push(input);
        return `File description ${this.fileInputs.length}`;
      }
    }
    class FailingEmbeddingProvider extends FakeEmbeddingProvider {
      public fail = true;

      public override async embedDocuments(inputs: readonly string[]): Promise<number[][]> {
        if (this.fail && inputs.includes("File description 2")) throw new Error("file description embedding failed");
        return super.embedDocuments(inputs);
      }
    }
    const descriptions = new VersionedDescriptions();
    const provider = new FailingEmbeddingProvider();
    const index = new CodeIndex({ rootDir: root, provider, descriptionProvider: descriptions });
    await index.updateFiles({ upsert: ["function.ts"] });
    await index.useDescriptions();
    write(root, "function.ts", "export function one() { return 2; }\n");
    await index.updateFiles({ upsert: ["function.ts"] });

    await expect(index.reindexFiles()).rejects.toThrow(/file description embedding failed/);
    expect(descriptions.fileInputs).toHaveLength(2);
    expect(index.status().staleFileDescriptionCount).toBe(1);
    provider.fail = false;
    await expect(index.reindexFiles()).resolves.toEqual({
      filesReindexed: 1, fileDescriptionsCreated: 0, descriptionsCreated: 0,
    });
    expect(descriptions.fileInputs).toHaveLength(2);
    expect(index.status().staleFileDescriptionCount).toBe(0);
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

  it("skips unavailable files when measuring file-description coverage", async () => {
    const root = temporaryRoot();
    write(root, "healthy.ts", "export function one() { return 1; }\n");
    write(root, "large.ts", "export function oversized() { return 'too large'; }\n");
    const index = new CodeIndex({
      rootDir: root,
      provider: new FakeEmbeddingProvider(),
      descriptionProvider: new FakeDescriptionProvider(),
      maxFileSize: 45,
    });
    await index.updateFiles({ upsert: ["healthy.ts", "large.ts"] });
    await expect(index.useDescriptions()).resolves.toMatchObject({
      descriptionsCreated: 1, fileDescriptionsCreated: 1, descriptionsEnabled: true,
    });
    expect(index.status()).toMatchObject({
      fileCount: 2, describableFileCount: 1, fileDescriptionCount: 1, staleFileDescriptionCount: 0,
    });

    write(root, "healthy.ts", "export function one() { return 'now too large for this index'; }\n");
    await index.updateFiles({ upsert: ["healthy.ts"] });
    expect(index.status()).toMatchObject({
      fileCount: 2, describableFileCount: 0, fileDescriptionCount: 0, staleFileDescriptionCount: 0,
    });
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
    await expect(resumed.useDescriptions()).resolves.toEqual({ descriptionsCreated: 1, fileDescriptionsCreated: 0, descriptionsEnabled: true });
    expect(descriptions.inputs.filter((input) => input.callable.name === "one")).toHaveLength(1);
    expect(descriptions.inputs.filter((input) => input.callable.name === "two")).toHaveLength(2);
    resumed.close();
  });

  it("replays cached descriptions when a file conversation resumes", async () => {
    const root = temporaryRoot();
    write(root, "functions.ts", "export function one() { return 1; }\nexport function two() { return 2; }\n");
    class SessionDescriptionProvider extends FakeDescriptionProvider {
      public events: string[] = [];
      public failOnTwo = true;

      public startFile(input: { path: string }) {
        this.events.push(`start:${input.path}`);
        return {
          describeFile: async () => {
            this.events.push("describe-file");
            return "Purpose of functions.ts";
          },
          replayFile: (description: string) => {
            this.events.push(`replay-file:${description}`);
          },
          describe: async (callable: DescriptionInput["callable"]) => {
            this.events.push(`describe:${callable.name}`);
            if (this.failOnTwo && callable.name === "two") throw new Error("description failed");
            return `Purpose of ${callable.name}`;
          },
          replay: (callable: DescriptionInput["callable"], description: string) => {
            this.events.push(`replay:${callable.name}:${description}`);
          },
        };
      }
    }
    const descriptions = new SessionDescriptionProvider();
    const provider = new FakeEmbeddingProvider();
    const first = new CodeIndex({ rootDir: root, provider, descriptionProvider: descriptions });
    await first.updateFiles({ upsert: ["functions.ts"] });
    await expect(first.useDescriptions()).rejects.toThrow(/description failed/);
    first.close();

    descriptions.failOnTwo = false;
    const resumed = new CodeIndex({ rootDir: root, provider, descriptionProvider: descriptions });
    await expect(resumed.useDescriptions()).resolves.toEqual({ descriptionsCreated: 1, fileDescriptionsCreated: 0, descriptionsEnabled: true });
    expect(descriptions.events).toEqual([
      "start:functions.ts",
      "describe-file",
      "describe:one",
      "describe:two",
      "start:functions.ts",
      "replay-file:Purpose of functions.ts",
      "replay:one:Purpose of one",
      "describe:two",
    ]);
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
          if (this.shouldFail && this.descriptionInputs.length === 3) throw new Error("description embedding failed");
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
    await expect(resumed.useDescriptions()).resolves.toEqual({ descriptionsCreated: 0, fileDescriptionsCreated: 0, descriptionsEnabled: true });
    expect(descriptions.inputs).toHaveLength(2);
    expect(provider.descriptionInputs.filter((input) => input.startsWith("Purpose of one in"))).toHaveLength(1);
    expect(provider.descriptionInputs.filter((input) => input.startsWith("Purpose of two in"))).toHaveLength(2);
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

  it("migrates schema 6 indexes to file-description storage", () => {
    const root = temporaryRoot();
    const indexPath = path.join(root, ".slopdex/index.sqlite");
    const provider = new FakeEmbeddingProvider();
    write(root, ".slopdex/.keep", "");
    const db = new DatabaseSync(indexPath);
    db.exec(`
      CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
      CREATE TABLE files (
        path TEXT PRIMARY KEY, content_hash TEXT NOT NULL, blob_oid TEXT,
        source_mode TEXT NOT NULL, indexed_commit TEXT, previous_path TEXT,
        language TEXT NOT NULL, byte_size INTEGER NOT NULL
      );
    `);
    const metadata = db.prepare("INSERT INTO metadata(key, value) VALUES (?, ?)");
    metadata.run("schema_version", "6");
    metadata.run("root_dir", root);
    metadata.run("embedding_profile", JSON.stringify(provider.profile));
    metadata.run("generation", "0");
    db.close();

    const index = new CodeIndex({ rootDir: root, provider });
    expect(index.status()).toMatchObject({ fileDescriptionCount: 0, staleFileDescriptionCount: 0 });
    index.close();
    const migrated = new DatabaseSync(indexPath, { readOnly: true });
    expect(migrated.prepare("SELECT value FROM metadata WHERE key = 'schema_version'").get()).toEqual({ value: "8" });
    expect((migrated.prepare("PRAGMA table_info(files)").all() as Array<{ name: string }>).map((column) => column.name))
      .toContain("file_description_embedding_id");
    migrated.close();
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
