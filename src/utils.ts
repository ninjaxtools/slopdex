import { createHash } from "node:crypto";
import path from "node:path";

import { CodeIndexError } from "./errors.js";

export function sha256(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

export function normalizeRelativePath(rootDir: string, inputPath: string): string {
  const absolute = path.resolve(rootDir, inputPath);
  const relative = path.relative(rootDir, absolute);
  if (relative === "" || relative === ".") {
    throw new CodeIndexError(`Expected a file path, received repository root: ${inputPath}`);
  }
  if (relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) {
    throw new CodeIndexError(`Path escapes repository root: ${inputPath}`);
  }
  return relative.split(path.sep).join("/");
}

export function throwIfAborted(signal?: AbortSignal): void {
  signal?.throwIfAborted();
}

export function chunk<T>(values: readonly T[], size: number): T[][] {
  const batches: T[][] = [];
  for (let index = 0; index < values.length; index += size) {
    batches.push(values.slice(index, index + size));
  }
  return batches;
}

export function assertPositiveInteger(value: number, name: string): void {
  if (!Number.isInteger(value) || value < 1) {
    throw new CodeIndexError(`${name} must be a positive integer.`);
  }
}

export function compileNameRegex(pattern: string | undefined, label = "name regex"): RegExp | undefined {
  if (pattern === undefined) return undefined;
  try {
    return new RegExp(pattern);
  } catch (error) {
    throw new CodeIndexError(`Invalid ${label}: ${error instanceof Error ? error.message : String(error)}`, { cause: error });
  }
}

export function normalizeEmbeddingVector(value: unknown, dimensions: number): number[] {
  if (!Array.isArray(value) || value.length !== dimensions) {
    throw new CodeIndexError(`Expected a finite ${dimensions}-dimensional embedding vector.`);
  }
  const converted: number[] = [];
  for (const component of value) {
    if (typeof component !== "number" || !Number.isFinite(component)) {
      throw new CodeIndexError(`Expected a finite ${dimensions}-dimensional embedding vector.`);
    }
    const normalized = Math.fround(component);
    if (!Number.isFinite(normalized)) {
      throw new CodeIndexError(`Expected a finite ${dimensions}-dimensional embedding vector.`);
    }
    converted.push(normalized);
  }
  if (!converted.some((component) => component !== 0)) {
    throw new CodeIndexError("Embedding vector must not be all zeroes.");
  }
  return converted;
}
