import { renameSync, rmSync } from "node:fs";
import path from "node:path";

import { describe, expect, it, onTestFinished } from "vitest";

import { analyzeCohesion } from "../src/analysis/cohesion.js";
import { CodeIndex } from "../src/code-index.js";
import { languageForPath, parseCallables } from "../src/parser/callable-parser.js";
import { crossSearch } from "../src/search/cross-search.js";
import { SourcePolicy } from "../src/source-policy.js";
import type { CallableKind, SummaryInput, SupportedLanguage } from "../src/types.js";
import { FakeEmbeddingProvider, commitAll, initGit, temporaryRoot, write } from "./helpers.js";

const fixtures: Array<{ language: SupportedLanguage; path: string; source: string; expected: Array<[string, CallableKind]> }> = [
  {
    language: "python", path: "src/service.py", source: `# Unicode before callables: café 🚀
@logged
async def load(value: str) -> str:
    def normalize(text):
        return text.strip()
    double = lambda n: n * 2
    return normalize(value)
class Store:
    def __init__(self):
        self.values = []
    @staticmethod
    def create():
        def nested():
            yield 1
        return list(nested())
    def values(self):
        yield from self.values
`, expected: [["load", "function"], ["load.normalize", "function"], ["load.double", "function"],
      ["Store.__init__", "constructor"], ["Store.create", "method"], ["Store.create.nested", "generator"], ["Store.values", "generator"]],
  },
  {
    language: "javascript", path: "src/service.js", source: `// café 🚀
export function load(value) { return value; }
const double = n => n * 2;
class Store { *values() { yield 1; } }
`, expected: [["load", "function"], ["double", "function"], ["Store.values", "generator"]],
  },
  {
    language: "jsx", path: "src/View.jsx", source: `// café 🚀
export const View = ({ title }) => <section>{title}</section>;
class Screen { render() { return <View title="Hello" />; } }
`, expected: [["View", "function"], ["Screen.render", "method"]],
  },
  {
    language: "typescript", path: "src/service.ts", source: `// café 🚀
export function load<T>(value: T): T { return value; }
interface Store { get(): string; }
const double = (n: number): number => n * 2;
`, expected: [["load", "function"], ["double", "function"]],
  },
  {
    language: "tsx", path: "src/View.tsx", source: `// café 🚀
export const View = <T,>({ title }: { title: T }) => <section>{String(title)}</section>;
class Screen { render(): JSX.Element { return <View title="Hello" />; } }
`, expected: [["View", "function"], ["Screen.render", "method"]],
  },
  {
    language: "rust", path: "src/service.rs", source: `// café 🚀
mod api {
    trait Read {
        fn read(&self);
        fn default(&self) -> i32 { 1 }
    }
    impl<T> Read for Store<T> {
        fn read(&self) { let next = |x: i32| x + 1; }
    }
    impl Store {
        pub fn new() -> Self { todo!() }
        pub async fn load(&self) -> i32 { 1 }
    }
    fn outer() { fn inner() {} }
}
extern "C" { fn external(); }
macro_rules! generated { () => { fn hidden() {} } }
`, expected: [["api.Read.default", "method"], ["api.<Store<T> as Read>.read", "method"],
      ["api.<Store<T> as Read>.read.next", "function"], ["api.Store.new", "method"], ["api.Store.load", "method"],
      ["api.outer", "function"], ["api.outer.inner", "function"]],
  },
  {
    language: "go", path: "src/service.go", source: `// café 🚀
package service
func (s *Store[T]) Get(value int) (int, error) {
    next := func(x int) int { return x + 1 }
    return next(value), nil
}
func (s Store[T]) Save() {}
func Load() {}
func external()
var run = func() {}
func outer() {
    first, second := func() {}, func() {}
    invoke(func() {})
}
`, expected: [["Store[T].Get", "method"], ["Store[T].Get.next", "function"], ["Store[T].Save", "method"],
      ["Load", "function"], ["run", "function"], ["outer", "function"], ["outer.first", "function"], ["outer.second", "function"]],
  },
  {
    language: "java", path: "src/Store.java", source: `// café 🚀
package app;
abstract class Store {
    Store() {}
    public int get(int value) { return value; }
    public String get(String value) { return value; }
    abstract void external();
    Runnable run = () -> {};
    class Nested { void save() {} }
}
interface Read { void read(); default int value() { return 1; } }
record Item(int id) { Item { if (id < 0) throw new IllegalArgumentException(); } }
enum Mode { ONE { int value() { return 1; } }; abstract int value(); }
`, expected: [["Store.Store", "constructor"], ["Store.get", "method"], ["Store.get", "method"], ["Store.run", "function"],
      ["Store.Nested.save", "method"], ["Read.value", "method"], ["Item.Item", "constructor"], ["Mode.ONE.value", "method"]],
  },
  {
    language: "c", path: "src/service.c", source: `// café 🚀
int prototype(int value);
typedef int (*Callback)(int);
static int *get(int value) { return 0; }
int (*factory(void))(int) { return 0; }
#ifdef ENABLED
int load(void) { const char *text = "int fake(void) {}"; return 1; }
#else
int fallback(void) { return 0; }
#endif
`, expected: [["get", "function"], ["factory", "function"], ["load", "function"], ["fallback", "function"]],
  },
];

describe("Tree-sitter languages", () => {
  it("adds Python function docstrings to the embedding input as documentation", () => {
    const [documented, undocumented] = parseCallables("service.py", `
def documented(value):
    """Normalize a value for storage."""
    return value.strip()

def undocumented(value):
    return value
`);

    expect(documented!.embeddingInput).toContain('documentation:\n"""Normalize a value for storage."""\nsource:');
    expect(undocumented!.embeddingInput).not.toContain("documentation:");
  });

  it.each(fixtures)("extracts $language callables with scopes, kinds, signatures, and source locations", (fixture) => {
    const warnings: string[] = [];
    const callables = parseCallables(fixture.path, fixture.source, (message) => warnings.push(message));
    expect(warnings).toEqual([]);
    expect(callables.map((item) => [item.qualifiedName, item.kind])).toEqual(fixture.expected);
    expect(new Set(callables.map((item) => item.identityKey)).size).toBe(callables.length);
    const lines = fixture.source.split("\n");
    for (const item of callables) {
      expect(item.language).toBe(fixture.language);
      expect(item.signature).toBeTruthy();
      expect(item.embeddingInput).toContain(`language: ${fixture.language}`);
      expect(item.embeddingInput).toContain(`symbol: ${item.qualifiedName}`);
      const selected = lines.slice(item.startLine - 1, item.endLine);
      selected[selected.length - 1] = selected.at(-1)!.slice(0, item.endColumn - 1);
      selected[0] = selected[0]!.slice(item.startColumn - 1);
      expect(selected.join("\n")).toBe(item.source);
      expect(item.lineCount).toBe(selected.length);
    }
  });

  it.each(fixtures.filter((fixture) => ["python", "rust", "go", "java", "c"].includes(fixture.language)))(
    "retains valid $language definitions and warns on malformed syntax", (fixture) => {
      const warnings: string[] = [];
      const callables = parseCallables(fixture.path, `${fixture.source}\n???`, (message) => warnings.push(message));
      expect(warnings).toHaveLength(1);
      expect(warnings[0]).toContain(fixture.path);
      expect(callables.map((item) => [item.qualifiedName, item.kind])).toEqual(fixture.expected);
    },
  );

  it("preserves decorators and language-specific signatures", () => {
    const parse = (language: SupportedLanguage) => {
      const fixture = fixtures.find((item) => item.language === language)!;
      return parseCallables(fixture.path, fixture.source);
    };
    expect(parse("python")[0]).toMatchObject({
      startLine: 2, endLine: 7, signature: "async def load(value: str) -> str:",
    });
    expect(parse("python")[0]!.source).toMatch(/^@logged\nasync def/);
    expect(parse("go")[0]!.signature).toBe("func (s *Store[T]) Get(value int) (int, error)");
    expect(parse("rust").find((item) => item.name === "load")!.signature).toBe("pub async fn load(&self) -> i32");
    expect(parse("java").filter((item) => item.name === "get").map((item) => item.signature))
      .toEqual(["public int get(int value)", "public String get(String value)"]);
    expect(parse("c")[1]!.signature).toBe("int (*factory(void))(int)");
  });

  it("recognizes extensions and excludes dependency and build trees", () => {
    const policy = new SourcePolicy();
    for (const fixture of fixtures) expect(policy.includes(fixture.path)).toBe(true);
    for (const [extension, language] of Object.entries({
      mts: "typescript", cts: "typescript", mjs: "javascript", cjs: "javascript", pyw: "python", h: "c",
    })) expect(languageForPath(`source.${extension}`)).toBe(language);
    expect(parseCallables("source.h", "int proto(void); static inline int ready(void) { return 1; }")
      .map((item) => item.name)).toEqual(["ready"]);
    for (const file of [".venv/lib/site.py", "venv/site.py", "__pycache__/cache.py", "target/debug/build/output.rs", "vendor/lib.go", "build/Generated.java", "generated/api.h", "notes.txt", "source.cpp"])
      expect(policy.includes(file)).toBe(false);
    expect(new SourcePolicy(["**/*.py"]).includes("src/service.go")).toBe(false);
    expect(new SourcePolicy([], ["**/*_test.go"]).includes("src/service_test.go")).toBe(false);
  });
});

describe("multilingual indexing", () => {
  it.each(["git", "working-tree"] as const)("indexes, searches, summarizes, refreshes, and removes languages from %s", async (mode) => {
    const root = temporaryRoot();
    if (mode === "git") initGit(root);
    for (const fixture of fixtures) write(root, fixture.path, fixture.source);
    write(root, ".venv/ignored.py", "def ignored(): pass\n");
    write(root, "target/ignored.rs", "fn ignored() {}\n");
    if (mode === "git") commitAll(root, "Add languages");
    const inputs: SummaryInput[] = [];
    const index = new CodeIndex({
      rootDir: root, provider: new FakeEmbeddingProvider(),
      summaryProvider: {
        profile: { provider: "test", model: "purpose", strategyVersion: "v1" },
        async summarize(input) { inputs.push(input); return `Implement ${input.callable.qualifiedName} in ${input.callable.language}`; },
      },
    });
    onTestFinished(() => index.close());
    const refresh = () => mode === "git" ? index.updateFromGit() : index.updateFromWorkingTree();
    const expectedCount = fixtures.reduce((sum, fixture) => sum + fixture.expected.length, 0);
    expect((await refresh()).functionsAdded).toBe(expectedCount);
    expect(index.status().fileCount).toBe(fixtures.length);
    const functions = index.allFunctions();
    expect(new Set(functions.map((item) => item.language)).size).toBe(9);
    expect((await refresh()).embeddingsCreated).toBe(0);
    expect(index.allFunctions().map((item) => item.id)).toEqual(functions.map((item) => item.id));
    expect(await index.similaritySearch({ query: "load values", limit: expectedCount })).toHaveLength(expectedCount);

    await index.useSummaries();
    expect(index.status().summaryCount).toBe(expectedCount);
    expect(new Set(inputs.map((input) => input.callable.language)).size).toBe(9);
    for (const input of inputs) expect(input.fileSource).toBe(fixtures.find((fixture) => fixture.path === input.callable.path)!.source);
    expect(await index.searchSummary({ query: "load values", limit: expectedCount })).toHaveLength(expectedCount);
    const report = await analyzeCohesion({ source: index, minLines: 1, minSimilarity: -1 });
    expect(report.summary.functionsAnalyzed).toBe(expectedCount);
    expect(report.parameters.similarityMode).toBe("code-summary-average");
    let crossLanguage = false;
    for await (const result of crossSearch({ source: index, minLines: 1, limitPerFunction: expectedCount })) {
      crossLanguage ||= result.matches.some((match) => match.function.language !== result.source.language);
      expect(result.scoring?.similarityMode).toBe("code-summary-average");
    }
    expect(crossLanguage).toBe(true);

    const python = fixtures[0]!;
    const previousId = functions.find((item) => item.path === python.path && item.name === "load")!.id;
    renameSync(path.join(root, python.path), path.join(root, "src/renamed.py"));
    write(root, "src/service.rs", `${fixtures.find((fixture) => fixture.language === "rust")!.source}\nfn added() {}\n`);
    rmSync(path.join(root, "src/service.c"));
    if (mode === "git") commitAll(root, "Rename Python, update Rust, remove C");
    await refresh();
    expect(index.allFunctions().some((item) => item.language === "c")).toBe(false);
    expect(index.allFunctions().some((item) => item.language === "rust" && item.name === "added")).toBe(true);
    expect(index.allFunctions().some((item) => item.path === "src/renamed.py" && item.name === "load")).toBe(true);
    if (mode === "git") expect(index.allFunctions().find((item) => item.path === "src/renamed.py" && item.name === "load")!.id).toBe(previousId);
    expect(index.status().summaryCount).toBe(index.status().functionCount);
  });
});
