import { renameSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { DatabaseSync } from "node:sqlite";
import * as sqliteVec from "sqlite-vec";

import { CodeIndex } from "../src/code-index.js";
import { chunkMarkdown } from "../src/parser/markdown.js";
import type { DescriptionProvider, EmbeddingProvider } from "../src/types.js";
import { commitAll, FakeEmbeddingProvider, initGit, temporaryRoot, write } from "./helpers.js";

describe("Markdown chunking", () => {
  it("adds the active heading ancestry to each section", () => {
    const chunks = chunkMarkdown(`# Heading 1

Intro

## Heading 2

Some text

### Heading 3

Some more text

## Heading 2 - 2

Some additional text
`);

    expect(chunks.map((chunk) => chunk.content)).toEqual([
      "# Heading 1\n\nIntro",
      "# Heading 1\n## Heading 2\n\nSome text",
      "# Heading 1\n## Heading 2\n### Heading 3\n\nSome more text",
      "# Heading 1\n## Heading 2 - 2\n\nSome additional text",
    ]);
    expect(chunks.map((chunk) => chunk.headingPath)).toEqual([
      ["Heading 1"],
      ["Heading 1", "Heading 2"],
      ["Heading 1", "Heading 2", "Heading 3"],
      ["Heading 1", "Heading 2 - 2"],
    ]);
  });

  it("supports preambles and skipped heading levels, and ignores headings in fences", () => {
    const chunks = chunkMarkdown(`Preamble

# Guide
### Details

Before the example.

\`\`\`md
# Not a heading
\`\`\`

After the example.

## Empty
`);

    expect(chunks).toHaveLength(2);
    expect(chunks[0]).toMatchObject({ headingPath: [], startLine: 1, content: "Preamble" });
    expect(chunks[1]).toMatchObject({
      headingPath: ["Guide", "Details"],
      content: "# Guide\n### Details\n\nBefore the example.\n\n```md\n# Not a heading\n```\n\nAfter the example.",
    });
  });

  describe.each(["```", "~~~"])("with %s fences", (fence) => {
    it.each([
      ["complete single-line", "<!-- literal comment -->"],
      ["complete multiline", "<!--\n# Literal comment heading\n-->"],
      ["unclosed", "<!--\n# Literal comment heading"],
    ])("preserves %s comments in fenced examples and subsequent sections", (_kind, comment) => {
      const example = `${fence}html\n${comment}\n${fence}`;
      const chunks = chunkMarkdown(`# Example\n\n${example}\n\nAfter the example.\n\n## Next\n\nNext body.\n`);

      expect(chunks.map((chunk) => ({ headingPath: chunk.headingPath, content: chunk.content }))).toEqual([
        { headingPath: ["Example"], content: `# Example\n\n${example}\n\nAfter the example.` },
        { headingPath: ["Example", "Next"], content: "# Example\n## Next\n\nNext body." },
      ]);
    });

    it("ignores fence text inside actual HTML comments", () => {
      const chunks = chunkMarkdown(`<!--\n${fence}md\n# Hidden\n-->\n<!-- Also hidden -->\n# Visible\n\nVisible body.\n`);

      expect(chunks).toHaveLength(1);
      expect(chunks[0]).toMatchObject({
        headingPath: ["Visible"],
        startLine: 6,
        endLine: 8,
        content: "# Visible\n\nVisible body.",
      });
    });
  });

  it("ignores headings inside HTML comments and parses empty closing headings", () => {
    const chunks = chunkMarkdown(`<!--
# Hidden
-->
# #

Empty heading body.

# Visible

Visible body.
`);

    expect(chunks.map((chunk) => ({ headingPath: chunk.headingPath, content: chunk.content }))).toEqual([
      { headingPath: [""], content: "# #\n\nEmpty heading body." },
      { headingPath: ["Visible"], content: "# Visible\n\nVisible body." },
    ]);
  });
});

describe("Markdown indexing and search", () => {
  it("indexes, searches, updates, and deletes Markdown chunks", async () => {
    const root = temporaryRoot();
    write(root, "docs/guide.md", `# Guide

## Authentication

Validate bearer tokens and authenticated sessions.

## Deployment

Publish a container image to production.
`);
    const provider: EmbeddingProvider = {
      profile: { provider: "controlled", model: "markdown", dimensions: 2 },
      embedDocuments: async (inputs) => inputs.map((input) => input.includes("bearer") ? [1, 0] : [0, 1]),
      embedQuery: async () => [1, 0],
    };
    const index = new CodeIndex({ rootDir: root, provider });

    const added = await index.updateFiles({ upsert: ["docs/guide.md"] });
    expect(added).toMatchObject({ filesUpdated: 1, functionsAdded: 0, embeddingsCreated: 2 });
    expect(index.status()).toMatchObject({ fileCount: 1, functionCount: 0, markdownChunkCount: 2 });
    expect(index.allMarkdownChunks().map((chunk) => chunk.headingPath)).toEqual([
      ["Guide", "Authentication"],
      ["Guide", "Deployment"],
    ]);

    const results = await index.searchMarkdown({ query: "session authentication", limit: 1 });
    expect(results[0]).toMatchObject({
      similarity: 1,
      chunk: { path: "docs/guide.md", headingPath: ["Guide", "Authentication"], startLine: 3 },
    });

    write(root, "docs/guide.md", "# Guide\n\nOnly one section remains.\n");
    await index.updateFiles({ upsert: ["docs/guide.md"] });
    expect(index.status().markdownChunkCount).toBe(1);

    await index.updateFiles({ delete: ["docs/guide.md"] });
    expect(index.status()).toMatchObject({ fileCount: 0, markdownChunkCount: 0 });
    index.close();
  });

  it("migrates schema 10 and scans unchanged Git Markdown files", async () => {
    const root = temporaryRoot();
    initGit(root);
    write(root, "code.ts", "export function value() { return 1; }\n");
    write(root, "guide.md", "# Guide\n\nMigration documentation.\n");
    commitAll(root, "initial");
    const provider = new FakeEmbeddingProvider();
    const original = new CodeIndex({ rootDir: root, provider });
    await original.updateFromGit();
    const indexPath = original.indexPath;
    original.close();

    const oldDatabase = new DatabaseSync(indexPath, { allowExtension: true });
    sqliteVec.load(oldDatabase);
    oldDatabase.enableLoadExtension(false);
    oldDatabase.exec(`
      DELETE FROM files WHERE path = 'guide.md';
      DROP INDEX markdown_chunks_path;
      DROP INDEX markdown_chunks_embedding;
      DROP TABLE markdown_chunks;
      UPDATE metadata SET value = '10' WHERE key = 'schema_version';
    `);
    oldDatabase.close();

    const migrated = new CodeIndex({ rootDir: root, provider });
    await migrated.updateFromGit();
    expect(migrated.status()).toMatchObject({ fileCount: 2, markdownChunkCount: 1 });
    expect(migrated.allMarkdownChunks()[0]).toMatchObject({ path: "guide.md", headingPath: ["Guide"] });
    migrated.close();

    const database = new DatabaseSync(indexPath, { readOnly: true });
    expect(database.prepare("SELECT value FROM metadata WHERE key = 'schema_version'").get()).toEqual({ value: "11" });
    database.close();
  });

  it("clears source descriptions when a file is renamed to Markdown", async () => {
    const root = temporaryRoot();
    write(root, "guide.ts", "export function guide() { return 'documentation'; }\n");
    const descriptions: DescriptionProvider = {
      profile: { provider: "test", model: "descriptions", strategyVersion: "test-v1" },
      describeFile: async () => "A guide file.",
      describe: async () => "Returns documentation.",
    };
    const index = new CodeIndex({ rootDir: root, provider: new FakeEmbeddingProvider(), descriptionProvider: descriptions });
    await index.updateFiles({ upsert: ["guide.ts"] });
    await index.useDescriptions();
    expect(index.status()).toMatchObject({ describableFileCount: 1, fileDescriptionCount: 1 });

    renameSync(`${root}/guide.ts`, `${root}/guide.md`);
    await index.updateFiles({ renames: [{ from: "guide.ts", to: "guide.md" }] });

    expect(index.status()).toMatchObject({
      functionCount: 0,
      markdownChunkCount: 1,
      describableFileCount: 0,
      fileDescriptionCount: 0,
      staleFileDescriptionCount: 0,
    });
    index.close();
  });

  it("reranks Markdown using path and chunk content", async () => {
    const root = temporaryRoot();
    write(root, "guide.md", "# First\n\nOne.\n\n# Second\n\nTwo.\n");
    const documents: string[][] = [];
    const index = new CodeIndex({
      rootDir: root,
      provider: {
        profile: { provider: "controlled", model: "equal", dimensions: 2 },
        embedDocuments: async (inputs) => inputs.map(() => [1, 0]),
        embedQuery: async () => [1, 0],
      },
      reranker: {
        profile: { provider: "test", model: "reranker" },
        rerank: async (_query, values) => {
          documents.push([...values]);
          return [{ index: 1, score: 0.9 }];
        },
      },
    });
    await index.updateFiles({ upsert: ["guide.md"] });

    const results = await index.searchMarkdown({ query: "second", limit: 1 });

    expect(documents[0]).toEqual([
      "path: guide.md\n# First\n\nOne.",
      "path: guide.md\n# Second\n\nTwo.",
    ]);
    expect(results[0]).toMatchObject({ rerankScore: 0.9, chunk: { headingPath: ["Second"] } });
    index.close();
  });

  it("searches code and Markdown together and supports index selection", async () => {
    const root = temporaryRoot();
    write(root, "code.ts", "export function authenticate() { return 'session'; }\n");
    write(root, "guide.md", "# Deployment\n\nPublish the release artifact.\n");
    const provider: EmbeddingProvider = {
      profile: { provider: "controlled", model: "combined-search", dimensions: 2 },
      embedDocuments: async (inputs) => inputs.map((input) => input.includes("authenticate") ? [1, 0] : [0, 1]),
      embedQuery: async () => [1, 0],
    };
    const index = new CodeIndex({ rootDir: root, provider });
    await index.updateFiles({ upsert: ["code.ts", "guide.md"] });

    const combined = await index.search({ query: "session", minSimilarity: -1 });
    expect(combined.map((result) => result.type)).toEqual(["function", "markdown"]);
    expect(combined[0]).toMatchObject({ type: "function", function: { name: "authenticate" }, similarity: 1 });

    const code = await index.search({ query: "session", indexes: ["code"] });
    expect(code).toMatchObject([{ type: "function", function: { name: "authenticate" } }]);
    const markdown = await index.search({ query: "session", indexes: ["markdown"], minSimilarity: -1 });
    expect(markdown).toMatchObject([{ type: "markdown", chunk: { headingPath: ["Deployment"] } }]);
    await expect(index.search({ query: "session", indexes: ["descriptions"] }))
      .rejects.toThrow(/Descriptions are not enabled/);
    index.close();
  });
});
