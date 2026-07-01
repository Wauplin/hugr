// The side-panel host: loads the WASM brain, builds the Engine, and renders the
// brain's OutputEvents to the DOM. This file is the front-end (ARCHITECTURE §9)
// plus the glue that wires the (identical) core into a browser environment.

import init, { HugrBrain, version } from "./wasm/hugr_wasm.js";
import { Engine, buildPolicy } from "./host/engine.js";
import { createTools } from "./host/tools.js";
import { loadConfig, saveConfig } from "./host/config.js";

const $ = (id) => document.getElementById(id);
const logEl = $("log");

// ---------------------------------------------------------------------------
// Tiny DOM helpers
// ---------------------------------------------------------------------------
function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}
function scrollDown() {
  logEl.scrollTop = logEl.scrollHeight;
}
function estimateTextTokens(text) {
  return Math.max(1, Math.ceil(String(text || "").length / 4));
}
function isSafeUrl(url) {
  try {
    const parsed = new URL(url, "https://example.invalid");
    return ["http:", "https:", "mailto:"].includes(parsed.protocol);
  } catch {
    return false;
  }
}

function nextInlineMarker(text, start) {
  let next = text.length;
  for (const marker of ["`", "**", "__", "~~", "*", "_", "[", "<", "\\"]) {
    const idx = text.indexOf(marker, start);
    if (idx !== -1 && idx < next) next = idx;
  }
  return next;
}

function appendInline(parent, source) {
  const text = String(source);
  let i = 0;
  while (i < text.length) {
    if (text[i] === "\\" && i + 1 < text.length) {
      parent.appendChild(document.createTextNode(text[i + 1]));
      i += 2;
      continue;
    }

    if (text[i] === "`") {
      const end = text.indexOf("`", i + 1);
      if (end !== -1) {
        parent.appendChild(el("code", null, text.slice(i + 1, end)));
        i = end + 1;
        continue;
      }
    }

    const paired = [
      ["**", "strong"],
      ["__", "strong"],
      ["~~", "s"],
      ["*", "em"],
      ["_", "em"],
    ];
    let consumed = false;
    for (const [mark, tag] of paired) {
      if (!text.startsWith(mark, i)) continue;
      const end = text.indexOf(mark, i + mark.length);
      if (end === -1) continue;
      const node = document.createElement(tag);
      appendInline(node, text.slice(i + mark.length, end));
      parent.appendChild(node);
      i = end + mark.length;
      consumed = true;
      break;
    }
    if (consumed) continue;

    if (text[i] === "[") {
      const labelEnd = text.indexOf("](", i + 1);
      const urlEnd = labelEnd === -1 ? -1 : text.indexOf(")", labelEnd + 2);
      if (labelEnd !== -1 && urlEnd !== -1) {
        const label = text.slice(i + 1, labelEnd);
        const url = text.slice(labelEnd + 2, urlEnd).trim();
        if (isSafeUrl(url)) {
          const link = document.createElement("a");
          link.href = url;
          link.target = "_blank";
          link.rel = "noreferrer";
          appendInline(link, label || url);
          parent.appendChild(link);
        } else {
          parent.appendChild(document.createTextNode(label || url));
        }
        i = urlEnd + 1;
        continue;
      }
    }

    if (text[i] === "<") {
      const end = text.indexOf(">", i + 1);
      const maybeUrl = end === -1 ? "" : text.slice(i + 1, end).trim();
      if (maybeUrl && isSafeUrl(maybeUrl)) {
        const link = document.createElement("a");
        link.href = maybeUrl;
        link.target = "_blank";
        link.rel = "noreferrer";
        link.textContent = maybeUrl;
        parent.appendChild(link);
        i = end + 1;
        continue;
      }
    }

    const next = nextInlineMarker(text, i + 1);
    parent.appendChild(document.createTextNode(text.slice(i, next)));
    i = next;
  }
}

function appendInlineBlock(parent, tag, text, cls) {
  const node = el(tag, cls);
  appendInline(node, text);
  parent.appendChild(node);
  return node;
}

function isFence(line) {
  return line.match(/^\s*```(.*)$/);
}

function isHr(line) {
  return /^\s{0,3}([-*_])(?:\s*\1){2,}\s*$/.test(line);
}

function isTableSeparator(line) {
  return /^\s*\|?\s*:?-{3,}:?\s*(?:\|\s*:?-{3,}:?\s*)+\|?\s*$/.test(line);
}

function splitTableRow(line) {
  const trimmed = line.trim().replace(/^\|/, "").replace(/\|$/, "");
  const cells = [];
  let cell = "";
  let escaped = false;
  for (const ch of trimmed) {
    if (escaped) {
      cell += ch;
      escaped = false;
    } else if (ch === "\\") {
      escaped = true;
    } else if (ch === "|") {
      cells.push(cell.trim());
      cell = "";
    } else {
      cell += ch;
    }
  }
  cells.push(cell.trim());
  return cells;
}

function startsBlock(line, nextLine = "") {
  return Boolean(
    isFence(line) ||
      /^\s{0,3}#{1,6}\s+/.test(line) ||
      /^\s{0,3}>\s?/.test(line) ||
      /^\s{0,3}(?:[-*+]\s+|\d+[.)]\s+)/.test(line) ||
      isHr(line) ||
      (line.includes("|") && isTableSeparator(nextLine)),
  );
}

function renderMarkdownBlocks(lines, start = 0, stop = lines.length) {
  const frag = document.createDocumentFragment();
  let i = start;

  while (i < stop) {
    const line = lines[i];
    if (!line.trim()) {
      i++;
      continue;
    }

    const fence = isFence(line);
    if (fence) {
      const info = fence[1].trim();
      const chunks = [];
      i++;
      while (i < stop && !/^\s*```\s*$/.test(lines[i])) {
        chunks.push(lines[i]);
        i++;
      }
      if (i < stop) i++;
      const pre = el("pre", "md-code");
      const code = el("code", null, chunks.join("\n"));
      if (info) code.dataset.language = info.split(/\s+/)[0];
      pre.appendChild(code);
      frag.appendChild(pre);
      continue;
    }

    const heading = line.match(/^\s{0,3}(#{1,6})\s+(.+?)\s*#*\s*$/);
    if (heading) {
      const level = Math.min(heading[1].length + 1, 6);
      appendInlineBlock(frag, `h${level}`, heading[2], "md-heading");
      i++;
      continue;
    }

    if (isHr(line)) {
      frag.appendChild(document.createElement("hr"));
      i++;
      continue;
    }

    if (/^\s{0,3}>\s?/.test(line)) {
      const quoteLines = [];
      while (i < stop && (/^\s{0,3}>\s?/.test(lines[i]) || !lines[i].trim())) {
        quoteLines.push(lines[i].replace(/^\s{0,3}>\s?/, ""));
        i++;
      }
      const quote = el("blockquote");
      quote.appendChild(renderMarkdownBlocks(quoteLines));
      frag.appendChild(quote);
      continue;
    }

    const listMatch = line.match(/^\s{0,3}(([-*+])|(\d+)[.)])\s+(.+)$/);
    if (listMatch) {
      const ordered = Boolean(listMatch[3]);
      const list = document.createElement(ordered ? "ol" : "ul");
      while (i < stop) {
        const item = lines[i].match(/^\s{0,3}(([-*+])|(\d+)[.)])\s+(.+)$/);
        if (!item || Boolean(item[3]) !== ordered) break;
        const li = document.createElement("li");
        const checkbox = item[4].match(/^\[( |x|X)\]\s+(.*)$/);
        if (checkbox) {
          const input = document.createElement("input");
          input.type = "checkbox";
          input.checked = checkbox[1].toLowerCase() === "x";
          input.disabled = true;
          li.appendChild(input);
          appendInline(li, checkbox[2]);
        } else {
          appendInline(li, item[4]);
        }
        list.appendChild(li);
        i++;
      }
      frag.appendChild(list);
      continue;
    }

    if (line.includes("|") && i + 1 < stop && isTableSeparator(lines[i + 1])) {
      const headers = splitTableRow(line);
      const table = el("table");
      const thead = document.createElement("thead");
      const headRow = document.createElement("tr");
      for (const header of headers) appendInlineBlock(headRow, "th", header);
      thead.appendChild(headRow);
      table.appendChild(thead);
      i += 2;
      const tbody = document.createElement("tbody");
      while (i < stop && lines[i].includes("|") && lines[i].trim()) {
        const row = document.createElement("tr");
        const cells = splitTableRow(lines[i]);
        for (let c = 0; c < headers.length; c++) appendInlineBlock(row, "td", cells[c] || "");
        tbody.appendChild(row);
        i++;
      }
      table.appendChild(tbody);
      frag.appendChild(table);
      continue;
    }

    const paragraph = [line.trim()];
    i++;
    while (i < stop && lines[i].trim() && !startsBlock(lines[i], lines[i + 1] || "")) {
      paragraph.push(lines[i].trim());
      i++;
    }
    appendInlineBlock(frag, "p", paragraph.join(" "));
  }

  return frag;
}

function renderMarkdown(markdown) {
  return renderMarkdownBlocks(String(markdown).replace(/\r\n?/g, "\n").split("\n"));
}

function renderMarkdownInto(target, markdown) {
  target.replaceChildren(renderMarkdown(markdown));
}

function createHint() {
  const hint = el("div", "hint");
  hint.id = "hint";
  const p1 = document.createElement("p");
  appendInline(p1, "**Hugr** is a runtime-free agent brain (`hugr-core`, compiled to WebAssembly) running entirely in this panel - no backend.");
  const p2 = document.createElement("p");
  appendInline(p2, "It can **read pages** and **navigate tabs**, but cannot click or submit forms.");
  const p3 = document.createElement("p");
  p3.append("Try: ");
  p3.appendChild(el("em", null, '"Summarize the current page"'));
  p3.append(" · ");
  p3.appendChild(el("em", null, '"List my open tabs"'));
  p3.append(" · ");
  p3.appendChild(el("em", null, '"Open Hacker News and give me the top 5 headlines."'));
  hint.append(p1, p2, p3);
  return hint;
}

function traceJsonl(engine) {
  const log = JSON.parse(engine.brain.logJson());
  const lines = [
    {
      kind: "meta",
      format: "hugr-browser-trace-jsonl",
      format_version: 1,
      codename: "hugr",
      created_at: engine.createdAt,
      exported_at: Date.now(),
      core_version: version(),
    },
    { kind: "policy", policy: buildPolicy(engine.config) },
    { kind: "blobs", blobs: [] },
  ];
  engine.events.forEach((event, index) => lines.push({ kind: "event", index, event }));
  log.forEach((entry, index) => lines.push({ kind: "log", index, entry }));
  return lines.map((line) => JSON.stringify(line)).join("\n") + "\n";
}

function downloadText(filename, text, type) {
  const blob = new Blob([text], { type });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.style.display = "none";
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 0);
}

function enumType(value) {
  if (typeof value === "string") return value;
  if (!value || typeof value !== "object") return "Unknown";
  return Object.keys(value)[0] || "Unknown";
}

function enumBody(value) {
  if (!value || typeof value !== "object") return null;
  const key = Object.keys(value)[0];
  return key ? value[key] : null;
}

function sourceLabel(source) {
  const type = enumType(source);
  const body = enumBody(source);
  if (type === "System") return "system";
  if (type === "LogEntry") return `log:${body?.seq ?? "?"}`;
  if (type === "Synthetic") return `synthetic:${body?.label ?? "?"}`;
  return type;
}

function dispositionLabel(disposition) {
  return enumType(disposition).replace(/([a-z])([A-Z])/g, "$1 $2").toLowerCase();
}

function selectorName(selector) {
  const type = enumType(selector);
  const body = enumBody(selector);
  if (type === "Named") return String(body || "unknown");
  return type;
}

function countDisposition(plan, kind) {
  return (plan.entries || []).filter((entry) => dispositionLabel(entry.disposition) === kind).length;
}

function renderContextPlan(plan) {
  const body = $("context-body");
  const totals = plan.totals || {};
  const summary = el("div", "context-summary");
  const stats = [
    ["Used", `${totals.used_tokens || 0}/${plan.budget?.max_tokens || 0}`],
    ["Retained", String(countDisposition(plan, "included"))],
    ["Summaries", String(countDisposition(plan, "summarized"))],
    ["Refs", String(countDisposition(plan, "referenced"))],
    ["Omitted", String(countDisposition(plan, "omitted"))],
    ["Tools", String(plan.tools?.length || 0)],
  ];
  for (const [label, value] of stats) {
    const stat = el("div", "context-stat");
    stat.appendChild(el("strong", null, value));
    stat.append(label);
    summary.appendChild(stat);
  }

  const entries = el("div", "context-entries");
  for (const entry of plan.entries || []) {
    const disposition = dispositionLabel(entry.disposition);
    const row = el("div", "context-entry");
    row.appendChild(el("span", "context-source", sourceLabel(entry.source)));
    row.appendChild(el("span", `context-disposition ${disposition}`, disposition));
    row.appendChild(el("span", "context-tokens", `${entry.est_tokens || 0}`));
    row.appendChild(el("span", "context-reason", entry.reason || ""));
    entries.appendChild(row);
  }

  body.replaceChildren(summary, entries);
}

function renderSkills() {
  const body = $("skills-body");
  const skills = currentConfig?.skills || [];
  if (!skills.length) {
    body.replaceChildren(el("div", "context-entry", "No skills configured."));
    return;
  }
  const active = engine?.activeSkill || null;
  const entries = el("div", "context-entries");
  for (const skill of skills) {
    const row = el("div", "context-entry");
    row.appendChild(el("span", "context-source", skill.id || "skill"));
    row.appendChild(el("span", "context-disposition", active === skill.id ? "active" : "available"));
    row.appendChild(el("span", "context-tokens", `${skill.est_tokens || estimateTextTokens(skill.instructions || "")}`));
    row.appendChild(el("span", "context-reason", skill.summary || skill.title || ""));
    entries.appendChild(row);
  }
  body.replaceChildren(entries);
}

// ---------------------------------------------------------------------------
// The front-end: turns brain OutputEvents + lifecycle hooks into DOM.
// ---------------------------------------------------------------------------
class Frontend {
  constructor() {
    this.assistantBubbles = new Map(); // op -> {contentEl, buffer}
    this.toolCards = new Map(); // op -> card element
  }

  clearHint() {
    $("hint")?.remove();
  }

  reset() {
    logEl.replaceChildren(createHint());
  }

  userMessage(text) {
    this.clearHint();
    const row = el("div", "msg user");
    row.appendChild(el("div", "bubble", text));
    logEl.appendChild(row);
    scrollDown();
  }

  onModelStart(op, model) {
    const row = el("div", "msg assistant");
    const bubble = el("div", "bubble");
    const head = el("div", "response-head");
    head.appendChild(el("span", `tier-chip tier-${selectorName(model)}`, `used ${selectorName(model)}`));
    const contentEl = el("div", "stream markdown");
    bubble.appendChild(head);
    bubble.appendChild(contentEl);
    row.appendChild(bubble);
    logEl.appendChild(row);
    this.assistantBubbles.set(op, { contentEl, buffer: "" });
    scrollDown();
  }

  onOutput(event) {
    const [type, body] = Object.entries(event)[0];
    if (type === "ModelText") {
      const b = this.assistantBubbles.get(body.op);
      if (b) {
        b.buffer += body.text;
        renderMarkdownInto(b.contentEl, b.buffer);
        scrollDown();
      }
    } else if (type === "ModelReasoning") {
      const b = this.assistantBubbles.get(body.op);
      if (b) {
        if (!b.reasoningEl) {
          b.reasoningEl = el("div", "reasoning");
          b.contentEl.parentElement.prepend(b.reasoningEl);
        }
        b.reasoningEl.textContent += body.text;
        scrollDown();
      }
    } else if (type === "ToolChunk") {
      const card = this.toolCards.get(body.op);
      if (card) {
        const pre = card.querySelector(".tool-stream") || card.appendChild(el("pre", "tool-stream"));
        pre.textContent += typeof body.chunk === "string" ? body.chunk : JSON.stringify(body.chunk);
      }
    } else if (type === "Notice") {
      logEl.appendChild(el("div", "notice", body));
      scrollDown();
    }
  }

  onModelEnd(op, usage) {
    const b = this.assistantBubbles.get(op);
    if (!b) return;
    if (usage && (usage.input_tokens || usage.output_tokens)) {
      const cost = usage.extra?.cost != null ? ` · $${usage.extra.cost.toFixed(4)}` : "";
      const meta = el("div", "meta", `${usage.input_tokens}→${usage.output_tokens} tok${cost}`);
      b.contentEl.parentElement.appendChild(meta);
    }
    // If the model produced no text (pure tool call), drop the empty bubble.
    if (!b.buffer && !b.reasoningEl) {
      b.contentEl.closest(".msg")?.remove();
    }
  }

  onToolStart(op, name, args) {
    const card = el("div", "tool-card");
    const head = el("div", "tool-head");
    head.appendChild(el("span", "tool-name", name));
    const argStr = JSON.stringify(args);
    if (argStr && argStr !== "{}") head.appendChild(el("span", "tool-args", argStr.slice(0, 160)));
    card.appendChild(head);
    logEl.appendChild(card);
    this.toolCards.set(op, card);
    scrollDown();
  }

  onToolEnd(op, name, result, isError) {
    const card = this.toolCards.get(op);
    if (!card) return;
    card.classList.add(isError ? "err" : "ok");
    const body = el("pre", "tool-result");
    const text = typeof result === "string" ? result : JSON.stringify(result, null, 2);
    const lines = text.split("\n");
    const head = lines.slice(0, 8).join("\n");
    body.textContent = lines.length > 8 ? `${head}\n… +${lines.length - 8} lines` : head;
    if (lines.length > 8) {
      body.title = "click to expand";
      body.classList.add("collapsed");
      body.addEventListener("click", () => {
        body.textContent = text;
        body.classList.remove("collapsed");
      });
    }
    card.appendChild(body);
    scrollDown();
  }

  onDone() {
    /* the composer re-enables in runTurn's finally */
  }

  onSkillActive() {
    if (!$("skills-drawer").classList.contains("hidden")) renderSkills();
  }

  // --- interactive prompts (return promises) ------------------------------
  choice(title, bodyContent, buttons) {
    return new Promise((resolve) => {
      this.clearHint();
      const card = el("div", "prompt");
      card.appendChild(el("div", "prompt-title", title));
      const bodyEl = el("div", "prompt-body");
      if (bodyContent && typeof bodyContent.nodeType === "number") bodyEl.appendChild(bodyContent);
      else bodyEl.textContent = String(bodyContent);
      card.appendChild(bodyEl);
      const row = el("div", "prompt-actions");
      for (const b of buttons) {
        const btn = el("button", `btn ${b.cls || ""}`, b.label);
        btn.addEventListener("click", () => {
          row.querySelectorAll("button").forEach((x) => (x.disabled = true));
          card.classList.add("answered");
          card.appendChild(el("div", "prompt-answer", b.answerLabel ?? b.label));
          resolve(b.value);
        });
        row.appendChild(btn);
      }
      card.appendChild(row);
      logEl.appendChild(card);
      scrollDown();
    });
  }

  confirmPermission(capability, args) {
    const body = document.createDocumentFragment();
    body.append("Allow Hugr to run ");
    body.appendChild(el("code", null, capability));
    if (args) {
      body.append(" with ");
      body.appendChild(el("code", null, JSON.stringify(args).slice(0, 200)));
    }
    body.append("?");
    return this.choice("Permission requested", body, [
      { label: "Allow", value: true, cls: "primary", answerLabel: "Allowed" },
      { label: "Deny", value: false, answerLabel: "Denied" },
    ]);
  }

  // ui.confirm for the ask_user_confirmation tool
  confirm(markdown) {
    return this.choice("Hugr asks", renderMarkdown(markdown), [
      { label: "Yes", value: true, cls: "primary", answerLabel: "Yes" },
      { label: "No", value: false, answerLabel: "No" },
    ]);
  }

  // ui.showPlan for the show_plan tool
  showPlan(steps) {
    this.clearHint();
    const card = el("div", "plan");
    card.appendChild(el("div", "plan-title", "Plan"));
    const ol = el("ol");
    for (const s of steps) ol.appendChild(el("li", null, s));
    card.appendChild(ol);
    logEl.appendChild(card);
    scrollDown();
  }

  ask(message) {
    return new Promise((resolve) => {
      const card = el("div", "prompt");
      card.appendChild(el("div", "prompt-title", message));
      const input = el("input", "prompt-input");
      const row = el("div", "prompt-actions");
      const btn = el("button", "btn primary", "Send");
      const submit = () => {
        btn.disabled = true;
        input.disabled = true;
        resolve(input.value);
      };
      btn.addEventListener("click", submit);
      input.addEventListener("keydown", (e) => e.key === "Enter" && submit());
      row.appendChild(input);
      row.appendChild(btn);
      card.appendChild(row);
      logEl.appendChild(card);
      input.focus();
      scrollDown();
    });
  }
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------
let engine = null;
let frontend = null;
let busy = false;
let currentConfig = null;

function setBusy(v) {
  busy = v;
  $("send").disabled = v;
  $("stop").classList.toggle("hidden", !v);
  $("input").disabled = v;
  $("new-chat-btn").disabled = v;
  $("context-btn").disabled = v;
  $("skills-btn").disabled = v;
  $("compact-btn").disabled = v;
  $("export-trace-btn").disabled = v;
  $("tier-override").disabled = v;
}

function banner(msg, kind = "info") {
  const b = $("banner");
  b.textContent = msg;
  b.className = `banner ${kind}`;
}

function startSession(config, resetLog = false) {
  if (resetLog) frontend?.reset();
  frontend = new Frontend();
  const tools = createTools(frontend); // ui.confirm / ui.showPlan live on the frontend
  const brain = new HugrBrain(JSON.stringify(buildPolicy(config)));
  engine = new Engine({ brain, config, tools, frontend });
  $("tier-override").value = "";
  if (!$("context-drawer").classList.contains("hidden")) refreshContextDrawer();
  if (!$("skills-drawer").classList.contains("hidden")) renderSkills();
}

function refreshContextDrawer() {
  if (!engine) return;
  try {
    renderContextPlan(engine.contextPlan());
  } catch (e) {
    banner(`Failed to inspect context: ${e?.message || e}`, "warn");
    console.error(e);
  }
}

function openContextDrawer() {
  $("context-drawer").classList.remove("hidden");
  refreshContextDrawer();
}

function openSkillsDrawer() {
  $("skills-drawer").classList.remove("hidden");
  renderSkills();
}

async function boot() {
  await init(); // instantiate the WASM module (needs 'wasm-unsafe-eval' CSP)
  currentConfig = await loadConfig();
  $("subtitle").textContent = `core v${version()} · medium ${currentConfig.models.medium}`;
  $("auto-approve").checked = currentConfig.autoApprove;

  if (!currentConfig.apiKey) {
    banner("No API key set — open Settings (⚙) to add one before chatting.", "warn");
  }
  if ((currentConfig.mcpServers || []).length) {
    banner(
      "MCP stdio servers are configured but unavailable in the browser host; use the CLI --mcp path or a future native bridge.",
      "warn",
    );
  }

  startSession(currentConfig);
}

async function runTurn(text) {
  if (busy || !engine) return;
  frontend.userMessage(text);
  setBusy(true);
  try {
    await engine.userTurn(text);
  } catch (e) {
    banner(`Error: ${e?.message || e}`, "warn");
    console.error(e);
  } finally {
    setBusy(false);
    $("tier-override").value = engine?.tierOverride || "";
    $("input").focus();
  }
}

// --- wire up the UI ---------------------------------------------------------
$("composer").addEventListener("submit", (e) => {
  e.preventDefault();
  const text = $("input").value.trim();
  if (!text) return;
  $("input").value = "";
  runTurn(text);
});

$("input").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    $("composer").requestSubmit();
  }
});

$("stop").addEventListener("click", () => engine?.abort());

$("context-btn").addEventListener("click", () => {
  if (busy) return;
  if ($("context-drawer").classList.contains("hidden")) openContextDrawer();
  else $("context-drawer").classList.add("hidden");
});

$("context-close").addEventListener("click", () => $("context-drawer").classList.add("hidden"));

$("skills-btn").addEventListener("click", () => {
  if (busy) return;
  if ($("skills-drawer").classList.contains("hidden")) openSkillsDrawer();
  else $("skills-drawer").classList.add("hidden");
});

$("skills-close").addEventListener("click", () => $("skills-drawer").classList.add("hidden"));

$("compact-btn").addEventListener("click", async () => {
  if (busy || !engine) return;
  setBusy(true);
  try {
    await engine.compactContext();
    if (!$("context-drawer").classList.contains("hidden")) refreshContextDrawer();
    banner("Compaction requested.", "info");
  } catch (e) {
    banner(`Failed to compact context: ${e?.message || e}`, "warn");
    console.error(e);
  } finally {
    setBusy(false);
    $("input").focus();
  }
});

$("auto-approve").addEventListener("change", (e) => {
  if (engine) engine.config.autoApprove = e.target.checked;
  if (currentConfig) currentConfig.autoApprove = e.target.checked;
  saveConfig({ autoApprove: e.target.checked });
});

$("tier-override").addEventListener("change", (e) => {
  if (!engine) return;
  const tier = e.target.value || null;
  engine.overrideNextModel(tier);
  banner(tier ? `Next turn tier: ${tier}` : "Next turn tier: auto", "info");
});

$("new-chat-btn").addEventListener("click", () => {
  if (busy || !currentConfig) return;
  $("input").value = "";
  startSession(currentConfig, true);
  renderSkills();
  $("input").focus();
});

$("export-trace-btn").addEventListener("click", () => {
  if (busy || !engine) return;
  try {
    const stamp = new Date().toISOString().replace(/[:.]/g, "-");
    downloadText(`hugr-trace-${stamp}.jsonl`, traceJsonl(engine), "application/x-ndjson");
  } catch (e) {
    banner(`Failed to export trace: ${e?.message || e}`, "warn");
    console.error(e);
  }
});

$("settings-btn").addEventListener("click", () => chrome.runtime.openOptionsPage());

boot().catch((e) => {
  banner(`Failed to start Hugr: ${e?.message || e}`, "warn");
  console.error(e);
});
