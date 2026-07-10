# Your first Chrome extension

In this tutorial you'll build your own browser-agent Chrome extension — different tools, different UI — from the same three reusable pieces the shipped example uses: the generic WASM brain bindings in `crates/hugr-wasm`, the generic JS host modules in `bindings/typescript`, and a thin Chrome-specific layer you write yourself. You'll learn what each layer provides, how `examples/chrome-extension` wires them together, and exactly which files to copy, keep, and swap. For why the brain is sans-IO and every effect is injected, see [`ARCHITECTURE.md`](../../ARCHITECTURE.md) — this tutorial is about assembly, not rationale.

## The three layers

A browser host is a stack, and only the top layer knows about Chrome:

- **`crates/hugr-wasm`** compiles `hugr-core` to `wasm32-unknown-unknown` and exposes it over wasm-bindgen. The extension-facing class is `HugrWasm`: you construct it with a JSON config (`BrowserAgentConfig`: `base_url`, `model`, `api_key`, `max_model_calls`, `max_cost_micro_usd`, `system_prompt`, `context`), then drive it with `submit_user_input`, `poll_commands_json`, `submit_model_done`/`submit_model_error`, `submit_capability_done`/`submit_capability_error`, `submit_permission_decision`, `abort`, and read results with `final_text()`, `trace_json()`, `log_json()`. It also ships the browser tool contract — `browser_capabilities()` / `browser_tool_schemas()` — the model⇄browser vocabulary (`tabs_list`, `page_snapshot`, `page_click`, `file_download_url`, …). Crucially, it contains **no Chrome APIs and bakes in no prompt**: the schemas name the tools, but every implementation is injected by JS.
- **`bindings/typescript`** provides the generic plain-JS host modules that any browser extension can vendor: `agent_driver.js` (`runAgent(question, host, hooks)` — the submit/poll loop that turns brain commands into host calls), `openai_adapter.js` (`callOpenAiCompatible(request, settings, hooks)` — a streaming `/chat/completions` fetch client with 429/5xx retries), and `indexed_db.js` (settings, session/trace records, and a local file store in IndexedDB). The driver never touches `chrome.*` either; it only calls the `host` object you hand it.
- **Your extension folder** supplies everything Chrome-specific: the MV3 manifest, the service worker, the side-panel UI, the system prompt, and — the interesting part — the **capability dispatcher** that maps tool names from the brain's `StartCapability` commands onto real `chrome.*` calls.

## The host interface — the one shape to keep

The whole wiring hangs on a single object. `examples/chrome-extension/host.js` is 30 lines and this is its entire contract:

```js
// host.js
import { invokeBrowserCapability } from "./chrome_api.js";
import { loadSettings, saveSession } from "./vendor/indexed_db.js";
import { SYSTEM_PROMPT } from "./system_prompt.js";

export const host = {
  async loadWasm() { /* import ./pkg/hugr_wasm.js, await module.default(), return module.HugrWasm */ },
  invokeCapability: invokeBrowserCapability,  // (name, args) => Promise<result>
  loadSettings,                               // () => Promise<{apiKey, baseUrl, model, ...limits}>
  saveSession,                                // (record) => Promise  — autosaved during the run
  systemPrompt: SYSTEM_PROMPT,
  defaults: { baseUrl: "https://router.huggingface.co/v1", model: "google/gemma-4-31B-it:cerebras",
              maxModelCalls: 20, maxCostMicroUsd: 50000 },
};
```

Keep this shape (`loadWasm`, `invokeCapability`, `loadSettings`, `saveSession`, `systemPrompt`) and you can swap every implementation behind it. The UI then runs a whole ask in one call:

```js
import { runAgent } from "./vendor/agent_driver.js";
import { host } from "./host.js";

const result = await runAgent("Close all the shopping tabs", host, {
  onEvent: (event) => renderEvent(event),   // start / model / tool / done timeline items
});
```

## How the shipped example wires Chrome

Read these five files once — they are the whole Chrome layer:

- **`manifest.json`** (MV3): `"background": { "service_worker": "service_worker.js", "type": "module" }`, a `side_panel.default_path` pointing at `sidepanel.html`, `content_security_policy.extension_pages` including `'wasm-unsafe-eval'` (required to instantiate the WASM brain), permissions `activeTab, sidePanel, scripting, storage, tabs, webNavigation`, plus broad host permissions and a `content_scripts` entry injecting `content_script.js` into pages.
- **`service_worker.js`**: opens the side panel on icon click and answers `chrome.runtime.onMessage` requests (`hugr.tabs.list`, `hugr.tab.open`, `hugr.tab.close`, `hugr.tab.switch`) — the privileged tab operations that must run in the background context.
- **`chrome_api.js`** — the capability dispatcher. `invokeBrowserCapability(name, args)` is one big `switch` on the tool name: tab tools message the service worker via `chromeCall`, page tools (`page_snapshot`, `page_click`, `page_type`, waits, …) message the content script in the target tab via `contentCall(tabId, message)`, and file tools read/write the IndexedDB-local file store. Unknown names throw `capability not implemented yet: <name>` — which routes back to the model as a tool error, not a crash.
- **`sidepanel.html` / `sidepanel.js`**: the UI. It calls `runAgent(question, host, { onEvent })`, renders the event timeline, supports an Interrupt button, and lists saved sessions and files straight from `indexed_db.js`.
- **`system_prompt.js`**: host config, passed into the WASM brain at construction — the crate bakes nothing in.

## The build

`build.sh` does three things: compile the crate, generate the JS glue, and vendor the generic modules (extensions can only load modules from inside their own folder):

```bash
cargo build -p hugr-wasm --target wasm32-unknown-unknown --release
wasm-bindgen --target web --out-dir "$HERE/pkg" target/wasm32-unknown-unknown/release/hugr_wasm.wasm
cp bindings/typescript/{agent_driver.js,openai_adapter.js,indexed_db.js} "$HERE/vendor/"
```

Run it from the repo root, then load the folder via `chrome://extensions` → Developer mode → Load unpacked, open the side panel, and enter an API key in Settings. If `wasm-bindgen` complains about a schema mismatch, `cargo install -f wasm-bindgen-cli --version 0.2.126`.

## Now build a different one

Copy `examples/chrome-extension/` to a new folder and swap pieces. A minimal "tab janitor" extension — no content script, no file store, just tab tools and a popup instead of a side panel — looks like this:

1. **Trim the manifest.** Drop `content_scripts`, `scripting`, and `webNavigation`; replace `side_panel` with `"action": { "default_popup": "popup.html" }`. Keep `'wasm-unsafe-eval'` in the CSP — the brain won't start without it.
2. **Write your dispatcher.** Replace `chrome_api.js` with only what you grant; registration is the sandbox, so a capability you don't implement is a capability the model cannot use:

```js
export async function invokeCapability(name, args) {
  switch (name) {
    case "tabs_list": return (await chrome.tabs.query({})).map((t) => ({ id: t.id, title: t.title, url: t.url }));
    case "tab_close": await chrome.tabs.remove(args.tab_id); return { closed: true };
    case "tab_switch": await chrome.tabs.update(args.tab_id, { active: true }); return { active: true };
    default: throw new Error(`capability not implemented yet: ${name}`);
  }
}
```

3. **Write your prompt.** Replace `system_prompt.js` with instructions scoped to what you actually implemented — the shipped prompt talks about snapshots and file uploads your extension no longer has.
4. **Assemble your `host`.** Same five keys, your implementations: `invokeCapability` from step 2, `loadSettings`/`saveSession` from the vendored `indexed_db.js` (or your own storage — the driver only awaits promises), the `loadWasm` body copied verbatim.
5. **Build your UI.** Any HTML that calls `runAgent(question, host, { onEvent })` works; `sidepanel.js` is a good crib for rendering `model`/`tool`/`done` events and wiring an AbortController-style interrupt.
6. **Reuse `build.sh` as-is** (adjust `HERE` if you moved out of `examples/`), reload the unpacked extension, and ask it to tidy your tabs.

Two boundaries to respect as you extend: tool *schemas* live in `crates/hugr-wasm/src/capabilities.rs` (the model⇄browser contract), so a genuinely new tool means adding its schema there and its implementation in your dispatcher — never a Chrome API in the crate; and the dispatcher is trusted host code, so a tool that shells out of its declared scope is a hole you drilled, not one Hugr can close.

## Next

Continue with [04 — An agent binary from Python](04-agent-binary-from-python.md), or jump to [06 — An agent entirely in TypeScript](06-agent-entirely-in-typescript.md) to see the same WASM brain driven by the typed `hugr-agents` package instead of the plain-JS extension modules.
