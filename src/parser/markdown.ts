import { sha256 } from "../utils.js";

export interface ParsedMarkdownChunk {
  headingPath: string[];
  startLine: number;
  endLine: number;
  content: string;
  sourceHash: string;
  embeddingInput: string;
}

interface Heading {
  level: number;
  title: string;
  source: string;
}

export function isMarkdownPath(filePath: string): boolean {
  return /\.(?:md|markdown)$/i.test(filePath);
}

export function chunkMarkdown(content: string): ParsedMarkdownChunk[] {
  const lines = content.split(/\r?\n/);
  const visibleLines = [...lines];
  const chunks: ParsedMarkdownChunk[] = [];
  const headings: Heading[] = [];
  let sectionStart = 0;
  let bodyStart = 0;
  let fence: { marker: "`" | "~"; length: number } | undefined;
  let htmlComment = false;

  const flush = (end: number): void => {
    let firstBodyLine = bodyStart;
    let lastBodyLine = end - 1;
    while (firstBodyLine <= lastBodyLine && !visibleLines[firstBodyLine]!.trim()) firstBodyLine += 1;
    while (lastBodyLine >= firstBodyLine && !visibleLines[lastBodyLine]!.trim()) lastBodyLine -= 1;
    if (firstBodyLine > lastBodyLine) return;
    const body = visibleLines.slice(firstBodyLine, lastBodyLine + 1).join("\n");
    const headingSources = headings.map((heading) => heading.source);
    const chunk = headingSources.length > 0 ? `${headingSources.join("\n")}\n\n${body}` : body;
    chunks.push({
      headingPath: headings.map((heading) => heading.title),
      startLine: headings.length > 0 ? sectionStart + 1 : firstBodyLine + 1,
      endLine: lastBodyLine + 1,
      content: chunk,
      sourceHash: sha256(chunk),
      embeddingInput: chunk,
    });
  };

  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index]!;
    if (fence) {
      const closingFence = line.match(/^ {0,3}(`{3,}|~{3,})[ \t]*$/);
      if (closingFence && closingFence[1]![0] === fence.marker && closingFence[1]!.length >= fence.length) fence = undefined;
      continue;
    }
    if (htmlComment) {
      visibleLines[index] = "";
      if (line.includes("-->")) htmlComment = false;
      continue;
    }
    if (/^ {0,3}<!--/.test(line)) {
      visibleLines[index] = "";
      htmlComment = !line.includes("-->");
      continue;
    }
    const openingFence = line.match(/^ {0,3}(`{3,}|~{3,})/);
    if (openingFence) {
      fence = { marker: openingFence[1]![0] as "`" | "~", length: openingFence[1]!.length };
      continue;
    }

    const match = line.match(/^ {0,3}(#{1,6})(?:[ \t]+(.*?)\s*|[ \t]*)$/);
    if (!match) continue;
    flush(index);
    const level = match[1]!.length;
    const title = (match[2] ?? "").replace(/(?:^|[ \t]+)#+[ \t]*$/, "").trim();
    while (headings.at(-1)?.level && headings.at(-1)!.level >= level) headings.pop();
    headings.push({ level, title, source: line.trimStart() });
    sectionStart = index;
    bodyStart = index + 1;
  }
  flush(lines.length);
  return chunks;
}
