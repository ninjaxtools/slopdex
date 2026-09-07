import { describe, expect, it } from "vitest";

import { parseCallables } from "../src/parser/callable-parser.js";

describe("parseCallables", () => {
  it("extracts TypeScript declarations, methods, arrows, and nested functions", () => {
    const functions = parseCallables("src/example.ts", `
export async function load(id: string): Promise<string> {
  function normalize(value: string) { return value.trim(); }
  return normalize(id);
}
const save = (value: string) => value;
class Store {
  constructor() {}
  static find(id: string) { return id; }
}
`);

    expect(functions.map((item) => [item.qualifiedName, item.kind])).toEqual([
      ["load", "function"],
      ["load.normalize", "function"],
      ["save", "function"],
      ["Store.constructor", "constructor"],
      ["Store.find", "method"],
    ]);
    expect(functions.map((item) => [item.qualifiedName, item.lineCount])).toEqual([
      ["load", 4],
      ["load.normalize", 1],
      ["save", 1],
      ["Store.constructor", 1],
      ["Store.find", 1],
    ]);
  });

  it("extracts JavaScript object properties and assignments", () => {
    const functions = parseCallables("index.js", `
const api = {
  get(id) { return id; },
  put: function(value) { return value; }
};
module.exports.remove = (id) => id;
`);

    expect(functions.map((item) => item.qualifiedName)).toEqual([
      "api.get",
      "api.put",
      "remove",
    ]);
  });

  it("warns and retains recoverable callables when syntax is invalid", () => {
    const warnings: string[] = [];
    const functions = parseCallables(
      "bad.ts",
      "export function valid() { return true; } function broken( {",
      (message) => warnings.push(message),
    );

    expect(functions.map((item) => item.qualifiedName)).toEqual(["valid"]);
    expect(warnings).toEqual([
      "Cannot fully parse bad.ts: tree-sitter reported syntax errors; indexing recoverable callables only.",
    ]);
  });

  it("accepts type-only star exports unsupported by the tree-sitter grammar", () => {
    const functions = parseCallables("index.ts", `
export type * from "./types.js";
export type * as Models from "./models.js";
export function create() { return {}; }
`);

    expect(functions.map((item) => item.qualifiedName)).toEqual(["create"]);
  });

  it("accepts import type expressions unsupported by the tree-sitter grammar", () => {
    const functions = parseCallables("import-types.ts", `
async function load(original: <T>() => Promise<T>) {
  return await original<
    typeof import("./application.js")
  >();
}
type Input = import("ai").InferToolInput<typeof load>;
`);

    expect(functions.map((item) => item.qualifiedName)).toEqual(["load"]);
  });

  it("accepts generic tagged templates unsupported by the tree-sitter grammar", () => {
    const functions = parseCallables("query.ts", `
const query = sql<{ id: number }>\`
  SELECT id FROM records
\`;
export function execute() { return query; }
`);

    expect(functions.map((item) => item.qualifiedName)).toEqual(["execute"]);
  });

  it("parses files larger than tree-sitter's default input buffer", () => {
    const padding = "// padding\n".repeat(4_000);
    const functions = parseCallables("large.ts", `${padding}\nexport function afterPadding() { return true; }\n`);

    expect(functions.map((item) => item.qualifiedName)).toEqual(["afterPadding"]);
  });

  it("uses namespace and class-field bindings as callable scopes", () => {
    const functions = parseCallables("fields.ts", `
namespace Services {
  export class User {
    load = () => 1;
    save = function internalName() { return 2; };
  }
}
`);

    expect(functions.map((item) => [item.qualifiedName, item.kind])).toEqual([
      ["Services.User.load", "method"],
      ["Services.User.save", "method"],
    ]);
  });
});
