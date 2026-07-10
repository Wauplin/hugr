// The Chrome host: wires the generic Hugr agent driver to this extension's
// capability dispatcher (chrome.* APIs), IndexedDB storage, and system prompt.
import { invokeBrowserCapability } from "./chrome_api.js";
import { loadSettings, saveSession } from "./vendor/indexed_db.js";
import { SYSTEM_PROMPT } from "./system_prompt.js";

let wasmReady;

export const host = {
  async loadWasm() {
    if (!wasmReady) {
      wasmReady = import("./pkg/hugr_wasm.js").then(async (module) => {
        await module.default();
        return module.HugrWasm;
      });
    }
    return wasmReady;
  },
  invokeCapability: invokeBrowserCapability,
  loadSettings,
  saveSession,
  systemPrompt: SYSTEM_PROMPT,
  defaults: {
    baseUrl: "https://router.huggingface.co/v1",
    model: "google/gemma-4-31B-it:cerebras",
    maxModelCalls: 20,
    maxCostMicroUsd: 50000
  }
};
