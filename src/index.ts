export { CodeIndex } from "./code-index.js";
export { readIndexErrors } from "./storage/database.js";
export { analyzeCohesion, cohesionLocation } from "./analysis/cohesion.js";
export { crossSearch } from "./search/cross-search.js";
export { OpenAIEmbeddingProvider } from "./embeddings/openai.js";
export { OpenAIDescriptionProvider } from "./descriptions/openai.js";
export type { DescriptionProviderName, OpenAIDescriptionProviderOptions } from "./descriptions/openai.js";
export type { OpenAIEmbeddingProviderOptions } from "./embeddings/openai.js";
export { JinaEmbeddingProvider } from "./embeddings/jina.js";
export type { JinaEmbeddingProviderOptions } from "./embeddings/jina.js";
export { CohereReranker, JinaReranker } from "./rerankers/hosted.js";
export type { CohereRerankerOptions, JinaRerankerOptions } from "./rerankers/hosted.js";
export { CodeIndexError, GitDivergenceError, GitUnavailableError, IncompatibleIndexError } from "./errors.js";
export type * from "./types.js";

import { CodeIndex } from "./code-index.js";
import { analyzeCohesion } from "./analysis/cohesion.js";
import { crossSearch } from "./search/cross-search.js";
import type {
  CodeIndexOptions,
  CohesionAnalysisOptions,
  CrossSearchOptions,
  ReindexFilesOptions,
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

export function reindexFiles(index: CodeIndex, options?: ReindexFilesOptions) {
  return index.reindexFiles(options);
}

export function similaritySearch(index: CodeIndex, options: SimilaritySearchOptions) {
  return index.similaritySearch(options);
}

export function useDescriptions(index: CodeIndex, options?: { signal?: AbortSignal }) {
  return index.useDescriptions(options);
}

export function disableDescriptions(index: CodeIndex) {
  return index.disableDescriptions();
}

export function searchDescription(index: CodeIndex, options: SimilaritySearchOptions) {
  return index.searchDescription(options);
}

export function crossSearchFunctions(options: CrossSearchOptions) {
  return crossSearch(options);
}

export function analyzeCodeCohesion(options: CohesionAnalysisOptions) {
  return analyzeCohesion(options);
}
