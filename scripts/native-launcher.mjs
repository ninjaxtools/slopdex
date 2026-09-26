#!/usr/bin/env node
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { binaryPaths, buildRequirements } from "./native-common.mjs";

const { prebuilt, built } = binaryPaths();
// A source build may be present because a bundled prebuilt could not run on
// this host (for example, an older glibc). Prefer that successful local build.
const binary = [built, prebuilt].find(existsSync);
if (!binary) {
  console.error(`slopdex: native executable is missing. Reinstall with npm install -g @ninjaxtools/slopdex with install scripts enabled, or run node scripts/native-install.mjs in the package directory. ${buildRequirements}`);
  process.exitCode = 1;
} else {
  const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });
  const handlers = new Map();
  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    const handler = () => { if (!child.killed) child.kill(signal); };
    handlers.set(signal, handler);
    process.on(signal, handler);
  }
  const cleanup = () => {
    for (const [signal, handler] of handlers) process.removeListener(signal, handler);
  };
  child.on("error", (error) => {
    cleanup();
    console.error(`slopdex: failed to start ${binary}: ${error.message}. Run node scripts/native-install.mjs in the package directory to repair the installation.`);
    process.exitCode = 1;
  });
  child.on("exit", (code, signal) => {
    cleanup();
    if (signal) process.kill(process.pid, signal);
    else process.exitCode = code ?? 1;
  });
}
