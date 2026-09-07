import path from "node:path";

import { languageForPath } from "./parser/callable-parser.js";

const DEFAULT_EXCLUDED_SEGMENTS = new Set([
  ".git",
  ".slopdex",
  "node_modules",
  "dist",
  "build",
  "coverage",
  "vendor",
  "generated",
]);

export class SourcePolicy {
  readonly #include: readonly string[];
  readonly #exclude: readonly string[];

  public constructor(include: readonly string[] = [], exclude: readonly string[] = []) {
    this.#include = include;
    this.#exclude = exclude;
  }

  public includes(relativePath: string): boolean {
    const normalized = relativePath.replaceAll("\\", "/");
    if (!languageForPath(normalized)) return false;
    if (hasExcludedSegment(normalized)) return false;
    if (this.#exclude.some((pattern) => path.matchesGlob(normalized, pattern))) return false;
    return this.#include.length === 0 || this.#include.some((pattern) => path.matchesGlob(normalized, pattern));
  }

  public traversesDirectory(relativePath: string): boolean {
    return !hasExcludedSegment(relativePath.replaceAll("\\", "/"));
  }
}

function hasExcludedSegment(relativePath: string): boolean {
  return relativePath.split("/").some((segment) => DEFAULT_EXCLUDED_SEGMENTS.has(segment));
}
