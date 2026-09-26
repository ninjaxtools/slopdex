import type { IndexingIssue, ParsedCallable, SourceMode } from "../types.js";

export interface PreparedDescription {
  key: string;
  description: string;
  vector?: readonly number[];
}

export interface PreparedFileDescription {
  path: string;
  contentHash: string;
  descriptionKey: string;
  value: PreparedDescription;
}

export interface PreparedCallable extends ParsedCallable {
  embeddingKey: string;
  vector?: readonly number[];
  descriptionKey?: string;
  description?: PreparedDescription;
}

export interface PreparedMarkdownChunk {
  headingPath: string[];
  startLine: number;
  endLine: number;
  content: string;
  sourceHash: string;
  embeddingInput: string;
  embeddingKey: string;
  vector?: readonly number[];
}

export interface PreparedFile {
  path: string;
  contentHash: string;
  blobOid: string | null;
  sourceMode: SourceMode;
  indexedCommit: string | null;
  language: string;
  byteSize: number;
  source: string;
  previousPath?: string;
  replacePath?: string;
  callables: PreparedCallable[];
  markdownChunks: PreparedMarkdownChunk[];
  fileDescription?: PreparedFileDescription;
  errors: IndexingIssue[];
  unavailable?: boolean;
}

export interface CachedParse {
  callables: ParsedCallable[];
  errors: IndexingIssue[];
}

export interface IndexedFileState {
  path: string;
  contentHash: string;
  blobOid: string | null;
  sourceMode: SourceMode;
  previousPath: string | null;
  language: string;
  describable: boolean;
  fileDescription: string | null;
  fileDescriptionPath: string | null;
  fileDescriptionContentHash: string | null;
}
