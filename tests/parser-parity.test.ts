import { describe, expect, it } from "vitest";

import { parseCallables } from "../src/parser/callable-parser.js";
import { parityFixtures } from "./parser-parity-fixtures.js";

describe("treesitter-index callable parity", () => {
  it.each(parityFixtures)("preserves $language implementation symbols and signature details", (fixture) => {
    const warnings: string[] = [];
    const functions = parseCallables(`parity.${fixture.extension}`, fixture.source, (message) => warnings.push(message));
    expect(warnings).toEqual([]);
    expect(functions.map((item) => [item.qualifiedName, item.kind, item.signature])).toEqual(fixture.expected);
    expect(new Set(functions.map((item) => item.identityKey)).size).toBe(functions.length);
    for (const item of functions) {
      expect(item.embeddingInput).toContain(`signature: ${item.signature}`);
      expect(fixture.source).toContain(item.source);
    }
  });
});
