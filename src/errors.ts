export class CodeIndexError extends Error {
  public constructor(message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = "CodeIndexError";
  }
}

export class GitDivergenceError extends CodeIndexError {
  public constructor(checkpoint: string, target: string) {
    super(`Stored Git checkpoint ${checkpoint} is not an ancestor of ${target}; rebuild the index.`);
    this.name = "GitDivergenceError";
  }
}

export class IncompatibleIndexError extends CodeIndexError {
  public constructor(message: string) {
    super(message);
    this.name = "IncompatibleIndexError";
  }
}
