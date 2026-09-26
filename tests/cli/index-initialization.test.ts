import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

import { describe, expect, it } from "vitest";

import { CodeIndex } from "../../src/code-index.js";
import { OpenAIEmbeddingProvider } from "../../src/embeddings/openai.js";
import { commitAll, initGit, temporaryRoot, write } from "../helpers.js";
import { runCli, runCliWithEnv, testTimeoutMs } from "./helpers.js";

describe("CLI index initialization", { timeout: testTimeoutMs }, () => {
  it("initializes a missing index from HEAD and prints a notice", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Example\n");
    const head = commitAll(root, "initial");

    const result = await runCli(root, "status");

    expect(result.status).toBe(0);
    expect(result.stderr).toContain("index not found");
    expect(result.stderr).toContain("initializing automatically from HEAD and the working tree");
    expect(JSON.parse(result.stdout)).toMatchObject({ gitCheckpoint: head, fileCount: 1, functionCount: 0, markdownChunkCount: 0 });
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(true);
  });

  it("initializes a missing index with uncommitted non-source changes", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Example\n");
    commitAll(root, "initial");
    write(root, "README.md", "# Changed locally\n");
    write(root, "notes.txt", "Untracked notes\n");

    const result = await runCli(root, "status");

    expect(result.status).toBe(0);
    expect(result.stderr).toContain("index not found");
    expect(JSON.parse(result.stdout)).toMatchObject({ fileCount: 1, functionCount: 0, markdownChunkCount: 0 });
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(true);
  });

  it.each([
    { name: "the --verbose flag", configVerbose: false, args: ["--verbose"] },
    { name: "verbose config", configVerbose: true, args: [] },
  ])("logs every external request with $name", async ({ configVerbose, args }) => {
    const root = temporaryRoot();
    write(root, "functions.ts", "export function one() { return 1; }\nexport function two() { return 2; }\n");
    write(root, ".slopdex/config.json", JSON.stringify({
      dimensions: 2,
      embeddingBatchSize: 1,
      parallelism: 3,
      verbose: configVerbose,
    }));
    write(root, ".slopdex/mock-api.mjs", `
globalThis.fetch = async (_url, options) => {
  const body = JSON.parse(options.body);
  return Response.json({data: body.input.map((_, index) => ({index, embedding: [1, 0]}))});
};
`);
    const result = await runCliWithEnv(root, {
      ...process.env,
      NODE_OPTIONS: `--import=${pathToFileURL(path.join(root, ".slopdex/mock-api.mjs")).href}`,
    }, "status", ...args);

    expect(result.status, result.stderr).toBe(0);
    expect(result.stderr.match(/external model call: kind=vectors/g)).toHaveLength(2);
    expect(result.stderr).toContain('provider="openai" model="text-embedding-3-large" parallelism=3');
  });

  it("retains a partial initialization cache and resumes without repeating completed API calls", async () => {
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

    const failed = await runCliWithEnv(root, env, "status");
    expect(failed.status).not.toBe(0);
    expect(failed.stderr).toContain("intentional failure");
    expect(existsSync(path.join(root, ".slopdex/index.sqlite"))).toBe(true);

    const resumed = await runCliWithEnv(root, env, "status");
    expect(resumed.status, resumed.stderr).toBe(0);
    const calls = JSON.parse(readFileSync(statePath, "utf8")) as string[];
    expect(calls.filter((input) => input.includes("symbol: one"))).toHaveLength(1);
    expect(calls.filter((input) => input.includes("symbol: two"))).toHaveLength(2);
  });

  it("initializes a missing cross-search target index", async () => {
    const sourceRoot = temporaryRoot();
    const targetRoot = temporaryRoot();
    for (const root of [sourceRoot, targetRoot]) {
      initGit(root);
      write(root, "README.md", "# Example\n");
      commitAll(root, "initial");
    }
    const targetIndex = path.join(targetRoot, ".slopdex", "target.sqlite");

    const result = await runCli(
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

  it("refreshes a cross-search target with the target repository policy", async () => {
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

    const result = await runCli(
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

  it("accepts a recursive source path and threshold range", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Example\n");
    commitAll(root, "initial");

    const result = await runCli(root, "cross-search", "--source-path", "src", "--threshold", "0.85-0.95");

    expect(result.status).toBe(0);
    expect(result.stderr).not.toContain("must be a number");
  });

  it("refreshes an existing index before reporting status", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Initial\n");
    commitAll(root, "initial");
    expect((await runCli(root, "status")).status).toBe(0);

    write(root, "README.md", "# Updated\n");
    const target = commitAll(root, "updated");
    write(root, "notes.txt", "Uncommitted\n");
    const result = await runCli(root, "status");

    expect(result.status).toBe(0);
    expect(JSON.parse(result.stdout)).toMatchObject({ gitCheckpoint: target });
  });

  it("indexes the full working tree when the root is not a Git repository", async () => {
    const root = temporaryRoot();
    write(root, "src/types.ts", "export interface First {}\n");

    const first = await runCli(root, "status");
    expect(first.status).toBe(0);
    expect(first.stderr).toContain("warning: no Git repository is available for index; re-indexing all source files");
    expect(JSON.parse(first.stdout)).toMatchObject({ fileCount: 1, gitCheckpoint: null });

    write(root, "src/more.ts", "export interface Second {}\n");
    const second = await runCli(root, "status");
    expect(second.status).toBe(0);
    expect(second.stderr).toContain("re-indexing all source files");
    expect(JSON.parse(second.stdout)).toMatchObject({ fileCount: 2, gitCheckpoint: null });
  });

  it("uses a non-empty filesystem index with --no-reindex", async () => {
    const root = temporaryRoot();
    write(root, "src/types.ts", "export interface First {}\n");

    const initial = await runCli(root, "status", "--no-reindex");
    expect(initial.status).toBe(0);
    expect(initial.stderr).toContain("re-indexing all source files");
    expect(JSON.parse(initial.stdout)).toMatchObject({ fileCount: 1 });

    write(root, "src/more.ts", "export interface Second {}\n");
    const cached = await runCli(root, "status", "--no-reindex");
    expect(cached.status).toBe(0);
    expect(cached.stderr).toContain("full working-tree re-index skipped because --no-reindex was specified");
    expect(JSON.parse(cached.stdout)).toMatchObject({ fileCount: 1, gitCheckpoint: null });
  });

  it("populates an empty filesystem index despite --no-reindex", async () => {
    const root = temporaryRoot();
    expect((await runCli(root, "status")).status).toBe(0);

    write(root, "src/types.ts", "export interface First {}\n");
    const result = await runCli(root, "status", "--no-reindex");

    expect(result.status).toBe(0);
    expect(result.stderr).toContain("re-indexing all source files");
    expect(JSON.parse(result.stdout)).toMatchObject({ fileCount: 1, gitCheckpoint: null });
  });

  it("uses only committed files with --no-reindex in a Git repository", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "tracked.ts", "export interface Tracked {}\n");
    const head = commitAll(root, "base");
    write(root, "untracked.ts", "export interface Untracked {}\n");

    const committedOnly = await runCli(root, "status", "--no-reindex");
    expect(committedOnly.status).toBe(0);
    expect(committedOnly.stderr).not.toContain("no Git repository");
    expect(JSON.parse(committedOnly.stdout)).toMatchObject({ fileCount: 1, gitCheckpoint: head });

    expect((await runCli(root, "status")).status).toBe(0);
    const restored = await runCli(root, "status", "--no-reindex");
    expect(restored.status).toBe(0);
    expect(JSON.parse(restored.stdout)).toMatchObject({ fileCount: 1, gitCheckpoint: head });
  });

  it("falls back when the Git executable is unavailable", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "src/types.ts", "export interface Value {}\n");
    commitAll(root, "base");

    const result = await runCliWithEnv(root, { ...process.env, PATH: "" }, "status");

    expect(result.status).toBe(0);
    expect(result.stderr).toContain("warning: no Git repository is available for index; re-indexing all source files");
    expect(JSON.parse(result.stdout)).toMatchObject({ fileCount: 1, gitCheckpoint: null });
  });

  it("does not treat an invalid Git target as an unavailable repository", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "src/types.ts", "export interface Value {}\n");
    commitAll(root, "base");

    const result = await runCli(root, "update-git", "--target", "missing-ref");

    expect(result.status).toBe(2);
    expect(result.stderr).toContain("Git command failed");
    expect(result.stderr).not.toContain("no Git repository is available");
  });

  it("does not hide Git configuration failures behind filesystem fallback", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "src/types.ts", "export interface Value {}\n");
    commitAll(root, "base");

    const result = await runCliWithEnv(root, { ...process.env, GIT_DIR: path.join(root, "missing-git-dir") }, "status");

    expect(result.status).toBe(2);
    expect(result.stderr).toContain("Git command failed");
    expect(result.stderr).not.toContain("no Git repository is available");
  });

  it("force rebuilds an incompatible index and prints a warning", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Example\n");
    commitAll(root, "initial");
    expect((await runCli(root, "status", "--model", "text-embedding-3-small")).status).toBe(0);

    const incompatible = await runCli(root, "status");
    expect(incompatible.status).toBe(2);
    expect(incompatible.stderr).toContain("Embedding provider, model, dimensions, or strategy differs");

    const rebuilt = await runCli(root, "status", "--force-reindex", "--yes-really-rebuild-the-index");
    expect(rebuilt.status).toBe(0);
    expect(rebuilt.stderr).toContain("warning: index is incompatible");
    expect(rebuilt.stderr).toContain("rebuilding automatically because --force-reindex was specified");
    expect(JSON.parse(rebuilt.stdout)).toMatchObject({
      embeddingProfile: { provider: "openai", model: "text-embedding-3-large", dimensions: 3072 },
    });
  });

  it.each(["--force-reindex", "--rebuild-on-divergence"])(
    "requires explicit confirmation before using %s",
    async (rebuildFlag) => {
      const root = temporaryRoot();

      const result = await runCli(root, "status", rebuildFlag);

      expect(result.status).toBe(2);
      expect(result.stderr).toContain(`${rebuildFlag} require`);
      expect(result.stderr).toContain("--yes-really-rebuild-the-index");
      expect(existsSync(path.join(root, ".slopdex/index.sqlite"))).toBe(false);
    },
  );

  it("supports clusters, changed-since, and uncommitted filters", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Initial\n");
    const base = commitAll(root, "initial");

    const clusters = await runCli(root, "cross-search", "--format", "clusters", "--changed-since", base, "--cross-file-only");
    expect(clusters.status).toBe(0);
    expect(clusters.stdout).toBe("No clusters.\n");

    const uncommitted = await runCli(root, "cross-search", "--cohesion", "--uncommitted", "--changed-since", base, "--source-path", "src", "-e", "keep$", "--format", "json");
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

    const result = await runCli(
      root,
      "cross-search",
      "--cross-file-only",
      "--include-symmetric-duplicates",
      "--matches",
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

    const defaultFormat = await runCli(root, "cross-search");
    expect(defaultFormat.status).toBe(0);
    expect(defaultFormat.stdout).toMatch(/^Cluster 1 /);

    const minimum = await runCli(root, "cross-search", "--min-lines", "4");
    expect(minimum.status).toBe(0);
    expect(minimum.stdout).toBe("No clusters.\n");

    const regex = await runCli(
      root,
      "cross-search",
      "--regex",
      "^one$",
      "--cross-file-only",
      "--min-lines",
      "3",
      "--matches",
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

    const sourceRegex = await runCli(root, "cross-search", "-e", "^one$", "--source-path", "same.ts",
      "--cross-file-only", "--min-lines", "3", "--matches", "1", "--threshold", "0.8-1.1", "--format", "json");
    expect(sourceRegex.status).toBe(0);
    const selected = JSON.parse(sourceRegex.stdout);
    expect(selected.source.qualifiedName).toBe("one");
    expect(selected.matches.map((match: { function: { qualifiedName: string } }) => match.function.qualifiedName)).toEqual(["external"]);
    expect((await runCli(root, "cross-search", "--regex", "^one$", "--source-path", "same.ts",
      "--cross-file-only", "--min-lines", "3", "--matches", "1", "--threshold", "0.8-1.1", "--format", "json")).stdout).toBe(sourceRegex.stdout);
    const empty = await runCli(root, "cross-search", "--regexp", "^missing$", "--format", "json");
    expect(empty.status).toBe(0);
    expect(empty.stdout).toBe("");
  });

  it("defaults --threshold to 0.3 and allows an explicit override", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "one.ts", `export function one() {
  return 1;
}\n`);
    write(root, "two.ts", `export function two() {
  return 2;
}\n`);
    commitAll(root, "functions");
    const openAI = new OpenAIEmbeddingProvider({ apiKey: "test" });
    const vectors = [
      [1, ...Array<number>(openAI.profile.dimensions - 1).fill(0)],
      [0, 1, ...Array<number>(openAI.profile.dimensions - 2).fill(0)],
    ];
    const index = new CodeIndex({
      rootDir: root,
      provider: {
        profile: openAI.profile,
        embedDocuments: async (inputs) => inputs.map((_, position) => vectors[position]!),
        embedQuery: async () => vectors[0]!,
      },
    });
    await index.updateFromGit();
    index.close();

    const defaultThreshold = await runCli(root, "cross-search");
    expect(defaultThreshold.status).toBe(0);
    expect(defaultThreshold.stdout).toBe("No clusters.\n");

    const overridden = await runCli(root, "cross-search", "--threshold=-1");
    expect(overridden.status).toBe(0);
    expect(overridden.stdout).toMatch(/^Cluster 1 /);
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

    const summary = await runCli(root, "cross-search", "--cohesion", "--include-symmetric-duplicates", "--matches", "2");
    expect(summary.status).toBe(0);
    expect(summary.stdout).toContain("[distance 4]");
    expect(summary.stdout.indexOf("packages/feature/three.ts")).toBeLessThan(summary.stdout.indexOf("src/two.ts"));

    const json = await runCli(root, "cross-search", "--cohesion", "--include-symmetric-duplicates", "--matches", "2", "--format", "json");
    expect(json.status).toBe(0);
    const rows = json.stdout.trim().split("\n").map((line) => JSON.parse(line));
    const one = rows.find((row) => row.source.qualifiedName === "one");
    expect(one.matches.map((match: { function: { qualifiedName: string } }) => match.function.qualifiedName)).toEqual(["three", "two"]);
    expect(one.matches.map((match: { physicalDistance: number }) => match.physicalDistance)).toEqual([4, 1]);

    const clusters = await runCli(root, "cross-search", "--cohesion", "--format", "clusters");
    expect(clusters.status).toBe(2);
    expect(clusters.stderr).toContain("does not preserve cohesion match order");

    const removed = await runCli(root, "cohesion");
    expect(removed.status).toBe(2);
    expect(removed.stderr).toContain("Unknown command: cohesion");
  });

  it.each(["--min-similarity", "--added-since"])("rejects removed option %s cleanly", async (option) => {
    const result = await runCli("/", "cross-search", option, "0.8");
    expect(result.status).toBe(2);
    expect(result.stderr).toMatch(/^slopdex: Unknown option/);
    expect(result.stderr).not.toContain("node:internal");
  });

  it("validates options before creating or refreshing an index", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Initial\n");
    commitAll(root, "initial");

    const invalidRegex = await runCli(root, "cross-search", "--regex", "[");
    expect(invalidRegex.status).toBe(2);
    expect(invalidRegex.stderr).toContain("Invalid --regex value");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const conflictingAliases = await runCli(root, "search", "query", "-e", "one", "--regex", "two");
    expect(conflictingAliases.status).toBe(2);
    expect(conflictingAliases.stderr).toContain("are aliases and cannot use different values");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const misplacedIndexSelector = await runCli(root, "search-code", "query", "--md");
    expect(misplacedIndexSelector.status).toBe(2);
    expect(misplacedIndexSelector.stderr).toContain("only available for search");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const emptyRange = await runCli(root, "cross-search", "--threshold", "0.9-0.9");
    expect(emptyRange.status).toBe(2);
    expect(emptyRange.stderr).toContain("minimum must be less than its maximum");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    for (const command of ["cross-search", "search", "search-descriptions"]) {
      for (const option of ["-e", "--regex"]) {
        const invalidSourceRegex = await runCli(root, command, ...(command.startsWith("search") ? ["query"] : []), option, "[");
        expect(invalidSourceRegex.status).toBe(2);
        expect(invalidSourceRegex.stderr).toContain(option === "--regex" ? "Invalid --regex value" : "Invalid -e/--regexp value");
        expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);
      }
    }

    const invalidDescriptionProvider = await runCli(root, "status", "--description-provider", "unknown");
    expect(invalidDescriptionProvider.status).toBe(2);
    expect(invalidDescriptionProvider.stderr).toContain("Unsupported description provider: unknown");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    for (const command of ["search", "search-descriptions"]) {
      const matchesRejected = await runCli(root, command, "query", "--matches", "2");
      expect(matchesRejected.status).toBe(2);
      expect(matchesRejected.stderr).toContain("--matches is only available for cross-search");
      expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);
    }

    for (const args of [
      ["cross-search", "--matches", "0"],
      ["cross-search", "--limit", "0"],
      ["search", "query", "--limit", "0"],
    ]) {
      const invalid = await runCli(root, ...args);
      expect(invalid.status).toBe(2);
      expect(invalid.stderr).toMatch(/must be a positive integer/);
      expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);
    }

  });
});
