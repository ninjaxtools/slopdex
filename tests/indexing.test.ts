import { rmSync } from "node:fs";

import { describe, expect, it } from "vitest";

import { CodeIndex } from "../src/code-index.js";
import { FakeEmbeddingProvider, commitAll, git, initGit, temporaryRoot, write } from "./helpers.js";

describe("explicit indexing", () => {
  it("updates, reconciles, and deletes specific files", async () => {
    const root = temporaryRoot();
    write(root, "src/math.ts", `
export function add(a: number, b: number) { return a + b; }
export const subtract = (a: number, b: number) => a - b;
`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });

    const initial = await index.updateFiles({ upsert: ["src/math.ts"] });
    expect(initial).toMatchObject({ filesUpdated: 1, functionsAdded: 2, embeddingsCreated: 2 });
    const addId = index.allFunctions().find((item) => item.name === "add")!.id;

    write(root, "src/math.ts", `
export function add(a: number, b: number) { return Number(a) + Number(b); }
export function multiply(a: number, b: number) { return a * b; }
`);
    const updated = await index.updateFiles({ upsert: ["src/math.ts"] });
    expect(updated).toMatchObject({ functionsAdded: 1, functionsUpdated: 1, functionsDeleted: 1 });
    expect(index.allFunctions().find((item) => item.name === "add")!.id).toBe(addId);

    const deleted = await index.updateFiles({ delete: ["src/math.ts"] });
    expect(deleted).toMatchObject({ filesDeleted: 1, functionsDeleted: 2 });
    expect(index.status().functionCount).toBe(0);
    index.close();
  });

  it("deduplicates identical embedding inputs across files", async () => {
    const root = temporaryRoot();
    write(root, "one.ts", `export function same() { return 1; }\n`);
    write(root, "two.ts", `export function same() { return 1; }\n`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    const stats = await index.updateFiles({ upsert: ["one.ts", "two.ts"] });
    expect(stats).toMatchObject({ functionsAdded: 2, embeddingsCreated: 1 });
    expect(index.status().functionCount).toBe(2);
    index.close();
  });
});

describe("Git indexing", () => {
  it.each(["unstaged", "staged", "untracked"] as const)("overlays %s changes without advancing past HEAD", async (change) => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "valid.ts", `export function valid() { return true; }\n`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    if (change === "untracked") {
      write(root, "new.ts", `export function added() { return true; }\n`);
    } else {
      write(root, "valid.ts", `export function changed() { return true; }\n`);
      if (change === "staged") git(root, "add", "valid.ts");
    }

    await index.updateFromGit();
    expect(index.status().gitCheckpoint).toBe(base);
    const functions = index.allFunctions();
    expect(functions.map((item) => item.name)).toEqual(change === "untracked" ? ["added", "valid"] : ["changed"]);
    expect(functions.find((item) => item.name === (change === "untracked" ? "added" : "changed"))!.sourceMode).toBe("working-tree");
    index.close();
  });

  it("filters changed-since and uncommitted functions from the working-tree overlay", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "tracked.ts", `
export function stable() { return 1; }
export function modified() { return 2; }
`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    write(root, "tracked.ts", `
export function stable() { return 1; }
export function modified() { return 3; }
`);
    write(root, "untracked.ts", `export function untracked() { return 4; }\n`);
    await index.updateFromGit();

    const changed = await index.sourceFunctions({ type: "changed-since", commit: base });
    expect(changed.map((item) => item.name)).toEqual(["modified", "untracked"]);
    const uncommitted = await index.sourceFunctions({ type: "uncommitted" });
    expect(uncommitted.map((item) => item.name)).toEqual(["stable", "modified", "untracked"]);
    index.close();
  });

  it("indexes net changes, records a checkpoint, and identifies changed functions", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "src/service.ts", `export function existing() { return "old"; }\n`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });

    await index.updateFromGit();
    const existingId = index.allFunctions()[0]!.id;
    expect(index.status().gitCheckpoint).toBe(base);

    write(root, "src/service.ts", `
export function existing() { return "changed"; }
export function added() { return "new"; }
`);
    const target = commitAll(root, "add function");
    const stats = await index.updateFromGit();
    expect(stats.checkpoint).toBe(target);
    expect(index.allFunctions().find((item) => item.name === "existing")!.id).toBe(existingId);
    const changed = await index.sourceFunctions({ type: "changed-since", commit: base });
    expect(changed.map((item) => item.name)).toEqual(["existing", "added"]);
    expect(changed.find((item) => item.name === "added")!.firstSeenCommit).toBe(target);
    index.close();
  });

  it("keeps an explicit historical target separate from the current checkout", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "value.ts", `export function historical() { return 1; }\n`);
    const historical = commitAll(root, "historical");
    write(root, "value.ts", `export function current() { return 2; }\n`);
    commitAll(root, "current");
    write(root, "value.ts", `export function dirty() { return 3; }\n`);
    write(root, "untracked.ts", `export function untracked() { return 4; }\n`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });

    await index.updateFromGit({ target: historical });

    expect(index.status().gitCheckpoint).toBe(historical);
    expect(index.allFunctions().map((item) => [item.name, item.sourceMode])).toEqual([["historical", "git"]]);
    index.close();
  });

  it("aborts if a working-tree file changes while embeddings are prepared", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "value.ts", `export function committed() { return 1; }\n`);
    const base = commitAll(root, "base");
    const initial = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await initial.updateFromGit();
    initial.close();

    write(root, "value.ts", `export function dirty() { return 2; }\n`);
    class MutatingProvider extends FakeEmbeddingProvider {
      public override async embedDocuments(inputs: readonly string[]): Promise<number[][]> {
        write(root, "value.ts", `export function changedAgain() { return 3; }\n`);
        return await super.embedDocuments(inputs);
      }
    }
    const index = new CodeIndex({ rootDir: root, provider: new MutatingProvider() });

    await expect(index.updateFromGit()).rejects.toThrow(/changed while indexing/);
    expect(index.status().gitCheckpoint).toBe(base);
    expect(index.allFunctions().map((item) => item.name)).toEqual(["committed"]);
    index.close();
  });

  it("preserves function identity across a Git file rename", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "old.ts", `export function stable() { return 1; }\n`);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    const original = index.allFunctions()[0]!;

    git(root, "mv", "old.ts", "new.ts");
    commitAll(root, "rename");
    await index.updateFromGit();
    const renamed = index.allFunctions()[0]!;
    expect(renamed.path).toBe("new.ts");
    expect(renamed.id).toBe(original.id);
    expect(renamed.firstSeenCommit).toBe(original.firstSeenCommit);
    index.close();
  });

  it("removes files deleted by a Git commit", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "keep.ts", `export function keep() { return 1; }\n`);
    write(root, "remove.ts", `export function remove() { return 2; }\n`);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    rmSync(`${root}/remove.ts`);
    const target = commitAll(root, "remove file");
    const stats = await index.updateFromGit();

    expect(stats).toMatchObject({ filesDeleted: 1, functionsDeleted: 1, checkpoint: target });
    expect(index.allFunctions().map((item) => item.name)).toEqual(["keep"]);
    index.close();
  });

  it("warns and advances the checkpoint when parsing is incomplete", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "valid.ts", `export function valid() { return true; }\n`);
    const base = commitAll(root, "base");
    const warnings: string[] = [];
    const index = new CodeIndex({
      rootDir: root,
      provider: new FakeEmbeddingProvider(),
      onWarning: (message) => warnings.push(message),
    });
    await index.updateFromGit();

    write(root, "valid.ts", "export function broken( {");
    const broken = commitAll(root, "broken");
    await index.updateFromGit();
    expect(index.status().gitCheckpoint).toBe(broken);
    expect(index.status().gitCheckpoint).not.toBe(base);
    expect(index.allFunctions()).toEqual([]);
    expect(warnings).toEqual([
      "Cannot fully parse valid.ts: tree-sitter reported syntax errors; indexing recoverable callables only.",
    ]);
    index.close();
  });

  it("reconciles a working-tree update after it is committed", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "value.ts", `export function committed() { return 1; }\n`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    write(root, "value.ts", `export function dirty() { return 2; }\n`);
    await index.updateFiles({ upsert: ["value.ts"] });
    expect(index.allFunctions()[0]!.name).toBe("dirty");

    await index.updateFromGit();
    expect(index.status().gitCheckpoint).toBe(base);
    expect(index.allFunctions()[0]!.sourceMode).toBe("working-tree");

    commitAll(root, "update value");
    await index.updateFromGit();
    expect(index.allFunctions()[0]!.name).toBe("dirty");
    expect(index.allFunctions()[0]!.sourceMode).toBe("git");
    index.close();
  });

  it("retains first-seen provenance across temporary working-tree replacements", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "value.ts", `export function original() { return 1; }\n`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    write(root, "value.ts", `export function temporary() { return 2; }\n`);
    write(root, "unrelated.ts", `export function unrelated() { return 3; }\n`);
    git(root, "add", "unrelated.ts");
    git(root, "commit", "-q", "-m", "unrelated");
    const target = git(root, "rev-parse", "HEAD");
    await index.updateFromGit();

    write(root, "value.ts", `export function original() { return 1; }\n`);
    await index.updateFromGit();

    const original = index.allFunctions().find((item) => item.name === "original")!;
    expect(index.status().gitCheckpoint).toBe(target);
    expect(original.firstSeenCommit).toBe(base);
    index.close();
  });

  it("removes transient working-tree files before applying a new commit and current overlay", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "committed.ts", `export function committed() { return 1; }\n`);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    write(root, "transient.ts", `export function transient() { return 2; }\n`);
    await index.updateFiles({ upsert: ["transient.ts"] });
    rmSync(`${root}/transient.ts`);
    write(root, "committed.ts", `export function committedAtTarget() { return 3; }\n`);
    const target = commitAll(root, "next commit");
    write(root, "dirty.ts", `export function dirty() { return 4; }\n`);

    await index.updateFromGit();

    expect(index.status().gitCheckpoint).toBe(target);
    expect(index.allFunctions().map((item) => [item.name, item.sourceMode])).toEqual([
      ["committedAtTarget", "git"],
      ["dirty", "working-tree"],
    ]);
    index.close();
  });

  it("restores an explicitly deleted committed file", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "kept.ts", `export function kept() { return 1; }\n`);
    commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    await index.updateFiles({ delete: ["kept.ts"] });
    expect(index.status().functionCount).toBe(0);

    await index.updateFromGit();
    expect(index.allFunctions().map((item) => item.name)).toEqual(["kept"]);
    index.close();
  });

  it("reconciles include and exclude policy at an unchanged checkpoint", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "keep.ts", `export function keep() { return 1; }\n`);
    write(root, "skip.ts", `export function skip() { return 2; }\n`);
    commitAll(root, "base");
    const provider = new FakeEmbeddingProvider();
    const first = new CodeIndex({ rootDir: root, provider });
    await first.updateFromGit();
    first.close();

    const second = new CodeIndex({ rootDir: root, provider, exclude: ["skip.ts"] });
    await second.updateFromGit();
    expect(second.allFunctions().map((item) => item.name)).toEqual(["keep"]);
    second.close();
  });

  it("classifies an explicit working-tree rename as changed", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "old.ts", `export function stable() { return 1; }\n`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();
    git(root, "mv", "old.ts", "new.ts");
    await index.updateFiles({ renames: [{ from: "old.ts", to: "new.ts" }] });

    const changed = await index.sourceFunctions({ type: "changed-since", commit: base });
    expect(changed.map((item) => item.name)).toEqual(["stable"]);
    index.close();
  });

  it("composes sequential working-tree rename provenance", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "old.ts", `export function stable() { return 1; }\n`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    git(root, "mv", "old.ts", "mid.ts");
    await index.updateFiles({ renames: [{ from: "old.ts", to: "mid.ts" }] });
    git(root, "mv", "mid.ts", "new.ts");
    await index.updateFiles({ renames: [{ from: "mid.ts", to: "new.ts" }] });
    write(root, "new.ts", `export function stable() { return Number(1); }\n`);
    await index.updateFiles({ upsert: ["new.ts"] });

    expect((await index.sourceFunctions({ type: "changed-since", commit: base })).map((item) => item.name)).toEqual(["stable"]);
    index.close();
  });

  it("matches duplicate names by source before declaration order", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "duplicates.js", `function same() { return "old"; }\n`);
    const base = commitAll(root, "base");
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFromGit();

    write(root, "duplicates.js", `
function same() { return "new"; }
function same() { return "old"; }
`);
    const target = commitAll(root, "insert duplicate");
    await index.updateFromGit();
    const changed = await index.sourceFunctions({ type: "changed-since", commit: base });
    expect(changed).toHaveLength(1);
    expect(changed[0]!.source).toContain('return "new"');
    expect(changed[0]!.firstSeenCommit).toBe(target);
    const old = index.allFunctions().find((item) => item.source.includes('return "old"'))!;
    expect(old.firstSeenCommit).toBe(base);
    index.close();
  });

  it("rejects cyclic explicit rename batches", async () => {
    const root = temporaryRoot();
    write(root, "a.ts", `export function a() { return 1; }\n`);
    write(root, "b.ts", `export function b() { return 2; }\n`);
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider() });
    await index.updateFiles({ upsert: ["a.ts", "b.ts"] });
    await expect(index.updateFiles({
      renames: [
        { from: "a.ts", to: "b.ts" },
        { from: "b.ts", to: "a.ts" },
      ],
    })).rejects.toThrow(/cyclic/);
    expect(index.status().functionCount).toBe(2);
    index.close();
  });
});
