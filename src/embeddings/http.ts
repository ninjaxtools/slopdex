import { CodeIndexError } from "../errors.js";
import { normalizeEmbeddingVector } from "../utils.js";

interface EmbeddingResponse {
  data?: Array<{ index?: number; embedding?: number[] }>;
}

export async function requestEmbeddings(options: {
  url: string;
  apiKey: string;
  body: Record<string, unknown>;
  expectedCount: number;
  dimensions: number;
  signal?: AbortSignal;
}): Promise<number[][]> {
  let lastError: unknown;
  for (let attempt = 0; attempt < 3; attempt += 1) {
    try {
      const response = await fetch(options.url, {
        method: "POST",
        headers: {
          authorization: `Bearer ${options.apiKey}`,
          "content-type": "application/json",
        },
        body: JSON.stringify(options.body),
        ...(options.signal ? { signal: options.signal } : {}),
      });
      if (!response.ok) {
        const detail = (await response.text()).slice(0, 500);
        const error = new CodeIndexError(`Embedding request failed (${response.status}): ${detail}`);
        if (response.status !== 429 && response.status < 500) throw error;
        lastError = error;
      } else {
        const payload = await response.json() as EmbeddingResponse;
        if (!Array.isArray(payload.data) || payload.data.length !== options.expectedCount) {
          throw new CodeIndexError("Embedding provider returned an unexpected number of vectors.");
        }
        const indexes = payload.data.map((item) => item.index);
        if (indexes.some((index) => !Number.isInteger(index))
          || new Set(indexes).size !== options.expectedCount
          || indexes.some((index) => index! < 0 || index! >= options.expectedCount)) {
          throw new CodeIndexError("Embedding provider returned invalid vector indexes.");
        }
        const ordered = new Array<{ index?: number; embedding?: number[] }>(options.expectedCount);
        for (const item of payload.data) ordered[item.index!] = item;
        try {
          return ordered.map((item) => normalizeEmbeddingVector(item.embedding, options.dimensions));
        } catch {
          throw new CodeIndexError("Embedding provider returned a malformed vector.");
        }
      }
    } catch (error) {
      if (options.signal?.aborted) throw error;
      lastError = error;
      if (error instanceof CodeIndexError && !/\((429|5\d\d)\)/.test(error.message)) throw error;
    }
    await new Promise((resolve) => setTimeout(resolve, 250 * 2 ** attempt));
  }
  throw new CodeIndexError("Embedding request failed after three attempts.", { cause: lastError });
}
