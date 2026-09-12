import { createOpenAI } from "@ai-sdk/openai";
import { embed, embedMany } from "ai";
import { Tiktoken } from "js-tiktoken/lite";
import cl100kBase from "js-tiktoken/ranks/cl100k_base";

import type { EmbeddingProvider } from "../types.js";
import { requestEmbeddings } from "./ai-sdk.js";

const MAX_INPUT_TOKENS = 8192;
let tokenizer: Tiktoken | undefined;

function truncateInput(input: string): string {
  tokenizer ??= new Tiktoken(cl100kBase);
  const tokens = tokenizer.encode(input, [], []);
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
  readonly #model;

  public constructor(options: OpenAIEmbeddingProviderOptions = {}) {
    const apiKey = options.apiKey ?? process.env.OPENAI_API_KEY ?? "";
    if (!apiKey) throw new Error("OPENAI_API_KEY is required.");
    const model = options.model ?? "text-embedding-3-large";
    const dimensions = options.dimensions ?? 3072;
    if (!Number.isInteger(dimensions) || dimensions < 1) throw new Error("dimensions must be a positive integer.");
    this.profile = { provider: "openai", model, dimensions, strategyVersion: "callable-v2" } as const;
    this.#model = createOpenAI({
      apiKey,
      baseURL: (options.baseUrl ?? "https://api.openai.com/v1").replace(/\/$/, ""),
    }).embeddingModel(model);
  }

  public async embedDocuments(inputs: readonly string[], options?: { signal?: AbortSignal }): Promise<number[][]> {
    if (inputs.length === 0) return [];
    return requestEmbeddings(
      embedMany({
        model: this.#model,
        values: inputs.map(truncateInput),
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
        model: this.#model,
        value: truncateInput(input),
        providerOptions: { openai: { dimensions: this.profile.dimensions } },
        ...(options?.signal ? { abortSignal: options.signal } : {}),
      }).then(({ embedding }) => [embedding]),
      this.profile.dimensions,
      options?.signal,
    ))[0]!;
  }
}
