import { lstat, readFile } from "node:fs/promises";
import { lstatSync, readFileSync } from "node:fs";
import path from "node:path";

import { IndexDatabase } from "../storage/database.js";
import { CodeIndexError } from "../errors.js";
import { CALLABLE_PARSER_CACHE_VERSION, parseFileCallables } from "../parser/callable-parser.js";
import { languageForPath } from "../parser/languages.js";
import { chunkMarkdown, isMarkdownPath } from "../parser/markdown.js";
import { sha256 } from "../utils.js";
import type { PreparedCallable, PreparedFile, PreparedMarkdownChunk } from "./prepared.js";

type FileProvenance = Pick<PreparedFile, "blobOid" | "sourceMode" | "indexedCommit" | "previousPath" | "replacePath">;

export class FilePreparer {
  readonly #rootDir: string;
  public readonly maxFileSize: number;
  readonly #database: IndexDatabase;
  readonly #onWarning: (message: string) => void;

  public constructor(
    rootDir: string,
    maxFileSize: number,
    database: IndexDatabase,
    onWarning: (message: string) => void,
  ) {
    this.#rootDir = rootDir;
    this.maxFileSize = maxFileSize;
    this.#database = database;
    this.#onWarning = onWarning;
  }

  public prepareFile(relativePath: string, buffer: Buffer, provenance: FileProvenance): PreparedFile {
    const content = buffer.toString("utf8");
    const contentHash = sha256(content);
    const markdown = isMarkdownPath(relativePath);
    const parseKey = sha256(`${CALLABLE_PARSER_CACHE_VERSION}\0${relativePath}\0${contentHash}`);
    let parsed = markdown ? { callables: [], errors: [] } : this.#database.cachedParse(parseKey);
    if (!parsed) {
      parsed = parseFileCallables(relativePath, content, this.#onWarning);
      if (!parsed.errors.some((error) => error.code === "parse-failed" || error.code === "extraction-error")) {
        this.#database.storeParse(parseKey, parsed);
      }
    } else if (parsed.errors.length > 0) {
      this.#onWarning(`Cannot fully parse ${relativePath}: tree-sitter reported syntax errors; indexing recoverable callables only.`);
    }
    const language = markdown ? "markdown" : languageForPath(relativePath) ?? path.extname(relativePath).slice(1);
    return {
      path: relativePath,
      contentHash,
      blobOid: provenance.blobOid,
      sourceMode: provenance.sourceMode,
      indexedCommit: provenance.indexedCommit,
      language,
      byteSize: buffer.byteLength,
      source: content,
      ...(provenance.previousPath ? { previousPath: provenance.previousPath } : {}),
      ...(provenance.replacePath ? { replacePath: provenance.replacePath } : {}),
      callables: parsed.callables as PreparedCallable[],
      markdownChunks: markdown
        ? chunkMarkdown(content).map((chunk): PreparedMarkdownChunk => ({ ...chunk, embeddingKey: "" }))
        : [],
      errors: parsed.errors,
    };
  }

  public async prepareWorkingFile(
    relativePath: string,
    provenance: FileProvenance,
    strict = false,
  ): Promise<PreparedFile | null> {
    let size = 0;
    try {
      const absolutePath = path.join(this.#rootDir, relativePath);
      const info = await lstat(absolutePath);
      if (info.isSymbolicLink() || !info.isFile()) {
        if (strict) throw new CodeIndexError(`Not a regular file or symbolic links are not supported: ${relativePath}`);
        return null;
      }
      size = info.size;
      if (size > this.maxFileSize) {
        return this.failedFile(relativePath, "file-too-large", `File exceeds maxFileSize (${this.maxFileSize} bytes).`, provenance, size);
      }
      return this.prepareFile(relativePath, await readFile(absolutePath), provenance);
    } catch (error) {
      if (error instanceof CodeIndexError) throw error;
      return this.failedFile(relativePath, "read-error", error instanceof Error ? error.message : String(error), provenance, size);
    }
  }

  public failedFile(
    relativePath: string,
    code: "read-error" | "file-too-large",
    message: string,
    provenance: FileProvenance,
    byteSize: number,
  ): PreparedFile {
    this.#onWarning(`Cannot index ${relativePath}: ${message}`);
    return {
      ...provenance, path: relativePath, contentHash: sha256(""), source: "", byteSize,
      language: isMarkdownPath(relativePath) ? "markdown" : languageForPath(relativePath) ?? "unknown",
      callables: [], markdownChunks: [], unavailable: true,
      errors: [{
        path: relativePath, language: languageForPath(relativePath), scope: "file", code, message,
        qualifiedName: null, startLine: null, startColumn: null, endLine: null, endColumn: null, source: null,
      }],
    };
  }

  public async workingFileChanged(file: PreparedFile): Promise<boolean> {
    try {
      const info = await lstat(path.join(this.#rootDir, file.path));
      if (!info.isFile() || info.isSymbolicLink() || info.size !== file.byteSize) return true;
      if (info.size > this.maxFileSize) return !file.errors.some((error) => error.code === "file-too-large");
      const content = await readFile(path.join(this.#rootDir, file.path));
      return file.unavailable === true || sha256(content.toString("utf8")) !== file.contentHash;
    } catch {
      return file.unavailable !== true;
    }
  }

  public workingFileChangedSynchronously(file: PreparedFile): boolean {
    try {
      const absolutePath = path.join(this.#rootDir, file.path);
      const info = lstatSync(absolutePath);
      if (!info.isFile() || info.isSymbolicLink() || info.size !== file.byteSize) return true;
      if (info.size > this.maxFileSize) return !file.errors.some((error) => error.code === "file-too-large");
      const contentHash = sha256(readFileSync(absolutePath, "utf8"));
      return file.unavailable === true || contentHash !== file.contentHash;
    } catch {
      return file.unavailable !== true;
    }
  }
}
