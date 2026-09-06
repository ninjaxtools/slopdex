import path from "node:path";

import Parser from "tree-sitter";
import JavaScript from "tree-sitter-javascript";
import TypeScript from "tree-sitter-typescript";

import { CodeIndexError } from "../errors.js";
import type { CallableKind, ParsedCallable, SupportedLanguage } from "../types.js";
import { sha256 } from "../utils.js";

interface CallableCandidate {
  node: Parser.SyntaxNode;
  name: string;
  kind: CallableKind;
  scope: readonly string[];
}

const parsers = new Map<SupportedLanguage, Parser>();

export function languageForPath(filePath: string): SupportedLanguage | null {
  switch (path.extname(filePath).toLowerCase()) {
    case ".ts":
    case ".mts":
    case ".cts":
      return "typescript";
    case ".tsx":
      return "tsx";
    case ".js":
    case ".mjs":
    case ".cjs":
      return "javascript";
    case ".jsx":
      return "jsx";
    default:
      return null;
  }
}

function getParser(language: SupportedLanguage): Parser {
  const existing = parsers.get(language);
  if (existing) return existing;

  const parser = new Parser();
  if (language === "typescript") parser.setLanguage(TypeScript.typescript);
  else if (language === "tsx") parser.setLanguage(TypeScript.tsx);
  else parser.setLanguage(JavaScript);
  parsers.set(language, parser);
  return parser;
}

function parseTree(parser: Parser, content: string, relativePath: string): Parser.Tree {
  try {
    return parser.parse(content, undefined, { bufferSize: Buffer.byteLength(content) + 1 });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new CodeIndexError(`Cannot index ${relativePath}: tree-sitter failed: ${message}`, { cause: error });
  }
}

function maskSyntax(value: string, replacement: string): string {
  return replacement + value.slice(replacement.length).replace(/[^\r\n]/g, " ");
}

function recoverUnsupportedTypeScriptSyntax(
  parser: Parser,
  tree: Parser.Tree,
  content: string,
  relativePath: string,
): Parser.Tree | null {
  const errors: Parser.SyntaxNode[] = [];
  const collectErrors = (node: Parser.SyntaxNode): void => {
    if (node.isError || node.isMissing) errors.push(node);
    for (const child of node.children) collectErrors(child);
  };
  collectErrors(tree.rootNode);

  if (errors.length === 0) return null;

  let recoveredContent = content;
  let changed = false;
  const taggedTemplateTypeArguments = new Map<number, Parser.SyntaxNode>();
  for (const error of errors) {
    const parent = error.parent;
    if (error.isMissing
      && error.type === "!"
      && parent?.type === "type_arguments"
      && /^\s*`/.test(content.slice(parent.endIndex))) {
      taggedTemplateTypeArguments.set(parent.startIndex, parent);
    }
  }
  for (const node of [...taggedTemplateTypeArguments.values()].toSorted((left, right) => right.startIndex - left.startIndex)) {
    const value = recoveredContent.slice(node.startIndex, node.endIndex);
    recoveredContent = `${recoveredContent.slice(0, node.startIndex)}${value.replace(/[^\r\n]/g, " ")}${recoveredContent.slice(node.endIndex)}`;
    changed = true;
  }

  for (const node of errors.filter((candidate) => {
    const parent = candidate.parent;
    return !candidate.isMissing
      && candidate.type === "ERROR"
      && candidate.text === "type"
      && parent?.type === "export_statement"
      && content.slice(parent.startIndex, candidate.startIndex).trim() === "export"
      && /^\s*\*/.test(content.slice(candidate.endIndex, parent.endIndex));
  }).toSorted((left, right) => right.startIndex - left.startIndex)) {
    recoveredContent = `${recoveredContent.slice(0, node.startIndex)}    ${recoveredContent.slice(node.endIndex)}`;
    changed = true;
  }

  recoveredContent = recoveredContent.replace(
    /\bimport\s*\(\s*(["'])(?:\\.|(?!\1)[^\\])*\1\s*\)/g,
    (value) => {
      changed = true;
      return maskSyntax(value, "X");
    },
  );
  if (!changed) return null;

  const recoveredTree = parseTree(parser, recoveredContent, relativePath);
  return recoveredTree.rootNode.hasError ? null : recoveredTree;
}

function textFor(content: string, node: Parser.SyntaxNode): string {
  return content.slice(node.startIndex, node.endIndex);
}

function fieldText(content: string, node: Parser.SyntaxNode, field: string): string | null {
  const child = node.childForFieldName(field);
  return child ? textFor(content, child) : null;
}

function callableKind(node: Parser.SyntaxNode, name: string): CallableKind {
  if (name === "constructor") return "constructor";
  if (node.type.includes("generator") || /^\s*(?:async\s+)?\*/.test(node.text)) return "generator";
  return node.type === "method_definition" ? "method" : "function";
}

function bindingName(content: string, node: Parser.SyntaxNode): string | null {
  const nameNode = node.childForFieldName("name");
  if (!nameNode || !["identifier", "property_identifier", "private_property_identifier"].includes(nameNode.type)) {
    return null;
  }
  return textFor(content, nameNode).replace(/^#/, "");
}

function assignmentName(content: string, node: Parser.SyntaxNode): string | null {
  const left = node.childForFieldName("left");
  if (!left) return null;
  const value = textFor(content, left).trim();
  const match = value.match(/(?:^|[.#])([A-Za-z_$][\w$]*)$/);
  return match?.[1] ?? null;
}

function objectScopeName(content: string, node: Parser.SyntaxNode): string | null {
  const parent = node.parent;
  if (!parent) return null;
  if (parent.type === "variable_declarator" && parent.childForFieldName("value")?.id === node.id) {
    return bindingName(content, parent);
  }
  if (parent.type === "pair" && parent.childForFieldName("value")?.id === node.id) {
    return fieldText(content, parent, "key")?.replace(/["']/g, "") ?? null;
  }
  if (parent.type === "assignment_expression" && parent.childForFieldName("right")?.id === node.id) {
    return assignmentName(content, parent);
  }
  return null;
}

function walk(
  node: Parser.SyntaxNode,
  content: string,
  scope: readonly string[],
  candidates: CallableCandidate[],
): void {
  if (node.type === "class_declaration" || node.type === "class") {
    const name = fieldText(content, node, "name") ?? objectScopeName(content, node);
    const nextScope = name ? [...scope, name] : scope;
    for (const child of node.namedChildren) walk(child, content, nextScope, candidates);
    return;
  }

  if (node.type === "internal_module" || node.type === "module") {
    const name = fieldText(content, node, "name")?.replace(/["']/g, "");
    const nextScope = name ? [...scope, name] : scope;
    for (const child of node.namedChildren) walk(child, content, nextScope, candidates);
    return;
  }

  if (node.type === "object") {
    const name = objectScopeName(content, node);
    const nextScope = name ? [...scope, name] : scope;
    for (const child of node.namedChildren) walk(child, content, nextScope, candidates);
    return;
  }

  if (["function_declaration", "generator_function_declaration"].includes(node.type)) {
    const name = fieldText(content, node, "name");
    const body = node.childForFieldName("body");
    if (name && body) {
      candidates.push({ node, name, kind: callableKind(node, name), scope });
      walk(body, content, [...scope, name], candidates);
    }
    return;
  }

  if (node.type === "method_definition") {
    const name = fieldText(content, node, "name")?.replace(/^#/, "");
    const body = node.childForFieldName("body");
    if (name && body) {
      candidates.push({ node, name, kind: callableKind(node, name), scope });
      walk(body, content, [...scope, name], candidates);
    }
    return;
  }

  if (node.type === "public_field_definition" || node.type === "field_definition") {
    const name = (fieldText(content, node, "name") ?? fieldText(content, node, "property"))?.replace(/^#/, "");
    const value = node.childForFieldName("value");
    if (name && value && ["arrow_function", "function_expression", "generator_function"].includes(value.type)) {
      const kind = callableKind(value, name) === "generator" ? "generator" : "method";
      candidates.push({ node: value, name, kind, scope });
      const body = value.childForFieldName("body");
      if (body) walk(body, content, [...scope, name], candidates);
      return;
    }
  }

  if (node.type === "variable_declarator") {
    const name = bindingName(content, node);
    const value = node.childForFieldName("value");
    if (name && value && ["arrow_function", "function_expression", "generator_function"].includes(value.type)) {
      candidates.push({ node: value, name, kind: callableKind(value, name), scope });
      const body = value.childForFieldName("body");
      if (body) walk(body, content, [...scope, name], candidates);
      return;
    }
  }

  if (node.type === "pair") {
    const name = fieldText(content, node, "key")?.replace(/["']/g, "");
    const value = node.childForFieldName("value");
    if (name && value && ["arrow_function", "function_expression", "generator_function"].includes(value.type)) {
      candidates.push({ node: value, name, kind: callableKind(value, name), scope });
      const body = value.childForFieldName("body");
      if (body) walk(body, content, [...scope, name], candidates);
      return;
    }
  }

  if (node.type === "assignment_expression") {
    const name = assignmentName(content, node);
    const value = node.childForFieldName("right");
    if (name && value && ["arrow_function", "function_expression", "generator_function"].includes(value.type)) {
      candidates.push({ node: value, name, kind: callableKind(value, name), scope });
      const body = value.childForFieldName("body");
      if (body) walk(body, content, [...scope, name], candidates);
      return;
    }
  }

  if (["function_expression", "generator_function"].includes(node.type)) {
    const name = fieldText(content, node, "name");
    const body = node.childForFieldName("body");
    if (name && body) {
      candidates.push({ node, name, kind: callableKind(node, name), scope });
      walk(body, content, [...scope, name], candidates);
      return;
    }
  }

  for (const child of node.namedChildren) walk(child, content, scope, candidates);
}

function signatureFor(content: string, node: Parser.SyntaxNode, name: string): string | null {
  const parameters = node.childForFieldName("parameters");
  if (!parameters) return name;
  const returnType = node.childForFieldName("return_type");
  return `${name}${textFor(content, parameters)}${returnType ? textFor(content, returnType) : ""}`;
}

export function parseCallables(
  relativePath: string,
  content: string,
  onWarning: (message: string) => void = console.warn,
): ParsedCallable[] {
  const language = languageForPath(relativePath);
  if (!language) return [];

  const parser = getParser(language);
  const parsedTree = parseTree(parser, content, relativePath);
  const tree = parsedTree.rootNode.hasError
    ? recoverUnsupportedTypeScriptSyntax(parser, parsedTree, content, relativePath) ?? parsedTree
    : parsedTree;
  if (tree.rootNode.hasError) {
    onWarning(`Cannot fully parse ${relativePath}: tree-sitter reported syntax errors; indexing recoverable callables only.`);
  }

  const candidates: CallableCandidate[] = [];
  walk(tree.rootNode, content, [], candidates);
  candidates.sort((left, right) => left.node.startIndex - right.node.startIndex);

  const occurrences = new Map<string, number>();
  return candidates.map(({ node, name, kind, scope }) => {
    const qualifiedName = [...scope, name].join(".");
    const baseIdentity = `${relativePath}\0${qualifiedName}\0${kind}`;
    const occurrence = occurrences.get(baseIdentity) ?? 0;
    occurrences.set(baseIdentity, occurrence + 1);
    const identityKey = sha256(`${baseIdentity}\0${occurrence}`);
    const source = textFor(content, node);
    const signature = signatureFor(content, node, name);
    const embeddingInput = [
      `language: ${language}`,
      `kind: ${kind}`,
      `symbol: ${qualifiedName}`,
      signature ? `signature: ${signature}` : null,
      "source:",
      source,
    ].filter((value): value is string => value !== null).join("\n");

    return {
      path: relativePath,
      language,
      kind,
      name,
      qualifiedName,
      signature,
      identityKey,
      startLine: node.startPosition.row + 1,
      startColumn: node.startPosition.column + 1,
      endLine: node.endPosition.row + 1,
      endColumn: node.endPosition.column + 1,
      source,
      sourceHash: sha256(source),
      embeddingInput,
    };
  });
}
