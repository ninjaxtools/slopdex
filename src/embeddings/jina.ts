import { createOpenAI } from "@ai-sdk/openai";
import { embed, embedMany } from "ai";

import type { EmbeddingProvider } from "../types.js";
import { requestEmbeddings } from "./ai-sdk.js";

export interface JinaEmbeddingProviderOptions {
  apiKey?: string;
  model?: string;
  dimensions?: number;
  baseUrl?: string;
}

export class JinaEmbeddingProvider implements EmbeddingProvider {
  public readonly profile;
  readonly #apiKey: string;
  readonly #baseUrl: string;

  public constructor(options: JinaEmbeddingProviderOptions = {}) {
    this.#apiKey = options.apiKey ?? process.env.JINA_API_KEY ?? "";
    if (!this.#apiKey) throw new Error("JINA_API_KEY is required.");
    const model = options.model ?? "jina-embeddings-v4";
    const dimensions = options.dimensions ?? 1024;
    if (!Number.isInteger(dimensions) || dimensions < 1) throw new Error("dimensions must be a positive integer.");
    this.profile = {
      provider: "jina",
      model,
      dimensions,
      strategyVersion: "callable-v2:code-query-passage",
    } as const;
    const url = (options.baseUrl ?? "https://api.jina.ai/v1/embeddings").replace(/\/$/, "");
    this.#baseUrl = url.endsWith("/embeddings") ? url.slice(0, -"/embeddings".length) : url;
  }

  public async embedDocuments(inputs: readonly string[], options?: { signal?: AbortSignal }): Promise<number[][]> {
    if (inputs.length === 0) return [];
    return requestEmbeddings(
      embedMany({
        model: this.#model("code.passage"),
        values: [...inputs],
        providerOptions: { openai: { dimensions: this.profile.dimensions } },
        ...(options?.signal ? { abortSignal: options.signal } : {}),
      }).then(({ embeddings }) => embeddings),
      this.profile.dimensions,
      options?.signal,
    );
  }

  public async embedQuery(input: string, options?: { signal?: AbortSignal }): Promise<number[]> {
    return (await requestEmbeddings(
      embed({
        model: this.#model("code.query"),
        value: input,
        providerOptions: { openai: { dimensions: this.profile.dimensions } },
        ...(options?.signal ? { abortSignal: options.signal } : {}),
      }).then(({ embedding }) => [embedding]),
      this.profile.dimensions,
      options?.signal,
    ))[0]!;
  }

  #model(task: "code.passage" | "code.query") {
    const request: typeof fetch = (input, init) => {
      const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
      delete body.encoding_format;
      return fetch(input, {
        ...init,
        body: JSON.stringify({ ...body, embedding_type: "float", truncate: true, task }),
      });
    };
    return createOpenAI({
      apiKey: this.#apiKey,
      baseURL: this.#baseUrl,
      name: "jina",
      fetch: request,
    }).embeddingModel(this.profile.model);
  }
}
