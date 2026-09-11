import type Parser from "tree-sitter";

import type { IndexingIssue, SupportedLanguage } from "../types.js";
import type { CallableCandidate } from "./candidate.js";

export function parseDiagnostics(
  root: Parser.SyntaxNode, content: string, filePath: string, language: SupportedLanguage,
  candidates: readonly CallableCandidate[],
): IndexingIssue[] {
  if (!root.hasError) return [];
  const errors: IndexingIssue[] = [];
  const brokenNodes = new Set<number>();
  const issue = (node: Parser.SyntaxNode, qualifiedName: string | null, message: string): IndexingIssue => ({
    path: filePath, language, scope: qualifiedName === null ? "file" : "function", code: "parse-error", message,
    qualifiedName, startLine: node.startPosition.row + 1, startColumn: node.startPosition.column + 1,
    endLine: node.endPosition.row + 1, endColumn: node.endPosition.column + 1,
    source: content.slice(node.startIndex, node.endIndex),
  });
  for (const candidate of candidates) {
    if (!candidate.node.hasError) continue;
    const name = [...candidate.scope, candidate.name].join(".");
    errors.push(issue(candidate.node, name, `Cannot fully parse callable ${name}; omitted from the searchable index.`));
    brokenNodes.add(candidate.node.id);
  }
  const walk = (node: Parser.SyntaxNode, insideBrokenCallable: boolean): void => {
    const inside = insideBrokenCallable || brokenNodes.has(node.id);
    if (node.isError || node.isMissing) {
      if (!inside) errors.push(issue(node, null, node.isMissing
        ? `Tree-sitter expected ${node.type}.`
        : "Tree-sitter could not parse this source region."));
      // A malformed declaration can be reduced to an ERROR node instead of a
      // callable node. Recover names from declaration tokens, without guessing
      // function names from arbitrary identifiers or source text.
      const children = node.children;
      for (let index = 0; index < children.length; index += 1) {
        if (!["function", "def", "fn", "func"].includes(children[index]!.type)) continue;
        let next = index + 1;
        if (children[next]?.type === "*") next += 1;
        const name = children[next];
        if (name && ["identifier", "field_identifier"].includes(name.type)) {
          errors.push(issue(node, name.text, `Cannot parse declaration for ${name.text}; name recovered from an error node.`));
        }
      }
      // The whole unparsed region is retained; nested error nodes would repeat it.
      return;
    }
    for (const child of node.children) if (child.hasError || child.isError || child.isMissing) walk(child, inside);
  };
  walk(root, false);
  return errors;
}
