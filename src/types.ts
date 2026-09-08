export type SupportedLanguage = "typescript" | "tsx" | "javascript" | "jsx";

export type CallableKind = "function" | "method" | "constructor" | "generator";

export type SourceMode = "git" | "working-tree";

export interface ParsedCallable {
  path: string;
  language: SupportedLanguage;
  kind: CallableKind;
  name: string;
  qualifiedName: string;
  signature: string | null;
  identityKey: string;
  startLine: number;
  startColumn: number;
  endLine: number;
  endColumn: number;
  lineCount: number;
  source: string;
  sourceHash: string;
  embeddingInput: string;
}

export interface IndexedFunction extends ParsedCallable {
  id: number;
  firstSeenCommit: string | null;
  lastSeenCommit: string | null;
  sourceMode: SourceMode;
  embeddingId: number;
}

export interface EmbeddingProfile {
  provider: "openai" | "jina" | string;
  model: string;
  dimensions: number;
  strategyVersion?: string;
}

export interface EmbeddingProvider {
  readonly profile: EmbeddingProfile;
  embedDocuments(inputs: readonly string[], options?: { signal?: AbortSignal }): Promise<number[][]>;
  embedQuery(input: string, options?: { signal?: AbortSignal }): Promise<number[]>;
}

export interface CodeIndexOptions {
  rootDir: string;
  indexPath?: string;
  provider: EmbeddingProvider;
  onWarning?: (message: string) => void;
  include?: readonly string[];
  exclude?: readonly string[];
  maxFileSize?: number;
  embeddingBatchSize?: number;
  readOnly?: boolean;
}

export interface UpdateFilesOptions {
  upsert?: readonly string[];
  delete?: readonly string[];
  renames?: readonly { from: string; to: string }[];
  signal?: AbortSignal;
}

export interface UpdateFromGitOptions {
  target?: string;
  rebuildOnDivergence?: boolean;
  includeWorkingTree?: boolean;
  signal?: AbortSignal;
}

export interface UpdateFromWorkingTreeOptions {
  signal?: AbortSignal;
}

export interface UpdateStats {
  filesUpdated: number;
  filesDeleted: number;
  functionsAdded: number;
  functionsUpdated: number;
  functionsDeleted: number;
  embeddingsCreated: number;
  checkpoint: string | null;
}

export interface SimilaritySearchOptions {
  query: string;
  limit?: number;
  minSimilarity?: number;
  maxSimilarity?: number;
  signal?: AbortSignal;
}

export interface SimilarityResult {
  function: IndexedFunction;
  similarity: number;
}

export type CrossSearchSourceFilter =
  | { type: "all"; path?: string }
  | { type: "changed-since"; commit: string; path?: string }
  | { type: "uncommitted"; path?: string };

export interface CrossSearchOptions {
  source: import("./code-index.js").CodeIndex;
  target?: import("./code-index.js").CodeIndex;
  sourceFilter?: CrossSearchSourceFilter;
  limitPerFunction?: number;
  minSimilarity?: number;
  maxSimilarity?: number;
  includeSymmetricDuplicates?: boolean;
  crossFileOnly?: boolean;
  minLines?: number;
  nameRegex?: string;
  signal?: AbortSignal;
  onProgress?: (progress: { completed: number; total: number }) => void;
}

export interface CrossSearchResult {
  source: IndexedFunction;
  matches: SimilarityResult[];
}

export type CohesionLocationCategory = "same-file" | "same-folder" | "different-folder";

export interface CohesionLocation {
  category: CohesionLocationCategory;
  physicalDistance: number;
  folderHops: number;
  commonAncestor: string | null;
  sourceTestPair: boolean;
}

export interface CohesionFunctionReference {
  path: string;
  qualifiedName: string;
  kind: CallableKind;
  signature: string | null;
  startLine: number;
  startColumn: number;
  endLine: number;
  endColumn: number;
  lineCount: number;
  source?: string;
}

export interface CohesionPair<FunctionValue = IndexedFunction> {
  rank: number;
  left: FunctionValue;
  right: FunctionValue;
  similarity: number;
  reciprocal: boolean | null;
  semanticWeight: number;
  separationWeight: number;
  cohesionGap: number;
  location: CohesionLocation;
}

export interface CohesionFileReport<FunctionValue = IndexedFunction> {
  path: string;
  functionCount: number;
  internalAffinity: number;
  sameFolderAffinity: number;
  externalAffinity: number;
  externalAffinityRatio: number;
  strongestExternalMatch?: {
    function: FunctionValue;
    similarity: number;
    cohesionGap: number;
  };
}

export interface CohesionGroup<FunctionValue = IndexedFunction> {
  rank: number;
  memberCount: number;
  fileCount: number;
  minimumEdgeSimilarity: number;
  maximumEdgeSimilarity: number;
  maximumPhysicalDistance: number;
  maximumCohesionGap: number;
  members: FunctionValue[];
}

export interface CohesionSummary {
  scope: "repository" | "selected-sources";
  functionsAnalyzed: number;
  candidateFunctions: number;
  semanticEdges: number;
  sameFileRatio: number;
  sameFolderRatio: number;
  remoteRatio: number;
  weightedMeanDistance: number;
}

export interface CohesionAnalysisOptions {
  source: import("./code-index.js").CodeIndex;
  sourceFilter?: CrossSearchSourceFilter;
  neighbors?: number;
  limit?: number;
  minSimilarity?: number;
  maxSimilarity?: number;
  minLines?: number;
  nameRegex?: string;
  signal?: AbortSignal;
  onProgress?: (progress: { completed: number; total: number }) => void;
}

export interface CohesionReport<FunctionValue = IndexedFunction> {
  schemaVersion: 1;
  repository: {
    generation: number;
    gitCheckpoint: string | null;
    embeddingProfile: Required<EmbeddingProfile>;
  };
  parameters: {
    neighbors: number;
    limit: number;
    minSimilarity: number;
    maxSimilarity?: number;
    minLines: number;
    nameRegex?: string;
    sourceFilter: CrossSearchSourceFilter;
  };
  summary: CohesionSummary;
  pairs: CohesionPair<FunctionValue>[];
  files: CohesionFileReport<FunctionValue>[];
  groups: CohesionGroup<FunctionValue>[];
}

export type CohesionJsonReport = CohesionReport<CohesionFunctionReference>;

export interface IndexStatus {
  rootDir: string;
  indexPath: string;
  functionCount: number;
  fileCount: number;
  generation: number;
  gitCheckpoint: string | null;
  embeddingProfile: Required<EmbeddingProfile>;
}
