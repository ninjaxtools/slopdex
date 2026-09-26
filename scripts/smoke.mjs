import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { packageRoot, packageVersion } from "./native-common.mjs";

// No credentials or network calls: run from outside the package to catch
// accidental cwd-relative resolution in globally installed npm launchers.
const cwd = mkdtempSync(path.join(tmpdir(), "slopdex-smoke-"));
try {
  const launcher = path.join(packageRoot, "scripts", "native-launcher.mjs");
  const run = (...args) => execFileSync(process.execPath, [launcher, ...args], { cwd, encoding: "utf8" });
  const version = run("--version").trim();
  assert.ok([packageVersion(), `slopdex ${packageVersion()}`].includes(version), `Unexpected CLI version: ${version}`);
  const help = run("--help");
  for (const command of ["search", "cross-search", "describe"]) {
    assert.ok(help.includes(command), `CLI help does not list ${command}`);
  }
  console.log("Native CLI smoke passed (version and help, outside package directory).");
} finally {
  rmSync(cwd, { recursive: true, force: true });
}
