import { existsSync } from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { pathToFileURL } from "node:url";

import { describe, expect, it } from "vitest";

import { temporaryRoot, write } from "../helpers.js";
import { runCli, runCliWithEnv, testTimeoutMs } from "./helpers.js";

describe("CLI descriptions", { timeout: testTimeoutMs }, () => {
  it("initializes, searches, updates, changes models, and preserves description mode through a rebuild", async () => {
    const root = temporaryRoot();
    write(root, "src/a.ts", "export function one() { return 1; }\n");
    write(root, ".slopdex/config.json", JSON.stringify({ dimensions: 2, descriptionProvider: "opencode-go" }));
    write(root, ".slopdex/mock-api.mjs", `
globalThis.fetch = async (url, options) => {
  const body = JSON.parse(options.body);
  if (url === 'https://api.openai.com/v1/responses' || url === 'https://opencode.ai/zen/v1/responses' || url === 'https://opencode.ai/zen/go/v1/responses') {
    const message = body.input[body.input.length - 1];
    const input = JSON.parse(message.content[0].text);
    return Response.json({status: 'completed', output: [{type: 'message', role: 'assistant', id: 'message-1', content: [{
      type: 'output_text', text: 'Purpose of ' + input.qualifiedName + ' using ' + body.model, annotations: []
    }]}]});
  }
  if (url === 'https://api.openai.com/v1/embeddings') {
    return Response.json({data: body.input.map((input, index) => ({index, embedding: [1, 0]}))});
  }
  throw new Error('Unexpected API URL: ' + url);
};
`);
    const env = {
      ...process.env,
      NODE_OPTIONS: `--import=${pathToFileURL(path.join(root, ".slopdex/mock-api.mjs")).href}`,
      OPENCODE_API_KEY: "test",
    };
    const run = (...args: string[]) => runCliWithEnv(root, env, ...args);
    const initialized = await run("descriptions", "enable");
    expect(initialized.status, initialized.stderr).toBe(0);
    expect(initialized.stderr).toContain('kind=descriptions provider="opencode-go" model="muse-spark-1.3-contributor" parallelism=10');
    expect(initialized.stderr).toContain('kind=vectors provider="openai" model="text-embedding-3-large" parallelism=10');
    expect(JSON.parse(initialized.stdout)).toEqual({ descriptionsCreated: 1, fileDescriptionsCreated: 1, descriptionsEnabled: true });
    expect(JSON.parse((await run("status")).stdout).descriptionProfile).toMatchObject({
      provider: "opencode-go",
      model: "muse-spark-1.3-contributor",
    });
    const old = new DatabaseSync(path.join(root, ".slopdex/index.sqlite"));
    old.prepare("UPDATE metadata SET value = ? WHERE key = 'description_profile'").run(JSON.stringify({
      provider: "opencode-go", model: "muse-spark-1.3-contributor", strategyVersion: "callable-purpose-v1",
    }));
    old.exec("DELETE FROM description_cache;");
    old.close();
    const migrated = await run("descriptions", "enable");
    expect(migrated.status, migrated.stderr).toBe(0);
    expect(JSON.parse(migrated.stdout).descriptionsCreated).toBe(1);
    expect(JSON.parse((await run("status")).stdout).descriptionProfile.strategyVersion).toBe("callable-purpose-v2");
    expect(JSON.parse((await run("descriptions", "enable")).stdout).descriptionsCreated).toBe(0);

    const disabled = await run("descriptions", "disable");
    expect(disabled.status, disabled.stderr).toBe(0);
    expect(JSON.parse(disabled.stdout)).toEqual({ descriptionsCreated: 0, fileDescriptionsCreated: 0, descriptionsEnabled: false });
    expect(JSON.parse((await run("status")).stdout)).toMatchObject({ descriptionCount: 0, descriptionsEnabled: false });
    expect((await run("search-descriptions", "workflow")).stderr).toContain("run descriptions enable first");
    expect(JSON.parse((await run("descriptions", "enable")).stdout)).toEqual({ descriptionsCreated: 0, fileDescriptionsCreated: 0, descriptionsEnabled: true });

    const search = await run("search-descriptions", "workflow", "--threshold", "0.9", "--limit", "1", "--format", "json");
    expect(search.status, search.stderr).toBe(0);
    expect(JSON.parse(search.stdout)[0].function.description).toBe("Purpose of one using muse-spark-1.3-contributor");
    expect(JSON.parse(search.stdout)[0].function).not.toHaveProperty("descriptionEmbeddingId");
    const selectedDescriptions = await run("search", "workflow", "--descriptions", "--threshold", "0.9", "--format", "json");
    expect(JSON.parse(selectedDescriptions.stdout)).toMatchObject([{ type: "function", function: { name: "one" } }]);
    const text = await run("search-descriptions", "workflow");
    expect(text.stdout).toContain("Purpose of one using muse-spark-1.3-contributor");
    expect((await run("search-description", "workflow")).status).toBe(0);

    write(root, "src/b.ts", "export function two() { return 2; }\n");
    const updated = await run("status");
    expect(updated.status, updated.stderr).toBe(0);
    expect(JSON.parse(updated.stdout)).toMatchObject({ functionCount: 2, descriptionCount: 2, descriptionsEnabled: true });
    for (const command of ["search", "search-descriptions"]) {
      const filtered = await run(command, "workflow", "-e", "^two$", "--limit", "1", "--format", "json");
      expect(filtered.status, filtered.stderr).toBe(0);
      expect(JSON.parse(filtered.stdout).map((match: { function: { name: string } }) => match.function.name)).toEqual(["two"]);
    }
    const changedModel = await run(
      "descriptions", "enable",
      "--description-provider", "opencode",
      "--description-model", "custom-description-model",
    );
    expect(changedModel.status, changedModel.stderr).toBe(0);
    expect(JSON.parse(changedModel.stdout)).toEqual({ descriptionsCreated: 0, fileDescriptionsCreated: 0, descriptionsEnabled: true });
    const reused = await run("search-descriptions", "workflow", "--threshold", "0.9", "--limit", "2", "--format", "json");
    expect(reused.status, reused.stderr).toBe(0);
    expect(JSON.parse(reused.stdout).map((match: { function: { description: string } }) => match.function.description).sort())
      .toEqual(["Purpose of one using muse-spark-1.3-contributor", "Purpose of two using muse-spark-1.3-contributor"]);
    write(root, ".slopdex/config.json", JSON.stringify({
      dimensions: 2,
      descriptionProvider: "opencode",
      descriptionModel: "custom-description-model",
    }));
    expect(JSON.parse((await run("status")).stdout).descriptionProfile).toMatchObject({
      provider: "opencode",
      model: "custom-description-model",
    });

    const cross = await run("cross-search", "--min-lines", "1", "--format", "json");
    expect(cross.status, cross.stderr).toBe(0);
    const crossRow = JSON.parse(cross.stdout.trim());
    expect(crossRow.matches[0]).toMatchObject({ similarity: 1, codeSimilarity: 1, descriptionSimilarity: 1 });
    expect(crossRow.scoring).toMatchObject({
      similarityMode: "code-description-file-average",
      similarityWeights: { code: 1 / 3, description: 1 / 3, fileDescription: 1 / 3 },
      sourceDescriptionProfile: { model: "custom-description-model" }, targetDescriptionProfile: { model: "custom-description-model" },
    });
    expect((await run("cross-search", "--min-lines", "1")).stdout).toContain("combined code + callable description + file description");
    const cohesion = await run("cross-search", "--cohesion", "--min-lines", "1", "--format", "json");
    expect(cohesion.status, cohesion.stderr).toBe(0);
    const report = JSON.parse(cohesion.stdout.trim());
    expect(report.matches[0]).toMatchObject({ similarity: 1, codeSimilarity: 1, descriptionSimilarity: 1, physicalDistance: 1 });
    expect(report.scoring).toMatchObject({
      similarityMode: "code-description-file-average",
      similarityWeights: { code: 1 / 3, description: 1 / 3, fileDescription: 1 / 3 },
    });

    const rebuilt = await run(
      "status", "--model", "text-embedding-3-small", "--force-reindex", "--yes-really-rebuild-the-index",
    );
    expect(rebuilt.status, rebuilt.stderr).toBe(0);
    expect(JSON.parse(rebuilt.stdout)).toMatchObject({
      functionCount: 2, descriptionCount: 2, descriptionsEnabled: true,
      descriptionProfile: { provider: "opencode", model: "custom-description-model" },
    });
  });

  it("reindexes stale file descriptions and optionally callable descriptions", async () => {
    const root = temporaryRoot();
    write(root, "src/a.ts", "export function one() { return 1; }\n");
    write(root, ".slopdex/config.json", JSON.stringify({ dimensions: 2 }));
    write(root, ".slopdex/mock-api.mjs", `
globalThis.fetch = async (url, options) => {
  const body = JSON.parse(options.body);
  if (url === 'https://api.openai.com/v1/responses') {
    const input = JSON.parse(body.input[body.input.length - 1].content[0].text);
    const text = input.request === 'Describe this file overall.' ? 'File purpose' : 'Callable purpose';
    return Response.json({status: 'completed', output: [{type: 'message', role: 'assistant', id: 'message-1', content: [{
      type: 'output_text', text, annotations: []
    }]}]});
  }
  if (url === 'https://api.openai.com/v1/embeddings') {
    return Response.json({data: body.input.map((_, index) => ({index, embedding: [1, 0]}))});
  }
  throw new Error('Unexpected API URL: ' + url);
};
`);
    const env = {
      ...process.env,
      NODE_OPTIONS: `--import=${pathToFileURL(path.join(root, ".slopdex/mock-api.mjs")).href}`,
      OPENAI_API_KEY: "test",
    };
    const run = (...args: string[]) => runCliWithEnv(root, env, ...args);
    expect((await run("descriptions", "enable")).status).toBe(0);

    write(root, "src/a.ts", "export function one() { return 2; }\n");
    const filesOnly = await run("reindex-files");
    expect(filesOnly.status, filesOnly.stderr).toBe(0);
    expect(JSON.parse(filesOnly.stdout)).toEqual({
      filesReindexed: 1, fileDescriptionsCreated: 1, descriptionsCreated: 0,
    });

    write(root, "src/a.ts", "export function one() { return 3; }\n");
    const withCallables = await run("reindex-files", "--callables");
    expect(withCallables.status, withCallables.stderr).toBe(0);
    expect(JSON.parse(withCallables.stdout)).toEqual({
      filesReindexed: 1, fileDescriptionsCreated: 1, descriptionsCreated: 1,
    });
  });

  it("validates description search arguments and explains how to enable descriptions", async () => {
    const root = temporaryRoot();
    const missingQuery = await runCli(root, "search-descriptions");
    expect(missingQuery.status).toBe(2);
    expect(missingQuery.stderr).toContain("search-descriptions requires a query");
    expect(existsSync(path.join(root, ".slopdex/index.sqlite"))).toBe(false);
    const disabled = await runCli(root, "search-descriptions", "workflow");
    expect(disabled.status).toBe(2);
    expect(disabled.stderr).toContain("run descriptions enable first");

    for (const args of [["descriptions"], ["descriptions", "maybe"], ["descriptions", "enable", "extra"]]) {
      const invalid = await runCli(root, ...args);
      expect(invalid.status).toBe(2);
      expect(invalid.stderr).toContain("descriptions requires enable or disable");
    }
    const invalidReindex = await runCli(root, "reindex-files", "extra");
    expect(invalidReindex.status).toBe(2);
    expect(invalidReindex.stderr).toContain("does not accept positional arguments");
  });
});
