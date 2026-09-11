import type Parser from "tree-sitter";

import type { CallableKind, SupportedLanguage } from "../types.js";
import type { CallableCandidate } from "./candidate.js";

type Node = Parser.SyntaxNode;

// Follow the declarator spine, not parameter lists: C functions can return
// pointers, including pointers to other functions.
function cFunctionName(node: Node | null): string | null {
  if (!node) return null;
  if (node.type === "identifier") return node.text;
  return cFunctionName(node.childForFieldName("declarator")
    ?? (node.type === "parenthesized_declarator" ? node.namedChildren[0] ?? null : null));
}

function containsYield(node: Node): boolean {
  if (node.type === "yield") return true;
  if (["function_definition", "class_definition", "lambda"].includes(node.type)) return false;
  return node.namedChildren.some(containsYield);
}

function unwrap(node: Node | null): Node | null {
  while (node?.type === "parenthesized_expression" && node.namedChildCount === 1) {
    node = node.namedChildren[0]!;
  }
  return node;
}

function pythonIsMethod(node: Node): boolean {
  const definition = node.parent?.type === "decorated_definition" ? node.parent : node;
  return definition.parent?.parent?.type === "class_definition";
}

function pythonDocstring(body: Node | null, content: string): string | null {
  const statement = body?.namedChildren[0];
  if (statement?.type !== "expression_statement") return null;
  const value = statement.namedChildren[0];
  if (value?.type !== "string" && value?.type !== "concatenated_string") return null;
  return content.slice(value.startIndex, value.endIndex);
}

function goReceiver(node: Node): string | null {
  let type = node.childForFieldName("receiver")?.namedChildren[0]?.childForFieldName("type");
  while (type?.type === "pointer_type") type = type.namedChildren[0] ?? null;
  return type?.text ?? null;
}

export function collectNativeCallables(root: Node, content: string, language: SupportedLanguage): CallableCandidate[] {
  const candidates: CallableCandidate[] = [];
  const text = (node: Node): string => content.slice(node.startIndex, node.endIndex);

  const add = (node: Node, name: string, kind: CallableKind, scope: readonly string[], bound = false): boolean => {
    const body = node.childForFieldName("body");
    if ((!body || body.isMissing) && !node.hasError) return false;
    const sourceNode = language === "python" && node.parent?.type === "decorated_definition" ? node.parent : node;
    const header = content.slice(node.startIndex, body?.startIndex ?? node.endIndex).trimEnd();
    const documentation = language === "python" && body ? pythonDocstring(body, content) : null;
    candidates.push({
      node: sourceNode, name, kind, scope,
      signature: bound ? `${name} = ${header}` : header,
      ...(documentation ? { documentation } : {}),
    });
    if (body) walk(body, [...scope, name]);
    return true;
  };

  const addBound = (value: Node | null, name: Node | null, scope: readonly string[]): boolean => {
    const callable = unwrap(value);
    if (!callable || !name || !["identifier", "attribute", "selector_expression"].includes(name.type)) return false;
    const closureType = { python: "lambda", rust: "closure_expression", go: "func_literal", java: "lambda_expression" };
    if (!(language in closureType) || callable.type !== closureType[language as keyof typeof closureType]) return false;
    return add(callable, text(name), "function", scope, true);
  };

  const walk = (node: Node, scope: readonly string[]): void => {
    if (language === "python") {
      if (node.type === "class_definition") {
        const name = node.childForFieldName("name");
        const body = node.childForFieldName("body");
        if (body) walk(body, name ? [...scope, text(name)] : scope);
        return;
      }
      if (node.type === "function_definition") {
        const name = node.childForFieldName("name");
        const body = node.childForFieldName("body");
        const method = pythonIsMethod(node);
        const kind = method && ["__init__", "__new__"].includes(name?.text ?? "") ? "constructor"
          : body && containsYield(body) ? "generator" : method ? "method" : "function";
        if (name) add(node, text(name), kind, scope);
        return;
      }
      if (node.type === "assignment" || node.type === "named_expression") {
        if (addBound(node.childForFieldName("right") ?? node.childForFieldName("value"),
          node.childForFieldName("left") ?? node.childForFieldName("name"), scope)) return;
      }
    } else if (language === "rust") {
      if (["mod_item", "trait_item", "impl_item"].includes(node.type)) {
        const body = node.childForFieldName("body");
        const type = node.childForFieldName("type");
        const trait = node.childForFieldName("trait");
        const name = node.type === "impl_item"
          ? type && (trait ? `<${text(type)} as ${text(trait)}>` : text(type))
          : node.childForFieldName("name")?.text;
        if (body) walk(body, name ? [...scope, name] : scope);
        return;
      }
      if (node.type === "function_item") {
        const name = node.childForFieldName("name");
        const owner = node.parent?.parent?.type;
        if (name) add(node, text(name), owner === "impl_item" || owner === "trait_item" ? "method" : "function", scope);
        return;
      }
      if (node.type === "let_declaration"
        && addBound(node.childForFieldName("value"), node.childForFieldName("pattern"), scope)) return;
    } else if (language === "go") {
      if (node.type === "function_declaration" || node.type === "method_declaration") {
        const name = node.childForFieldName("name");
        const receiver = goReceiver(node);
        if (name) add(node, text(name), receiver ? "method" : "function", receiver ? [...scope, receiver] : scope);
        return;
      }
      if (["short_var_declaration", "assignment_statement", "var_spec"].includes(node.type)) {
        const names = node.type === "var_spec" ? node.childrenForFieldName("name")
          : node.childForFieldName("left")?.namedChildren ?? [];
        const values = (node.childForFieldName("right") ?? node.childForFieldName("value"))?.namedChildren ?? [];
        // A one-to-one binding avoids inventing names for multiple return values.
        if (names.length === values.length) {
          for (let i = 0; i < names.length; i += 1) {
            if (!addBound(values[i]!, names[i]!, scope)) walk(values[i]!, scope);
          }
          return;
        }
      }
    } else if (language === "java") {
      if (["class_declaration", "interface_declaration", "enum_declaration", "record_declaration", "annotation_type_declaration", "enum_constant"].includes(node.type)) {
        const name = node.childForFieldName("name");
        for (const child of node.namedChildren) walk(child, name ? [...scope, text(name)] : scope);
        return;
      }
      if (node.type === "class_body" && node.parent?.type === "object_creation_expression") {
        const anonymousScope = [...scope, `<anonymous@${node.startPosition.row + 1}:${node.startPosition.column + 1}>`];
        for (const child of node.namedChildren) walk(child, anonymousScope);
        return;
      }
      if (["method_declaration", "constructor_declaration", "compact_constructor_declaration"].includes(node.type)) {
        const name = node.childForFieldName("name");
        if (name) add(node, text(name), node.type === "method_declaration" ? "method" : "constructor", scope);
        return;
      }
      if (node.type === "variable_declarator"
        && addBound(node.childForFieldName("value"), node.childForFieldName("name"), scope)) return;
    } else if (language === "c" && node.type === "function_definition") {
      const name = cFunctionName(node.childForFieldName("declarator"));
      if (name) add(node, name, "function", scope);
      return;
    }

    // Anonymous callbacks have no stable symbol to index.
    if (["lambda", "closure_expression", "func_literal", "lambda_expression"].includes(node.type)) return;
    for (const child of node.namedChildren) walk(child, scope);
  };

  walk(root, []);
  return candidates;
}
