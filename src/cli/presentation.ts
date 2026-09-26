import { formatSimilaritySummary } from "../format.js";
import type {
  IndexedFunction,
  MarkdownSearchResult,
  SearchResult,
  SimilarityResult,
} from "../types.js";

export function presentFunction(value: IndexedFunction) {
  const { embeddingInput: _embeddingInput, embeddingId: _embeddingId, descriptionEmbeddingId: _descriptionEmbeddingId, ...result } = value;
  return result;
}

export function presentMatch(value: SimilarityResult) {
  const { function: callable, ...scores } = value;
  return { ...scores, function: presentFunction(callable) };
}

export function presentSearchResult(value: SearchResult) {
  if (value.type === "markdown") return value;
  const { type, ...result } = value;
  return { type, ...presentMatch(result) };
}

export function formatSearchResult(value: SearchResult): string {
  return value.type === "function" ? formatSimilaritySummary([value]) : formatMarkdownResult(value);
}

export function formatMarkdownResult(result: MarkdownSearchResult): string {
  const score = result.rerankScore === undefined
    ? result.similarity.toFixed(4)
    : `${result.rerankScore.toFixed(4)} rerank (${result.similarity.toFixed(4)} similarity)`;
  const heading = result.chunk.headingPath.join(" > ");
  return `${score}  ${result.chunk.path}:${result.chunk.startLine}${heading ? ` :: ${heading}` : ""}\n${result.chunk.content}`;
}

export function printJson(value: unknown): void {
  process.stdout.write(`${JSON.stringify(value, null, 2)}\n`);
}
