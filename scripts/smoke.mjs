import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const library = await import("../dist/index.js");
if (typeof library.openCodeIndex !== "function"
  || typeof library.crossSearch !== "function"
  || typeof library.analyzeCohesion !== "function") {
  throw new Error("Built library exports are incomplete.");
}

execFileSync(process.execPath, ["dist/cli.js", "--help"], { stdio: "ignore" });

// Exercise the native grammars through the built library, with no network calls.
const sources = [
  ["python", "sample.py", "def run():\n    return 1\n"],
  ["javascript", "sample.js", "function run() { return 1; }"],
  ["jsx", "sample.jsx", "const run = () => <div />;"],
  ["typescript", "sample.ts", "function run(): number { return 1; }"],
  ["tsx", "sample.tsx", "const run = (): JSX.Element => <div />;"],
  ["rust", "sample.rs", "fn run() -> i32 { 1 }"],
  ["go", "sample.go", "package sample\nfunc run() int { return 1 }"],
  ["java", "Sample.java", "class Sample { int run() { return 1; } }"],
  ["c", "sample.c", "int run(void) { return 1; }"],
  ["c", "sample.h", "static inline int run(void) { return 1; }"],
];
const root = mkdtempSync(path.join(tmpdir(), "slopdex-smoke-"));
let index;
const warnings = [];
try {
  for (const [, file, source] of sources) writeFileSync(path.join(root, file), source);
  writeFileSync(path.join(root, ".gitignore"), "ignored.py\n");
  writeFileSync(path.join(root, "ignored.py"), "def ignored():\n    return 1\n");
  index = library.openCodeIndex({
    rootDir: root,
    provider: {
      profile: { provider: "smoke", model: "local", dimensions: 2 },
      embedDocuments: async (inputs) => inputs.map(() => [1, 0]),
      embedQuery: async () => [1, 0],
    },
    onWarning: (message) => { warnings.push(message); },
  });
  await index.updateFromWorkingTree();
  const functions = index.allFunctions();
  if (warnings.length > 0) throw new Error(warnings.join("\n"));
  if (functions.length !== sources.length) throw new Error("Built library did not index all language fixtures.");
  for (const [language, file] of sources) {
    if (!functions.some((item) => item.path === file && item.language === language && item.name === "run")) {
      throw new Error(`Built library failed to parse ${file}.`);
    }
  }
  writeFileSync(path.join(root, "broken.py"), "def broken(\n");
  await index.updateFiles({ upsert: ["broken.py"] });
  if (index.indexErrors().length === 0 || library.readIndexErrors(index.indexPath).length === 0) {
    throw new Error("Built library did not persist indexing diagnostics.");
  }
} finally {
  index?.close();
  rmSync(root, { recursive: true, force: true });
}
