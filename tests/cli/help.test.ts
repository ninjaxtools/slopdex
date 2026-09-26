import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { projectRoot, runCli, testTimeoutMs } from "./helpers.js";

describe("CLI help", { timeout: testTimeoutMs }, () => {
  it("prints the package version", async () => {
    const { version } = JSON.parse(readFileSync(path.join(projectRoot, "package.json"), "utf8")) as { version: string };
    const result = await runCli("/", "--version");

    expect(result.status).toBe(0);
    expect(result.stdout).toBe(`${version}\n`);
    expect(result.stderr).toBe("");
  });

  it("includes an example for every command", async () => {
    const result = await runCli("/", "--help");

    expect(result.status).toBe(0);
    for (const example of [
      "slopdex status",
      "slopdex models opencode-go",
      "slopdex config model opencode-go/gpt-5.6-luna",
      "slopdex config fallback-model opencode-go/muse-spark-1.3-contributor",
      "slopdex config descriptions enable",
      "slopdex config parallelism 10",
      "slopdex config reranker cohere",
      "slopdex config reranker openai",
      "slopdex config reranker jina",
      "slopdex update-files",
      "slopdex reindex-files",
      "slopdex delete-files",
      "slopdex update-git",
      "slopdex search \"validate an authenticated session\"",
      "slopdex search \"keep the repository index synchronized\"",
      "slopdex search-code \"keep the repository index synchronized\"",
      "slopdex search-md \"configure the embedding provider\"",
      "slopdex describe \"I want to implement a new rpc endpoint\"",
      "slopdex descriptions enable",
      "slopdex search-descriptions \"keep the repository index synchronized\"",
      "slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.9",
      "slopdex cross-search --cross-file-only --min-lines 4 --threshold 0.85-0.9",
      "slopdex cross-search --uncommitted --cross-file-only --min-lines 4 --threshold 0.9",
      "slopdex cross-search --changed-since origin/main --threshold 0.9",
      "slopdex cross-search --source-path src/services -e '^UserService\\.' --threshold 0.9",
      "slopdex cross-search --cross-file-only --cohesion --threshold 0.8",
      "slopdex search \"...\" --threshold 0.5",
      "slopdex cross-search --uncommitted --threshold 0.8",
      "slopdex cross-search --target-root /path/to/other/repo",
      "slopdex index-errors --format summary",
      "slopdex --version",
      "slopdex cross-search -e 'validate' --source-path src --changed-since origin/main --uncommitted",
      "slopdex search \"validate session\" -e '^Session\\.' --limit 10",
    ]) expect(result.stdout).toContain(example);
    expect(result.stdout).toContain("--source-path <path>");
    expect(result.stdout).toContain("--description-provider <name>");
    expect(result.stdout).toContain("--description-fallback-model <name>");
    expect(result.stdout).toContain("--reranker-candidates <number>");
    expect(result.stdout).toContain("--threshold <number|range>          Show similarities at/above a value or within a range (default: 0.3)");
    expect(result.stdout).toContain("--describe-full-file-threshold <number>");
    expect(result.stdout).toContain("--format <json|summary|clusters>");
    expect(result.stdout).toContain("--changed-since <commit>");
    expect(result.stdout).toContain("--uncommitted");
    expect(result.stdout).toContain("--target-config <path>");
    expect(result.stdout).toContain("--force-reindex");
    expect(result.stdout).toContain("--yes-really-rebuild-the-index");
    expect(result.stdout).toContain("--no-reindex");
    expect(result.stdout).toContain("--cross-file-only");
    expect(result.stdout).toContain("--min-lines <number>");
    expect(result.stdout).toContain("--matches <number>");
    expect(result.stdout).toContain("--limit <number>");
    expect(result.stdout).toContain("--regex <regex>");
    expect(result.stdout).toContain("-e, --regexp <regex>");
    expect(result.stdout).toContain("--cohesion");
    expect(result.stdout).toContain("--version");
    expect(result.stdout).toContain("Cluster 1 (3 functions, similarity 0.9124-0.9568)");
    expect(result.stdout).toContain("Review them for repeated validation or session logic that could be shared");
    expect(result.stdout).toContain("transitive links, so every function need not directly match every other function");
    expect(result.stdout).toContain("[distance 4]");
    expect(result.stdout).toContain("orders each source's matches");
    expect(result.stdout.indexOf("Commands:")).toBeLessThan(result.stdout.indexOf("Examples:"));
    expect(result.stdout.indexOf("Examples:")).toBeLessThan(result.stdout.indexOf("Options:"));
    expect(result.stdout.indexOf("Options:")).toBeLessThan(result.stdout.indexOf("Other Examples:"));
    expect(result.stdout).not.toContain("Reading Analysis Output:");
    expect(result.stdout).not.toContain("Greater distance first; similarity breaks ties");
    expect(result.stdout).not.toContain("Languages (automatically detected");
    expect(result.stdout).not.toContain("File discovery respects root");
    expect(result.stdout).not.toContain("--min-similarity");
    expect(result.stdout).not.toContain("--added-since");
  });
});
