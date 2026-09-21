import { createServer, type Server } from "node:http";

import { Tiktoken } from "js-tiktoken/lite";
import cl100kBase from "js-tiktoken/ranks/cl100k_base";
import { afterEach, describe, expect, it, vi } from "vitest";

import { JinaEmbeddingProvider } from "../src/embeddings/jina.js";
import { OpenAIEmbeddingProvider } from "../src/embeddings/openai.js";
import { OpenAIDescriptionProvider } from "../src/descriptions/openai.js";
import { parseCallables } from "../src/parser/callable-parser.js";
import { CohereReranker, JinaReranker } from "../src/rerankers/hosted.js";
import { OpenAILLMReranker } from "../src/rerankers/openai.js";
import type { DescribeContext } from "../src/types.js";
import { temporaryRoot, write } from "./helpers.js";

const servers: Server[] = [];

afterEach(async () => {
  vi.unstubAllEnvs();
  await Promise.all(servers.splice(0).map((server) => new Promise<void>((resolve, reject) => {
    server.close((error) => error ? reject(error) : resolve());
  })));
});

describe("OpenAI description provider", () => {
  const fileSource = "export function deliver() { send(); }";
  const input = { repository: "example", callable: parseCallables("client.ts", fileSource)[0]!, fileSource };

  it("uses gpt-5.6-luna and sends purpose-oriented context to the Responses API", async () => {
    const requests: Array<Record<string, unknown>> = [];
    const url = await startServer(requests, {
      status: "completed",
      output: [{
        type: "message",
        role: "assistant",
        id: "message-1",
        content: [{ type: "output_text", text: "Delivers application messages.", annotations: [] }],
      }],
    });
    const provider = new OpenAIDescriptionProvider({ apiKey: "test", baseUrl: url });
    await expect(provider.describe(input)).resolves.toBe("Delivers application messages.");
    expect(requests[0]).toMatchObject({ model: "gpt-5.6-luna", store: false });
    expect(requests[0]!.instructions).toContain("requested file or callable within its codebase");
    const requestInput = requests[0]!.input as Array<{ content: Array<{ text: string }> }>;
    expect(JSON.parse(requestInput[0]!.content[0]!.text)).toMatchObject({ repository: "example", path: "client.ts", fileContext: fileSource });
  });

  it("sends complete discovery context with describe-only instructions", async () => {
    const requests: Array<Record<string, unknown>> = [];
    const url = await startServer(requests, {
      status: "completed",
      output: [{
        type: "message",
        role: "assistant",
        id: "message-1",
        content: [{ type: "output_text", text: "Existing delivery code is in client.ts:1.", annotations: [] }],
      }],
    });
    const context: DescribeContext = {
      repository: "example",
      query: "deliver messages",
      minSimilarity: 0.3,
      fullFileThreshold: 0.8,
      files: [{ path: "client.ts", similarity: 0.95, description: "Delivers messages.", content: fileSource }],
      functions: [{
        path: "client.ts",
        qualifiedName: "deliver",
        kind: "function",
        signature: "function deliver()",
        startLine: 1,
        endLine: 1,
        similarity: 0.95,
        rerankScore: 0.99,
        description: "Delivers application messages.",
        source: fileSource,
      }],
      fileContentErrors: [],
    };
    const provider = new OpenAIDescriptionProvider({ apiKey: "test", baseUrl: url });

    await expect(provider.describeContext(context)).resolves.toBe("Existing delivery code is in client.ts:1.");

    expect(requests[0]!.instructions).toContain("Explain existing code relevant to a request");
    expect(requests[0]!.instructions).toContain("Do not propose an implementation");
    const requestInput = requests[0]!.input as Array<{ content: Array<{ text: string }> }>;
    expect(JSON.parse(requestInput[0]!.content[0]!.text)).toEqual({
      repository: "example",
      request: "deliver messages",
      minSimilarity: 0.3,
      fullFileThreshold: 0.8,
      files: [{ path: "client.ts", similarity: 0.95, description: "Delivers messages.", content: fileSource }],
      functions: context.functions,
    });
  });

  it("reuses one growing conversation for callables in the same file", async () => {
    const source = "export function one() { return 1; }\nexport function two() { return one() + 1; }\n";
    const callables = parseCallables("functions.ts", source);
    const requests: Array<Record<string, unknown>> = [];
    const url = await startServer(requests, {
      status: "completed",
      output: [{
        type: "message",
        role: "assistant",
        id: "message-1",
        content: [{ type: "output_text", text: "Supports the application workflow.", annotations: [] }],
      }],
    });
    const provider = new OpenAIDescriptionProvider({ apiKey: "test", baseUrl: url });
    const session = provider.startFile({ repository: "example", path: "functions.ts", fileSource: source });

    await session.describeFile();
    await session.describe(callables[0]!);
    await session.describe(callables[1]!);

    const firstInput = requests[0]!.input as unknown[];
    const secondInput = requests[1]!.input as unknown[];
    const thirdInput = requests[2]!.input as unknown[];
    expect(secondInput.slice(0, firstInput.length)).toEqual(firstInput);
    expect(thirdInput.slice(0, secondInput.length)).toEqual(secondInput);
    expect(JSON.stringify(thirdInput)).toContain("Supports the application workflow.");
    expect(JSON.stringify(thirdInput).match(/export function one/g)).toHaveLength(1);
    expect(requests.map((request) => request.instructions)).toEqual([
      requests[0]!.instructions,
      requests[0]!.instructions,
      requests[0]!.instructions,
    ]);
  });

  it.each([
    { status: "incomplete", output: [] },
    { status: "completed", output: [] },
    { status: "completed", output: [{ type: "message", role: "assistant", id: "message-1", content: [] }] },
  ])("retries unusable descriptions before rejecting: %j", async (response) => {
    const requests: unknown[] = [];
    const url = await startServer(requests, response);
    const provider = new OpenAIDescriptionProvider({ apiKey: "test", baseUrl: url, retryDelayMs: 1 });
    await expect(provider.describe(input)).rejects.toThrow(/incomplete|empty/);
    expect(requests).toHaveLength(6);
  });

  it("retries empty descriptions and reports each failure to stderr", async () => {
    const requests: unknown[] = [];
    const url = await startServer(requests, () => requests.length <= 1
      ? { status: "completed", output: [] }
      : {
        status: "completed",
        output: [{
          type: "message",
          role: "assistant",
          id: "message-1",
          content: [{ type: "output_text", text: "Recovered from an empty description.", annotations: [] }],
        }],
      });
    const stderr = vi.spyOn(process.stderr, "write").mockImplementation(() => true);
    const lines: string[] = [];
    try {
      const provider = new OpenAIDescriptionProvider({ apiKey: "test", baseUrl: url, retryDelayMs: 1 });
      await expect(provider.describe(input)).resolves.toBe("Recovered from an empty description.");
      for (const [value] of stderr.mock.calls) lines.push(String(value));
    } finally {
      stderr.mockRestore();
    }
    expect(requests).toHaveLength(2);
    expect(lines.filter((line) => line.includes("empty description")))
      .toEqual(["slopdex: Description provider returned an empty description.\n"]);
  });

  it("keeps using the fallback until it fails, then switches back to the primary with backoff", async () => {
    const requests: Array<Record<string, unknown>> = [];
    const failedRequests = new Set([1, 4]);
    const url = await startServer(requests, () => failedRequests.has(requests.length)
      ? { error: { message: "model unavailable", type: "server_error" } }
      : {
        status: "completed",
        output: [{
          type: "message",
          role: "assistant",
          id: "message-1",
          content: [{ type: "output_text", text: `Generated by ${String(requests.at(-1)?.model)}.`, annotations: [] }],
        }],
      }, false, undefined, undefined, () => failedRequests.has(requests.length) ? 400 : 200);
    const stderr = vi.spyOn(process.stderr, "write").mockImplementation(() => true);
    try {
      const provider = new OpenAIDescriptionProvider({
        apiKey: "test",
        baseUrl: url,
        model: "primary-model",
        fallbackModel: "fallback-model",
        retryDelayMs: 1,
      });

      await expect(provider.describe(input)).resolves.toBe("Generated by fallback-model.");
      await expect(provider.describe(input)).resolves.toBe("Generated by fallback-model.");
      await expect(provider.describe(input)).resolves.toBe("Generated by primary-model.");
      expect(requests.map((request) => request.model))
        .toEqual(["primary-model", "fallback-model", "fallback-model", "fallback-model", "primary-model"]);
      const output = stderr.mock.calls.map(([value]) => String(value)).join("");
      expect(output).toContain('description model "primary-model" failed; switching to "fallback-model"');
      expect(output).toContain('description model "fallback-model" failed; switching to "primary-model"');
    } finally {
      stderr.mockRestore();
    }
  });

  it("supports OpenCode Go with its API key, endpoint, and default model", async () => {
    const requests: Array<Record<string, unknown>> = [];
    const url = await startServer(requests, {
      output: [{
        type: "message",
        role: "assistant",
        id: "message-1",
        content: [{ type: "output_text", text: "Delivers through OpenCode Go.", annotations: [] }],
      }],
    });
    const provider = new OpenAIDescriptionProvider({ provider: "opencode-go", apiKey: "test", baseUrl: url });

    await expect(provider.describe(input)).resolves.toBe("Delivers through OpenCode Go.");
    expect(provider.profile).toMatchObject({ provider: "opencode-go", model: "muse-spark-1.3-contributor" });
    expect(requests[0]).toMatchObject({ model: "muse-spark-1.3-contributor", store: false });
  });

  it.each(["opencode", "opencode-go"] as const)(
    "reads the %s API key from opencode's auth.json when the environment has none",
    async (provider) => {
      const dataHome = temporaryRoot("slopdex-auth-");
      write(dataHome, "opencode/auth.json", JSON.stringify({
        [provider]: { type: "api", key: `auth-key-${provider}` },
      }));
      vi.stubEnv("XDG_DATA_HOME", dataHome);
      vi.stubEnv("OPENCODE_API_KEY", "");
      const requests: Array<Record<string, unknown>> = [];
      const authorizations: Array<string | undefined> = [];
      const url = await startServer(requests, {
        output: [{
          type: "message",
          role: "assistant",
          id: "message-1",
          content: [{ type: "output_text", text: "Delivers through OpenCode.", annotations: [] }],
        }],
      }, false, undefined, authorizations);
      const descriptionProvider = new OpenAIDescriptionProvider({ provider, baseUrl: url });

      await expect(descriptionProvider.describe(input)).resolves.toBe("Delivers through OpenCode.");
      expect(authorizations).toEqual([`Bearer auth-key-${provider}`]);
    },
  );

  it("prefers OPENCODE_API_KEY over opencode's auth.json", async () => {
    const dataHome = temporaryRoot("slopdex-auth-");
    write(dataHome, "opencode/auth.json", JSON.stringify({
      "opencode-go": { type: "api", key: "auth-key" },
    }));
    vi.stubEnv("XDG_DATA_HOME", dataHome);
    vi.stubEnv("OPENCODE_API_KEY", "env-key");
    const requests: Array<Record<string, unknown>> = [];
    const authorizations: Array<string | undefined> = [];
    const url = await startServer(requests, {
      output: [{
        type: "message",
        role: "assistant",
        id: "message-1",
        content: [{ type: "output_text", text: "Delivers through OpenCode.", annotations: [] }],
      }],
    }, false, undefined, authorizations);
    const descriptionProvider = new OpenAIDescriptionProvider({ provider: "opencode-go", baseUrl: url });

    await expect(descriptionProvider.describe(input)).resolves.toBe("Delivers through OpenCode.");
    expect(authorizations).toEqual(["Bearer env-key"]);
  });

  it("mentions the auth file when no OpenCode API key is available", async () => {
    vi.stubEnv("XDG_DATA_HOME", temporaryRoot("slopdex-auth-"));
    vi.stubEnv("OPENCODE_API_KEY", "");
    const descriptionProvider = new OpenAIDescriptionProvider({ provider: "opencode-go" });

    await expect(descriptionProvider.describe(input)).rejects.toThrow(/OPENCODE_API_KEY or .*auth\.json is required/);
  });

  it("routes OpenCode chat-completions models through the compatible AI SDK provider", async () => {
    const requests: Array<Record<string, unknown>> = [];
    const url = await startServer(requests, {
      choices: [{ finish_reason: "stop", message: { role: "assistant", content: "Handles Kimi requests." } }],
    });
    const provider = new OpenAIDescriptionProvider({
      provider: "opencode-go",
      model: "kimi-k3",
      apiKey: "test",
      baseUrl: url,
    });

    await expect(provider.describe(input)).resolves.toBe("Handles Kimi requests.");
    expect(requests[0]).toMatchObject({ model: "kimi-k3" });
  });

  it("routes OpenCode Messages models through the Anthropic AI SDK provider", async () => {
    const requests: Array<Record<string, unknown>> = [];
    const paths: string[] = [];
    const url = await startServer(requests, {
      id: "message-1",
      type: "message",
      role: "assistant",
      model: "qwen3.8-max",
      content: [{ type: "text", text: "Handles Qwen requests." }],
      stop_reason: "end_turn",
      stop_sequence: null,
      usage: { input_tokens: 10, output_tokens: 4 },
    }, false, paths);
    const provider = new OpenAIDescriptionProvider({
      provider: "opencode-go",
      model: "qwen3.8-max",
      apiKey: "test",
      baseUrl: url,
    });

    await expect(provider.describe(input)).resolves.toBe("Handles Qwen requests.");
    expect(paths[0]).toBe("/v1/messages");
    expect(requests[0]).toMatchObject({ model: "qwen3.8-max" });
  });

  it("routes OpenCode Gemini models through the Google AI SDK provider", async () => {
    const requests: Array<Record<string, unknown>> = [];
    const paths: string[] = [];
    const url = await startServer(requests, {
      candidates: [{
        content: { role: "model", parts: [{ text: "Handles Gemini requests." }] },
        finishReason: "STOP",
      }],
      usageMetadata: { promptTokenCount: 10, candidatesTokenCount: 4, totalTokenCount: 14 },
    }, false, paths);
    const provider = new OpenAIDescriptionProvider({
      provider: "opencode",
      model: "gemini-3.8-flash",
      apiKey: "test",
      baseUrl: url,
    });

    await expect(provider.describe(input)).resolves.toBe("Handles Gemini requests.");
    expect(paths[0]).toContain("/v1/models/gemini-3.8-flash:generateContent");
  });
});

describe("embedding providers", () => {
  it("uses the large OpenAI embedding model by default", () => {
    const provider = new OpenAIEmbeddingProvider({ apiKey: "test" });

    expect(provider.profile).toMatchObject({ model: "text-embedding-3-large", dimensions: 3072 });
  });

  it("sends OpenAI embedding batches", async () => {
    const requests: unknown[] = [];
    const url = await startServer(requests, {
      data: [
        { index: 0, embedding: [1, 0] },
        { index: 1, embedding: [0, 1] },
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

    await expect(provider.embedDocuments(["one"])).rejects.toThrow(/Embedding request failed/);
  });

  it("treats tokenizer special tokens as ordinary embedding input", async () => {
    const requests: Array<Record<string, unknown>> = [];
    const url = await startServer(requests, { data: [{ index: 0, embedding: [1, 0] }] });
    const provider = new OpenAIEmbeddingProvider({ apiKey: "test", baseUrl: url, dimensions: 2 });

    await provider.embedDocuments([`function marker() { return "<|endoftext|>"; }`]);
    await provider.embedQuery("find <|endoftext|>");

    expect(requests.map((request) => request.input)).toEqual([
      [`function marker() { return "<|endoftext|>"; }`],
      ["find <|endoftext|>"],
    ]);
  });
});

describe("rerankers", () => {
  it("uses OpenAI structured output with the Luna model and high reasoning", async () => {
    const requests: Array<Record<string, unknown>> = [];
    const paths: string[] = [];
    const url = await startServer(requests, {
      status: "completed",
      output: [{
        type: "message",
        role: "assistant",
        id: "message-1",
        content: [{
          type: "output_text",
          text: JSON.stringify({ ranking: [{ index: 1, score: 0.95 }] }),
          annotations: [],
        }],
      }],
    }, false, paths);
    const reranker = new OpenAILLMReranker({ apiKey: "test", baseUrl: url });

    await expect(reranker.rerank("find validation", ["first function", "second function"], { limit: 1 })).resolves.toEqual([
      { index: 1, score: 0.95 },
    ]);
    expect(reranker.profile).toEqual({ provider: "openai", model: "gpt-5.6-luna" });
    expect(reranker.candidateCount).toBe(10);
    expect(paths).toEqual(["/v1/responses"]);
    expect(requests[0]).toMatchObject({
      model: "gpt-5.6-luna",
      store: false,
      reasoning: { effort: "high" },
      text: { format: { type: "json_schema", name: "function_ranking", strict: true } },
    });
    expect(requests[0]!.instructions).toContain("purpose descriptions and source code when present");
    expect(requests[0]).not.toHaveProperty("max_output_tokens");
    expect((requests[0]!.reasoning as Record<string, unknown>)).not.toHaveProperty("summary");
    expect(JSON.stringify(requests[0]!.input)).toContain("second function");
  });

  it("sends Cohere rerank requests and maps ranked indexes to scores", async () => {
    const requests: Array<Record<string, unknown>> = [];
    const paths: string[] = [];
    const url = await startServer(requests, {
      results: [
        { index: 1, relevance_score: 0.9 },
        { index: 0, relevance_score: 0.2 },
      ],
    }, false, paths);
    const reranker = new CohereReranker({ apiKey: "test", baseUrl: url });

    await expect(reranker.rerank("find validation", ["first", "second"], { limit: 2 })).resolves.toEqual([
      { index: 1, score: 0.9 },
      { index: 0, score: 0.2 },
    ]);
    expect(reranker.profile).toEqual({ provider: "cohere", model: "rerank-v4.0-pro" });
    expect(paths).toEqual(["/v1/rerank"]);
    expect(requests[0]).toEqual({
      model: "rerank-v4.0-pro",
      query: "find validation",
      documents: ["first", "second"],
      top_n: 2,
    });
  });

  it("uses Jina's current model and omits documents from the response", async () => {
    const requests: Array<Record<string, unknown>> = [];
    const url = await startServer(requests, { results: [{ index: 0, relevance_score: 0.75 }] });
    const reranker = new JinaReranker({ apiKey: "test", baseUrl: url });

    await expect(reranker.rerank("find parsing", ["parser"], { limit: 1 })).resolves.toEqual([
      { index: 0, score: 0.75 },
    ]);
    expect(reranker.profile).toEqual({ provider: "jina", model: "jina-reranker-v3.5" });
    expect(requests[0]).toMatchObject({ return_documents: false, top_n: 1 });
  });

  it("rejects malformed reranking responses", async () => {
    const url = await startServer([], { results: [{ index: 3, relevance_score: "high" }] });
    const reranker = new CohereReranker({ apiKey: "test", baseUrl: url });

    await expect(reranker.rerank("query", ["document"])).rejects.toThrow(/malformed reranking response/);
  });

  it("rejects duplicate indexes from the LLM", async () => {
    const url = await startServer([], {
      status: "completed",
      output: [{
        type: "message",
        role: "assistant",
        id: "message-1",
        content: [{
          type: "output_text",
          text: JSON.stringify({ ranking: [{ index: 0, score: 0.9 }, { index: 0, score: 0.8 }] }),
          annotations: [],
        }],
      }],
    });
    const reranker = new OpenAILLMReranker({ apiKey: "test", baseUrl: url });

    await expect(reranker.rerank("query", ["one", "two"], { limit: 2 })).rejects.toThrow(/invalid LLM reranking results/);
  });

  it("rejects the wrong number of LLM results", async () => {
    const url = await startServer([], {
      status: "completed",
      output: [{
        type: "message",
        role: "assistant",
        id: "message-1",
        content: [{ type: "output_text", text: JSON.stringify({ ranking: [] }), annotations: [] }],
      }],
    });
    const reranker = new OpenAILLMReranker({ apiKey: "test", baseUrl: url });

    await expect(reranker.rerank("query", ["one"], { limit: 1 })).rejects.toThrow(/invalid LLM reranking results/);
  });

  it("bounds source code sent to the LLM", async () => {
    const requests: Array<Record<string, unknown>> = [];
    const url = await startServer(requests, {
      status: "completed",
      output: [{
        type: "message",
        role: "assistant",
        id: "message-1",
        content: [{
          type: "output_text",
          text: JSON.stringify({ ranking: [{ index: 0, score: 0.8 }] }),
          annotations: [],
        }],
      }],
    });
    const reranker = new OpenAILLMReranker({ apiKey: "test", baseUrl: url });

    await reranker.rerank("query", [`description: important purpose\nsource:\n${"const value = 1;\n".repeat(20_000)}END_MARKER`]);

    const input = JSON.stringify(requests[0]!.input);
    expect(input).toContain("important purpose");
    expect(input).not.toContain("END_MARKER");
  });

  it("treats tokenizer special tokens as ordinary reranker input", async () => {
    const requests: Array<Record<string, unknown>> = [];
    const url = await startServer(requests, {
      status: "completed",
      output: [{
        type: "message",
        role: "assistant",
        id: "message-1",
        content: [{
          type: "output_text",
          text: JSON.stringify({ ranking: [{ index: 0, score: 1 }] }),
          annotations: [],
        }],
      }],
    });
    const reranker = new OpenAILLMReranker({ apiKey: "test", baseUrl: url });

    await expect(reranker.rerank("find marker", ["return '<|endoftext|>'"])).resolves.toEqual([
      { index: 0, score: 1 },
    ]);
    expect(JSON.stringify(requests[0]!.input)).toContain("<|endoftext|>");
  });
});

describe("external model call notices", () => {
  it("reports each kind once by default and every request in verbose mode", async () => {
    const fileSource = "export function deliver() { send(); }";
    const descriptionInput = {
      repository: "example",
      callable: parseCallables("client.ts", fileSource)[0]!,
      fileSource,
    };
    const embeddingUrl = await startServer([], { data: [{ index: 0, embedding: [1, 0] }] });
    const descriptionUrl = await startServer([], {
      status: "completed",
      output: [{
        type: "message",
        role: "assistant",
        id: "message-1",
        content: [{ type: "output_text", text: "Describes the callable.", annotations: [] }],
      }],
    });
    const rerankerUrl = await startServer([], { results: [{ index: 0, relevance_score: 0.8 }] });
    const stderr = vi.spyOn(process.stderr, "write").mockImplementation(() => true);

    try {
      const defaultEmbedding = new OpenAIEmbeddingProvider({
        apiKey: "test", baseUrl: embeddingUrl, model: "notice-vectors-default", dimensions: 2,
      });
      const verboseEmbedding = new OpenAIEmbeddingProvider({
        apiKey: "test", baseUrl: embeddingUrl, model: "notice-vectors-verbose", dimensions: 2, verbose: true, parallelism: 3,
      });
      const defaultDescriptions = new OpenAIDescriptionProvider({
        apiKey: "test", baseUrl: descriptionUrl, model: "notice-descriptions-default",
      });
      const verboseDescriptions = new OpenAIDescriptionProvider({
        apiKey: "test", baseUrl: descriptionUrl, model: "notice-descriptions-verbose", verbose: true,
      });
      const defaultReranker = new CohereReranker({
        apiKey: "test", baseUrl: rerankerUrl, model: "notice-reranking-default",
      });
      const verboseReranker = new CohereReranker({
        apiKey: "test", baseUrl: rerankerUrl, model: "notice-reranking-verbose", verbose: true,
      });

      for (let call = 0; call < 2; call += 1) {
        await defaultEmbedding.embedDocuments(["one"]);
        await verboseEmbedding.embedDocuments(["one"]);
        await defaultDescriptions.describe(descriptionInput);
        await verboseDescriptions.describe(descriptionInput);
        await defaultReranker.rerank("query", ["one"]);
        await verboseReranker.rerank("query", ["one"]);
      }

      const notices = stderr.mock.calls.map(([value]) => String(value));
      for (const kind of ["vectors", "descriptions", "reranking"]) {
        expect(notices.filter((line) => line.includes(`kind=${kind}`) && line.includes(`notice-${kind}-default`))).toHaveLength(1);
        expect(notices.filter((line) => line.includes(`kind=${kind}`) && line.includes(`notice-${kind}-verbose`))).toHaveLength(2);
      }
      expect(notices.filter((line) => line.includes("notice-vectors-verbose")).every((line) => line.includes("parallelism=3"))).toBe(true);
      expect(notices.filter((line) => line.includes("notice-vectors-default")).every((line) => line.includes("parallelism=10"))).toBe(true);
      expect(notices.filter((line) => line.includes("notice-reranking-default")).every((line) => line.includes("parallelism=1"))).toBe(true);
      expect(notices.every((line) => line.startsWith("slopdex: notice: external model call:") && line.endsWith("\n"))).toBe(true);
    } finally {
      stderr.mockRestore();
    }
  });
});

async function startServer(
  requests: unknown[],
  response: unknown | (() => unknown),
  exactUrl = false,
  paths?: string[],
  authorizations?: Array<string | undefined>,
  responseStatus: number | (() => number) = 200,
): Promise<string> {
  const server = createServer((request, serverResponse) => {
    const body: Buffer[] = [];
    request.on("data", (value: Buffer) => body.push(value));
    request.on("end", () => {
      paths?.push(request.url ?? "");
      authorizations?.push(request.headers.authorization);
      requests.push(JSON.parse(Buffer.concat(body).toString("utf8")));
      serverResponse.statusCode = typeof responseStatus === "function" ? responseStatus() : responseStatus;
      serverResponse.setHeader("content-type", "application/json");
      serverResponse.end(JSON.stringify(typeof response === "function" ? response() : response));
    });
  });
  servers.push(server);
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("Expected TCP server address.");
  const base = `http://127.0.0.1:${address.port}`;
  return exactUrl ? `${base}/v1/embeddings` : `${base}/v1`;
}
