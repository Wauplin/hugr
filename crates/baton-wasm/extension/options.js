import { loadConfig, saveConfig, DEFAULTS } from "./host/config.js";

const $ = (id) => document.getElementById(id);

async function load() {
  const c = await loadConfig();
  $("apiKey").value = c.apiKey;
  $("baseUrl").value = c.baseUrl;
  $("model").value = c.model;
  $("temperature").value = String(c.temperature);
  $("autoApprove").checked = c.autoApprove;
}

$("save").addEventListener("click", async () => {
  const temp = parseFloat($("temperature").value);
  await saveConfig({
    apiKey: $("apiKey").value.trim(),
    baseUrl: $("baseUrl").value.trim() || DEFAULTS.baseUrl,
    model: $("model").value.trim() || DEFAULTS.model,
    temperature: Number.isFinite(temp) ? temp : DEFAULTS.temperature,
    autoApprove: $("autoApprove").checked,
  });
  const s = $("saved");
  s.textContent = "Saved ✓";
  setTimeout(() => (s.textContent = ""), 1500);
});

load();
