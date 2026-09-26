import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

import { describe, expect, it } from "vitest";

import { temporaryRoot, write } from "../helpers.js";
import { runCli, runCliWithEnv, testTimeoutMs } from "./helpers.js";

describe("CLI model configuration", { timeout: testTimeoutMs }, () => {
  it("requires a terminal for argument-free interactive configuration", async () => {
    const root = temporaryRoot();
    const result = await runCli(root, "config");

    expect(result.status).toBe(2);
    expect(result.stderr).toContain("config without an action requires an interactive terminal");
  });

  it("lists published models and validates index-free config changes", async () => {
    const root = temporaryRoot();
    const configPath = path.join(root, ".slopdex", "config.json");
    write(root, "src/a.ts", "export function one() { return 1; }\n");
    write(root, ".slopdex/config.json", JSON.stringify({ dimensions: 2, exclude: ["fixtures/**"] }));
    write(root, ".slopdex/mock-api.mjs", `
globalThis.fetch = async (input, options) => {
  const url = String(input);
  if (url === 'https://opencode.ai/zen/v1/models') {
    return Response.json({data: [{id: 'gpt-5.6-sol'}, {id: 'shared-model'}]});
  }
  if (url === 'https://opencode.ai/zen/go/v1/models') {
    return Response.json({data: [{id: 'gpt-5.6-luna'}, {id: 'muse-spark-1.3-contributor'}, {id: 'shared-model'}]});
  }
  const body = JSON.parse(options.body);
  if (url === 'https://api.openai.com/v1/embeddings') {
    return Response.json({data: body.input.map((_, index) => ({index, embedding: [1, 0]}))});
  }
  if (url === 'https://opencode.ai/zen/go/v1/responses') {
    return Response.json({output: [{type: 'message', role: 'assistant', id: 'message-1', content: [{
      type: 'output_text', text: 'Purpose of one', annotations: []
    }]}]});
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

    const listed = await run("models", "opencode", "--format", "json");
    expect(listed.status, listed.stderr).toBe(0);
    expect(JSON.parse(listed.stdout)).toEqual([
      { provider: "opencode", model: "gpt-5.6-sol" },
      { provider: "opencode", model: "shared-model" },
    ]);
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const ambiguous = await run("config", "model", "shared-model");
    expect(ambiguous.status).toBe(2);
    expect(ambiguous.stderr).toContain("available from multiple providers");
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toEqual({ dimensions: 2, exclude: ["fixtures/**"] });

    const automatic = await run("config", "model", "gpt-5.6-sol");
    expect(automatic.status, automatic.stderr).toBe(0);
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toMatchObject({
      descriptionProvider: "opencode",
      descriptionModel: "gpt-5.6-sol",
      exclude: ["fixtures/**"],
    });

    const qualified = await run("config", "model", "opencode-go/gpt-5.6-luna", "--format", "json");
    expect(qualified.status, qualified.stderr).toBe(0);
    expect(JSON.parse(qualified.stdout)).toMatchObject({
      configPath,
      descriptionProvider: "opencode-go",
      descriptionModel: "gpt-5.6-luna",
    });

    const fallback = await run("config", "fallback-model", "muse-spark-1.3-contributor", "--format", "json");
    expect(fallback.status, fallback.stderr).toBe(0);
    expect(JSON.parse(fallback.stdout)).toMatchObject({
      configPath,
      descriptionProvider: "opencode-go",
      descriptionFallbackModel: "muse-spark-1.3-contributor",
    });
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toMatchObject({
      descriptionProvider: "opencode-go",
      descriptionModel: "gpt-5.6-luna",
      descriptionFallbackModel: "muse-spark-1.3-contributor",
    });

    const mismatchedFallback = await run("config", "fallback-model", "opencode/gpt-5.6-sol");
    expect(mismatchedFallback.status).toBe(2);
    expect(mismatchedFallback.stderr).toContain("Fallback model provider must match");

    const invalid = await run("config", "model", "opencode-go/not-published");
    expect(invalid.status).toBe(2);
    expect(invalid.stderr).toContain("Unknown opencode-go model");
    expect(JSON.parse(readFileSync(configPath, "utf8")).descriptionModel).toBe("gpt-5.6-luna");

    const parallelism = await run("config", "parallelism", "4", "--format", "json");
    expect(parallelism.status, parallelism.stderr).toBe(0);
    expect(JSON.parse(parallelism.stdout)).toMatchObject({ configPath, parallelism: 4 });
    expect(JSON.parse(readFileSync(configPath, "utf8")).parallelism).toBe(4);
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const invalidParallelism = await run("config", "parallelism", "zero");
    expect(invalidParallelism.status).toBe(2);
    expect(invalidParallelism.stderr).toContain("config parallelism requires a positive integer");
    expect(JSON.parse(readFileSync(configPath, "utf8")).parallelism).toBe(4);

    const enabled = await run("config", "descriptions", "enable");
    expect(enabled.status, enabled.stderr).toBe(0);
    expect(JSON.parse(readFileSync(configPath, "utf8")).descriptionsEnabled).toBe(true);
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const status = await run("status");
    expect(status.status, status.stderr).toBe(0);
    expect(JSON.parse(status.stdout)).toMatchObject({
      descriptionsEnabled: true,
      descriptionCount: 1,
      descriptionProfile: { provider: "opencode-go", model: "gpt-5.6-luna" },
    });

    expect((await run("config", "descriptions", "disable")).status).toBe(0);
    const disabled = await run("status");
    expect(disabled.status, disabled.stderr).toBe(0);
    expect(JSON.parse(disabled.stdout).descriptionsEnabled).toBe(false);
  });
});
