import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const packageRoot = fileURLToPath(new URL("../", import.meta.url));

export function platformKey(platform = process.platform, arch = process.arch, report) {
  if (platform !== "linux") return `${platform}-${arch}`;
  report ??= process.report.getReport();
  return `${platform}-${arch}-${report.header?.glibcVersionRuntime ? "gnu" : "musl"}`;
}

export function binaryPaths(root = packageRoot) {
  const name = process.platform === "win32" ? "slopdex.exe" : "slopdex";
  return {
    prebuilt: path.join(root, "native", platformKey(), name),
    built: path.join(root, "target", "release", name),
  };
}

export function packageVersion(root = packageRoot) {
  return JSON.parse(readFileSync(path.join(root, "package.json"), "utf8")).version;
}

export function binaryProblem(binary, version) {
  if (!existsSync(binary)) return "not present";
  const result = spawnSync(binary, ["--version"], { encoding: "utf8", timeout: 15_000 });
  if (result.error) return result.error.message;
  if (result.status !== 0) return `version check failed (${result.signal ?? result.status})`;
  const output = result.stdout.trim();
  if (output !== version && output !== `slopdex ${version}`) {
    return `reported ${JSON.stringify(output)}, expected slopdex ${version}`;
  }
  return undefined;
}

export const buildRequirements = "Building from source requires Rust (rustc and cargo), a C/C++ compiler and platform build tools on PATH. See docs/implementation.md for setup.";
