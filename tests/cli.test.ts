import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";

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

    const uncommitted = runCli(root, "cross-search", "--uncommitted");
    expect(uncommitted.status).toBe(0);
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
      "^(one|external)$",
      "--include-symmetric-duplicates",
      "--limit",
      "1",
      "--format",
      "json",
    );
    expect(regex.status).toBe(0);
    const regexRows = regex.stdout.trim().split("\n").map((line) => JSON.parse(line));
    expect(regexRows.map((row) => row.source.qualifiedName).sort()).toEqual(["external", "one"]);
    expect(regexRows.every((row) => /^(one|external)$/.test(row.matches[0].function.qualifiedName))).toBe(true);
  });

  it("reports cohesion as compact JSON or a summary", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "src/one.ts", `export function one() {
  return 1;
}\n`);
    write(root, "packages/feature/two.ts", `export function two() {
  return 2;
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

    const json = runCli(root, "cohesion", "--neighbors", "5", "--limit", "1");
    expect(json.status).toBe(0);
    const report = JSON.parse(json.stdout);
    expect(report.summary).toMatchObject({ functionsAnalyzed: 2, semanticEdges: 1, remoteRatio: 1 });
    expect(report.pairs).toHaveLength(1);
    expect(report.pairs[0].left).not.toHaveProperty("source");
    expect(report.groups[0].members[0]).not.toHaveProperty("source");

    const included = runCli(root, "cohesion", "--neighbors", "5", "--limit", "1", "--include-source");
    expect(JSON.parse(included.stdout).pairs[0].left).toHaveProperty("source");

    const summary = runCli(root, "cohesion", "--neighbors", "5", "--limit", "1", "--format", "summary");
    expect(summary.status).toBe(0);
    expect(summary.stdout).toContain("Cohesion: 2 functions analyzed, 1 semantic edge");
    expect(summary.stdout).toContain("distance 4");
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

    const emptyRange = runCli(root, "cross-search", "--threshold", "0.9-0.9");
    expect(emptyRange.status).toBe(2);
    expect(emptyRange.stderr).toContain("minimum must be less than its maximum");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const result = runCli(root, "cross-search", "--changed-since", "HEAD", "--uncommitted");

    expect(result.status).toBe(2);
    expect(result.stderr).toContain("either --changed-since or --uncommitted");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const invalidNeighbors = runCli(root, "cohesion", "--neighbors", "0");
    expect(invalidNeighbors.status).toBe(2);
    expect(invalidNeighbors.stderr).toContain("neighbors must be a positive integer");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);

    const invalidCohesionThreshold = runCli(root, "cohesion", "--threshold", "1");
    expect(invalidCohesionThreshold.status).toBe(2);
    expect(invalidCohesionThreshold.stderr).toContain("cohesion threshold must be at least -1 and less than 1");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);
  });
});

describe("CLI help", () => {
  it("includes an example for every command", () => {
    const result = runCli("/", "--help");

    expect(result.status).toBe(0);
    for (const example of [
      "slopdex status",
      "slopdex update-files",
      "slopdex delete-files",
      "slopdex update-git",
      "slopdex search",
      "slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9 --limit 5",
      "slopdex cohesion --threshold 0.8 --neighbors 20 --limit 50 --format summary",
    ]) expect(result.stdout).toContain(example);
    expect(result.stdout).toContain("--source-path <path>");
    expect(result.stdout).toContain("--format <json|summary|clusters>");
    expect(result.stdout).toContain("--changed-since <commit>");
    expect(result.stdout).toContain("--uncommitted");
    expect(result.stdout).toContain("--target-config <path>");
    expect(result.stdout).toContain("--force-reindex");
    expect(result.stdout).toContain("--no-reindex");
    expect(result.stdout).toContain("--cross-file-only");
    expect(result.stdout).toContain("--min-lines <number>");
    expect(result.stdout).toContain("--regex <regex>");
    expect(result.stdout).toContain("--neighbors <number>");
    expect(result.stdout).toContain("--include-source");
    expect(result.stdout).toContain("Cluster 1 (3 functions, similarity 0.9124-0.9568)");
    expect(result.stdout).toContain("Review them for repeated validation or session logic that could be shared");
    expect(result.stdout).toContain("transitive links, so every function need not directly match every other function");
    expect(result.stdout).toContain("Cohesion: 184 functions analyzed, 37 semantic edges");
    expect(result.stdout).toContain("same file 35.1%  same folder 29.7%  remote 35.2%  mean distance 1.84");
    expect(result.stdout).toContain("There is no universal pass/fail cutoff, but this warrants review");
    expect(result.stdout).toContain("top pair is 0.94 similar but four units apart");
    expect(result.stdout).toContain("Compare modules or history rather than treating one percentage as a fixed");
    expect(result.stdout.indexOf("Commands:")).toBeLessThan(result.stdout.indexOf("Analysis Examples:"));
    expect(result.stdout.indexOf("Analysis Examples:")).toBeLessThan(result.stdout.indexOf("Options:"));
    expect(result.stdout.indexOf("Options:")).toBeLessThan(result.stdout.indexOf("Other Examples:"));
    expect(result.stdout).not.toContain("--min-similarity");
    expect(result.stdout).not.toContain("--added-since");
  });
});
