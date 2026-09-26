import { spawn } from "node:child_process";
import path from "node:path";

export const projectRoot = path.resolve(import.meta.dirname, "../..");
export const testTimeoutMs = 120_000;

const cliTimeoutMs = 10_000;

export function runCli(root: string, ...args: string[]) {
  return runCliWithEnv(root, process.env, ...args);
}

export function runCliWithEnv(root: string, env: NodeJS.ProcessEnv, ...args: string[]) {
  return new Promise<{ status: number | null; stdout: string; stderr: string }>((resolve, reject) => {
    const child = spawn(process.execPath, ["--import", "tsx", "src/cli.ts", ...args, "--root", root], {
      cwd: projectRoot,
      env: { ...env, OPENAI_API_KEY: "test" },
    });
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => { stdout += chunk; });
    child.stderr.on("data", (chunk: string) => { stderr += chunk; });

    const timeout = setTimeout(() => {
      timedOut = true;
      child.kill("SIGKILL");
    }, cliTimeoutMs);
    child.once("error", (error) => {
      clearTimeout(timeout);
      reject(error);
    });
    child.once("close", (status) => {
      clearTimeout(timeout);
      if (timedOut) {
        reject(new Error(`CLI timed out after ${cliTimeoutMs}ms${stderr ? `:\n${stderr}` : ""}`));
        return;
      }
      resolve({ status, stdout, stderr });
    });
  });
}
