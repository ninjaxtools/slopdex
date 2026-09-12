import { APICallError } from "ai";

import { CodeIndexError } from "../errors.js";
import { normalizeEmbeddingVector } from "../utils.js";

export async function requestEmbeddings(
  request: Promise<readonly unknown[]>,
  dimensions: number,
  signal?: AbortSignal,
): Promise<number[][]> {
  try {
    const embeddings = await request;
    return embeddings.map((embedding) => normalizeEmbeddingVector(embedding, dimensions));
  } catch (error) {
    if (signal?.aborted) throw error;
    const detail = APICallError.isInstance(error) && error.responseBody
      ? error.responseBody.slice(0, 500)
      : error instanceof Error ? error.message : String(error);
    throw new CodeIndexError(`Embedding request failed: ${detail}`, { cause: error });
  }
}
