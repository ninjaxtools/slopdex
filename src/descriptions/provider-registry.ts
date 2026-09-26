export const DESCRIPTION_PROVIDER_NAMES = ["openai", "opencode", "opencode-go"] as const;

export type DescriptionProviderName = typeof DESCRIPTION_PROVIDER_NAMES[number];

export const DESCRIPTION_PROVIDER_DEFAULTS: Record<
  DescriptionProviderName,
  { apiKey: string; baseUrl: string; model: string }
> = {
  openai: { apiKey: "OPENAI_API_KEY", baseUrl: "https://api.openai.com/v1", model: "gpt-5.6-luna" },
  opencode: { apiKey: "OPENCODE_API_KEY", baseUrl: "https://opencode.ai/zen/v1", model: "muse-spark-1.3-contributor" },
  "opencode-go": { apiKey: "OPENCODE_API_KEY", baseUrl: "https://opencode.ai/zen/go/v1", model: "muse-spark-1.3-contributor" },
};

export function isDescriptionProviderName(value: unknown): value is DescriptionProviderName {
  return typeof value === "string" && DESCRIPTION_PROVIDER_NAMES.includes(value as DescriptionProviderName);
}

export function descriptionProviderBaseUrl(provider: DescriptionProviderName): string {
  return DESCRIPTION_PROVIDER_DEFAULTS[provider].baseUrl;
}
