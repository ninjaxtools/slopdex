import type { EmbeddingProvider } from "../types.js";
import { requestEmbeddings } from "./http.js";

export interface JinaEmbeddingProviderOptions {
  apiKey?: string;
  model?: string;
  dimensions?: number;
  baseUrl?: string;
}

export class JinaEmbeddingProvider implements EmbeddingProvider {
  public readonly profile;
  readonly #apiKey: string;
  readonly #url: string;

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
      strategyVersion: "callable-v1:code-query-passage",
    } as const;
    this.#url = options.baseUrl ?? "https://api.jina.ai/v1/embeddings";
  }

  public embedDocuments(inputs: readonly string[], options?: { signal?: AbortSignal }): Promise<number[][]> {
    if (inputs.length === 0) return Promise.resolve([]);
    return this.#embed(inputs, "code.passage", options?.signal);
  }

  public async embedQuery(input: string, options?: { signal?: AbortSignal }): Promise<number[]> {
    return (await this.#embed([input], "code.query", options?.signal))[0]!;
  }

  #embed(inputs: readonly string[], task: "code.passage" | "code.query", signal?: AbortSignal): Promise<number[][]> {
    return requestEmbeddings({
      url: this.#url,
      apiKey: this.#apiKey,
      body: {
        model: this.profile.model,
        dimensions: this.profile.dimensions,
        embedding_type: "float",
        truncate: true,
        task,
        input: inputs,
      },
      expectedCount: inputs.length,
      dimensions: this.profile.dimensions,
      ...(signal ? { signal } : {}),
    });
  }
}
