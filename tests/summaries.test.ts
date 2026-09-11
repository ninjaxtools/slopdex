import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { describe, expect, it } from "vitest";

import { CodeIndex } from "../src/code-index.js";
import { OpenAISummaryProvider } from "../src/summaries/openai.js";
import { resetIndexState } from "../src/storage/database.js";
import type { EmbeddingProvider, SummaryInput, SummaryProvider } from "../src/types.js";
import { FakeEmbeddingProvider, commitAll, git, initGit, temporaryRoot, write } from "./helpers.js";

class FakeSummaryProvider implements SummaryProvider {
  public readonly profile = { provider: "fake", model: "purpose", strategyVersion: "v1" };
  public inputs: SummaryInput[] = [];
  public fail = false;

  public async summarize(input: SummaryInput): Promise<string> {
    this.inputs.push(input);
    if (this.fail) throw new Error("Summary service unavailable");
    return `Purpose of ${input.callable.qualifiedName} in ${input.callable.path}: support the application workflow.`;
  }
}

describe("purpose summaries", () => {
  it("is optional, backfills every callable, persists, and reuses unchanged summaries", async () => {
    const root = temporaryRoot();
    const source = `import { send } from './transport';
export function deliver() { send(); }
export class Client { constructor() {} send() { deliver(); } }
`;
    write(root, "client.ts", source);
    const provider = new FakeEmbeddingProvider();
    const summaries = new FakeSummaryProvider();
    const index = new CodeIndex({ rootDir: root, provider, summaryProvider: summaries });
    await index.updateFiles({ upsert: ["client.ts"] });
    expect(index.status()).toMatchObject({ summariesEnabled: false, summaryCount: 0 });
    expect(index.allFunctions().every((value) => value.summary === null)).toBe(true);
    expect(summaries.inputs).toHaveLength(0);
    await expect(index.searchSummary({ query: "workflow" })).rejects.toThrow(/summaries enable/);

    const functionsBefore = index.allFunctions();
    const vectorsBefore = functionsBefore.map((value) => index.vectorForFunction(value.id));
    await expect(index.useSummaries()).resolves.toEqual({ summariesCreated: 3, summariesEnabled: true });
    expect(index.status()).toMatchObject({ summaryCount: 3, summaryProfile: summaries.profile });
    expect(summaries.inputs.every((input) => input.fileSource === source && input.repository === path.basename(root))).toBe(true);
    expect(index.allFunctions().map((value) => value.id)).toEqual(functionsBefore.map((value) => value.id));
    expect(index.allFunctions().map((value) => index.vectorForFunction(value.id))).toEqual(vectorsBefore);
    await expect(index.useSummaries()).resolves.toEqual({ summariesCreated: 0, summariesEnabled: true });
    expect(index.disableSummaries()).toEqual({ summariesCreated: 0, summariesEnabled: false });
    expect(index.status()).toMatchObject({ summariesEnabled: false, summaryCount: 3 });
    await expect(index.searchSummary({ query: "workflow" })).rejects.toThrow(/summaries enable/);
    await expect(index.useSummaries()).resolves.toEqual({ summariesCreated: 0, summariesEnabled: true });
    index.close();

    const reopened = new CodeIndex({ rootDir: root, provider, summaryProvider: summaries });
    await reopened.updateFromWorkingTree();
    expect(summaries.inputs).toHaveLength(3);
    expect(reopened.status()).toMatchObject({ summariesEnabled: true, summaryCount: 3 });
    const results = await reopened.searchSummary({ query: "workflow" });
    expect(results).toHaveLength(3);
    expect(results.every((value) => value.function.summary?.includes("Purpose of"))).toBe(true);
    reopened.close();
  });

  it("refreshes context changes, additions, renames and deletions through Git updates", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, ".gitignore", ".slopdex/\n");
    write(root, "a.ts", "const mode = 'initial'; export function one() { return mode; }\n");
    write(root, "b.ts", "export function two() { return 2; }\n");
    commitAll(root, "initial");
    const summaries = new FakeSummaryProvider();
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider(), summaryProvider: summaries });
    await index.updateFromGit();
    await index.useSummaries();
    await index.updateFromGit();
    expect(summaries.inputs).toHaveLength(2);

    write(root, "a.ts", "const mode = 'changed'; export function one() { return mode; }\n");
    write(root, "c.ts", "export const three = () => 3;\n");
    await index.updateFromGit();
    expect(summaries.inputs.slice(2).map((value) => value.callable.name).sort()).toEqual(["one", "three"]);
    commitAll(root, "context and new function");
    await index.updateFromGit();
    expect(summaries.inputs).toHaveLength(4);

    git(root, "mv", "a.ts", "renamed.ts");
    git(root, "rm", "b.ts");
    await index.updateFromGit();
    expect(summaries.inputs).toHaveLength(5);
    expect(summaries.inputs[4]!.callable.path).toBe("renamed.ts");
    expect(index.status()).toMatchObject({ functionCount: 2, summaryCount: 2 });
    const results = await index.searchSummary({ query: "workflow" });
    expect(results.map((value) => value.function.path).sort()).toEqual(["c.ts", "renamed.ts"]);
    index.close();
  });

  it("searches the summary vectors independently of the code vectors and applies thresholds", async () => {
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
    const index = new CodeIndex({ rootDir: root, provider, summaryProvider: new FakeSummaryProvider() });
    await index.updateFiles({ upsert: ["functions.ts"] });
    await index.useSummaries();
    expect((await index.similaritySearch({ query: "purpose", limit: 1 }))[0]!.function.name).toBe("one");
    expect((await index.searchSummary({ query: "purpose", limit: 1 }))[0]!.function.name).toBe("two");
    expect((await index.searchSummary({ query: "purpose", minSimilarity: 0, maxSimilarity: 1 })).map((value) => value.function.name)).toEqual(["one"]);
    expect(await index.searchSummary({ query: "purpose", minSimilarity: 1.1 })).toEqual([]);
    await expect(index.searchSummary({ query: " " })).rejects.toThrow(/empty/);
    await expect(index.searchSummary({ query: "purpose", limit: 0 })).rejects.toThrow(/positive integer/);
    index.close();
  });

  it("rolls back initialization and subsequent updates when generation or embedding fails", async () => {
    const root = temporaryRoot();
    write(root, "a.ts", "export function one() { return 1; }\n");
    const provider = new FakeEmbeddingProvider();
    const summaries = new FakeSummaryProvider();
    const index = new CodeIndex({ rootDir: root, provider, summaryProvider: summaries });
    await index.updateFiles({ upsert: ["a.ts"] });
    const before = index.status();
    const embed = provider.embedDocuments.bind(provider);
    provider.embedDocuments = async () => [[NaN]];
    await expect(index.useSummaries()).rejects.toThrow(/vector/);
    expect(index.status()).toEqual(before);
    expect(index.allFunctions()[0]!.summary).toBeNull();
    provider.embedDocuments = embed;
    await index.useSummaries();
    const enabledStatus = index.status();
    const enabledFunctions = index.allFunctions();
    write(root, "a.ts", "export function one() { return 2; }\n");
    summaries.fail = true;
    await expect(index.updateFiles({ upsert: ["a.ts"] })).rejects.toThrow(/unavailable/);
    expect(index.status()).toEqual(enabledStatus);
    expect(index.allFunctions()).toEqual(enabledFunctions);
    summaries.fail = false;
    await index.updateFiles({ upsert: ["a.ts"] });
    expect(index.allFunctions()[0]!.source).toContain("return 2");
    index.close();
  });

  it("persists each generated summary before a later summary request fails", async () => {
    const root = temporaryRoot();
    write(root, "functions.ts", "export function one() { return 1; }\nexport function two() { return 2; }\n");
    class FailingSummaryProvider extends FakeSummaryProvider {
      public shouldFail = true;

      public override async summarize(input: SummaryInput): Promise<string> {
        if (this.shouldFail && input.callable.name === "two") {
          this.inputs.push(input);
          throw new Error("summary failed");
        }
        return await super.summarize(input);
      }
    }
    const summaries = new FailingSummaryProvider();
    const provider = new FakeEmbeddingProvider();
    const first = new CodeIndex({ rootDir: root, provider, summaryProvider: summaries, embeddingBatchSize: 1 });
    await first.updateFiles({ upsert: ["functions.ts"] });
    await expect(first.useSummaries()).rejects.toThrow(/summary failed/);
    expect(first.status()).toMatchObject({ summariesEnabled: false, summaryCount: 0 });
    first.close();

    summaries.shouldFail = false;
    const resumed = new CodeIndex({ rootDir: root, provider, summaryProvider: summaries, embeddingBatchSize: 1 });
    await expect(resumed.useSummaries()).resolves.toEqual({ summariesCreated: 1, summariesEnabled: true });
    expect(summaries.inputs.filter((input) => input.callable.name === "one")).toHaveLength(1);
    expect(summaries.inputs.filter((input) => input.callable.name === "two")).toHaveLength(2);
    resumed.close();
  });

  it("persists each completed summary vector before a later vector request fails", async () => {
    const root = temporaryRoot();
    write(root, "functions.ts", "export function one() { return 1; }\nexport function two() { return 2; }\n");
    class FailingEmbeddingProvider extends FakeEmbeddingProvider {
      public summaryInputs: string[] = [];
      public shouldFail = true;

      public override async embedDocuments(inputs: readonly string[]): Promise<number[][]> {
        if (inputs.every((input) => input.startsWith("Purpose of"))) {
          this.summaryInputs.push(...inputs);
          if (this.shouldFail && this.summaryInputs.length === 2) throw new Error("summary embedding failed");
        }
        return await super.embedDocuments(inputs);
      }
    }
    const provider = new FailingEmbeddingProvider();
    const summaries = new FakeSummaryProvider();
    const first = new CodeIndex({ rootDir: root, provider, summaryProvider: summaries, embeddingBatchSize: 1 });
    await first.updateFiles({ upsert: ["functions.ts"] });
    await expect(first.useSummaries()).rejects.toThrow(/summary embedding failed/);
    first.close();

    provider.shouldFail = false;
    const resumed = new CodeIndex({ rootDir: root, provider, summaryProvider: summaries, embeddingBatchSize: 1 });
    await expect(resumed.useSummaries()).resolves.toEqual({ summariesCreated: 0, summariesEnabled: true });
    expect(summaries.inputs).toHaveLength(2);
    expect(provider.summaryInputs.filter((input) => input.includes("one"))).toHaveLength(1);
    expect(provider.summaryInputs.filter((input) => input.includes("two"))).toHaveLength(2);
    resumed.close();
  });

  it("retains summary text across a logical rebuild with a new embedding profile", async () => {
    const root = temporaryRoot();
    write(root, "function.ts", "export function one() { return 1; }\n");
    const provider = new FakeEmbeddingProvider();
    const summaries = new FakeSummaryProvider();
    const first = new CodeIndex({ rootDir: root, provider, summaryProvider: summaries });
    await first.updateFiles({ upsert: ["function.ts"] });
    await first.useSummaries();
    const indexPath = first.indexPath;
    first.close();

    const replacement: EmbeddingProvider = {
      profile: { ...provider.profile, model: "replacement" },
      embedDocuments: provider.embedDocuments.bind(provider),
      embedQuery: provider.embedQuery.bind(provider),
    };
    resetIndexState(indexPath, root, replacement.profile);
    const rebuilt = new CodeIndex({ rootDir: root, provider: replacement, summaryProvider: summaries });
    await rebuilt.useSummaries();
    await rebuilt.updateFiles({ upsert: ["function.ts"] });
    expect(summaries.inputs).toHaveLength(1);
    expect(rebuilt.status()).toMatchObject({ functionCount: 1, summaryCount: 1, summariesEnabled: true });
    rebuilt.close();
  });

  it("rejects concurrent changes and aborted initialization without enabling summaries", async () => {
    const root = temporaryRoot();
    write(root, "a.ts", "export function one() { return 1; }\n");
    const provider = new FakeEmbeddingProvider();
    const summaries = new FakeSummaryProvider();
    const index = new CodeIndex({ rootDir: root, provider, summaryProvider: summaries });
    await index.updateFiles({ upsert: ["a.ts"] });
    const other = new CodeIndex({ rootDir: root, provider });
    summaries.summarize = async () => {
      await other.updateFiles({ delete: ["a.ts"] });
      return "Purpose";
    };
    await expect(index.useSummaries()).rejects.toThrow(/Index changed/);
    expect(index.status()).toMatchObject({ summariesEnabled: false, summaryCount: 0 });
    await expect(index.useSummaries({ signal: AbortSignal.abort() })).rejects.toThrow();
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
    db.exec("UPDATE metadata SET value = '4' WHERE key = 'schema_version';");
    db.close();
    expect(() => new CodeIndex({ rootDir: root, provider })).toThrow(/Unsupported index schema version 4/);
  });

  it("persists the OpenAI model choice and enables summaries on empty indexes", async () => {
    const root = temporaryRoot();
    const provider = new FakeEmbeddingProvider();
    const index = new CodeIndex({ rootDir: root, provider, summaryProvider: new OpenAISummaryProvider({ model: "custom-model" }) });
    await index.useSummaries();
    index.close();
    const reopened = new CodeIndex({ rootDir: root, provider });
    expect(reopened.summaryProvider.profile.model).toBe("custom-model");
    expect(reopened.status()).toMatchObject({ summariesEnabled: true, summaryCount: 0 });
    reopened.close();
  });
});
