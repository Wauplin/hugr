import { runAgent } from "./vendor/agent_driver.js";
import { host } from "./host.js";
import { deleteLocalFile, deleteSession, getLocalFile, getSession, listFiles, listSessions, loadSettings, saveSettings } from "./vendor/indexed_db.js";

const views = new Map([...document.querySelectorAll(".view")].map((node) => [node.id.replace("view-", ""), node]));
const buttons = [...document.querySelectorAll("nav button")];
const status = document.querySelector("#status");
const chat = document.querySelector("#chat");
const events = document.querySelector("#events");
const composer = document.querySelector("#composer");
const questionInput = document.querySelector("#question");
const askButton = document.querySelector("#ask");
const interruptButton = document.querySelector("#interrupt");
let activeSessionId = null;
let activeAbortController = null;

buttons.forEach((button) => {
  button.addEventListener("click", () => showView(button.dataset.view));
});

questionInput.addEventListener("keydown", (event) => {
  if (event.key !== "Enter" || event.shiftKey || event.isComposing) return;
  event.preventDefault();
  if (!askButton.disabled) {
    composer.requestSubmit();
  }
});

composer.addEventListener("submit", async (event) => {
  event.preventDefault();
  const question = questionInput.value.trim();
  if (!question) return;
  questionInput.value = "";
  setRunning(true);
  addMessage("user", question);
  const assistant = addMessage("assistant", "");
  activeAbortController = new AbortController();
  try {
    const result = await runAgent(question, host, {
      signal: activeAbortController.signal,
      onText: (text) => {
        assistant.textContent += text;
        chat.scrollTop = chat.scrollHeight;
      },
      onEvent: renderEvent
    });
    if (!result.status.ok && !result.answer) {
      assistant.remove();
      addMessage("error", result.status.label);
    } else if (!assistant.textContent.trim()) {
      assistant.textContent = result.answer || "(no text answer)";
    }
    setStatus(result.status.ok ? "Completed" : result.status.label, result.status.ok ? "idle" : "error");
    await renderSessions(result.traceId);
    await renderFiles();
  } catch (error) {
    const message = String(error?.message || error);
    assistant.remove();
    addMessage("error", message);
    renderEvent({ type: "error", label: "Run failed", detail: { error: message } });
    setStatus(`Error: ${message}`, "error");
  } finally {
    activeAbortController = null;
    setRunning(false);
  }
});

document.querySelector("#save-settings").addEventListener("click", async () => {
  await saveSettings(readSettingsForm());
  setStatus("Settings saved", "idle");
});

document.querySelector("#new-chat").addEventListener("click", () => {
  activeAbortController?.abort();
  clearRun();
  addMessage("system", "New chat started. Saved sessions are unchanged.");
  setStatus("Idle", "idle");
  questionInput.focus();
});

interruptButton.addEventListener("click", () => {
  if (!activeAbortController) return;
  activeAbortController.abort();
  renderEvent({ type: "interrupt", label: "Interrupt requested", detail: {} });
  setStatus("Interrupting", "running");
});

document.querySelector("#download-session").addEventListener("click", async () => {
  if (!activeSessionId) return;
  const session = await getSession(activeSessionId);
  if (session) downloadJson(`hugr-wasm-session-${activeSessionId}.json`, session);
});

document.querySelector("#download-sessions").addEventListener("click", async () => {
  downloadJson(`hugr-wasm-sessions-${new Date().toISOString().replace(/[:.]/g, "-")}.json`, await listSessions());
});

document.querySelector("#download-files-metadata").addEventListener("click", async () => {
  const files = await listFiles();
  downloadJson(`hugr-wasm-files-${new Date().toISOString().replace(/[:.]/g, "-")}.json`, files.map(withoutBlob));
});

await hydrate();

function showView(name) {
  for (const [viewName, node] of views) {
    node.classList.toggle("active", viewName === name);
  }
  for (const button of buttons) {
    button.classList.toggle("active", button.dataset.view === name);
  }
}

async function hydrate() {
  const settings = await loadSettings();
  document.querySelector("#api-key").value = settings.apiKey || "";
  document.querySelector("#base-url").value = settings.baseUrl || "https://router.huggingface.co/v1";
  document.querySelector("#model").value = settings.model || "google/gemma-4-31B-it:cerebras";
  addMessage("system", "Ready. Browser actions run without permission prompts.");
  await renderSessions();
  await renderFiles();
}

function readSettingsForm() {
  return {
    apiKey: document.querySelector("#api-key").value,
    baseUrl: document.querySelector("#base-url").value,
    model: document.querySelector("#model").value,
  };
}

async function renderSessions(selectTraceId = activeSessionId) {
  const list = document.querySelector("#sessions");
  const sessions = await listSessions();
  if (!sessions.length) {
    list.replaceChildren(emptyItem("No sessions yet"));
    renderSessionDetail(null);
    return;
  }
  activeSessionId = selectTraceId || sessions[0].traceId;
  const rows = sessions.map((session) => {
    const li = document.createElement("li");
    const button = document.createElement("button");
    button.className = "session-row";
    button.classList.toggle("active", session.traceId === activeSessionId);
    button.type = "button";
    const title = document.createElement("strong");
    title.textContent = session.question || "Untitled session";
    const meta = document.createElement("small");
    meta.textContent = `${session.status || "unknown"} · ${formatDate(session.createdAt)} · ${session.traceId}`;
    button.append(title, meta);
    button.addEventListener("click", async () => {
      activeSessionId = session.traceId;
      await renderSessions(activeSessionId);
      renderSessionDetail(await getSession(session.traceId));
    });
    const remove = document.createElement("button");
    remove.className = "danger-button";
    remove.type = "button";
    remove.textContent = "Delete";
    remove.addEventListener("click", async () => {
      if (!confirm("Delete this session?")) return;
      await deleteSession(session.traceId);
      if (activeSessionId === session.traceId) activeSessionId = null;
      await renderSessions();
    });
    li.append(button, remove);
    return li;
  });
  list.replaceChildren(...rows);
  renderSessionDetail(await getSession(activeSessionId));
}

function renderSessionDetail(session) {
  const detail = document.querySelector("#session-detail");
  if (!session) {
    detail.replaceChildren(withClass("p", "muted", "Select a session"));
    return;
  }
  const title = document.createElement("strong");
  title.textContent = session.question || "Untitled session";
  const meta = withClass("div", "muted", `${session.status || "unknown"} · ${formatDate(session.createdAt)}`);
  const answer = document.createElement("pre");
  answer.textContent = session.answer || "(no text answer)";
  const timeline = document.createElement("pre");
  timeline.textContent = (session.events || []).map((event) => `${event.at || ""} ${event.label || event.type}`).join("\n");
  detail.replaceChildren(title, meta, answer, timeline);
}

async function renderFiles() {
  const list = document.querySelector("#files");
  const files = await listFiles();
  list.replaceChildren(...(files.length ? files.map(fileItem) : [emptyItem("No files")]));
}

function clearRun() {
  chat.replaceChildren();
  events.replaceChildren();
}

function addMessage(role, text) {
  const node = document.createElement("div");
  node.className = `message ${role}`;
  node.textContent = text;
  chat.append(node);
  chat.scrollTop = chat.scrollHeight;
  return node;
}

function renderEvent(event) {
  const row = document.createElement("div");
  row.className = "event";
  const title = document.createElement("strong");
  title.textContent = event.label || event.type || "event";
  const detail = document.createElement("code");
  detail.textContent = event.detail === undefined ? "" : JSON.stringify(event.detail, null, 2);
  row.append(title);
  if (detail.textContent) row.append(detail);
  events.append(row);
  events.scrollTop = events.scrollHeight;
}

function setRunning(running) {
  askButton.disabled = running;
  questionInput.disabled = running;
  interruptButton.disabled = !running;
  if (running) setStatus("Running", "running");
}

function setStatus(text, mode) {
  status.textContent = text;
  status.className = `status ${mode || "idle"}`;
}

function item(text) {
  const li = document.createElement("li");
  li.textContent = text;
  return li;
}

function emptyItem(text) {
  const li = item(text);
  li.className = "muted";
  return li;
}

function withClass(tag, className, text) {
  const node = document.createElement(tag);
  node.className = className;
  node.textContent = text;
  return node;
}

function formatDate(value) {
  if (!value) return "unknown time";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

function downloadJson(filename, value) {
  const blob = new Blob([JSON.stringify(value, null, 2)], { type: "application/json" });
  downloadBlob(filename, blob);
}

function fileItem(file) {
  const li = document.createElement("li");
  const row = document.createElement("div");
  row.className = "file-row";
  const label = document.createElement("span");
  label.textContent = `${file.filename || file.fileId} (${file.byteLength || 0} bytes)`;
  const button = document.createElement("button");
  button.type = "button";
  button.textContent = "Download";
  button.addEventListener("click", async () => {
    const stored = await getLocalFile(file.fileId);
    if (!stored?.blob) return;
    downloadBlob(stored.filename || stored.fileId || "hugr-wasm-file", stored.blob);
  });
  const remove = document.createElement("button");
  remove.className = "danger-button";
  remove.type = "button";
  remove.textContent = "Delete";
  remove.addEventListener("click", async () => {
    if (!confirm("Delete this file?")) return;
    await deleteLocalFile(file.fileId);
    await renderFiles();
  });
  row.append(label, button, remove);
  li.append(row);
  return li;
}

function withoutBlob(file) {
  const { blob, ...metadata } = file;
  return metadata;
}

function downloadBlob(filename, blob) {
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  document.body.append(anchor);
  anchor.click();
  anchor.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
