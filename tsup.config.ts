import { readFileSync } from "node:fs";

import { defineConfig } from "tsup";

const { version } = JSON.parse(readFileSync(new URL("./package.json", import.meta.url), "utf8")) as { version: string };

export default defineConfig({
  entry: ["src/index.ts", "src/cli.ts"],
  format: ["esm"],
  dts: true,
  sourcemap: true,
  clean: true,
  // Enabled so `src/cli.ts` dynamic imports stay lazy: --help/--version load
  // only the light CLI chunk, not the heavy index/provider graph (ai SDK,
  // tree-sitter, sqlite-vec).
  splitting: true,
  removeNodeProtocol: false,
  define: { __SLOPDEX_VERSION__: JSON.stringify(version) },
});
