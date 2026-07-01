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
   - **Hugging Face router** (default): a `hf_…` token, base URL `https://router.huggingface.co/v1`, and `small`/`medium`/`big` tier model ids such as `google/gemma-4-31B-it:cerebras`.
   - **OpenAI**: an `sk-…` key, base URL `https://api.openai.com/v1`, and tool-calling tier model ids.
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
| Permission policy         | `host/engine.js` (`requestPermission`)   | `hugr-host` `AutoApprove`/`AllowAll` |

The brain is `submit(eventJson)` / `poll() -> commandsJson`, synchronous and pure, exactly as in the terminal. Everything asynchronous (streaming fetches, tab tools, and the small-tier permission judge) lives in the JS host and is merged into the one ordered event stream the brain consumes an event at a time.

Assistant output and confirmation prompts render Markdown directly in the side panel, including headings, lists, quotes, code blocks, links, tables, emphasis, and task checkboxes. The renderer is dependency-free and builds DOM nodes rather than injecting model text as HTML.

The header includes a new-chat button that clears the panel and starts a fresh WASM brain, a context drawer button that shows the live `ContextPlan`, a compact button that fires one lossless `CompactContext` pass, and an export button that downloads a `.jsonl` trace envelope containing metadata, the routing policy, the exact submitted event stream (including injected `Tick`s), and the folded durable log. Export does not include your API key, and browser-side resume/import from that file is not wired yet.

Each assistant response shows the logical tier it used (`small`, `medium`, or `big`). The composer tier menu defaults to `auto`; choosing a tier injects a recorded one-turn `ModelOverride` event and clears after the next normal model call consumes it.

MCP server declarations can be saved in Settings, but Chrome MV3 pages cannot spawn local stdio subprocesses, so the browser host does not load stdio MCP directly. The supported fallback today is the native CLI `--mcp <cmd>` / `HUGR_CONFIG` path; the stored browser declarations are reserved for a future native bridge or browser-compatible MCP transport.

Skill descriptors can be saved in Settings as JSON (`id`, `title`, optional `summary`, and `instructions`). The side panel's Skills drawer lists configured skills and marks the active skill after the model invokes its `skill__<id>` descriptor.

## The tools

Read-only (no permission): `list_tabs`, `get_current_page`, `get_page_text`, `get_page_links`, `get_page_outline`, `get_interactive_elements`, `wait_for_page`.

The read tools auto-wait briefly for the page to finish loading; `wait_for_page` (optionally with a CSS `selector` and a `settle_ms` quiet period) is the explicit, more reliable wait for heavy/JS-rendered pages.

Navigation (permission-gated): `open_tab`, `navigate_tab`, `activate_tab`, `close_tab`. By default these go through the configured `small` tier judge and a denial reason is routed back to the model; the **yolo navigation** checkbox skips the judge and allows them.

Agent-UX: `ask_user_confirmation` (yes/no card), `show_plan` (numbered plan card).

## Notes & limits

- Chrome refuses script injection into privileged pages (`chrome://`, the Web Store, the PDF viewer, the new-tab page); the read tools return a clear semantic error there.
- MV3 requires `'wasm-unsafe-eval'` in the extension CSP to instantiate WebAssembly — see `manifest.json`.
- No clicking/typing/form submission by design (this build is read + navigate only).
- Stdio MCP servers are not available from the browser host without a native bridge; configure and use them from the CLI for now.
- Sub-agents (`Command::StartAgent`) are not wired in the browser host; they surface as a semantic error rather than hanging.
