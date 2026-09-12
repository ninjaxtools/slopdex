const reportedCalls = new Set<string>();

export type ModelCallKind = "vectors" | "descriptions" | "reranking";

export function reportModelCall(
  kind: ModelCallKind,
  profile: { provider: string; model: string },
  verbose = false,
): void {
  const key = JSON.stringify([kind, profile.provider, profile.model]);
  if (!verbose && reportedCalls.has(key)) return;
  reportedCalls.add(key);
  process.stderr.write(
    `slopdex: notice: external model call: kind=${kind} provider=${JSON.stringify(profile.provider)} model=${JSON.stringify(profile.model)}\n`,
  );
}
