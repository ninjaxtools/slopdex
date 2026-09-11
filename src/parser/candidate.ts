import type Parser from "tree-sitter";

import type { CallableKind } from "../types.js";

export interface CallableCandidate {
  node: Parser.SyntaxNode;
  name: string;
  kind: CallableKind;
  scope: readonly string[];
  signature?: string;
  documentation?: string;
}
