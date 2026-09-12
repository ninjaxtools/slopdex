import { CodeIndexError } from "../errors.js";
import { reportModelCall } from "../model-call-notice.js";
import type { Reranker } from "../types.js";
import { assertPositiveInteger, throwIfAborted } from "../utils.js";

interface HostedRerankerOptions {
  apiKey?: string;
  model?: string;
  baseUrl?: string;
  verbose?: boolean;
}

interface HostedRerankerSettings {
  provider: "cohere" | "jina";
  apiKeyName: "COHERE_API_KEY" | "JINA_API_KEY";
  defaultModel: string;
  defaultUrl: string;
  returnDocuments?: boolean;
}

abstract class HostedReranker implements Reranker {
  public readonly profile;
  readonly #apiKey: string;
  readonly #url: string;
  readonly #returnDocuments: boolean | undefined;
  readonly #verbose: boolean;

  protected constructor(options: HostedRerankerOptions, settings: HostedRerankerSettings) {
    this.#apiKey = options.apiKey ?? process.env[settings.apiKeyName] ?? "";
    if (!this.#apiKey) throw new Error(`${settings.apiKeyName} is required.`);
    const model = options.model ?? settings.defaultModel;
    if (!model.trim()) throw new Error("reranker model must not be empty.");
    this.profile = { provider: settings.provider, model } as const;
    this.#verbose = options.verbose ?? false;
    const url = (options.baseUrl ?? settings.defaultUrl).replace(/\/$/, "");
    this.#url = url.endsWith("/rerank") ? url : `${url}/rerank`;
    this.#returnDocuments = settings.returnDocuments;
  }

  public async rerank(
    query: string,
    documents: readonly string[],
    options: { limit?: number; signal?: AbortSignal } = {},
  ): Promise<Array<{ index: number; score: number }>> {
    if (documents.length === 0) return [];
    const limit = options.limit ?? documents.length;
    assertPositiveInteger(limit, "rerank limit");
    throwIfAborted(options.signal);
    reportModelCall("reranking", this.profile, this.#verbose);
    let response: Response;
    try {
      response = await fetch(this.#url, {
        method: "POST",
        headers: {
          accept: "application/json",
          authorization: `Bearer ${this.#apiKey}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({
          model: this.profile.model,
          query,
          documents,
          top_n: Math.min(limit, documents.length),
          ...(this.#returnDocuments !== undefined ? { return_documents: this.#returnDocuments } : {}),
        }),
        ...(options.signal ? { signal: options.signal } : {}),
      });
    } catch (error) {
      if (options.signal?.aborted) throw error;
      throw new CodeIndexError(`Reranking request failed: ${error instanceof Error ? error.message : String(error)}`, { cause: error });
    }
    if (!response.ok) {
      throw new CodeIndexError(`Reranking request failed (${response.status}): ${(await response.text()).slice(0, 500)}`);
    }
    let body: unknown;
    try {
      body = await response.json();
    } catch (error) {
      throw new CodeIndexError(`${this.profile.provider} returned a malformed reranking response.`, { cause: error });
    }
    const results = body && typeof body === "object" && "results" in body
      ? (body as { results?: unknown }).results
      : undefined;
    const expected = Math.min(limit, documents.length);
    if (!Array.isArray(results) || results.length !== expected) {
      throw new CodeIndexError(`${this.profile.provider} returned a malformed reranking response.`);
    }
    const seen = new Set<number>();
    return results.map((result) => {
      const value = result as { index?: unknown; relevance_score?: unknown };
      if (!result || typeof result !== "object"
        || !Number.isInteger(value.index) || (value.index as number) < 0 || (value.index as number) >= documents.length
        || typeof value.relevance_score !== "number" || !Number.isFinite(value.relevance_score)
        || seen.has(value.index as number)) {
        throw new CodeIndexError(`${this.profile.provider} returned a malformed reranking response.`);
      }
      seen.add(value.index as number);
      return { index: value.index as number, score: value.relevance_score };
    });
  }
}

export type CohereRerankerOptions = HostedRerankerOptions;

export class CohereReranker extends HostedReranker {
  public constructor(options: CohereRerankerOptions = {}) {
    super(options, {
      provider: "cohere",
      apiKeyName: "COHERE_API_KEY",
      defaultModel: "rerank-v4.0-pro",
      defaultUrl: "https://api.cohere.com/v2",
    });
  }
}

export type JinaRerankerOptions = HostedRerankerOptions;

export class JinaReranker extends HostedReranker {
  public constructor(options: JinaRerankerOptions = {}) {
    super(options, {
      provider: "jina",
      apiKeyName: "JINA_API_KEY",
      defaultModel: "jina-reranker-v3.5",
      defaultUrl: "https://api.jina.ai/v1",
      returnDocuments: false,
    });
  }
}
