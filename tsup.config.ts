import { readFileSync } from "node:fs";

import { defineConfig } from "tsup";

const { version } = JSON.parse(readFileSync(new URL("./package.json", import.meta.url), "utf8")) as { version: string };

export default defineConfig({
  entry: ["src/index.ts", "src/cli.ts"],
  format: ["esm"],
  dts: true,
  sourcemap: true,
  clean: true,
  splitting: false,
  removeNodeProtocol: false,
  define: { __SLOPDEX_VERSION__: JSON.stringify(version) },
});
