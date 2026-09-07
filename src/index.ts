export { CodeIndex } from "./code-index.js";
export { crossSearch } from "./search/cross-search.js";
export { OpenAIEmbeddingProvider } from "./embeddings/openai.js";
export type { OpenAIEmbeddingProviderOptions } from "./embeddings/openai.js";
export { JinaEmbeddingProvider } from "./embeddings/jina.js";
export type { JinaEmbeddingProviderOptions } from "./embeddings/jina.js";
export { CodeIndexError, GitDivergenceError, GitUnavailableError, IncompatibleIndexError } from "./errors.js";
export type * from "./types.js";

import { CodeIndex } from "./code-index.js";
import { crossSearch } from "./search/cross-search.js";
import type {
  CodeIndexOptions,
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

export function crossSearchFunctions(options: CrossSearchOptions) {
  return crossSearch(options);
}
