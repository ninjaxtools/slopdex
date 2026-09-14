import { writeStderr } from "./progress.js";

const reportedCalls = new Set<string>();

export type ModelCallKind = "vectors" | "descriptions" | "reranking";

export function reportModelCall(
  kind: ModelCallKind,
  profile: { provider: string; model: string },
  verbose = false,
  parallelism = 1,
): void {
  const key = JSON.stringify([kind, profile.provider, profile.model, parallelism]);
  if (!verbose && reportedCalls.has(key)) return;
  reportedCalls.add(key);
  writeStderr(
    `slopdex: notice: external model call: kind=${kind} provider=${JSON.stringify(profile.provider)} model=${JSON.stringify(profile.model)} parallelism=${parallelism}\n`,
  );
}
