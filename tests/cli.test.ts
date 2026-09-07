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

    const rebuilt = runCli(root, "status", "--force-rebuild");
    expect(rebuilt.status).toBe(0);
    expect(rebuilt.stderr).toContain("warning: index is incompatible");
    expect(rebuilt.stderr).toContain("rebuilding automatically because --force-rebuild was specified");
    expect(JSON.parse(rebuilt.stdout)).toMatchObject({
      embeddingProfile: { provider: "openai", model: "text-embedding-3-large", dimensions: 3072 },
    });
  });

  it("supports clusters, changed-since, and uncommitted filters", () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Initial\n");
    const base = commitAll(root, "initial");

    const clusters = runCli(root, "cross-search", "--format", "clusters", "--changed-since", base);
    expect(clusters.status).toBe(0);
    expect(clusters.stdout).toBe("No clusters.\n");

    const uncommitted = runCli(root, "cross-search", "--uncommitted");
    expect(uncommitted.status).toBe(0);
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

    const result = runCli(root, "cross-search", "--changed-since", "HEAD", "--uncommitted");

    expect(result.status).toBe(2);
    expect(result.stderr).toContain("either --changed-since or --uncommitted");
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
      "slopdex cross-search --format summary --threshold 0.8",
    ]) expect(result.stdout).toContain(example);
    expect(result.stdout).toContain("--source-path <path>");
    expect(result.stdout).toContain("--format <json|summary|clusters>");
    expect(result.stdout).toContain("--changed-since <commit>");
    expect(result.stdout).toContain("--uncommitted");
    expect(result.stdout).toContain("--target-config <path>");
    expect(result.stdout).toContain("--force-rebuild");
    expect(result.stdout).toContain("--no-reindex");
    expect(result.stdout).not.toContain("--min-similarity");
    expect(result.stdout).not.toContain("--added-since");
  });
});
