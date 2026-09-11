import { createServer, type Server } from "node:http";

import { Tiktoken } from "js-tiktoken/lite";
import cl100kBase from "js-tiktoken/ranks/cl100k_base";
import { afterEach, describe, expect, it } from "vitest";

import { JinaEmbeddingProvider } from "../src/embeddings/jina.js";
import { OpenAIEmbeddingProvider } from "../src/embeddings/openai.js";
import { OpenAISummaryProvider } from "../src/summaries/openai.js";
import { parseCallables } from "../src/parser/callable-parser.js";

const servers: Server[] = [];

afterEach(async () => {
  await Promise.all(servers.splice(0).map((server) => new Promise<void>((resolve, reject) => {
    server.close((error) => error ? reject(error) : resolve());
  })));
});

describe("OpenAI summary provider", () => {
  const fileSource = "export function deliver() { send(); }";
  const input = { repository: "example", callable: parseCallables("client.ts", fileSource)[0]!, fileSource };

  it("uses gpt-5.6-sol and sends purpose-oriented context to the Responses API", async () => {
    const requests: Array<Record<string, unknown>> = [];
    const url = await startServer(requests, {
      status: "completed",
      output: [{ type: "reasoning" }, { type: "message", content: [{ type: "output_text", text: "Delivers application messages." }] }],
    });
    const provider = new OpenAISummaryProvider({ apiKey: "test", baseUrl: url });
    await expect(provider.summarize(input)).resolves.toBe("Delivers application messages.");
    expect(requests[0]).toMatchObject({ model: "gpt-5.6-sol", store: false });
    expect(requests[0]!.instructions).toContain("purpose of the specified callable within its codebase");
    expect(JSON.parse(requests[0]!.input as string)).toMatchObject({ repository: "example", path: "client.ts", fileContext: fileSource });
  });

  it.each([
    { status: "incomplete", output: [] },
    { status: "completed", output: [] },
    { status: "completed", output: [{ type: "message", content: [{ type: "refusal", refusal: "no" }] }] },
  ])("rejects unusable summaries: %j", async (response) => {
    const url = await startServer([], response);
    const provider = new OpenAISummaryProvider({ apiKey: "test", baseUrl: url });
    await expect(provider.summarize(input)).rejects.toThrow(/incomplete|empty/);
  });
});

describe("embedding providers", () => {
  it("uses the large OpenAI embedding model by default", () => {
    const provider = new OpenAIEmbeddingProvider({ apiKey: "test" });

    expect(provider.profile).toMatchObject({ model: "text-embedding-3-large", dimensions: 3072 });
  });

  it("sends OpenAI batches and restores response index order", async () => {
    const requests: unknown[] = [];
    const url = await startServer(requests, {
      data: [
        { index: 1, embedding: [0, 1] },
        { index: 0, embedding: [1, 0] },
      ],
    });
    const provider = new OpenAIEmbeddingProvider({
      apiKey: "test",
      baseUrl: url,
      model: "test-model",
      dimensions: 2,
    });

    await expect(provider.embedDocuments(["one", "two"])).resolves.toEqual([[1, 0], [0, 1]]);
    expect(requests[0]).toMatchObject({ model: "test-model", input: ["one", "two"] });
  });

  it("truncates OpenAI inputs to the model token limit", async () => {
    const requests: unknown[] = [];
    const url = await startServer(requests, { data: [{ index: 0, embedding: [1, 0] }] });
    const provider = new OpenAIEmbeddingProvider({ apiKey: "test", baseUrl: url, dimensions: 2 });
    const input = `language: typescript\nsource:\n${"const value = 1;\n".repeat(10_000)}`;

    await provider.embedDocuments([input]);

    const sent = (requests[0] as { input: string[] }).input[0]!;
    const tokenizer = new Tiktoken(cl100kBase);
    expect(tokenizer.encode(sent)).toHaveLength(8192);
    expect(sent.length).toBeLessThan(input.length);
    expect(input.startsWith(sent)).toBe(true);
  });

  it("uses Jina code passage and query tasks", async () => {
    const requests: Array<Record<string, unknown>> = [];
    const url = await startServer(requests, { data: [{ index: 0, embedding: [1, 0] }] }, true);
    const provider = new JinaEmbeddingProvider({
      apiKey: "test",
      baseUrl: url,
      model: "jina-embeddings-v4",
      dimensions: 2,
    });

    await provider.embedDocuments(["function one() {}"]);
    await provider.embedQuery("find one");
    expect(requests.map((request) => request.task)).toEqual(["code.passage", "code.query"]);
  });

  it("rejects provider vectors containing non-number components", async () => {
    const url = await startServer([], { data: [{ index: 0, embedding: ["1", 0] }] });
    const provider = new OpenAIEmbeddingProvider({ apiKey: "test", baseUrl: url, dimensions: 2 });

    await expect(provider.embedDocuments(["one"])).rejects.toThrow(/malformed vector/);
  });
});

async function startServer(
  requests: unknown[],
  response: unknown,
  exactUrl = false,
): Promise<string> {
  const server = createServer((request, serverResponse) => {
    const body: Buffer[] = [];
    request.on("data", (value: Buffer) => body.push(value));
    request.on("end", () => {
      requests.push(JSON.parse(Buffer.concat(body).toString("utf8")));
      serverResponse.setHeader("content-type", "application/json");
      serverResponse.end(JSON.stringify(response));
    });
  });
  servers.push(server);
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("Expected TCP server address.");
  const base = `http://127.0.0.1:${address.port}`;
  return exactUrl ? `${base}/v1/embeddings` : `${base}/v1`;
}
