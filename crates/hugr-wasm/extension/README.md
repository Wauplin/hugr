# Hugr — Chrome extension

The **same** Hugr agent brain that powers the native CLI, compiled to WebAssembly and running entirely inside a Chrome side panel — **no backend**. This is the Phase 4 portability showcase (`docs/ROADMAP.md`): `hugr-core` never did IO, so the only thing that changes between the terminal and the browser is the *host* (here: a `fetch`-based model adapter, a DOM front-end, and tab/page capabilities).

It can **read pages** (text, links, headings, interactive elements) and **navigate tabs** (list, open, navigate, activate, close). It deliberately **cannot** click elements, type into fields, or submit forms.

## Install (unpacked)

1. Build the WASM module (only needed if `wasm/` is missing or you changed the Rust — the repo ships a prebuilt copy):
   ```bash
   rustup target add wasm32-unknown-unknown
   cargo install wasm-bindgen-cli --version 0.2.100
   ./crates/hugr-wasm/build-extension.sh
   ```
2. Open `chrome://extensions`, enable **Developer mode**, click **Load unpacked**, and select `crates/hugr-wasm/extension/`.
3. Click the Hugr toolbar icon to open the side panel. Open **Settings (⚙)** and paste an API key:
   - **Hugging Face router** (default): a `hf_…` token, base URL `https://router.huggingface.co/v1`, model e.g. `google/gemma-4-31B-it:cerebras`.
   - **OpenAI**: an `sk-…` key, base URL `https://api.openai.com/v1`, model e.g. `gpt-4o-mini`.
4. Chat. See [`DEMOS.md`](./DEMOS.md) for things to try.

Requires Chrome 116+ (side panel API). Your key lives in `chrome.storage.local` and is only sent to the endpoint you configure.

## How it maps to the architecture

| Piece                     | File                                     | Native equivalent                    |
| ------------------------- | ---------------------------------------- | ------------------------------------ |
| The brain (WASM)          | `wasm/hugr_wasm.js` + `crates/hugr-wasm` | `hugr-core`                          |
| Driver loop               | `host/engine.js`                         | `hugr-host/src/engine.rs`            |
| Model adapter (fetch/SSE) | `host/model.js`                          | `hugr-providers/src/openai.rs`       |
| Capabilities (tabs/pages) | `host/tools.js` + `host/schemas.js`      | `hugr-host/src/capabilities/`        |
| Front-end (DOM)           | `sidepanel.js`                           | `StdoutFrontend`                     |
| Permission policy         | `host/engine.js` (`requestPermission`)   | `hugr-host` `Interactive`/`AllowAll` |

The brain is `submit(eventJson)` / `poll() -> commandsJson`, synchronous and pure, exactly as in the terminal. Everything asynchronous (streaming fetches, tab tools, permission prompts) lives in the JS host and is merged into the one ordered event stream the brain consumes an event at a time.

Assistant output and confirmation prompts render Markdown directly in the side panel, including headings, lists, quotes, code blocks, links, tables, emphasis, and task checkboxes. The renderer is dependency-free and builds DOM nodes rather than injecting model text as HTML.

The header includes a new-chat button that clears the panel and starts a fresh WASM brain, plus an export button that downloads a `.jsonl` trace envelope containing metadata, the static policy, the exact submitted event stream (including injected `Tick`s), and the folded durable log. Export does not include your API key, and browser-side resume/import from that file is not wired yet.

## The tools

Read-only (no permission): `list_tabs`, `get_current_page`, `get_page_text`, `get_page_links`, `get_page_outline`, `get_interactive_elements`, `wait_for_page`.

The read tools auto-wait briefly for the page to finish loading; `wait_for_page` (optionally with a CSS `selector` and a `settle_ms` quiet period) is the explicit, more reliable wait for heavy/JS-rendered pages.

Navigation (permission-gated — a prompt appears unless *auto-approve* is on): `open_tab`, `navigate_tab`, `activate_tab`, `close_tab`.

Agent-UX: `ask_user_confirmation` (yes/no card), `show_plan` (numbered plan card).

## Notes & limits

- Chrome refuses script injection into privileged pages (`chrome://`, the Web Store, the PDF viewer, the new-tab page); the read tools return a clear semantic error there.
- MV3 requires `'wasm-unsafe-eval'` in the extension CSP to instantiate WebAssembly — see `manifest.json`.
- No clicking/typing/form submission by design (this build is read + navigate only).
- Sub-agents (`Command::StartAgent`) are not wired in the browser host; they surface as a semantic error rather than hanging.
