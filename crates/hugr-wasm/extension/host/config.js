// Host configuration, persisted in chrome.storage.local. In a browser there are
// no env vars or token files (unlike the native CLI's OpenAiAdapter::from_env),
// so the user sets these once on the Options page.

/** @typedef {{ apiKey: string, baseUrl: string, model: string, autoApprove: boolean, temperature: number }} Config */

/** Defaults mirror the native adapter: the Hugging Face router, OpenAI-compatible. */
export const DEFAULTS = {
  apiKey: "",
  // The base URL the adapter posts to; it appends `/chat/completions`.
  baseUrl: "https://router.huggingface.co/v1",
  // A capable, widely-available default on the HF router.
  model: "google/gemma-4-31B-it:cerebras",
  // When true, navigation tools run without a permission prompt (the "-y" mode).
  autoApprove: false,
  temperature: 0.2,
};

/** Load the merged config (defaults + stored overrides). @returns {Promise<Config>} */
export async function loadConfig() {
  const stored = await chrome.storage.local.get(Object.keys(DEFAULTS));
  return { ...DEFAULTS, ...stored };
}

/** Persist a partial config update. @param {Partial<Config>} patch */
export async function saveConfig(patch) {
  await chrome.storage.local.set(patch);
}
