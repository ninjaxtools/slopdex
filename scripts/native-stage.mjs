#!/usr/bin/env node
import { chmodSync, copyFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { binaryPaths, binaryProblem, packageVersion } from "./native-common.mjs";

// Run on the same OS/CPU/libc as the release build. Never label a cross-built
// executable using the host's platform key.
const { built, prebuilt } = binaryPaths();
const problem = binaryProblem(built, packageVersion());
if (problem) throw new Error(`Cannot stage ${built}: ${problem}`);
mkdirSync(path.dirname(prebuilt), { recursive: true });
copyFileSync(built, prebuilt);
if (process.platform !== "win32") chmodSync(prebuilt, 0o755);
console.log(prebuilt);
