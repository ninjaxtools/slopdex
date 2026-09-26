export { CodeIndex } from "./code-index.js";
export { readIndexErrors } from "./database.js";
export { analyzeCohesion, cohesionLocation } from "./cohesion.js";
export { crossSearch } from "./search/cross-search.js";
export { OpenAIEmbeddingProvider } from "./embeddings/openai.js";
export { OpenAIDescriptionProvider } from "./openai-description.js";
export type { DescriptionProviderName, OpenAIDescriptionProviderOptions } from "./openai-description.js";
export type { OpenAIEmbeddingProviderOptions } from "./embeddings/openai.js";
export { JinaEmbeddingProvider } from "./embeddings/jina.js";
export type { JinaEmbeddingProviderOptions } from "./embeddings/jina.js";
export { CohereReranker, JinaReranker } from "./rerankers/hosted.js";
export type { CohereRerankerOptions, JinaRerankerOptions } from "./rerankers/hosted.js";
export { OpenAILLMReranker } from "./rerankers/openai.js";
export { chunkMarkdown } from "./parser/markdown.js";
export type { OpenAILLMRerankerOptions } from "./rerankers/openai.js";
export { CodeIndexError, GitDivergenceError, GitUnavailableError, IncompatibleIndexError } from "./errors.js";
export type * from "./types.js";

import { CodeIndex } from "./code-index.js";
import { analyzeCohesion } from "./cohesion.js";
import { crossSearch } from "./search/cross-search.js";
import type {
  CodeIndexOptions,
  CohesionAnalysisOptions,
  CrossSearchOptions,
  DescribeOptions,
  MarkdownSearchOptions,
  ReindexFilesOptions,
  SearchOptions,
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

export function search(index: CodeIndex, options: SearchOptions) {
  return index.search(options);
}

export function searchCode(index: CodeIndex, options: SimilaritySearchOptions) {
  return index.searchCode(options);
}

export function describe(index: CodeIndex, options: DescribeOptions) {
  return index.describe(options);
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

export function searchMarkdown(index: CodeIndex, options: MarkdownSearchOptions) {
  return index.searchMarkdown(options);
}

export function crossSearchFunctions(options: CrossSearchOptions) {
  return crossSearch(options);
}

export function analyzeCodeCohesion(options: CohesionAnalysisOptions) {
  return analyzeCohesion(options);
}
