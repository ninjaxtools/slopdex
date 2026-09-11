import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { CodeIndex } from "../src/code-index.js";
import { OpenAIEmbeddingProvider } from "../src/embeddings/openai.js";
import { readIndexErrors } from "../src/storage/database.js";
import { temporaryRoot, write } from "./helpers.js";

function run(root: string, ...args: string[]) {
  return spawnSync(process.execPath, ["--import", "tsx", "src/cli.ts", ...args, "--root", root], {
    cwd: path.resolve(import.meta.dirname, ".."), encoding: "utf8",
    env: { ...process.env, OPENAI_API_KEY: args.includes("index-errors") ? "" : "test", JINA_API_KEY: "" },
  });
}

async function brokenIndex(root: string): Promise<string> {
  write(root, "broken.ts", "function broken( {\n");
  write(root, ".slopdex/config.json", JSON.stringify({ dimensions: 2 }));
  const profile = new OpenAIEmbeddingProvider({ dimensions: 2, apiKey: "test" }).profile;
  const index = new CodeIndex({
    rootDir: root, onWarning: () => {},
    provider: { profile, embedDocuments: async (inputs) => inputs.map(() => [1, 0]), embedQuery: async () => [1, 0] },
  });
  try {
    await index.updateFromWorkingTree();
    return index.indexPath;
  } finally {
    index.close();
  }
}

describe("CLI indexing diagnostics", () => {
  it("lists persisted errors offline, warns on each invocation, and silences warnings without discarding errors", async () => {
    const root = temporaryRoot();
    const indexPath = await brokenIndex(root);
    const stored = readIndexErrors(indexPath);
    expect(stored.length).toBeGreaterThan(0);
    const json = run(root, "index-errors");
    expect(json.status, json.stderr).toBe(0);
    expect(JSON.parse(json.stdout)).toEqual(stored);
    expect(json.stderr).toContain("unresolved indexing error(s)");
    expect(json.stderr).toContain("1 file(s)");
    const summary = run(root, "index-errors", "--format", "summary", "--ignore-errors");
    expect(summary.status).toBe(0);
    expect(summary.stdout).toContain("broken.ts:1:1");
    expect(summary.stdout).toContain(":: broken");
    expect(summary.stderr).toBe("");
    expect(readIndexErrors(indexPath)).toEqual(stored);
    for (const args of [["status", "--no-reindex"], ["cross-search", "--no-reindex"], ["--help"]]) {
      const result = run(root, ...args);
      expect(result.status, result.stderr).toBe(0);
      expect(result.stderr).toContain("unresolved indexing error(s)");
    }
    const silenced = run(root, "status", "--no-reindex", "--ignore-errors");
    expect(silenced.status).toBe(0);
    expect(JSON.parse(silenced.stdout)).toMatchObject({ indexingErrorCount: stored.length, failedFileCount: 1 });
    expect(silenced.stderr).not.toContain("unresolved indexing error(s)");
    // No callable remains, so fixing the source makes no embedding requests.
    write(root, "broken.ts", "const ready = true;\n");
    const fixed = run(root, "status");
    expect(fixed.status, fixed.stderr).toBe(0);
    expect(fixed.stderr).not.toContain("unresolved indexing error(s)");
    expect(JSON.parse(fixed.stdout)).toMatchObject({ indexingErrorCount: 0, failedFileCount: 0 });
  });

  it("continues a fresh invocation with a fully unparseable file and persists the failure", () => {
    const root = temporaryRoot();
    write(root, "broken.py", "def broken(\n");
    const result = run(root, "status");
    expect(result.status, result.stderr).toBe(0);
    expect(JSON.parse(result.stdout)).toMatchObject({ fileCount: 1, functionCount: 0, failedFileCount: 1 });
    expect(result.stderr).toContain("unresolved indexing error(s)");
    expect(JSON.parse(run(root, "index-errors", "--ignore-errors").stdout).some((error: { qualifiedName: string | null }) => error.qualifiedName === "broken")).toBe(true);
    const quiet = run(root, "status", "--ignore-errors");
    expect(quiet.status).toBe(0);
    expect(quiet.stderr).not.toMatch(/unresolved indexing|Cannot fully parse|Cannot index/);
    expect(JSON.parse(quiet.stdout).failedFileCount).toBe(1);
  });

  it("reports source and target diagnostics for cross-index search", async () => {
    const root = temporaryRoot();
    const targetRoot = temporaryRoot();
    await brokenIndex(root);
    const targetPath = await brokenIndex(targetRoot);
    const result = run(root, "cross-search", "--no-reindex", "--target-root", targetRoot, "--target-index", targetPath);
    expect(result.status, result.stderr).toBe(0);
    expect(result.stderr.match(/unresolved indexing error\(s\)/g)).toHaveLength(2);
    expect(result.stderr).toContain(targetPath);
  });

  it("does not create an index just to inspect errors", () => {
    const root = temporaryRoot();
    const result = run(root, "index-errors");
    expect(result.status).toBe(0);
    expect(JSON.parse(result.stdout)).toEqual([]);
    expect(result.stderr).toBe("");
    expect(existsSync(path.join(root, ".slopdex/index.sqlite"))).toBe(false);
  });
});
