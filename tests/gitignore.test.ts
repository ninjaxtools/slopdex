import { execFileSync } from "node:child_process";
import { rmSync, symlinkSync } from "node:fs";
import path from "node:path";

import { describe, expect, it, onTestFinished } from "vitest";

import { CodeIndex } from "../src/code-index.js";
import { GitignoreRules } from "../src/gitignore.js";
import { FakeEmbeddingProvider, commitAll, git, initGit, temporaryRoot, write } from "./helpers.js";

function openIndex(root: string, provider = new FakeEmbeddingProvider()): CodeIndex {
  const index = new CodeIndex({
    rootDir: root, provider,
    summaryProvider: {
      profile: { provider: "test", model: "purpose", strategyVersion: "v1" },
      summarize: async ({ callable }) => `Purpose of ${callable.name}`,
    },
  });
  onTestFinished(() => index.close());
  return index;
}

const callable = (name: string) => `export function ${name}() { return 1; }\n`;

describe("gitignore rules", () => {
  it("agrees with git check-ignore for nested rules, negation, anchoring, escapes, and directory patterns", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, ".gitignore", `# comment
*.tmp.ts
/root-only.ts
cache/
blocked/
!blocked/keep.ts
scoped/*
\\#literal.ts
\\!literal.ts
spaced\\ .ts
**/scratch/**
[ab]?.ts
case.ts
dir.ts/
`);
    write(root, "blocked/.gitignore", "!keep.ts\n");
    write(root, "scoped/.gitignore", "!rescued/\n!allowed.tmp.ts\n!    \n/   \n");
    write(root, "scoped/rescued/.gitignore", "allowed.tmp.ts\n");
    write(root, "[dir]!/.gitignore", "local.ts\r\n/anchored.ts\r\nfolder/   \r\ntrailing.ts    \r\n!allowed.tmp.ts\r\n");
    const files = [
      "keep.ts", "lost.tmp.ts", "root-only.ts", "src/root-only.ts", "cache/file.ts", "blocked/keep.ts",
      "scoped/drop.ts", "scoped/allowed.tmp.ts", "scoped/rescued/keep.ts", "scoped/rescued/allowed.tmp.ts",
      "scoped/rescued/drop.tmp.ts", "#literal.ts", "!literal.ts", "spaced .ts", "src/scratch/file.ts",
      "a1.ts", "c1.ts", "case.ts", "Case.ts", "dir.ts/child.ts", "[dir]!/local.ts", "[dir]!/sub/local.ts",
      "[dir]!/anchored.ts", "[dir]!/sub/anchored.ts", "[dir]!/sub/folder/file.ts", "[dir]!/trailing.ts", "[dir]!/allowed.tmp.ts",
    ];
    for (const file of files) write(root, file, callable("example"));
    const output = execFileSync("git", ["-C", root, "check-ignore", "--no-index", "--stdin", "-z"], {
      input: `${files.join("\0")}\0`, encoding: "utf8",
    });
    const expected = new Set(output.split("\0").filter(Boolean));
    const rules = GitignoreRules.workingTree(root);
    for (const file of files) expect(await rules.ignores(file), file).toBe(expected.has(file));
    expect(await rules.ignores("dir.ts", true)).toBe(true);
    expect(await rules.ignores("dir.ts")).toBe(false);
    expect(await rules.ignores("scoped/rescued/keep.ts")).toBe(false);
    expect(await rules.ignores("blocked/keep.ts")).toBe(true);
  });

  it("does not load ignore files beneath excluded directories or follow ignore-file symlinks", async () => {
    const reads: string[] = [];
    const rules = new GitignoreRules(async (file) => {
      reads.push(file);
      if (file === ".gitignore") return "blocked/\n";
      throw new Error("Should not enter an ignored directory");
    });
    expect(await rules.ignores("blocked/deep/keep.ts")).toBe(true);
    expect(reads).toEqual([".gitignore"]);

    const root = temporaryRoot();
    write(root, "rules.txt", "keep.ts\n");
    symlinkSync("rules.txt", path.join(root, ".gitignore"));
    expect(await GitignoreRules.workingTree(root).ignores("keep.ts")).toBe(false);
  });
});

describe("gitignore-aware indexing", () => {
  it("filters non-Git discovery and removes stale functions and summaries when rules change", async () => {
    const root = temporaryRoot();
    write(root, ".gitignore", "ignored/\n*.generated.ts\n!keep.generated.ts\n");
    write(root, "src/.gitignore", "/local.ts\n");
    for (const file of ["keep.ts", "keep.generated.ts", "drop.generated.ts", "ignored/hidden.ts", "src/local.ts", "src/sub/local.ts"])
      write(root, file, callable("example"));
    const index = openIndex(root);
    await index.updateFromWorkingTree();
    expect(index.allFunctions().map((item) => item.path)).toEqual(["keep.generated.ts", "keep.ts", "src/sub/local.ts"]);
    await index.useSummaries();
    write(root, ".gitignore", "ignored/\n*.generated.ts\nkeep.ts\n");
    await index.updateFromWorkingTree();
    expect(index.allFunctions().map((item) => item.path)).toEqual(["src/sub/local.ts"]);
    expect(index.status()).toMatchObject({ functionCount: 1, summaryCount: 1 });
    expect((await index.searchSummary({ query: "example" })).map((item) => item.function.path)).toEqual(["src/sub/local.ts"]);
    rmSync(path.join(root, "src/.gitignore"));
    await index.updateFromWorkingTree();
    expect(index.allFunctions().map((item) => item.path)).toEqual(["src/local.ts", "src/sub/local.ts"]);
  });

  it("uses snapshot rules for historical and committed-only indexing, and live rules for HEAD overlays", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, ".gitignore", "b.ts\n");
    write(root, "src/.gitignore", "c.ts\n");
    write(root, "a.ts", callable("a"));
    write(root, "b.ts", callable("b"));
    write(root, "src/c.ts", callable("c"));
    git(root, "add", "-f", "b.ts", "src/c.ts");
    const base = commitAll(root, "tracked sources and ignore rules");
    write(root, ".gitignore", "# unignored\n");
    write(root, "src/.gitignore", "# unignored\n");
    commitAll(root, "allow all tracked sources");
    write(root, ".gitignore", "a.ts\nb.ts\n");
    write(root, "src/.gitignore", "c.ts\n");
    const index = openIndex(root);

    await index.updateFromGit({ target: base });
    expect(index.allFunctions().map((item) => item.name)).toEqual(["a"]);
    await index.updateFromGit({ includeWorkingTree: false });
    expect(index.allFunctions().map((item) => item.name)).toEqual(["a", "b", "c"]);
    await index.updateFromGit();
    expect(index.allFunctions()).toEqual([]);
    rmSync(path.join(root, ".gitignore"));
    rmSync(path.join(root, "src/.gitignore"));
    await index.updateFromGit();
    expect(index.allFunctions().map((item) => item.name)).toEqual(["a", "b", "c"]);
    // Changes to ignore files alone are enough to update an otherwise unchanged index.
    write(root, ".gitignore", "b.ts\n");
    await index.updateFromGit();
    expect(index.allFunctions().map((item) => item.name)).toEqual(["a", "c"]);
    await index.updateFromGit({ includeWorkingTree: false });
    expect(index.allFunctions().map((item) => item.name)).toEqual(["a", "b", "c"]);
  });

  it("removes previously indexed untracked files when they become ignored", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "main.ts", callable("main"));
    commitAll(root, "base");
    write(root, "scratch/local.ts", callable("local"));
    const index = openIndex(root);
    await index.updateFromGit();
    expect(index.allFunctions().map((item) => item.name)).toEqual(["main", "local"]);
    write(root, ".gitignore", "scratch/\n");
    await index.updateFromGit();
    expect(index.allFunctions().map((item) => item.name)).toEqual(["main"]);
    write(root, ".gitignore", "# allowed again\n");
    await index.updateFromGit();
    expect(index.allFunctions().map((item) => item.name)).toEqual(["main", "local"]);
  });

  it("rejects explicitly upserted or renamed ignored files atomically", async () => {
    const root = temporaryRoot();
    write(root, ".gitignore", "ignored.ts\n");
    write(root, "keep.ts", callable("keep"));
    write(root, "ignored.ts", callable("ignored"));
    const index = openIndex(root);
    await expect(index.updateFiles({ upsert: ["keep.ts", "ignored.ts"] })).rejects.toThrow("excluded source file: ignored.ts");
    expect(index.allFunctions()).toEqual([]);
    await index.updateFiles({ upsert: ["keep.ts"] });
    await expect(index.updateFiles({ renames: [{ from: "keep.ts", to: "ignored.ts" }] })).rejects.toThrow("excluded source file: ignored.ts");
    expect(index.allFunctions().map((item) => item.name)).toEqual(["keep"]);
  });

  it.each(["working-tree", "git", "explicit"] as const)("aborts %s indexing if ignore rules change during embedding", async (mode) => {
    const root = temporaryRoot();
    write(root, "keep.ts", callable("keep"));
    write(root, ".gitignore", "# base\n");
    if (mode === "git") {
      initGit(root);
      commitAll(root, "base");
    }
    write(root, ".gitignore", "# already modified\n");
    const provider = new FakeEmbeddingProvider();
    const embed = provider.embedDocuments.bind(provider);
    provider.embedDocuments = async (inputs) => {
      write(root, ".gitignore", "keep.ts\n");
      return embed(inputs);
    };
    const index = openIndex(root, provider);
    const update = mode === "git" ? index.updateFromGit()
      : mode === "explicit" ? index.updateFiles({ upsert: ["keep.ts"] }) : index.updateFromWorkingTree();
    await expect(update).rejects.toThrow(/rules changed while indexing/);
    expect(index.status()).toMatchObject({ generation: 0, functionCount: 0 });
  });
});
