import { confirm, input, number, search, select } from "@inquirer/prompts";

import type { DescriptionProviderName } from "./openai-description.js";
import { RERANKER_DEFAULT_MODELS, type FileConfig, type OpenCodeDescriptionProvider, type PublishedModel } from "./config.js";
import { DEFAULT_PARALLELISM } from "./utils.js";

export type { FileConfig, OpenCodeDescriptionProvider, PublishedModel } from "./config.js";

export interface InteractivePrompts {
  confirm: typeof confirm;
  input: typeof input;
  number: typeof number;
  search: typeof search;
  select: typeof select;
}

const defaultPrompts: InteractivePrompts = { confirm, input, number, search, select };

export async function configureInteractively(
  existing: FileConfig,
  fetchModels: (provider: OpenCodeDescriptionProvider) => Promise<PublishedModel[]>,
  prompts: InteractivePrompts = defaultPrompts,
): Promise<FileConfig> {
  const config = { ...existing };

  config.descriptionsEnabled = await prompts.confirm({
    message: "Generate file and function descriptions with an LLM?",
    default: existing.descriptionsEnabled ?? false,
  });
  if (config.descriptionsEnabled) {
    const provider = await prompts.select<DescriptionProviderName>({
      message: "Description provider",
      choices: [
        { name: "OpenCode Go", value: "opencode-go" },
        { name: "OpenCode Zen", value: "opencode" },
        { name: "OpenAI", value: "openai" },
      ],
      default: existing.descriptionProvider ?? "opencode-go",
    });
    config.descriptionProvider = provider;
    const openCodeProvider = isOpenCodeProvider(provider) ? provider : undefined;
    const published = openCodeProvider ? await fetchModels(openCodeProvider) : undefined;
    config.descriptionModel = published
      ? await choosePublishedModel(prompts, openCodeProvider!, published, "Description model", existing.descriptionProvider === provider ? existing.descriptionModel : undefined)
      : await prompts.input({
        message: "Description model",
        default: existing.descriptionProvider === provider ? existing.descriptionModel : "gpt-5.6-luna",
        required: true,
      });

    const useFallback = await prompts.confirm({
      message: "Configure a fallback description model?",
      default: Boolean(existing.descriptionFallbackModel),
    });
    if (useFallback) {
      config.descriptionFallbackModel = published
        ? await choosePublishedModel(
          prompts,
          openCodeProvider!,
          published,
          "Fallback description model",
          existing.descriptionProvider === provider ? existing.descriptionFallbackModel : undefined,
          config.descriptionModel,
        )
        : await prompts.input({
          message: "Fallback description model",
          default: existing.descriptionProvider === provider ? existing.descriptionFallbackModel : undefined,
          required: true,
        });
    } else {
      delete config.descriptionFallbackModel;
    }
  }

  config.rerankingEnabled = await prompts.confirm({
    message: "Enable second-stage reranking for searches?",
    default: existing.rerankingEnabled ?? false,
  });
  if (config.rerankingEnabled) {
    const provider = await prompts.select<"cohere" | "jina" | "openai">({
      message: "Reranker provider",
      choices: [
        { name: "Cohere", value: "cohere" },
        { name: "Jina", value: "jina" },
        { name: "OpenAI LLM", value: "openai" },
      ],
      default: existing.rerankerProvider ?? "cohere",
    });
    config.rerankerProvider = provider;
    config.rerankerModel = await prompts.input({
      message: "Reranker model",
      default: existing.rerankerProvider === provider ? existing.rerankerModel : RERANKER_DEFAULT_MODELS[provider],
      required: true,
    });
    if (provider === "openai") {
      config.rerankerCandidates = (await prompts.number({
        message: "Embedding-ranked candidates sent to the reranker",
        default: existing.rerankerProvider === provider ? existing.rerankerCandidates ?? 10 : 10,
        min: 1,
        max: 100,
        required: true,
        validate: positiveInteger,
      })) as number;
    } else {
      delete config.rerankerCandidates;
    }
  }

  const embeddingProvider = await prompts.select<"openai" | "jina">({
    message: "Embedding provider",
    choices: [
      { name: "OpenAI", value: "openai" },
      { name: "Jina", value: "jina" },
    ],
    default: existing.provider ?? "openai",
  });
  const embeddingDefaults = embeddingProvider === "openai"
    ? { model: "text-embedding-3-large", dimensions: 3072 }
    : { model: "jina-embeddings-v4", dimensions: 1024 };
  config.provider = embeddingProvider;
  config.model = await prompts.input({
    message: "Embedding model",
    default: existing.provider === embeddingProvider ? existing.model ?? embeddingDefaults.model : embeddingDefaults.model,
    required: true,
  });
  config.dimensions = (await prompts.number({
    message: "Embedding dimensions",
    default: existing.provider === embeddingProvider ? existing.dimensions ?? embeddingDefaults.dimensions : embeddingDefaults.dimensions,
    min: 1,
    required: true,
    validate: positiveInteger,
  })) as number;

  setOptionalString(config, "indexPath", await prompts.input({
    message: "Index path (blank for .slopdex/index.sqlite)",
    default: existing.indexPath,
  }));
  setOptionalList(config, "include", await prompts.input({
    message: "Include globs (comma-separated, blank for all eligible files)",
    default: existing.include?.join(", "),
  }));
  setOptionalList(config, "exclude", await prompts.input({
    message: "Additional exclude globs (comma-separated)",
    default: existing.exclude?.join(", "),
  }));
  config.maxFileSize = (await prompts.number({
    message: "Maximum source file size in bytes",
    default: existing.maxFileSize ?? 1_048_576,
    min: 1,
    required: true,
    validate: positiveInteger,
  })) as number;
  config.embeddingBatchSize = (await prompts.number({
    message: "Embedding batch size",
    default: existing.embeddingBatchSize ?? 32,
    min: 1,
    required: true,
    validate: positiveInteger,
  })) as number;
  config.parallelism = (await prompts.number({
    message: "Concurrent provider request limit",
    default: existing.parallelism ?? DEFAULT_PARALLELISM,
    min: 1,
    required: true,
    validate: positiveInteger,
  })) as number;
  config.verbose = await prompts.confirm({
    message: "Log every external model request?",
    default: existing.verbose ?? false,
  });

  return config;
}

async function choosePublishedModel(
  prompts: InteractivePrompts,
  provider: OpenCodeDescriptionProvider,
  published: readonly PublishedModel[],
  message: string,
  current?: string,
  excluded?: string,
): Promise<string> {
  const models = published.map(({ model }) => model).filter((model) => model !== excluded);
  if (models.length === 0) throw new Error(`${provider} returned no selectable models.`);
  const defaultModel = current && models.includes(current) ? current : undefined;
  return prompts.search<string>({
    message,
    ...(defaultModel ? { default: defaultModel } : {}),
    source: (term) => {
      const query = term?.trim().toLocaleLowerCase();
      const filtered = query ? models.filter((model) => model.toLocaleLowerCase().includes(query)) : models;
      return filtered.map((model) => ({ name: model, value: model }));
    },
  });
}

function isOpenCodeProvider(provider: DescriptionProviderName): provider is OpenCodeDescriptionProvider {
  return provider === "opencode" || provider === "opencode-go";
}

function setOptionalString(config: FileConfig, key: "indexPath", value: string): void {
  const normalized = value.trim();
  if (normalized) config[key] = normalized;
  else delete config[key];
}

function setOptionalList(config: FileConfig, key: "include" | "exclude", value: string): void {
  const entries = value.split(",").map((entry) => entry.trim()).filter(Boolean);
  if (entries.length > 0) config[key] = entries;
  else delete config[key];
}

function positiveInteger(value: number | undefined): boolean | string {
  return value !== undefined && Number.isInteger(value) && value > 0 ? true : "Enter a positive integer.";
}
