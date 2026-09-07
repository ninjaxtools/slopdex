import { spawn } from "node:child_process";
import { lstat } from "node:fs/promises";
import path from "node:path";

import { CodeIndexError, GitUnavailableError } from "../errors.js";

export type GitChange =
  | { status: "A" | "M" | "D" | "T"; path: string }
  | { status: "R" | "C"; oldPath: string; path: string };

export interface GitTreeEntry {
  path: string;
  oid: string;
  mode: string;
  size: number;
}

export class GitRepository {
  public constructor(public readonly rootDir: string) {}

  async #run(args: readonly string[], allowExitOne = false): Promise<{ stdout: Buffer; code: number }> {
    return await new Promise((resolve, reject) => {
      const child = spawn("git", ["-C", this.rootDir, ...args], { stdio: ["ignore", "pipe", "pipe"] });
      const stdout: Buffer[] = [];
      const stderr: Buffer[] = [];
      child.stdout.on("data", (value: Buffer) => stdout.push(value));
      child.stderr.on("data", (value: Buffer) => stderr.push(value));
      child.on("error", (error) => {
        if ((error as NodeJS.ErrnoException).code === "ENOENT") {
          reject(new GitUnavailableError("Git executable is not available.", { cause: error }));
          return;
        }
        reject(error);
      });
      child.on("close", (code) => {
        const exitCode = code ?? -1;
        if (exitCode === 0 || (allowExitOne && exitCode === 1)) {
          resolve({ stdout: Buffer.concat(stdout), code: exitCode });
          return;
        }
        reject(new CodeIndexError(`Git command failed: ${Buffer.concat(stderr).toString("utf8").trim()}`));
      });
    });
  }

  public async assertRepository(): Promise<void> {
    let result: { stdout: Buffer; code: number };
    try {
      result = await this.#run(["rev-parse", "--is-inside-work-tree"]);
    } catch (error) {
      if (error instanceof GitUnavailableError) throw error;
      try {
        await lstat(path.join(this.rootDir, ".git"));
      } catch (markerError) {
        if ((markerError as NodeJS.ErrnoException).code === "ENOENT") {
          throw new GitUnavailableError(`${this.rootDir} is not a Git worktree.`, { cause: error });
        }
        throw markerError;
      }
      throw error;
    }
    if (result.stdout.toString("utf8").trim() !== "true") {
      throw new GitUnavailableError(`${this.rootDir} is not a Git worktree.`);
    }
    const topLevel = (await this.#run(["rev-parse", "--show-toplevel"])).stdout.toString("utf8").trim();
    if (path.resolve(topLevel) !== path.resolve(this.rootDir)) {
      throw new CodeIndexError(`rootDir must be the Git worktree root: ${topLevel}`);
    }
  }

  public async workTreeChanges(base: string, excludePaths: readonly string[] = []): Promise<GitChange[]> {
    const pathspecs = [".", ...excludePaths.map((filePath) => `:(top,literal,exclude)${filePath}`)];
    const result = await this.#run([
      "diff",
      "--name-status",
      "-z",
      "--find-renames",
      base,
      "--",
      ...pathspecs,
    ]);
    const changes = parseChanges(splitNull(result.stdout));
    const untracked = await this.#run([
      "ls-files",
      "--others",
      "--exclude-standard",
      "-z",
      "--",
      ...pathspecs,
    ]);
    for (const filePath of splitNull(untracked.stdout)) changes.push({ status: "A", path: filePath });
    return changes;
  }

  public async resolveCommit(ref: string): Promise<string> {
    const result = await this.#run(["rev-parse", "--verify", "--end-of-options", `${ref}^{commit}`]);
    const oid = result.stdout.toString("ascii").trim();
    if (!/^[0-9a-f]{40,64}$/.test(oid)) throw new CodeIndexError(`Could not resolve Git commit: ${ref}`);
    return oid;
  }

  public async isAncestor(ancestor: string, descendant: string): Promise<boolean> {
    const result = await this.#run(["merge-base", "--is-ancestor", ancestor, descendant], true);
    return result.code === 0;
  }

  public async listTree(commit: string): Promise<Map<string, GitTreeEntry>> {
    const result = await this.#run(["ls-tree", "-r", "-z", "-l", "--full-tree", commit, "--"]);
    const entries = new Map<string, GitTreeEntry>();
    for (const record of splitNull(result.stdout)) {
      const tab = record.indexOf("\t");
      if (tab < 0) continue;
      const [mode, type, oid, sizeText] = record.slice(0, tab).trim().split(/\s+/);
      const filePath = record.slice(tab + 1);
      const size = Number(sizeText);
      if ((mode === "100644" || mode === "100755") && type === "blob" && oid && Number.isSafeInteger(size)) {
        entries.set(filePath, { path: filePath, oid, mode, size });
      }
    }
    return entries;
  }

  public async readBlob(oid: string): Promise<Buffer> {
    return (await this.#run(["cat-file", "blob", oid])).stdout;
  }

  public async diff(base: string, target: string): Promise<GitChange[]> {
    const result = await this.#run(["diff", "--name-status", "-z", "--find-renames", `${base}..${target}`, "--"]);
    return parseChanges(splitNull(result.stdout));
  }
}

function parseChanges(values: readonly string[]): GitChange[] {
  const changes: GitChange[] = [];
  for (let index = 0; index < values.length;) {
    const rawStatus = values[index++];
    if (!rawStatus) continue;
    const status = rawStatus[0];
    if (status === "R" || status === "C") {
      const oldPath = values[index++];
      const filePath = values[index++];
      if (oldPath && filePath) changes.push({ status, oldPath, path: filePath });
    } else if (status === "A" || status === "M" || status === "D" || status === "T") {
      const filePath = values[index++];
      if (filePath) changes.push({ status, path: filePath });
    } else {
      throw new CodeIndexError(`Unsupported Git diff status: ${rawStatus}`);
    }
  }
  return changes;
}

function splitNull(buffer: Buffer): string[] {
  const value = buffer.toString("utf8");
  const parts = value.split("\0");
  if (parts.at(-1) === "") parts.pop();
  return parts;
}
