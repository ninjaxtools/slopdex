export { CodeIndex } from "./code-index.js";
export { analyzeCohesion, cohesionLocation } from "./analysis/cohesion.js";
export { crossSearch } from "./search/cross-search.js";
export { OpenAIEmbeddingProvider } from "./embeddings/openai.js";
export { OpenAISummaryProvider } from "./summaries/openai.js";
export type { OpenAISummaryProviderOptions } from "./summaries/openai.js";
export type { OpenAIEmbeddingProviderOptions } from "./embeddings/openai.js";
export { JinaEmbeddingProvider } from "./embeddings/jina.js";
export type { JinaEmbeddingProviderOptions } from "./embeddings/jina.js";
export { CodeIndexError, GitDivergenceError, GitUnavailableError, IncompatibleIndexError } from "./errors.js";
export type * from "./types.js";

import { CodeIndex } from "./code-index.js";
import { analyzeCohesion } from "./analysis/cohesion.js";
import { crossSearch } from "./search/cross-search.js";
import type {
  CodeIndexOptions,
  CohesionAnalysisOptions,
  CrossSearchOptions,
  SimilaritySearchOptions,
  UpdateFilesOptions,
  UpdateFromGitOptions,
  UpdateFromWorkingTreeOptions,
} from "./types.js";

export function openCodeIndex(options: CodeIndexOptions): CodeIndex {
  return new CodeIndex(options);
}

export function updateFiles(index: CodeIndex, options: UpdateFilesOptions) {
  return index.updateFiles(options);
}

export function updateFromGit(index: CodeIndex, options?: UpdateFromGitOptions) {
  return index.updateFromGit(options);
}

export function updateFromWorkingTree(index: CodeIndex, options?: UpdateFromWorkingTreeOptions) {
  return index.updateFromWorkingTree(options);
}

export function similaritySearch(index: CodeIndex, options: SimilaritySearchOptions) {
  return index.similaritySearch(options);
}

export function useSummaries(index: CodeIndex, options?: { signal?: AbortSignal }) {
  return index.useSummaries(options);
}

export function searchSummary(index: CodeIndex, options: SimilaritySearchOptions) {
  return index.searchSummary(options);
}

export function crossSearchFunctions(options: CrossSearchOptions) {
  return crossSearch(options);
}

export function analyzeCodeCohesion(options: CohesionAnalysisOptions) {
  return analyzeCohesion(options);
}
