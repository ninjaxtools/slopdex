import { CodeIndexError } from "../errors.js";
import type { EffectiveConfig } from "../config.js";
import type { CodeIndex } from "../code-index.js";
import type { DescriptionProviderName, OpenAIDescriptionProvider } from "../descriptions/openai.js";
import type { DescriptionProfile, EmbeddingProvider, Reranker } from "../types.js";

export interface DescriptionRefreshHooks {
  beforeRefresh?: (index: CodeIndex) => void | Promise<void>;
  afterRefresh?: (index: CodeIndex) => void | Promise<void>;
}

interface DescriptionProviderContext {
  required?: boolean;
  storedProfile?: DescriptionProfile | null;
}

export async function createDescriptionProvider(
  config: EffectiveConfig,
  context: DescriptionProviderContext = {},
): Promise<OpenAIDescriptionProvider | undefined> {
  if (!context.required && !config.descriptionProvider && !config.descriptionModel && !config.descriptionFallbackModel) return undefined;
  let provider = config.descriptionProvider;
  let model = config.descriptionModel;
  if (context.required && !provider && !model && context.storedProfile) {
    provider = context.storedProfile.provider as DescriptionProviderName;
    model = context.storedProfile.model;
  }
  const { OpenAIDescriptionProvider: Provider } = await import("../descriptions/openai.js");
  return new Provider({
    ...(provider ? { provider } : {}),
    ...(model ? { model } : {}),
    ...(config.descriptionFallbackModel ? { fallbackModel: config.descriptionFallbackModel } : {}),
    parallelism: config.parallelism,
    ...(config.verbose ? { verbose: true } : {}),
  });
}

export function descriptionRefresh(
  config: EffectiveConfig,
  descriptionsAction: string | undefined | null = null,
): DescriptionRefreshHooks | undefined {
  if (descriptionsAction !== null) {
    return descriptionsAction === "disable"
      ? { beforeRefresh: (index) => { index.disableDescriptions(); } }
      : enableDescriptionRefresh(false);
  }
  if (config.descriptionsEnabled === true) return enableDescriptionRefresh();
  if (config.descriptionsEnabled === false) return { beforeRefresh: (index) => { index.disableDescriptions(); } };
  return undefined;
}

function enableDescriptionRefresh(afterRefresh = true): DescriptionRefreshHooks {
  return {
    ...(afterRefresh ? { afterRefresh: async (index: CodeIndex) => { await index.useDescriptions(); } } : {}),
  };
}

export async function createProvider(config: EffectiveConfig): Promise<EmbeddingProvider> {
  if (config.provider === "jina") {
    const { JinaEmbeddingProvider } = await import("../embeddings/jina.js");
    return new JinaEmbeddingProvider({
      ...(config.model ? { model: config.model } : {}),
      ...(config.dimensions ? { dimensions: config.dimensions } : {}),
      parallelism: config.parallelism,
      ...(config.verbose ? { verbose: true } : {}),
    });
  }
  const { OpenAIEmbeddingProvider } = await import("../embeddings/openai.js");
  return new OpenAIEmbeddingProvider({
    ...(config.model ? { model: config.model } : {}),
    ...(config.dimensions ? { dimensions: config.dimensions } : {}),
    parallelism: config.parallelism,
    ...(config.verbose ? { verbose: true } : {}),
  });
}

export async function createReranker(config: EffectiveConfig): Promise<Reranker | undefined> {
  if (config.rerankingEnabled !== true) return undefined;
  if (config.rerankerProvider === "cohere" || config.rerankerProvider === "jina") {
    const { CohereReranker, JinaReranker } = await import("../rerankers/hosted.js");
    if (config.rerankerProvider === "cohere") {
      return new CohereReranker({
        ...(config.rerankerModel ? { model: config.rerankerModel } : {}),
        ...(config.verbose ? { verbose: true } : {}),
      });
    }
    return new JinaReranker({
      ...(config.rerankerModel ? { model: config.rerankerModel } : {}),
      ...(config.verbose ? { verbose: true } : {}),
    });
  }
  if (config.rerankerProvider === "openai") {
    const { OpenAILLMReranker } = await import("../rerankers/openai.js");
    return new OpenAILLMReranker({
      ...(config.rerankerModel ? { model: config.rerankerModel } : {}),
      ...(config.rerankerCandidates ? { candidateCount: config.rerankerCandidates } : {}),
      ...(config.verbose ? { verbose: true } : {}),
    });
  }
  throw new CodeIndexError("rerankerProvider is required when rerankingEnabled is true.");
}
