import { Tiktoken } from "js-tiktoken/lite";
import cl100kBase from "js-tiktoken/ranks/cl100k_base";

import type { EmbeddingProvider } from "../types.js";
import { requestEmbeddings } from "./http.js";

const MAX_INPUT_TOKENS = 8192;
let tokenizer: Tiktoken | undefined;

function truncateInput(input: string): string {
  tokenizer ??= new Tiktoken(cl100kBase);
  const tokens = tokenizer.encode(input);
  return tokens.length <= MAX_INPUT_TOKENS ? input : tokenizer.decode(tokens.slice(0, MAX_INPUT_TOKENS));
}

export interface OpenAIEmbeddingProviderOptions {
  apiKey?: string;
  model?: string;
  dimensions?: number;
  baseUrl?: string;
}

export class OpenAIEmbeddingProvider implements EmbeddingProvider {
  public readonly profile;
  readonly #apiKey: string;
  readonly #url: string;

  public constructor(options: OpenAIEmbeddingProviderOptions = {}) {
    this.#apiKey = options.apiKey ?? process.env.OPENAI_API_KEY ?? "";
    if (!this.#apiKey) throw new Error("OPENAI_API_KEY is required.");
    const model = options.model ?? "text-embedding-3-large";
    const dimensions = options.dimensions ?? 3072;
    if (!Number.isInteger(dimensions) || dimensions < 1) throw new Error("dimensions must be a positive integer.");
    this.profile = { provider: "openai", model, dimensions, strategyVersion: "callable-v1" } as const;
    this.#url = `${(options.baseUrl ?? "https://api.openai.com/v1").replace(/\/$/, "")}/embeddings`;
  }

  public embedDocuments(inputs: readonly string[], options?: { signal?: AbortSignal }): Promise<number[][]> {
    if (inputs.length === 0) return Promise.resolve([]);
    return this.#embed(inputs, options?.signal);
  }

  public async embedQuery(input: string, options?: { signal?: AbortSignal }): Promise<number[]> {
    return (await this.#embed([input], options?.signal))[0]!;
  }

  #embed(inputs: readonly string[], signal?: AbortSignal): Promise<number[][]> {
    return requestEmbeddings({
      url: this.#url,
      apiKey: this.#apiKey,
      body: {
        model: this.profile.model,
        dimensions: this.profile.dimensions,
        encoding_format: "float",
        input: inputs.map(truncateInput),
      },
      expectedCount: inputs.length,
      dimensions: this.profile.dimensions,
      ...(signal ? { signal } : {}),
    });
  }
}
