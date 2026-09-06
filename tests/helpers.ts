import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { writeFileSync, mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

import { onTestFinished } from "vitest";

import type { EmbeddingProvider } from "../src/types.js";

export class FakeEmbeddingProvider implements EmbeddingProvider {
  public readonly profile = {
    provider: "fake",
    model: "deterministic",
    dimensions: 8,
    strategyVersion: "callable-v1",
  } as const;

  public async embedDocuments(inputs: readonly string[]): Promise<number[][]> {
    return inputs.map(embed);
  }

  public async embedQuery(input: string): Promise<number[]> {
    return embed(input);
  }
}

function embed(input: string): number[] {
  const vector = new Array<number>(8).fill(0);
  for (const word of input.toLowerCase().match(/[a-z_$][\w$]*/g) ?? []) {
    const hash = createHash("sha256").update(word).digest();
    vector[hash[0]! % vector.length]! += 1;
  }
  const norm = Math.sqrt(vector.reduce((sum, value) => sum + value * value, 0));
  if (norm === 0) return [1, 0, 0, 0, 0, 0, 0, 0];
  return vector.map((value) => value / norm);
}

export function write(root: string, relativePath: string, content: string): void {
  const target = path.join(root, relativePath);
  mkdirSync(path.dirname(target), { recursive: true });
  writeFileSync(target, content);
}

export function temporaryRoot(prefix = "slopdex-"): string {
  const root = mkdtempSync(path.join(tmpdir(), prefix));
  onTestFinished(() => rmSync(root, { recursive: true, force: true }));
  return root;
}

export function git(root: string, ...args: string[]): string {
  return execFileSync("git", ["-C", root, ...args], { encoding: "utf8" }).trim();
}

export function initGit(root: string): void {
  git(root, "init", "-q");
  git(root, "config", "user.email", "tests@example.com");
  git(root, "config", "user.name", "Tests");
}

export function commitAll(root: string, message: string): string {
  git(root, "add", "-A");
  git(root, "commit", "-q", "-m", message);
  return git(root, "rev-parse", "HEAD");
}
