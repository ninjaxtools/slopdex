import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

import { describe, expect, it } from "vitest";

import { temporaryRoot, write } from "../helpers.js";
import { runCli, runCliWithEnv, testTimeoutMs } from "./helpers.js";

describe("CLI describe", { timeout: testTimeoutMs }, () => {
  it("generates an explanation, includes whole files, and retries without them on failure", async () => {
    const root = temporaryRoot();
    const logPath = path.join(root, ".slopdex", "describe-log.jsonl");
    const allowedLogPath = path.join(root, ".slopdex", "describe-allowed-log.jsonl");
    write(root, "src/rpc.ts", "export function registerRpcEndpoint(name) {\n  return { name };\n}\n");
    write(root, "src/unrelated.ts", "export function helper() {\n  return 1;\n}\n");
    write(root, ".slopdex/config.json", JSON.stringify({ dimensions: 2 }));
    write(root, ".slopdex/mock-api.mjs", `
import { appendFileSync } from 'node:fs';
globalThis.fetch = async (url, options) => {
  const body = JSON.parse(options.body);
  if (url === 'https://api.openai.com/v1/embeddings') {
    return Response.json({data: body.input.map((_, index) => ({index, embedding: [1, 0]}))});
  }
  if (url.endsWith('/responses')) {
    const input = JSON.parse(body.input[body.input.length - 1].content[0].text);
    if (Array.isArray(input.functions)) {
      const filesWithContent = input.files.filter((file) => file.content !== undefined).map((file) => file.path);
      appendFileSync(process.env.SLOPDEX_LOG, JSON.stringify({
        url,
        model: body.model,
        filesWithContent,
        functions: input.functions.map((callable) => callable.qualifiedName),
      }) + '\\n');
      if (filesWithContent.length > 0 && process.env.SLOPDEX_ALLOW_FULL !== '1') {
        return new Response('context too large', {status: 400});
      }
      return Response.json({status: 'completed', output: [{type: 'message', role: 'assistant', id: 'message-1', content: [{
        type: 'output_text', text: 'The existing RPC endpoint registration lives in src/rpc.ts:1.', annotations: []
      }]}]});
    }
    const text = input.request === 'Describe this file overall.' ? 'File purpose' : 'Callable purpose';
    return Response.json({status: 'completed', output: [{type: 'message', role: 'assistant', id: 'message-1', content: [{
      type: 'output_text', text, annotations: []
    }]}]});
  }
  throw new Error('Unexpected API URL: ' + url);
};
`);
    const env = {
      ...process.env,
      NODE_OPTIONS: `--import=${pathToFileURL(path.join(root, ".slopdex", "mock-api.mjs")).href}`,
      SLOPDEX_LOG: logPath,
      OPENAI_API_KEY: "test",
      OPENCODE_API_KEY: "test",
    };
    expect((await runCliWithEnv(root, env, "descriptions", "enable")).status).toBe(0);

    const fallback = await runCliWithEnv(root, env, "describe", "implement a new rpc endpoint");
    expect(fallback.status, fallback.stderr).toBe(0);
    expect(fallback.stdout).toContain("The existing RPC endpoint registration lives in src/rpc.ts:1.");
    expect(fallback.stderr).toContain("Description request failed");
    expect(fallback.stderr).toContain("retrying the description without full file contents");
    const attempts = readFileSync(logPath, "utf8").trim().split("\n").map((line) => JSON.parse(line));
    expect(attempts).toHaveLength(2);
    expect(attempts[0].filesWithContent).toContain("src/rpc.ts");
    expect(attempts[1].filesWithContent).toEqual([]);
    expect(attempts[1].functions).toContain("registerRpcEndpoint");

    const full = await runCliWithEnv(root, { ...env, SLOPDEX_LOG: allowedLogPath, SLOPDEX_ALLOW_FULL: "1" },
      "describe", "implement a new rpc endpoint", "--format", "json");
    expect(full.status, full.stderr).toBe(0);
    expect(full.stderr).not.toContain("retrying the description");
    const parsed = JSON.parse(full.stdout) as {
      description: string;
      files: Array<{ path: string }>;
      functions: Array<{ qualifiedName: string; startLine: number }>;
    };
    expect(parsed.description).toContain("src/rpc.ts:1");
    expect(parsed.files.map((file) => file.path)).toContain("src/rpc.ts");
    expect(parsed.functions).toContainEqual(expect.objectContaining({ qualifiedName: "registerRpcEndpoint", startLine: 1 }));
    const allowed = readFileSync(allowedLogPath, "utf8").trim().split("\n").map((line) => JSON.parse(line));
    expect(allowed).toHaveLength(1);
    expect(allowed[0].filesWithContent).toContain("src/rpc.ts");

    const profileLogPath = path.join(root, ".slopdex", "describe-profile-log.jsonl");
    const enabled = await runCliWithEnv(root, env,
      "descriptions", "enable", "--description-provider", "opencode-go", "--description-model", "custom-guide-model");
    expect(enabled.status, enabled.stderr).toBe(0);
    const profiled = await runCliWithEnv(root, { ...env, SLOPDEX_LOG: profileLogPath, SLOPDEX_ALLOW_FULL: "1" },
      "describe", "implement a new rpc endpoint");
    expect(profiled.status, profiled.stderr).toBe(0);
    const profiledRequests = readFileSync(profileLogPath, "utf8").trim().split("\n").map((line) => JSON.parse(line));
    expect(profiledRequests).toHaveLength(1);
    expect(profiledRequests[0]).toMatchObject({
      url: "https://opencode.ai/zen/go/v1/responses",
      model: "custom-guide-model",
    });
  });

  it("validates describe arguments before opening an index", async () => {
    const root = temporaryRoot();
    const missing = await runCli(root, "describe");
    expect(missing.status).toBe(2);
    expect(missing.stderr).toContain("describe requires a query");
    expect(existsSync(path.join(root, ".slopdex/index.sqlite"))).toBe(false);

    const misplaced = await runCli(root, "search", "query", "--describe-full-file-threshold", "0.5");
    expect(misplaced.status).toBe(2);
    expect(misplaced.stderr).toContain("--describe-full-file-threshold is only available for describe");
    expect(existsSync(path.join(root, ".slopdex/index.sqlite"))).toBe(false);

    const invalidThreshold = await runCli(root, "describe", "query", "--describe-full-file-threshold", "high");
    expect(invalidThreshold.status).toBe(2);
    expect(invalidThreshold.stderr).toContain("describe-full-file-threshold must be a number");
    expect(existsSync(path.join(root, ".slopdex/index.sqlite"))).toBe(false);
  });
});
