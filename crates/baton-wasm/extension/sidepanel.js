// The side-panel host: loads the WASM brain, builds the Engine, and renders the
// brain's OutputEvents to the DOM. This file is the front-end (ARCHITECTURE §9)
// plus the glue that wires the (identical) core into a browser environment.

import init, { BatonBrain, version } from "./wasm/baton_wasm.js";
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
function escapeHtml(s) {
  return String(s).replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]);
}
// Extremely small markdown-ish renderer: escape, then bold + inline code + breaks.
function miniMarkdown(s) {
  return escapeHtml(s)
    .replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>")
    .replace(/`([^`]+?)`/g, "<code>$1</code>")
    .replace(/\n/g, "<br />");
}

// ---------------------------------------------------------------------------
// The front-end: turns brain OutputEvents + lifecycle hooks into DOM.
// ---------------------------------------------------------------------------
class Frontend {
  constructor() {
    this.assistantBubbles = new Map(); // op -> {textEl}
    this.toolCards = new Map(); // op -> card element
  }

  clearHint() {
    $("hint")?.remove();
  }

  userMessage(text) {
    this.clearHint();
    const row = el("div", "msg user");
    row.appendChild(el("div", "bubble", text));
    logEl.appendChild(row);
    scrollDown();
  }

  onModelStart(op) {
    const row = el("div", "msg assistant");
    const bubble = el("div", "bubble");
    const textEl = el("span", "stream");
    bubble.appendChild(textEl);
    row.appendChild(bubble);
    logEl.appendChild(row);
    this.assistantBubbles.set(op, { textEl, buffer: "" });
    scrollDown();
  }

  onOutput(event) {
    const [type, body] = Object.entries(event)[0];
    if (type === "ModelText") {
      const b = this.assistantBubbles.get(body.op);
      if (b) {
        b.buffer += body.text;
        b.textEl.textContent = b.buffer;
        scrollDown();
      }
    } else if (type === "ModelReasoning") {
      const b = this.assistantBubbles.get(body.op);
      if (b) {
        if (!b.reasoningEl) {
          b.reasoningEl = el("div", "reasoning");
          b.textEl.parentElement.prepend(b.reasoningEl);
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
      b.textEl.parentElement.appendChild(meta);
    }
    // If the model produced no text (pure tool call), drop the empty bubble.
    if (!b.buffer && !b.reasoningEl) {
      b.textEl.closest(".msg")?.remove();
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

  // --- interactive prompts (return promises) ------------------------------
  choice(title, bodyHtml, buttons) {
    return new Promise((resolve) => {
      this.clearHint();
      const card = el("div", "prompt");
      card.appendChild(el("div", "prompt-title", title));
      const bodyEl = el("div", "prompt-body");
      bodyEl.innerHTML = bodyHtml;
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
    const body = `Allow Baton to run <code>${escapeHtml(capability)}</code>` +
      (args ? ` with <code>${escapeHtml(JSON.stringify(args)).slice(0, 200)}</code>` : "") + "?";
    return this.choice("Permission requested", body, [
      { label: "Allow", value: true, cls: "primary", answerLabel: "Allowed" },
      { label: "Deny", value: false, answerLabel: "Denied" },
    ]);
  }

  // ui.confirm for the ask_user_confirmation tool
  confirm(markdown) {
    return this.choice("Baton asks", miniMarkdown(markdown), [
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

function setBusy(v) {
  busy = v;
  $("send").disabled = v;
  $("stop").classList.toggle("hidden", !v);
  $("input").disabled = v;
}

function banner(msg, kind = "info") {
  const b = $("banner");
  b.textContent = msg;
  b.className = `banner ${kind}`;
}

async function boot() {
  await init(); // instantiate the WASM module (needs 'wasm-unsafe-eval' CSP)
  const config = await loadConfig();
  $("subtitle").textContent = `core v${version()} · ${config.model}`;
  $("auto-approve").checked = config.autoApprove;

  if (!config.apiKey) {
    banner("No API key set — open Settings (⚙) to add one before chatting.", "warn");
  }

  frontend = new Frontend();
  const tools = createTools(frontend); // ui.confirm / ui.showPlan live on the frontend
  const brain = new BatonBrain(JSON.stringify(buildPolicy(config)));
  engine = new Engine({ brain, config, tools, frontend });
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

$("auto-approve").addEventListener("change", (e) => {
  if (engine) engine.config.autoApprove = e.target.checked;
  saveConfig({ autoApprove: e.target.checked });
});

$("settings-btn").addEventListener("click", () => chrome.runtime.openOptionsPage());

boot().catch((e) => {
  banner(`Failed to start Baton: ${e?.message || e}`, "warn");
  console.error(e);
});
