import { loadConfig, saveConfig, DEFAULTS } from "./host/config.js";

const $ = (id) => document.getElementById(id);

async function load() {
  const c = await loadConfig();
  $("apiKey").value = c.apiKey;
  $("baseUrl").value = c.baseUrl;
  $("smallModel").value = c.models.small;
  $("mediumModel").value = c.models.medium;
  $("bigModel").value = c.models.big;
  $("temperature").value = String(c.temperature);
  $("autoApprove").checked = c.autoApprove;
}

$("save").addEventListener("click", async () => {
  const temp = parseFloat($("temperature").value);
  await saveConfig({
    apiKey: $("apiKey").value.trim(),
    baseUrl: $("baseUrl").value.trim() || DEFAULTS.baseUrl,
    models: {
      small: $("smallModel").value.trim() || DEFAULTS.models.small,
      medium: $("mediumModel").value.trim() || DEFAULTS.models.medium,
      big: $("bigModel").value.trim() || DEFAULTS.models.big,
    },
    temperature: Number.isFinite(temp) ? temp : DEFAULTS.temperature,
    autoApprove: $("autoApprove").checked,
  });
  const s = $("saved");
  s.textContent = "Saved ✓";
  setTimeout(() => (s.textContent = ""), 1500);
});

load();
