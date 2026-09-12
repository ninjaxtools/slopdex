import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { pathToFileURL } from "node:url";

import { describe, expect, it } from "vitest";

import { CodeIndex } from "../src/code-index.js";
import { OpenAIEmbeddingProvider } from "../src/embeddings/openai.js";
import { commitAll, initGit, temporaryRoot, write } from "./helpers.js";

const projectRoot = path.resolve(import.meta.dirname, "..");

function runCli(root: string, ...args: string[]) {
  return runCliWithEnv(root, process.env, ...args);
}

function runCliWithEnv(root: string, env: NodeJS.ProcessEnv, ...args: string[]) {
  return spawnSync(process.execPath, ["--import", "tsx", "src/cli.ts", ...args, "--root", root], {
    cwd: projectRoot,
    encoding: "utf8",
    env: { ...env, OPENAI_API_KEY: "test" },
  });
}

describe("CLI index initialization", () => {
  it("initializes a missing index from HEAD and prints a notice", () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Example\n");
    const head = commitAll(root, "initial");

    const result = runCli(root, "status");

    expect(result.status).toBe(0);
    expect(result.stderr).toContain("index not found");
    expect(result.stderr).toContain("initializing automatically from HEAD and the working tree");
    expect(JSON.parse(result.stdout)).toMatchObject({ gitCheckpoint: head, fileCount: 0, functionCount: 0 });
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(true);
  });

  it("initializes a missing index with uncommitted non-source changes", () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Example\n");
    commitAll(root, "initial");
    write(root, "README.md", "# Changed locally\n");
    write(root, "notes.md", "Untracked notes\n");

    const result = runCli(root, "status");

    expect(result.status).toBe(0);
    expect(result.stderr).toContain("index not found");
    expect(JSON.parse(result.stdout)).toMatchObject({ fileCount: 0, functionCount: 0 });
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(true);
  });

  it("retains a partial initialization cache and resumes without repeating completed API calls", () => {
    const root = temporaryRoot();
    const statePath = path.join(root, ".slopdex", "api-state.json");
    write(root, "functions.ts", "export function one() { return 1; }\nexport function two() { return 2; }\n");
    write(root, ".slopdex/config.json", JSON.stringify({ dimensions: 2, embeddingBatchSize: 1 }));
    write(root, ".slopdex/mock-api.mjs", `
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
globalThis.fetch = async (_url, options) => {
  const body = JSON.parse(options.body);
  const statePath = process.env.SLOPDEX_STATE_PATH;
  const calls = existsSync(statePath) ? JSON.parse(readFileSync(statePath, 'utf8')) : [];
  calls.push(body.input[0]);
  writeFileSync(statePath, JSON.stringify(calls));
  if (calls.length === 2) return new Response('intentional failure', {status: 400});
  return Response.json({data: [{index: 0, embedding: [1, 0]}]});
};
`);
    const env = {
      ...process.env,
      NODE_OPTIONS: `--import=${pathToFileURL(path.join(root, ".slopdex/mock-api.mjs")).href}`,
      SLOPDEX_STATE_PATH: statePath,
    };

    const failed = runCliWithEnv(root, env, "status");
    expect(failed.status).not.toBe(0);
    expect(failed.stderr).toContain("intentional failure");
    expect(existsSync(path.join(root, ".slopdex/index.sqlite"))).toBe(true);

    const resumed = runCliWithEnv(root, env, "status");
    expect(resumed.status, resumed.stderr).toBe(0);
    const calls = JSON.parse(readFileSync(statePath, "utf8")) as string[];
    expect(calls.filter((input) => input.includes("symbol: one"))).toHaveLength(1);
    expect(calls.filter((input) => input.includes("symbol: two"))).toHaveLength(2);
  });

  it("initializes a missing cross-search target index", () => {
    const sourceRoot = temporaryRoot();
    const targetRoot = temporaryRoot();
    for (const root of [sourceRoot, targetRoot]) {
      initGit(root);
      write(root, "README.md", "# Example\n");
      commitAll(root, "initial");
    }
    const targetIndex = path.join(targetRoot, ".slopdex", "target.sqlite");

    const result = runCli(
      sourceRoot,
      "cross-search",
      "--target-root",
      targetRoot,
      "--target-index",
      targetIndex,
    );

    expect(result.status).toBe(0);
    expect(result.stderr).toContain("target index not found");
    expect(existsSync(targetIndex)).toBe(true);
  });

  it("refreshes a cross-search target with the target repository policy", () => {
    const sourceRoot = temporaryRoot();
    const targetRoot = temporaryRoot();
    initGit(sourceRoot);
    write(sourceRoot, ".slopdex/config.json", JSON.stringify({ include: ["src/**"] }));
    write(sourceRoot, "src/types.ts", "export interface Source {}\n");
    commitAll(sourceRoot, "source");
    initGit(targetRoot);
    const targetConfig = path.join(targetRoot, "config", "slopdex.json");
    write(targetRoot, "config/slopdex.json", JSON.stringify({ include: ["lib/**"] }));
    write(targetRoot, "lib/types.ts", "export interface Target {}\n");
    write(targetRoot, "other/types.ts", "export interface Excluded {}\n");
    commitAll(targetRoot, "target");
    const targetIndex = path.join(targetRoot, ".slopdex", "target.sqlite");

    const result = runCli(
      sourceRoot,
      "cross-search",
      "--target-root",
      targetRoot,
      "--target-index",
      targetIndex,
      "--target-config",
      targetConfig,
    );

    expect(result.status).toBe(0);
    const target = new CodeIndex({
      rootDir: targetRoot,
      indexPath: targetIndex,
      provider: new OpenAIEmbeddingProvider({ apiKey: "test" }),
      readOnly: true,
    });
    expect(target.status().fileCount).toBe(1);
    target.close();
  });

  it("accepts a recursive source path and threshold range", () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Example\n");
    commitAll(root, "initial");

    const result = runCli(root, "cross-search", "--source-path", "src", "--threshold", "0.85-0.95");

    expect(result.status).toBe(0);
    expect(result.stderr).not.toContain("must be a number");
  });

  it("refreshes an existing index before reporting status", () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Initial\n");
    commitAll(root, "initial");
    expect(runCli(root, "status").status).toBe(0);

    write(root, "README.md", "# Updated\n");
    const target = commitAll(root, "updated");
    write(root, "notes.md", "Uncommitted\n");
    const result = runCli(root, "status");

    expect(result.status).toBe(0);
    expect(JSON.parse(result.stdout)).toMatchObject({ gitCheckpoint: target });
  });

  it("indexes the full working tree when the root is not a Git repository", () => {
    const root = temporaryRoot();
    write(root, "src/types.ts", "export interface First {}\n");

    const first = runCli(root, "status");
    expect(first.status).toBe(0);
    expect(first.stderr).toContain("warning: no Git repository is available for index; re-indexing all source files");
    expect(JSON.parse(first.stdout)).toMatchObject({ fileCount: 1, gitCheckpoint: null });

    write(root, "src/more.ts", "export interface Second {}\n");
    const second = runCli(root, "status");
    expect(second.status).toBe(0);
    expect(second.stderr).toContain("re-indexing all source files");
    expect(JSON.parse(second.stdout)).toMatchObject({ fileCount: 2, gitCheckpoint: null });
  });

  it("uses a non-empty filesystem index with --no-reindex", () => {
    const root = temporaryRoot();
    write(root, "src/types.ts", "export interface First {}\n");

    const initial = runCli(root, "status", "--no-reindex");
    expect(initial.status).toBe(0);
    expect(initial.stderr).toContain("re-indexing all source files");
    expect(JSON.parse(initial.stdout)).toMatchObject({ fileCount: 1 });

    write(root, "src/more.ts", "export interface Second {}\n");
    const cached = runCli(root, "status", "--no-reindex");
    expect(cached.status).toBe(0);
    expect(cached.stderr).toContain("full working-tree re-index skipped because --no-reindex was specified");
    expect(JSON.parse(cached.stdout)).toMatchObject({ fileCount: 1, gitCheckpoint: null });
  });

  it("populates an empty filesystem index despite --no-reindex", () => {
    const root = temporaryRoot();
    expect(runCli(root, "status").status).toBe(0);

    write(root, "src/types.ts", "export interface First {}\n");
    const result = runCli(root, "status", "--no-reindex");

    expect(result.status).toBe(0);
    expect(result.stderr).toContain("re-indexing all source files");
    expect(JSON.parse(result.stdout)).toMatchObject({ fileCount: 1, gitCheckpoint: null });
  });

  it("uses only committed files with --no-reindex in a Git repository", () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "tracked.ts", "export interface Tracked {}\n");
    const head = commitAll(root, "base");
    write(root, "untracked.ts", "export interface Untracked {}\n");

    const committedOnly = runCli(root, "status", "--no-reindex");
    expect(committedOnly.status).toBe(0);
    expect(committedOnly.stderr).not.toContain("no Git repository");
    expect(JSON.parse(committedOnly.stdout)).toMatchObject({ fileCount: 1, gitCheckpoint: head });

    expect(runCli(root, "status").status).toBe(0);
    const restored = runCli(root, "status", "--no-reindex");
    expect(restored.status).toBe(0);
    expect(JSON.parse(restored.stdout)).toMatchObject({ fileCount: 1, gitCheckpoint: head });
  });

  it("falls back when the Git executable is unavailable", () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "src/types.ts", "export interface Value {}\n");
    commitAll(root, "base");

    const result = runCliWithEnv(root, { ...process.env, PATH: "" }, "status");

    expect(result.status).toBe(0);
    expect(result.stderr).toContain("warning: no Git repository is available for index; re-indexing all source files");
    expect(JSON.parse(result.stdout)).toMatchObject({ fileCount: 1, gitCheckpoint: null });
  });

  it("does not treat an invalid Git target as an unavailable repository", () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "src/types.ts", "export interface Value {}\n");
    commitAll(root, "base");

    const result = runCli(root, "update-git", "--target", "missing-ref");

    expect(result.status).toBe(2);
    expect(result.stderr).toContain("Git command failed");
    expect(result.stderr).not.toContain("no Git repository is available");
  });

  it("does not hide Git configuration failures behind filesystem fallback", () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "src/types.ts", "export interface Value {}\n");
    commitAll(root, "base");

    const result = runCliWithEnv(root, { ...process.env, GIT_DIR: path.join(root, "missing-git-dir") }, "status");

    expect(result.status).toBe(2);
    expect(result.stderr).toContain("Git command failed");
    expect(result.stderr).not.toContain("no Git repository is available");
  });

  it("force rebuilds an incompatible index and prints a warning", () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Example\n");
    commitAll(root, "initial");
    expect(runCli(root, "status", "--model", "text-embedding-3-small").status).toBe(0);

    const incompatible = runCli(root, "status");
    expect(incompatible.status).toBe(2);
    expect(incompatible.stderr).toContain("Embedding provider, model, dimensions, or strategy differs");

    const rebuilt = runCli(root, "status", "--force-reindex");
    expect(rebuilt.status).toBe(0);
    expect(rebuilt.stderr).toContain("warning: index is incompatible");
    expect(rebuilt.stderr).toContain("rebuilding automatically because --force-reindex was specified");
    expect(JSON.parse(rebuilt.stdout)).toMatchObject({
      embeddingProfile: { provider: "openai", model: "text-embedding-3-large", dimensions: 3072 },
    });
  });

  it("supports clusters, changed-since, and uncommitted filters", () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Initial\n");
    const base = commitAll(root, "initial");

    const clusters = runCli(root, "cross-search", "--format", "clusters", "--changed-since", base, "--cross-file-only");
    expect(clusters.status).toBe(0);
    expect(clusters.stdout).toBe("No clusters.\n");

    const uncommitted = runCli(root, "cross-search", "--cohesion", "--uncommitted", "--changed-since", base, "--source-path", "src", "-e", "keep$", "--format", "json");
    expect(uncommitted.status).toBe(0);
    expect(uncommitted.stdout).toBe("");
  });

  it("filters same-file cross-search matches with --cross-file-only", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "same.ts", `
export function one(value: string) {
  return value.trim();
}
export function two(value: string) {
  return value.trim();
}
`);
    write(root, "other.ts", `export function external(value: string) {
  return value.trim();
}\n`);
    commitAll(root, "functions");
    const openAI = new OpenAIEmbeddingProvider({ apiKey: "test" });
    const vector = () => [1, ...Array<number>(openAI.profile.dimensions - 1).fill(0)];
    const index = new CodeIndex({
      rootDir: root,
      provider: {
        profile: openAI.profile,
        embedDocuments: async (inputs) => inputs.map(vector),
        embedQuery: async () => vector(),
      },
    });
    await index.updateFromGit();
    index.close();

    const result = runCli(
      root,
      "cross-search",
      "--cross-file-only",
      "--include-symmetric-duplicates",
      "--limit",
      "1",
      "--format",
      "json",
    );

    expect(result.status).toBe(0);
    const rows = result.stdout.trim().split("\n").map((line) => JSON.parse(line));
    expect(rows).toHaveLength(3);
    expect(rows.every((row) => row.matches.length === 1)).toBe(true);
    expect(rows.every((row) => row.source.path !== row.matches[0].function.path)).toBe(true);
    expect(rows.every((row) => row.source.lineCount === 3 && row.matches[0].function.lineCount === 3)).toBe(true);

    const defaultFormat = runCli(root, "cross-search");
    expect(defaultFormat.status).toBe(0);
    expect(defaultFormat.stdout).toMatch(/^Cluster 1 /);

    const minimum = runCli(root, "cross-search", "--min-lines", "4");
    expect(minimum.status).toBe(0);
    expect(minimum.stdout).toBe("No clusters.\n");

    const regex = runCli(
      root,
      "cross-search",
      "--regex",
      "^one$",
      "--cross-file-only",
      "--min-lines",
      "3",
      "--limit",
      "1",
      "--threshold",
      "0.8-1.1",
      "--format",
      "json",
    );
    expect(regex.status).toBe(0);
    const regexRow = JSON.parse(regex.stdout);
    expect(regexRow.source.qualifiedName).toBe("one");
    expect(regexRow.matches.map((match: { function: { qualifiedName: string } }) => match.function.qualifiedName)).toEqual(["external"]);

    const sourceRegex = runCli(root, "cross-search", "-e", "^one$", "--source-path", "same.ts",
      "--cross-file-only", "--min-lines", "3", "--limit", "1", "--threshold", "0.8-1.1", "--format", "json");
    expect(sourceRegex.status).toBe(0);
    const selected = JSON.parse(sourceRegex.stdout);
    expect(selected.source.qualifiedName).toBe("one");
    expect(selected.matches.map((match: { function: { qualifiedName: string } }) => match.function.qualifiedName)).toEqual(["external"]);
    expect(runCli(root, "cross-search", "--regex", "^one$", "--source-path", "same.ts",
      "--cross-file-only", "--min-lines", "3", "--limit", "1", "--threshold", "0.8-1.1", "--format", "json").stdout).toBe(sourceRegex.stdout);
    const empty = runCli(root, "cross-search", "--regexp", "^missing$", "--format", "json");
    expect(empty.status).toBe(0);
    expect(empty.stdout).toBe("");
  });

  it("re-ranks cross-search matches by physical distance", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "src/one.ts", `export function one() {
  return 1;
}\n`);
    write(root, "src/two.ts", `export function two() {
  return 2;
}\n`);
    write(root, "packages/feature/three.ts", `export function three() {
  return 3;
}\n`);
    commitAll(root, "functions");
    const openAI = new OpenAIEmbeddingProvider({ apiKey: "test" });
    const vector = () => [1, ...Array<number>(openAI.profile.dimensions - 1).fill(0)];
    const index = new CodeIndex({
      rootDir: root,
      provider: {
        profile: openAI.profile,
        embedDocuments: async (inputs) => inputs.map(vector),
        embedQuery: async () => vector(),
      },
    });
    await index.updateFromGit();
    index.close();

    const summary = runCli(root, "cross-search", "--cohesion", "--include-symmetric-duplicates", "--limit", "2");
    expect(summary.status).toBe(0);
    expect(summary.stdout).toContain("[distance 4]");
    expect(summary.stdout.indexOf("packages/feature/three.ts")).toBeLessThan(summary.stdout.indexOf("src/two.ts"));

    const json = runCli(root, "cross-search", "--cohesion", "--include-symmetric-duplicates", "--limit", "2", "--format", "json");
    expect(json.status).toBe(0);
    const rows = json.stdout.trim().split("\n").map((line) => JSON.parse(line));
    const one = rows.find((row) => row.source.qualifiedName === "one");
    expect(one.matches.map((match: { function: { qualifiedName: string } }) => match.function.qualifiedName)).toEqual(["three", "two"]);
    expect(one.matches.map((match: { physicalDistance: number }) => match.physicalDistance)).toEqual([4, 1]);

    const clusters = runCli(root, "cross-search", "--cohesion", "--format", "clusters");
    expect(clusters.status).toBe(2);
    expect(clusters.stderr).toContain("does not preserve cohesion match order");

    const removed = runCli(root, "cohesion");
    expect(removed.status).toBe(2);
    expect(removed.stderr).toContain("Unknown command: cohesion");
  });

  it.each(["--min-similarity", "--added-since"])("rejects removed option %s cleanly", (option) => {
    const result = runCli("/", "cross-search", option, "0.8");
    expect(result.status).toBe(2);
    expect(result.stderr).toMatch(/^slopdex: Unknown option/);
    expect(result.stderr).not.toContain("node:internal");
  });

  it("validates options before creating or refreshing an index", () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Initial\n");
    commitAll(root, "initial");

    const invalidRegex = runCli(root, "cross-search", "--regex", "[");
    expect(invalidRegex.status).toBe(2);
    expect(invalidRegex.stderr).toContain("Invalid --regex value");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const conflictingAliases = runCli(root, "search", "query", "-e", "one", "--regex", "two");
    expect(conflictingAliases.status).toBe(2);
    expect(conflictingAliases.stderr).toContain("are aliases and cannot use different values");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const emptyRange = runCli(root, "cross-search", "--threshold", "0.9-0.9");
    expect(emptyRange.status).toBe(2);
    expect(emptyRange.stderr).toContain("minimum must be less than its maximum");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    for (const command of ["cross-search", "search", "search-description"]) {
      for (const option of ["-e", "--regex"]) {
        const invalidSourceRegex = runCli(root, command, ...(command.startsWith("search") ? ["query"] : []), option, "[");
        expect(invalidSourceRegex.status).toBe(2);
        expect(invalidSourceRegex.stderr).toContain(option === "--regex" ? "Invalid --regex value" : "Invalid -e/--regexp value");
        expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);
      }
    }

    const invalidDescriptionProvider = runCli(root, "status", "--description-provider", "unknown");
    expect(invalidDescriptionProvider.status).toBe(2);
    expect(invalidDescriptionProvider.stderr).toContain("Unsupported description provider: unknown");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

  });
});

describe("CLI help", () => {
  it("prints the package version", () => {
    const { version } = JSON.parse(readFileSync(path.join(projectRoot, "package.json"), "utf8")) as { version: string };
    const result = runCli("/", "--version");

    expect(result.status).toBe(0);
    expect(result.stdout).toBe(`${version}\n`);
    expect(result.stderr).toBe("");
  });

  it("includes an example for every command", () => {
    const result = runCli("/", "--help");

    expect(result.status).toBe(0);
    for (const example of [
      "slopdex status",
      "slopdex models opencode-go",
      "slopdex config model opencode-go/gpt-5.6-luna",
      "slopdex config descriptions enable",
      "slopdex config reranker cohere",
      "slopdex update-files",
      "slopdex reindex-files",
      "slopdex delete-files",
      "slopdex update-git",
      "slopdex search",
      "slopdex descriptions enable",
      "slopdex search-description",
      "slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9 --limit 5",
      "slopdex cross-search --cohesion --threshold 0.8 --limit 20 --format summary",
    ]) expect(result.stdout).toContain(example);
    expect(result.stdout).toContain("--source-path <path>");
    expect(result.stdout).toContain("--description-provider <name>");
    expect(result.stdout).toContain("--format <json|summary|clusters>");
    expect(result.stdout).toContain("--changed-since <commit>");
    expect(result.stdout).toContain("--uncommitted");
    expect(result.stdout).toContain("--target-config <path>");
    expect(result.stdout).toContain("--force-reindex");
    expect(result.stdout).toContain("--no-reindex");
    expect(result.stdout).toContain("--cross-file-only");
    expect(result.stdout).toContain("--min-lines <number>");
    expect(result.stdout).toContain("--regex <regex>");
    expect(result.stdout).toContain("-e, --regexp <regex>");
    expect(result.stdout).toContain("--cohesion");
    expect(result.stdout).toContain("--version");
    expect(result.stdout).toContain("Cluster 1 (3 functions, similarity 0.9124-0.9568)");
    expect(result.stdout).toContain("Review them for repeated validation or session logic that could be shared");
    expect(result.stdout).toContain("transitive links, so every function need not directly match every other function");
    expect(result.stdout).toContain("[distance 4]");
    expect(result.stdout).toContain("orders each source's matches");
    expect(result.stdout).toContain("Reading Analysis Output:");
    expect(result.stdout).toContain("Greater distance first; similarity breaks ties");
    expect(result.stdout.indexOf("Commands:")).toBeLessThan(result.stdout.indexOf("Analysis Examples:"));
    expect(result.stdout.indexOf("Analysis Examples:")).toBeLessThan(result.stdout.indexOf("Reading Analysis Output:"));
    expect(result.stdout.indexOf("Reading Analysis Output:")).toBeLessThan(result.stdout.indexOf("Options:"));
    expect(result.stdout.indexOf("Options:")).toBeLessThan(result.stdout.indexOf("Other Examples:"));
    expect(result.stdout).not.toContain("--min-similarity");
    expect(result.stdout).not.toContain("--added-since");
  });
});

describe("CLI model configuration", () => {
  it("lists published models and validates index-free config changes", () => {
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
    return Response.json({data: [{id: 'gpt-5.6-luna'}, {id: 'shared-model'}]});
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

    const listed = run("models", "opencode", "--format", "json");
    expect(listed.status, listed.stderr).toBe(0);
    expect(JSON.parse(listed.stdout)).toEqual([
      { provider: "opencode", model: "gpt-5.6-sol" },
      { provider: "opencode", model: "shared-model" },
    ]);
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const ambiguous = run("config", "model", "shared-model");
    expect(ambiguous.status).toBe(2);
    expect(ambiguous.stderr).toContain("available from multiple providers");
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toEqual({ dimensions: 2, exclude: ["fixtures/**"] });

    const automatic = run("config", "model", "gpt-5.6-sol");
    expect(automatic.status, automatic.stderr).toBe(0);
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toMatchObject({
      descriptionProvider: "opencode",
      descriptionModel: "gpt-5.6-sol",
      exclude: ["fixtures/**"],
    });

    const qualified = run("config", "model", "opencode-go/gpt-5.6-luna", "--format", "json");
    expect(qualified.status, qualified.stderr).toBe(0);
    expect(JSON.parse(qualified.stdout)).toMatchObject({
      configPath,
      descriptionProvider: "opencode-go",
      descriptionModel: "gpt-5.6-luna",
    });

    const invalid = run("config", "model", "opencode-go/not-published");
    expect(invalid.status).toBe(2);
    expect(invalid.stderr).toContain("Unknown opencode-go model");
    expect(JSON.parse(readFileSync(configPath, "utf8")).descriptionModel).toBe("gpt-5.6-luna");

    const enabled = run("config", "descriptions", "enable");
    expect(enabled.status, enabled.stderr).toBe(0);
    expect(JSON.parse(readFileSync(configPath, "utf8")).descriptionsEnabled).toBe(true);
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const status = run("status");
    expect(status.status, status.stderr).toBe(0);
    expect(JSON.parse(status.stdout)).toMatchObject({
      descriptionsEnabled: true,
      descriptionCount: 1,
      descriptionProfile: { provider: "opencode-go", model: "gpt-5.6-luna" },
    });

    expect(run("config", "descriptions", "disable").status).toBe(0);
    const disabled = run("status");
    expect(disabled.status, disabled.stderr).toBe(0);
    expect(JSON.parse(disabled.stdout).descriptionsEnabled).toBe(false);
  });
});

describe("CLI reranker configuration", () => {
  it("enables, uses, changes, and disables hosted reranking without opening an index during config", () => {
    const root = temporaryRoot();
    const configPath = path.join(root, ".slopdex", "config.json");
    write(root, "functions.ts", [
      "export function one() { return 1; }",
      "export function two() { return 2; }",
    ].join("\n"));
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
  throw new Error('Unexpected API URL: ' + url);
};
`);
    const env = {
      ...process.env,
      NODE_OPTIONS: `--import=${pathToFileURL(path.join(root, ".slopdex/mock-api.mjs")).href}`,
      COHERE_API_KEY: "test",
    };
    const run = (...args: string[]) => runCliWithEnv(root, env, ...args);

    const enabled = run("config", "reranker", "cohere", "rerank-v4.0-fast", "--format", "json");
    expect(enabled.status, enabled.stderr).toBe(0);
    expect(JSON.parse(enabled.stdout)).toMatchObject({
      configPath,
      rerankingEnabled: true,
      rerankerProvider: "cohere",
      rerankerModel: "rerank-v4.0-fast",
    });
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const search = run("search", "find two", "--limit", "1", "--format", "json");
    expect(search.status, search.stderr).toBe(0);
    expect(JSON.parse(search.stdout)).toMatchObject([
      { similarity: 1, rerankScore: 0.95, function: { name: "two" } },
    ]);

    const jina = run("config", "reranker", "jina");
    expect(jina.status, jina.stderr).toBe(0);
    expect(JSON.parse(readFileSync(configPath, "utf8"))).toMatchObject({
      rerankingEnabled: true,
      rerankerProvider: "jina",
      rerankerModel: "jina-reranker-v3.5",
    });
    const disabled = run("config", "reranker", "disable");
    expect(disabled.status, disabled.stderr).toBe(0);
    expect(JSON.parse(readFileSync(configPath, "utf8")).rerankingEnabled).toBe(false);
  });

  it("rejects invalid reranker configuration before creating an index", () => {
    const root = temporaryRoot();
    const invalid = runCli(root, "config", "reranker", "unknown");
    expect(invalid.status).toBe(2);
    expect(invalid.stderr).toContain("requires cohere, jina, or disable");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    write(root, ".slopdex/config.json", JSON.stringify({ rerankingEnabled: true, rerankerProvider: "cohere", rerankerModel: 3 }));
    const malformed = runCli(root, "status");
    expect(malformed.status).toBe(2);
    expect(malformed.stderr).toContain("rerankerModel must be a non-empty string");

    const disabled = runCli(root, "config", "reranker", "disable");
    expect(disabled.status, disabled.stderr).toBe(0);
    expect(runCli(root, "status").status).toBe(0);
  });
});

describe("CLI descriptions", () => {
  it("initializes, searches, updates, changes models, and preserves description mode through a rebuild", () => {
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
    const initialized = run("descriptions", "enable");
    expect(initialized.status, initialized.stderr).toBe(0);
    expect(JSON.parse(initialized.stdout)).toEqual({ descriptionsCreated: 1, fileDescriptionsCreated: 1, descriptionsEnabled: true });
    expect(JSON.parse(run("status").stdout).descriptionProfile).toMatchObject({
      provider: "opencode-go",
      model: "gpt-5.6-luna",
    });
    const old = new DatabaseSync(path.join(root, ".slopdex/index.sqlite"));
    old.prepare("UPDATE metadata SET value = ? WHERE key = 'description_profile'").run(JSON.stringify({
      provider: "opencode-go", model: "gpt-5.6-luna", strategyVersion: "callable-purpose-v1",
    }));
    old.exec("DELETE FROM description_cache;");
    old.close();
    const migrated = run("descriptions", "enable");
    expect(migrated.status, migrated.stderr).toBe(0);
    expect(JSON.parse(migrated.stdout).descriptionsCreated).toBe(1);
    expect(JSON.parse(run("status").stdout).descriptionProfile.strategyVersion).toBe("callable-purpose-v2");
    expect(JSON.parse(run("descriptions", "enable").stdout).descriptionsCreated).toBe(0);

    const disabled = run("descriptions", "disable");
    expect(disabled.status, disabled.stderr).toBe(0);
    expect(JSON.parse(disabled.stdout)).toEqual({ descriptionsCreated: 0, fileDescriptionsCreated: 0, descriptionsEnabled: false });
    expect(JSON.parse(run("status").stdout)).toMatchObject({ descriptionCount: 0, descriptionsEnabled: false });
    expect(run("search-description", "workflow").stderr).toContain("run descriptions enable first");
    expect(JSON.parse(run("descriptions", "enable").stdout)).toEqual({ descriptionsCreated: 0, fileDescriptionsCreated: 0, descriptionsEnabled: true });

    const search = run("search-description", "workflow", "--threshold", "0.9", "--limit", "1", "--format", "json");
    expect(search.status, search.stderr).toBe(0);
    expect(JSON.parse(search.stdout)[0].function.description).toBe("Purpose of one using gpt-5.6-luna");
    expect(JSON.parse(search.stdout)[0].function).not.toHaveProperty("descriptionEmbeddingId");
    const text = run("search-description", "workflow");
    expect(text.stdout).toContain("Purpose of one using gpt-5.6-luna");

    write(root, "src/b.ts", "export function two() { return 2; }\n");
    const updated = run("status");
    expect(updated.status, updated.stderr).toBe(0);
    expect(JSON.parse(updated.stdout)).toMatchObject({ functionCount: 2, descriptionCount: 2, descriptionsEnabled: true });
    for (const command of ["search", "search-description"]) {
      const filtered = run(command, "workflow", "-e", "^two$", "--limit", "1", "--format", "json");
      expect(filtered.status, filtered.stderr).toBe(0);
      expect(JSON.parse(filtered.stdout).map((match: { function: { name: string } }) => match.function.name)).toEqual(["two"]);
    }
    const changedModel = run(
      "descriptions", "enable",
      "--description-provider", "opencode",
      "--description-model", "custom-description-model",
    );
    expect(changedModel.status, changedModel.stderr).toBe(0);
    expect(JSON.parse(changedModel.stdout).descriptionsCreated).toBe(2);
    write(root, ".slopdex/config.json", JSON.stringify({
      dimensions: 2,
      descriptionProvider: "opencode",
      descriptionModel: "custom-description-model",
    }));
    expect(JSON.parse(run("status").stdout).descriptionProfile).toMatchObject({
      provider: "opencode",
      model: "custom-description-model",
    });

    const cross = run("cross-search", "--min-lines", "1", "--format", "json");
    expect(cross.status, cross.stderr).toBe(0);
    const crossRow = JSON.parse(cross.stdout.trim());
    expect(crossRow.matches[0]).toMatchObject({ similarity: 1, codeSimilarity: 1, descriptionSimilarity: 1 });
    expect(crossRow.scoring).toMatchObject({
      similarityMode: "code-description-file-average",
      similarityWeights: { code: 1 / 3, description: 1 / 3, fileDescription: 1 / 3 },
      sourceDescriptionProfile: { model: "custom-description-model" }, targetDescriptionProfile: { model: "custom-description-model" },
    });
    expect(run("cross-search", "--min-lines", "1").stdout).toContain("combined code + callable description + file description");
    const cohesion = run("cross-search", "--cohesion", "--min-lines", "1", "--format", "json");
    expect(cohesion.status, cohesion.stderr).toBe(0);
    const report = JSON.parse(cohesion.stdout.trim());
    expect(report.matches[0]).toMatchObject({ similarity: 1, codeSimilarity: 1, descriptionSimilarity: 1, physicalDistance: 1 });
    expect(report.scoring).toMatchObject({
      similarityMode: "code-description-file-average",
      similarityWeights: { code: 1 / 3, description: 1 / 3, fileDescription: 1 / 3 },
    });

    const rebuilt = run("status", "--model", "text-embedding-3-small", "--force-reindex");
    expect(rebuilt.status, rebuilt.stderr).toBe(0);
    expect(JSON.parse(rebuilt.stdout)).toMatchObject({
      functionCount: 2, descriptionCount: 2, descriptionsEnabled: true,
      descriptionProfile: { provider: "opencode", model: "custom-description-model" },
    });
  }, 30_000);

  it("reindexes stale file descriptions and optionally callable descriptions", () => {
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
    expect(run("descriptions", "enable").status).toBe(0);

    write(root, "src/a.ts", "export function one() { return 2; }\n");
    const filesOnly = run("reindex-files");
    expect(filesOnly.status, filesOnly.stderr).toBe(0);
    expect(JSON.parse(filesOnly.stdout)).toEqual({
      filesReindexed: 1, fileDescriptionsCreated: 1, descriptionsCreated: 0,
    });

    write(root, "src/a.ts", "export function one() { return 3; }\n");
    const withCallables = run("reindex-files", "--callables");
    expect(withCallables.status, withCallables.stderr).toBe(0);
    expect(JSON.parse(withCallables.stdout)).toEqual({
      filesReindexed: 1, fileDescriptionsCreated: 1, descriptionsCreated: 1,
    });
  });

  it("validates description search arguments and explains how to enable descriptions", () => {
    const root = temporaryRoot();
    const missingQuery = runCli(root, "search-description");
    expect(missingQuery.status).toBe(2);
    expect(missingQuery.stderr).toContain("search-description requires a query");
    expect(existsSync(path.join(root, ".slopdex/index.sqlite"))).toBe(false);
    const disabled = runCli(root, "search-description", "workflow");
    expect(disabled.status).toBe(2);
    expect(disabled.stderr).toContain("run descriptions enable first");

    for (const args of [["descriptions"], ["descriptions", "maybe"], ["descriptions", "enable", "extra"]]) {
      const invalid = runCli(root, ...args);
      expect(invalid.status).toBe(2);
      expect(invalid.stderr).toContain("descriptions requires enable or disable");
    }
    const invalidReindex = runCli(root, "reindex-files", "extra");
    expect(invalidReindex.status).toBe(2);
    expect(invalidReindex.stderr).toContain("does not accept positional arguments");
  });
});
