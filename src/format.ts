import type { CrossSearchResult, IndexedFunction, SimilarityResult } from "./types.js";

function functionName(value: Pick<IndexedFunction, "path" | "qualifiedName">): string {
  return `${value.path} :: ${value.qualifiedName}`;
}

export function formatSimilaritySummary(
  matches: readonly SimilarityResult[],
  source?: IndexedFunction,
): string {
  const lines = matches.map((match) => `${source ? "  " : ""}${match.similarity.toFixed(4)}  ${functionName(match.function)}`);
  if (!source) return lines.length > 0 ? lines.join("\n") : "No matches.";
  return [functionName(source), ...(lines.length > 0 ? lines : ["  No matches."])].join("\n");
}

export function formatSimilarityClusters(results: readonly CrossSearchResult[], sameIndex: boolean): string {
  const nodes = new Map<string, { function: IndexedFunction; role: "source" | "target" }>();
  const neighbors = new Map<string, Set<string>>();
  const edges: Array<{ left: string; right: string; similarity: number }> = [];
  const connect = (left: string, right: string): void => {
    const leftNeighbors = neighbors.get(left) ?? new Set<string>();
    const rightNeighbors = neighbors.get(right) ?? new Set<string>();
    leftNeighbors.add(right);
    rightNeighbors.add(left);
    neighbors.set(left, leftNeighbors);
    neighbors.set(right, rightNeighbors);
  };

  for (const result of results) {
    const sourceKey = `${sameIndex ? "index" : "source"}:${result.source.id}`;
    nodes.set(sourceKey, { function: result.source, role: "source" });
    for (const match of result.matches) {
      const matchKey = `${sameIndex ? "index" : "target"}:${match.function.id}`;
      nodes.set(matchKey, { function: match.function, role: sameIndex ? "source" : "target" });
      connect(sourceKey, matchKey);
      edges.push({ left: sourceKey, right: matchKey, similarity: match.similarity });
    }
  }

  const seen = new Set<string>();
  const clusters: Array<{ members: Array<{ function: IndexedFunction; role: "source" | "target" }>; min: number; max: number }> = [];
  for (const start of neighbors.keys()) {
    if (seen.has(start)) continue;
    const pending = [start];
    const keys = new Set<string>();
    while (pending.length > 0) {
      const key = pending.pop()!;
      if (seen.has(key)) continue;
      seen.add(key);
      keys.add(key);
      for (const neighbor of neighbors.get(key) ?? []) pending.push(neighbor);
    }
    const similarities = edges
      .filter((edge) => keys.has(edge.left) && keys.has(edge.right))
      .map((edge) => edge.similarity);
    clusters.push({
      members: [...keys].map((key) => nodes.get(key)!).sort((left, right) => (
        clusterFunctionName(left, sameIndex).localeCompare(clusterFunctionName(right, sameIndex))
      )),
      min: Math.min(...similarities),
      max: Math.max(...similarities),
    });
  }
  clusters.sort((left, right) => right.members.length - left.members.length
    || clusterFunctionName(left.members[0]!, sameIndex).localeCompare(clusterFunctionName(right.members[0]!, sameIndex)));
  if (clusters.length === 0) return "No clusters.";
  return clusters.map((cluster, index) => {
    const similarity = cluster.min === cluster.max
      ? cluster.min.toFixed(4)
      : `${cluster.min.toFixed(4)}-${cluster.max.toFixed(4)}`;
    return [
      `Cluster ${index + 1} (${cluster.members.length} functions, similarity ${similarity})`,
      ...cluster.members.map((member) => `  ${clusterFunctionName(member, sameIndex)}`),
    ].join("\n");
  }).join("\n\n");
}

function clusterFunctionName(
  member: { function: IndexedFunction; role: "source" | "target" },
  sameIndex: boolean,
): string {
  const role = sameIndex ? "" : `[${member.role}] `;
  return `${role}${member.function.path}:${member.function.startLine}:${member.function.startColumn} :: ${member.function.qualifiedName}`;
}
