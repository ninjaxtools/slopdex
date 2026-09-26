import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

import { describe, expect, it } from "vitest";

import { temporaryRoot, write } from "../helpers.js";
import { runCli, runCliWithEnv, testTimeoutMs } from "./helpers.js";

describe("CLI reranker configuration", { timeout: testTimeoutMs }, () => {
  it("enables, uses, changes, and disables reranking without opening an index during config", async () => {
    const root = temporaryRoot();
    const configPath = path.join(root, ".slopdex", "config.json");
    write(root, "functions.ts", [
      "export function one() { return 1; }",
      "export function two() { return 2; }",
    ].join("\n"));
    write(root, "guide.md", "# Guide\n\nDeployment documentation.\n");
    write(root, ".slopdex/config.json", JSON.stringify({ dimensions: 2 }));
    write(root, ".slopdex/mock-api.mjs", `
globalThis.fetch = async (url, options) => {
  const body = JSON.parse(options.body);
  if (String(url) === 'https://api.openai.com/v1/embeddings') {
    return Response.json({data: body.input.map((_, index) => ({index, embedding: [1, 0]}))});
  }
  if (String(url) === 'https://api.cohere.com/v2/rerank') {
    return Response.json({results: [
      {index: 1, relevance_score: 0.95},
      {index: 0, relevance_score: 0.25}
    ].slice(0, body.top_n)});
  }
  if (String(url) === 'https://api.openai.com/v1/responses') {
    return Response.json({status: 'completed', output: [{
      type: 'message', role: 'assistant', id: 'message-1', content: [{
        type: 'output_text', text: JSON.stringify({ranking: [{index: 1, score: 0.99}]}), annotations: []
      }]
    }]});
  }
  throw new Error('Unexpected API URL: ' + url);
};
`);
    const env = {
      ...process.env,
      NODE_OPTIONS: `--import=${pathToFileURL(path.join(root, ".slopdex/mock-api.mjs")).href}`,
      COHERE_API_KEY: "test",
    };
    const run = (...args: string[]) => runCliWithEnv(root, env, ...args);

    const enabled = await run("config", "reranker", "cohere", "rerank-v4.0-fast", "--format", "json");
    expect(enabled.status, enabled.stderr).toBe(0);
    expect(JSON.parse(enabled.stdout)).toMatchObject({
      configPath,
      rerankingEnabled: true,
      rerankerProvider: "cohere",
      rerankerModel: "rerank-v4.0-fast",
    });
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const search = await run("search", "find two", "--limit", "1", "--format", "json");
    expect(search.status, search.stderr).toBe(0);
    expect(JSON.parse(search.stdout)).toMatchObject([
      { similarity: 1, rerankScore: 0.95, function: { name: "two" } },
    ]);

    const jina = await run("config", "reranker", "jina");
    expect(jina.status, jina.stderr).toBe(0);
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toMatchObject({
      rerankingEnabled: true,
      rerankerProvider: "jina",
      rerankerModel: "jina-reranker-v3.5",
    });
    const openai = await run("config", "reranker", "openai", "custom-ranker", "--reranker-candidates", "12", "--format", "json");
    expect(openai.status, openai.stderr).toBe(0);
    expect(JSON.parse(openai.stdout)).toMatchObject({
      rerankingEnabled: true,
      rerankerProvider: "openai",
      rerankerModel: "custom-ranker",
      rerankerCandidates: 12,
    });
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toMatchObject({ rerankerCandidates: 12 });
    const resized = await run("config", "reranker", "openai", "--reranker-candidates", "15", "--format", "json");
    expect(resized.status, resized.stderr).toBe(0);
    expect(JSON.parse(resized.stdout)).toMatchObject({ rerankerModel: "custom-ranker", rerankerCandidates: 15 });
    const llmSearch = await run("search", "find two", "--limit", "1", "--format", "json");
    expect(llmSearch.status, llmSearch.stderr).toBe(0);
    expect(JSON.parse(llmSearch.stdout)).toMatchObject([
      { rerankScore: 0.99, function: { name: "two" } },
    ]);
    const disabled = await run("config", "reranker", "disable");
    expect(disabled.status, disabled.stderr).toBe(0);
    expect(JSON.parse(readFileSync(configPath, "utf8")).rerankingEnabled).toBe(false);

    const combined = await run("search", "anything", "--format", "json");
    expect(JSON.parse(combined.stdout).map((result: { type: string }) => result.type))
      .toEqual(["function", "function", "markdown"]);
    const markdownOnly = await run("search", "anything", "--md", "--format", "json");
    expect(JSON.parse(markdownOnly.stdout)).toMatchObject([{ type: "markdown", chunk: { path: "guide.md" } }]);
    const selectedCode = await run("search", "anything", "--code", "--format", "json");
    expect(JSON.parse(selectedCode.stdout).map((result: { type: string }) => result.type))
      .toEqual(["function", "function"]);
    const codeOnly = await run("search-code", "anything", "--format", "json");
    expect(JSON.parse(codeOnly.stdout)).toHaveLength(2);
    const dedicatedMarkdown = await run("search-md", "anything", "--format", "json");
    expect(JSON.parse(dedicatedMarkdown.stdout)).toMatchObject([{ chunk: { path: "guide.md" } }]);
    expect((await run("search-md", "anything", "--threshold", "1.1")).stdout).toBe("No matches.\n");
    expect((await run("search", "anything", "--threshold", "1.1")).stdout).toBe("No matches.\n");
    const unavailableDescriptions = await run("search", "anything", "--descriptions");
    expect(unavailableDescriptions.status).toBe(2);
    expect(unavailableDescriptions.stderr).toContain("run descriptions enable first");
  });

  it("rejects invalid reranker configuration before creating an index", async () => {
    const root = temporaryRoot();
    const invalid = await runCli(root, "config", "reranker", "unknown");
    expect(invalid.status).toBe(2);
    expect(invalid.stderr).toContain("requires cohere, jina, openai, or disable");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const misplacedCandidates = await runCli(root, "config", "reranker", "cohere", "--reranker-candidates", "10");
    expect(misplacedCandidates.status).toBe(2);
    expect(misplacedCandidates.stderr).toContain("only available with config reranker openai");

    const invalidCandidates = await runCli(root, "config", "reranker", "openai", "--reranker-candidates", "0");
    expect(invalidCandidates.status).toBe(2);
    expect(invalidCandidates.stderr).toContain("reranker candidate count must be a positive integer");

    const excessiveCandidates = await runCli(root, "config", "reranker", "openai", "--reranker-candidates", "101");
    expect(excessiveCandidates.status).toBe(2);
    expect(excessiveCandidates.stderr).toContain("reranker candidate count must not exceed 100");

    write(root, ".slopdex/config.json", JSON.stringify({ rerankingEnabled: true, rerankerProvider: "cohere", rerankerModel: 3 }));
    const malformed = await runCli(root, "status");
    expect(malformed.status).toBe(2);
    expect(malformed.stderr).toContain("rerankerModel must be a non-empty string");

    const disabled = await runCli(root, "config", "reranker", "disable");
    expect(disabled.status, disabled.stderr).toBe(0);
    expect((await runCli(root, "status")).status).toBe(0);
  });
});
