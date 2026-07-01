// Host configuration, persisted in chrome.storage.local. In a browser there are
// no env vars or token files (unlike the native CLI's OpenAiAdapter::from_env),
// so the user sets these once on the Options page.

/** @typedef {{ small: string, medium: string, big: string }} TierModels */
/** @typedef {{ apiKey: string, baseUrl: string, model?: string, models: TierModels, autoApprove: boolean, temperature: number }} Config */

/** Defaults mirror the native adapter: the Hugging Face router, OpenAI-compatible. */
export const DEFAULTS = {
  apiKey: "",
  // The base URL the adapter posts to; it appends `/chat/completions`.
  baseUrl: "https://router.huggingface.co/v1",
  // All three tiers may point at the same concrete HF router model initially.
  models: {
    small: "google/gemma-4-31B-it:cerebras",
    medium: "google/gemma-4-31B-it:cerebras",
    big: "google/gemma-4-31B-it:cerebras",
  },
  // Legacy single-model key; migrated by loadConfig.
  model: "google/gemma-4-31B-it:cerebras",
  // When true, navigation tools run in yolo mode and skip the judge.
  autoApprove: false,
  temperature: 0.2,
};

/** Load the merged config (defaults + stored overrides). @returns {Promise<Config>} */
export async function loadConfig() {
  const stored = await chrome.storage.local.get(Object.keys(DEFAULTS));
  const merged = { ...DEFAULTS, ...stored };
  const legacyModel = stored.model || DEFAULTS.model;
  merged.models = {
    ...DEFAULTS.models,
    ...(stored.models || {}),
  };
  for (const tier of ["small", "medium", "big"]) {
    if (!merged.models[tier]) merged.models[tier] = legacyModel;
  }
  return merged;
}

/** Persist a partial config update. @param {Partial<Config>} patch */
export async function saveConfig(patch) {
  await chrome.storage.local.set(patch);
}
