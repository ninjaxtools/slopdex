import { execFileSync } from "node:child_process";

const library = await import("../dist/index.js");
if (typeof library.openCodeIndex !== "function"
  || typeof library.crossSearch !== "function"
  || typeof library.analyzeCohesion !== "function") {
  throw new Error("Built library exports are incomplete.");
}

execFileSync(process.execPath, ["dist/cli.js", "--help"], { stdio: "ignore" });
