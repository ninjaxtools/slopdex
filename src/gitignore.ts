import { lstat, readFile } from "node:fs/promises";
import path from "node:path";

import ignore, { type Ignore } from "ignore";

import { CodeIndexError } from "./errors.js";
import { throwIfAborted } from "./utils.js";

export class GitignoreRules {
  readonly #directories = new Map<string, Promise<Ignore | null>>();
  readonly #contents = new Map<string, string | null>();

  public constructor(private readonly readIgnore: (relativePath: string) => Promise<string | null>) {}

  public static workingTree(rootDir: string): GitignoreRules {
    return new GitignoreRules(async (relativePath) => {
      const absolutePath = path.join(rootDir, relativePath);
      try {
        const info = await lstat(absolutePath);
        // Git does not follow symlinks when reading .gitignore files.
        if (!info.isFile() || info.isSymbolicLink()) return null;
        return await readFile(absolutePath, "utf8");
      } catch (error) {
        if (["ENOENT", "ENOTDIR"].includes((error as NodeJS.ErrnoException).code ?? "")) return null;
        throw error;
      }
    });
  }

  public async ignores(relativePath: string, directory = false): Promise<boolean> {
    const parent = path.posix.dirname(relativePath);
    const rules = await this.#directory(parent === "." ? "" : parent);
    return rules === null || rules.ignores(directory ? `${relativePath}/` : relativePath);
  }

  public async assertUnchanged(signal?: AbortSignal): Promise<void> {
    for (const [filePath, content] of this.#contents) {
      throwIfAborted(signal);
      if (await this.readIgnore(filePath) !== content) {
        throw new CodeIndexError(`Gitignore rules changed while indexing: ${filePath}; retry the update.`);
      }
    }
  }

  #directory(directory: string): Promise<Ignore | null> {
    let result = this.#directories.get(directory);
    if (!result) {
      result = this.#loadDirectory(directory);
      this.#directories.set(directory, result);
    }
    return result;
  }

  async #loadDirectory(directory: string): Promise<Ignore | null> {
    let parentRules: Ignore | undefined;
    if (directory) {
      const parent = path.posix.dirname(directory);
      const rules = await this.#directory(parent === "." ? "" : parent);
      // A child .gitignore cannot re-include a directory excluded by its parent.
      if (rules === null || rules.ignores(`${directory}/`)) return null;
      parentRules = rules;
    }
    const filePath = directory ? `${directory}/.gitignore` : ".gitignore";
    const content = await this.readIgnore(filePath);
    this.#contents.set(filePath, content);
    if (content === null && parentRules) return parentRules;
    const rules = ignore({ ignorecase: false });
    if (parentRules) rules.add(parentRules);
    if (content !== null) rules.add(directory ? scopedPatterns(content, directory) : content);
    return rules;
  }
}

function scopedPatterns(content: string, directory: string): string[] {
  // Prefix nested rules into the root-relative namespace. The ignore package
  // handles wildcard syntax, escapes, precedence, and excluded-parent semantics.
  const prefix = directory.replace(/[\\*?\[\]#!]/g, "\\$&");
  return content.split(/\r?\n/).map((line) => {
    const pattern = line.replace(/^\uFEFF/, "");
    if (/^ *$/.test(pattern) || pattern.startsWith("#")) return pattern;
    const negative = pattern.startsWith("!");
    const body = negative ? pattern.slice(1) : pattern;
    if (/^\/? *$/.test(body)) return "";
    // A slash only at the end denotes a directory basename, not an anchored path.
    const anchored = body.startsWith("/") || body.replace(/\/ *$/, "").includes("/");
    return `${negative ? "!" : ""}/${prefix}/${anchored ? "" : "**/"}${body.replace(/^\//, "")}`;
  });
}
