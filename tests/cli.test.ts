import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
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

    const uncommitted = runCli(root, "cohesion", "--uncommitted", "--changed-since", base, "--source-path", "src", "-e", "keep$");
    expect(uncommitted.status).toBe(0);
    expect(JSON.parse(uncommitted.stdout).parameters.sourceFilter).toEqual({
      type: "changed-since", commit: base, uncommitted: true, path: "src", nameRegex: "keep$",
    });
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

    const sourceRegex = runCli(root, "cross-search", "-e", "^one$", "--source-path", "same.ts",
      "--cross-file-only", "--min-lines", "3", "--limit", "1", "--threshold", "0.8-1.1", "--format", "json");
    expect(sourceRegex.status).toBe(0);
    const selected = JSON.parse(sourceRegex.stdout);
    expect(selected.source.qualifiedName).toBe("one");
    expect(selected.matches.map((match: { function: { qualifiedName: string } }) => match.function.qualifiedName)).toEqual(["external"]);
    const empty = runCli(root, "cross-search", "--regexp", "^missing$", "--format", "json");
    expect(empty.status).toBe(0);
    expect(empty.stdout).toBe("");
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

    const included = runCli(root, "cohesion", "--neighbors", "5", "--limit", "1", "--include-source", "--regexp", "^one$");
    expect(JSON.parse(included.stdout).pairs[0].left).toHaveProperty("source");
    expect(JSON.parse(included.stdout).parameters.sourceFilter).toEqual({ type: "all", nameRegex: "^one$" });
    expect(JSON.parse(included.stdout).summary).toMatchObject({ scope: "selected-sources", functionsAnalyzed: 1, candidateFunctions: 2 });

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

    for (const command of ["cross-search", "cohesion", "search", "search-summary"]) {
      const invalidSourceRegex = runCli(root, command, ...(command.startsWith("search") ? ["query"] : []), "-e", "[");
      expect(invalidSourceRegex.status).toBe(2);
      expect(invalidSourceRegex.stderr).toContain("Invalid -e/--regexp value");
      expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);
    }

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
      "slopdex update-files",
      "slopdex delete-files",
      "slopdex update-git",
      "slopdex search",
      "slopdex use-summaries",
      "slopdex search-summary",
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
    expect(result.stdout).toContain("-e, --regexp <regex>");
    expect(result.stdout).toContain("--neighbors <number>");
    expect(result.stdout).toContain("--include-source");
    expect(result.stdout).toContain("--version");
    expect(result.stdout).toContain("Cluster 1 (3 functions, similarity 0.9124-0.9568)");
    expect(result.stdout).toContain("Review them for repeated validation or session logic that could be shared");
    expect(result.stdout).toContain("transitive links, so every function need not directly match every other function");
    expect(result.stdout).toContain("Cohesion: 184 functions analyzed, 37 semantic edges");
    expect(result.stdout).toContain("same file 35.1%  same folder 29.7%  remote 35.2%  mean distance 1.84");
    expect(result.stdout).toContain("There is no universal pass/fail cutoff, but this warrants review");
    expect(result.stdout).toContain("top pair is 0.94 similar but four units apart");
    expect(result.stdout).toContain("Compare modules or history rather than treating one percentage as a fixed");
    expect(result.stdout).toContain("Reading Analysis Output:");
    expect(result.stdout).toContain("remote              Higher means more affinity crosses folders and weaker physical cohesion");
    expect(result.stdout).toContain("gap                 0-to-1 combined signal; higher means strongly related and farther apart");
    expect(result.stdout).toContain("externalAffinityRatio");
    expect(result.stdout.indexOf("Commands:")).toBeLessThan(result.stdout.indexOf("Analysis Examples:"));
    expect(result.stdout.indexOf("Analysis Examples:")).toBeLessThan(result.stdout.indexOf("Reading Analysis Output:"));
    expect(result.stdout.indexOf("Reading Analysis Output:")).toBeLessThan(result.stdout.indexOf("Options:"));
    expect(result.stdout.indexOf("Options:")).toBeLessThan(result.stdout.indexOf("Other Examples:"));
    expect(result.stdout).not.toContain("--min-similarity");
    expect(result.stdout).not.toContain("--added-since");
  });
});

describe("CLI summaries", () => {
  it("initializes, searches, updates, changes models, and preserves summary mode through a rebuild", () => {
    const root = temporaryRoot();
    write(root, "src/a.ts", "export function one() { return 1; }\n");
    write(root, ".slopdex/config.json", JSON.stringify({ dimensions: 2 }));
    write(root, ".slopdex/mock-api.mjs", `
globalThis.fetch = async (url, options) => {
  const body = JSON.parse(options.body);
  if (url === 'https://api.openai.com/v1/responses') {
    const input = JSON.parse(body.input);
    return Response.json({status: 'completed', output: [{type: 'message', content: [{
      type: 'output_text', text: 'Purpose of ' + input.qualifiedName + ' using ' + body.model
    }]}]});
  }
  if (url === 'https://api.openai.com/v1/embeddings') {
    return Response.json({data: body.input.map((input, index) => ({index, embedding: [1, 0]}))});
  }
  throw new Error('Unexpected API URL: ' + url);
};
`);
    const env = { ...process.env, NODE_OPTIONS: `--import=${pathToFileURL(path.join(root, ".slopdex/mock-api.mjs")).href}` };
    const run = (...args: string[]) => runCliWithEnv(root, env, ...args);
    const initialized = run("use-summaries");
    expect(initialized.status, initialized.stderr).toBe(0);
    expect(JSON.parse(initialized.stdout)).toEqual({ summariesCreated: 1, summariesEnabled: true });
    expect(JSON.parse(run("use-summaries").stdout).summariesCreated).toBe(0);

    const search = run("search-summary", "workflow", "--threshold", "0.9", "--limit", "1");
    expect(search.status, search.stderr).toBe(0);
    expect(JSON.parse(search.stdout)[0].function.summary).toBe("Purpose of one using gpt-5.6-sol");
    expect(JSON.parse(search.stdout)[0].function).not.toHaveProperty("summaryEmbeddingId");
    const text = run("search-summary", "workflow", "--format", "summary");
    expect(text.stdout).toContain("Purpose of one using gpt-5.6-sol");

    write(root, "src/b.ts", "export function two() { return 2; }\n");
    const updated = run("status");
    expect(updated.status, updated.stderr).toBe(0);
    expect(JSON.parse(updated.stdout)).toMatchObject({ functionCount: 2, summaryCount: 2, summariesEnabled: true });
    for (const command of ["search", "search-summary"]) {
      const filtered = run(command, "workflow", "-e", "^two$", "--limit", "1");
      expect(filtered.status, filtered.stderr).toBe(0);
      expect(JSON.parse(filtered.stdout).map((match: { function: { name: string } }) => match.function.name)).toEqual(["two"]);
    }
    const changedModel = run("use-summaries", "--summary-model", "custom-summary-model");
    expect(changedModel.status, changedModel.stderr).toBe(0);
    expect(JSON.parse(changedModel.stdout).summariesCreated).toBe(2);
    expect(JSON.parse(run("status").stdout).summaryProfile.model).toBe("custom-summary-model");

    const cross = run("cross-search", "--min-lines", "1", "--format", "json");
    expect(cross.status, cross.stderr).toBe(0);
    const crossRow = JSON.parse(cross.stdout.trim());
    expect(crossRow.matches[0]).toMatchObject({ similarity: 1, codeSimilarity: 1, summarySimilarity: 1 });
    expect(crossRow.scoring).toMatchObject({
      similarityMode: "code-summary-average", similarityWeights: { code: 0.5, summary: 0.5 },
      sourceSummaryProfile: { model: "custom-summary-model" }, targetSummaryProfile: { model: "custom-summary-model" },
    });
    expect(run("cross-search", "--min-lines", "1").stdout).toContain("combined 50% code + 50% summary");
    const cohesion = run("cohesion", "--min-lines", "1");
    expect(cohesion.status, cohesion.stderr).toBe(0);
    const report = JSON.parse(cohesion.stdout);
    expect(report.pairs[0]).toMatchObject({ similarity: 1, codeSimilarity: 1, summarySimilarity: 1 });
    expect(report.parameters).toMatchObject({ similarityMode: "code-summary-average", similarityWeights: { code: 0.5, summary: 0.5 } });
    expect(report.repository.summaryProfile.model).toBe("custom-summary-model");

    const rebuilt = run("status", "--model", "text-embedding-3-small", "--force-reindex");
    expect(rebuilt.status, rebuilt.stderr).toBe(0);
    expect(JSON.parse(rebuilt.stdout)).toMatchObject({
      functionCount: 2, summaryCount: 2, summariesEnabled: true,
      summaryProfile: { model: "custom-summary-model" },
    });
  });

  it("validates summary search arguments and explains how to enable summaries", () => {
    const root = temporaryRoot();
    const missingQuery = runCli(root, "search-summary");
    expect(missingQuery.status).toBe(2);
    expect(missingQuery.stderr).toContain("search-summary requires a query");
    expect(existsSync(path.join(root, ".slopdex/index.sqlite"))).toBe(false);
    const disabled = runCli(root, "search-summary", "workflow");
    expect(disabled.status).toBe(2);
    expect(disabled.stderr).toContain("run use-summaries first");
  });
});
