import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { commitAll, initGit, temporaryRoot, write } from "./helpers.js";

const projectRoot = path.resolve(import.meta.dirname, "..");

function runCli(root: string, ...args: string[]) {
  return spawnSync(process.execPath, ["--import", "tsx", "src/cli.ts", ...args, "--root", root], {
    cwd: projectRoot,
    encoding: "utf8",
    env: { ...process.env, OPENAI_API_KEY: "test" },
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
    expect(result.stderr).toContain("initializing automatically from committed HEAD");
    expect(JSON.parse(result.stdout)).toMatchObject({ gitCheckpoint: head, fileCount: 0, functionCount: 0 });
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(true);
  });

  it("removes a new index when automatic initialization fails", () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Example\n");
    commitAll(root, "initial");
    write(root, "dirty.ts", "export function dirty() {}\n");

    const result = runCli(root, "status");

    expect(result.status).toBe(2);
    expect(result.stderr).toContain("index not found");
    expect(result.stderr).toContain("working tree has uncommitted changes");
    expect(existsSync(path.join(root, ".slopdex", "index.sqlite"))).toBe(false);
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

  it("accepts a recursive source path and threshold range", () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "README.md", "# Example\n");
    commitAll(root, "initial");

    const result = runCli(root, "cross-search", "--source-path", "src", "--threshold", "0.85-0.95");

    expect(result.status).toBe(0);
    expect(result.stderr).not.toContain("must be a number");
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
  });
});
