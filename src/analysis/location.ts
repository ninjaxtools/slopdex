import path from "node:path";

import type { CohesionLocation } from "../types.js";

export function cohesionLocation(leftPath: string, rightPath: string): CohesionLocation {
  const normalizedLeft = leftPath.replaceAll("\\", "/");
  const normalizedRight = rightPath.replaceAll("\\", "/");
  const leftDirectory = directoryParts(normalizedLeft);
  const rightDirectory = directoryParts(normalizedRight);
  let commonLength = 0;
  while (commonLength < leftDirectory.length
    && commonLength < rightDirectory.length
    && leftDirectory[commonLength] === rightDirectory[commonLength]) {
    commonLength += 1;
  }
  const commonParts = leftDirectory.slice(0, commonLength);
  const commonAncestor = commonParts.length > 0 ? commonParts.join("/") : null;
  const sameFile = normalizedLeft === normalizedRight;
  const folderHops = sameFile
    ? 0
    : leftDirectory.length - commonLength + rightDirectory.length - commonLength;
  return {
    category: sameFile ? "same-file" : folderHops === 0 ? "same-folder" : "different-folder",
    physicalDistance: sameFile ? 0 : 1 + folderHops,
    folderHops,
    commonAncestor,
    sourceTestPair: isTestPath(normalizedLeft) !== isTestPath(normalizedRight),
  };
}

function directoryParts(filePath: string): string[] {
  const directory = path.posix.dirname(filePath.replaceAll("\\", "/"));
  return directory === "." ? [] : directory.split("/").filter(Boolean);
}

function isTestPath(filePath: string): boolean {
  const original = filePath.replaceAll("\\", "/");
  const normalized = original.toLowerCase();
  const segments = normalized.split("/");
  const fileName = segments.at(-1) ?? "";
  return segments.some((segment) => segment === "test" || segment === "tests" || segment === "__tests__")
    || /\.(?:test|spec)\.[cm]?[jt]sx?$/.test(fileName)
    || /_test\.go$/.test(fileName)
    || /^(?:test_.+|.+_test)\.(?:py|pyw|rs|c|h)$/.test(fileName)
    || /^(?:Test.+|.+Tests?|.+TestCase)\.java$/.test(original.split("/").at(-1) ?? "");
}
