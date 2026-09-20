import { describe, expect, it, vi } from "vitest";

import {
  configureInteractively,
  type InteractivePrompts,
  type PublishedModel,
} from "../src/config-wizard.js";

function fakePrompts(answers: {
  confirm: Record<string, boolean>;
  input?: Record<string, string>;
  number?: Record<string, number>;
  select?: Record<string, string>;
  search?: (options: {
    message: string;
    source: (term?: string) => ReadonlyArray<{ name?: string; value?: string }>;
  }) => Promise<string>;
}): InteractivePrompts {
  return {
    confirm: vi.fn(async ({ message }: { message: string }) => answers.confirm[message]!),
    input: vi.fn(async ({ message }: { message: string }) => answers.input?.[message] ?? ""),
    number: vi.fn(async ({ message }: { message: string }) => answers.number?.[message]),
    select: vi.fn(async ({ message }: { message: string }) => answers.select?.[message]),
    search: vi.fn(answers.search ?? (async () => "")),
  } as unknown as InteractivePrompts;
}

describe("interactive configuration", () => {
  it("disables descriptions without asking for an LLM provider or fetching models", async () => {
    const fetchModels = vi.fn<(provider: "opencode" | "opencode-go") => Promise<PublishedModel[]>>();
    const prompts = fakePrompts({
      confirm: {
        "Generate file and function descriptions with an LLM?": false,
        "Enable second-stage reranking for searches?": false,
        "Log every external model request?": true,
      },
      select: { "Embedding provider": "jina" },
      input: {
        "Embedding model": "jina-embeddings-v4",
        "Index path (blank for .slopdex/index.sqlite)": "",
        "Include globs (comma-separated, blank for all eligible files)": "src/**, packages/**",
        "Additional exclude globs (comma-separated)": "**/fixtures/**",
      },
      number: {
        "Embedding dimensions": 1024,
        "Maximum source file size in bytes": 2_000_000,
        "Embedding batch size": 64,
        "Concurrent provider request limit": 8,
      },
    });

    const config = await configureInteractively({
      descriptionsEnabled: true,
      descriptionProvider: "opencode-go",
      descriptionModel: "old-model",
      indexPath: "old.sqlite",
    }, fetchModels, prompts);

    expect(fetchModels).not.toHaveBeenCalled();
    expect(prompts.search).not.toHaveBeenCalled();
    expect(prompts.select).toHaveBeenCalledTimes(1);
    expect(config).toMatchObject({
      descriptionsEnabled: false,
      rerankingEnabled: false,
      provider: "jina",
      model: "jina-embeddings-v4",
      dimensions: 1024,
      include: ["src/**", "packages/**"],
      exclude: ["**/fixtures/**"],
      maxFileSize: 2_000_000,
      embeddingBatchSize: 64,
      parallelism: 8,
      verbose: true,
    });
    expect(config).not.toHaveProperty("indexPath");
  });

  it("fetches the selected OpenCode provider once and offers searchable primary and fallback models", async () => {
    const models: PublishedModel[] = [
      { provider: "opencode-go", model: "gpt-5.6-luna" },
      { provider: "opencode-go", model: "muse-spark-1.3-contributor" },
    ];
    const fetchModels = vi.fn(async () => models);
    const prompts = fakePrompts({
      confirm: {
        "Generate file and function descriptions with an LLM?": true,
        "Configure a fallback description model?": true,
        "Enable second-stage reranking for searches?": true,
        "Log every external model request?": false,
      },
      select: {
        "Description provider": "opencode-go",
        "Reranker provider": "openai",
        "Embedding provider": "openai",
      },
      search: async ({ message, source }) => {
        if (message === "Description model") {
          expect(source("luna").map(({ value }) => value)).toEqual(["gpt-5.6-luna"]);
          return "gpt-5.6-luna";
        }
        expect(source("spark").map(({ value }) => value)).toEqual(["muse-spark-1.3-contributor"]);
        return "muse-spark-1.3-contributor";
      },
      input: {
        "Reranker model": "gpt-5.6-luna",
        "Embedding model": "text-embedding-3-large",
        "Index path (blank for .slopdex/index.sqlite)": ".cache/slopdex.sqlite",
        "Include globs (comma-separated, blank for all eligible files)": "",
        "Additional exclude globs (comma-separated)": "",
      },
      number: {
        "Embedding-ranked candidates sent to the reranker": 20,
        "Embedding dimensions": 3072,
        "Maximum source file size in bytes": 1_048_576,
        "Embedding batch size": 32,
        "Concurrent provider request limit": 10,
      },
    });

    const config = await configureInteractively({}, fetchModels, prompts);

    expect(fetchModels).toHaveBeenCalledOnce();
    expect(fetchModels).toHaveBeenCalledWith("opencode-go");
    expect(prompts.search).toHaveBeenCalledTimes(2);
    expect(config).toMatchObject({
      descriptionsEnabled: true,
      descriptionProvider: "opencode-go",
      descriptionModel: "gpt-5.6-luna",
      descriptionFallbackModel: "muse-spark-1.3-contributor",
      indexPath: ".cache/slopdex.sqlite",
      provider: "openai",
      rerankingEnabled: true,
      rerankerProvider: "openai",
      rerankerModel: "gpt-5.6-luna",
      rerankerCandidates: 20,
    });
    expect(config).not.toHaveProperty("include");
    expect(config).not.toHaveProperty("exclude");
  });
});
