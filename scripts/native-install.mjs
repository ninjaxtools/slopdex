#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { binaryPaths, binaryProblem, buildRequirements, packageRoot, packageVersion } from "./native-common.mjs";

export function installNative(root = packageRoot) {
  const version = packageVersion(root);
  const { prebuilt, built } = binaryPaths(root);
  for (const binary of [built, prebuilt]) {
    if (!existsSync(binary)) continue;
    if (process.platform !== "win32") chmodSync(binary, 0o755);
    const problem = binaryProblem(binary, version);
    if (!problem) {
      if (binary !== built) {
        mkdirSync(path.dirname(built), { recursive: true });
        copyFileSync(binary, built);
        if (process.platform !== "win32") chmodSync(built, 0o755);
      }
      return built;
    }
    console.error(`slopdex: cannot use ${binary}: ${problem}`);
  }

  console.error(`slopdex: no usable native binary; compiling the bundled Rust sources. ${buildRequirements}`);
  for (const file of ["Cargo.toml", "Cargo.lock"]) {
    if (!existsSync(path.join(root, file))) throw new Error(`Source package is incomplete: missing ${file}.`);
  }
  // npm can invoke lifecycle scripts from a different working directory. Keep
  // the build and output inside this package, independent of Cargo's target-dir.
  const env = { ...process.env };
  delete env.CARGO_BUILD_TARGET;
  const result = spawnSync("cargo", [
    "build", "--locked", "--release", "--bin", "slopdex",
    "--manifest-path", path.join(root, "Cargo.toml"),
    "--target-dir", path.join(root, "target"),
  ], { cwd: root, env, stdio: "inherit" });
  if (result.error || result.status !== 0) {
    throw new Error(`Native build failed: ${result.error?.message ?? result.signal ?? `cargo exited ${result.status}`}. ${buildRequirements}`);
  }
  const problem = binaryProblem(built, version);
  if (problem) throw new Error(`Built executable is unusable: ${problem}. Expected ${built}.`);
  return built;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    installNative();
  } catch (error) {
    console.error(`slopdex: ${error.message}`);
    process.exitCode = 1;
  }
}
