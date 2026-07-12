# Your first Chrome extension

This guide builds a browser-agent Chrome extension with custom tools and a custom UI. It uses the same three reusable pieces as the shipped example: the generic WASM brain bindings in `crates/hugr-wasm`, the generic JavaScript host modules in `bindings/typescript`, and a thin Chrome-specific layer.

You will see what each layer provides, how `examples/chrome-extension` connects them, and which files to copy, keep, and replace.

The [runtime documentation](../runtime.md) explains why the brain is sans-IO and every effect is injected. This guide covers assembly.

## The three layers

A browser host is a stack, and only the top layer knows about Chrome:

- **`crates/hugr-wasm`** compiles `hugr-core` to `wasm32-unknown-unknown` and exposes it through wasm-bindgen.

  The extension-facing class is `HugrWasm`. Construct it with a `BrowserAgentConfig` JSON object containing `base_url`, `model`, `api_key`, `system_prompt`, and `context`, plus optional `max_model_calls` / `max_cost_micro_usd` caps (unset means unbounded).

  Drive it with `submit_user_input`, `poll_commands_json`, `submit_model_done`/`submit_model_error`, `submit_capability_done`/`submit_capability_error`, `submit_permission_decision`, and `abort`. Read results with `final_text()`, `trace_json()`, and `log_json()`.

  The crate also provides the browser tool contract through `browser_capabilities()` / `browser_tool_schemas()`. These functions define the model⇄browser vocabulary (`tabs_list`, `page_snapshot`, `page_click`, `file_download_url`, …).

  The crate contains **no Chrome APIs and no built-in prompt**. The schemas name the tools, but JavaScript injects every implementation.
- **`bindings/typescript`** provides generic plain-JavaScript host modules that any browser extension can vendor.

  `agent_driver.js` provides `runAgent(question, host, hooks)`, the submit/poll loop that turns brain commands into host calls. `openai_adapter.js` provides `callOpenAiCompatible(request, settings, hooks)`, a streaming `/chat/completions` fetch client with 429/5xx retries. `indexed_db.js` stores settings, session and trace records, and extension-local files in IndexedDB.

  The driver never touches `chrome.*`; it only calls the supplied `host` object.
- **Your extension folder** supplies everything Chrome-specific: the MV3 manifest, the service worker, the side-panel UI, the system prompt, and the **capability dispatcher** that maps tool names from the brain's `StartCapability` commands onto real `chrome.*` calls.

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

These five files define the Chrome layer:

- **`manifest.json`** (MV3) defines the service worker, side panel, content security policy, permissions, and content scripts.

  Set `"background": { "service_worker": "service_worker.js", "type": "module" }` and point `side_panel.default_path` at `sidepanel.html`. Include `'wasm-unsafe-eval'` in `content_security_policy.extension_pages` so the WASM brain can instantiate.

  The example requests `activeTab, sidePanel, scripting, storage, tabs, webNavigation`, plus broad host permissions. Its `content_scripts` entry injects `content_script.js` into pages.
- **`service_worker.js`:** opens the side panel on icon click and handles `chrome.runtime.onMessage` requests (`hugr.tabs.list`, `hugr.tab.open`, `hugr.tab.close`, `hugr.tab.switch`) for privileged tab operations that must run in the background context.
- **`chrome_api.js`:** the capability dispatcher. `invokeBrowserCapability(name, args)` is one large `switch` on the tool name.

  Tab tools message the service worker through `chromeCall`. Page tools (`page_snapshot`, `page_click`, `page_type`, waits, …) message the content script in the target tab through `contentCall(tabId, message)`. File tools read or write the IndexedDB-local file store.

  Unknown names throw `capability not implemented yet: <name>`, which routes back to the model as a tool error rather than crashing.
- **`sidepanel.html` / `sidepanel.js`**: the UI. It calls `runAgent(question, host, { onEvent })`, renders the event timeline, supports an Interrupt button, and lists saved sessions and files straight from `indexed_db.js`.
- **`system_prompt.js`:** host config passed into the WASM brain at construction. The crate has no built-in prompt.

## The build

`build.sh` does three things: compile the crate, generate the JS glue, and vendor the generic modules (extensions can only load modules from inside their own folder):

```bash
cargo build -p hugr-wasm --target wasm32-unknown-unknown --release
wasm-bindgen --target web --out-dir "$HERE/pkg" target/wasm32-unknown-unknown/release/hugr_wasm.wasm
cp bindings/typescript/{agent_driver.js,openai_adapter.js,indexed_db.js} "$HERE/vendor/"
```

Run it from the repo root, then load the folder via `chrome://extensions` → Developer mode → Load unpacked, open the side panel, and enter an API key in Settings. If `wasm-bindgen` complains about a schema mismatch, `cargo install -f wasm-bindgen-cli --version 0.2.126`.

## Now build a different one

Copy `examples/chrome-extension/` to a new folder and swap pieces. A minimal "tab janitor" extension has no content script or file store, uses only tab tools, and replaces the side panel with a popup:

1. **Trim the manifest.** Drop `content_scripts`, `scripting`, and `webNavigation`; replace `side_panel` with `"action": { "default_popup": "popup.html" }`. Keep `'wasm-unsafe-eval'` in the CSP; the brain won't start without it.
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

3. **Write your prompt.** Replace `system_prompt.js` with instructions scoped to what you actually implemented; the shipped prompt talks about snapshots and file uploads your extension no longer has.
4. **Assemble your `host`.** Same five keys, your implementations: `invokeCapability` from step 2, `loadSettings`/`saveSession` from the vendored `indexed_db.js` (or your own storage; the driver only awaits promises), the `loadWasm` body copied verbatim.
5. **Build your UI.** Any HTML that calls `runAgent(question, host, { onEvent })` works; `sidepanel.js` is a good crib for rendering `model`/`tool`/`done` events and wiring an AbortController-style interrupt.
6. **Reuse `build.sh` as-is** (adjust `HERE` if you moved out of `examples/`), reload the unpacked extension, and ask it to tidy your tabs.

Keep two boundaries when extending the example. Tool schemas live in `crates/hugr-wasm/src/capabilities.rs` (the model⇄browser contract), so a new tool needs both a schema there and an implementation in the dispatcher; Chrome APIs never belong in the crate. The dispatcher is trusted host code, so an implementation that exceeds its declared scope bypasses a boundary that Hugr cannot enforce.

## Next

Continue with [04: An agent binary from Python](04-agent-binary-from-python.md), or jump to [06: An agent entirely in TypeScript](06-agent-entirely-in-typescript.md) to see the same WASM brain driven by the typed `hugr-agents` package instead of the plain-JavaScript extension modules.
