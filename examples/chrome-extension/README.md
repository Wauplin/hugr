# chrome-extension example

A Chrome Manifest V3 extension that runs the Huggr brain in WebAssembly and performs browser IO through extension JavaScript. It is one concrete host built from the reusable pieces:

- `crates/huggr-wasm` — the generic WASM bindings around `huggr-core` (no Chrome APIs, nothing baked in).
- `bindings/typescript` — the generic agent driver, OpenAI-compatible fetch adapter, and IndexedDB storage.
- this folder — everything Chrome-specific: the capability dispatcher over `chrome.*` (`chrome_api.js`), the content script, the side-panel UI, the system prompt, and the MV3 manifest.

To build a *different* extension (other layout, tools, policies), copy this folder, keep the `host.js` shape (`loadWasm`, `invokeCapability`, `loadSettings`, `saveSession`, `systemPrompt`), and swap any piece.

## What works now

- The side panel runs a Huggr turn loop through `huggr-core` compiled to WASM.
- The extension always runs in YOLO mode: browser capabilities are auto-approved and no permission prompt is shown for tool calls.
- The browser model adapter calls an OpenAI-compatible streaming `/chat/completions` endpoint with the API key configured in the side panel.
- The WASM brain uses the built-in context policy by default: model-backed summaries trigger near the 64k-token range, and stale heavyweight page observations are dropped via `keep_last_per_tool` rules before the provider request is rendered.
- Browser capabilities include tab listing/open/close/switch/reload/history, page HTML/text/snapshot, click/type/select/scroll/submit/focus, waits, local downloads, local file listing/metadata/delete, and upload from the extension-local file store into a page file input.
- Settings, sessions/traces, and downloaded files are stored in browser-local IndexedDB.
- Sessions are autosaved during the run; the Ask view has an `Interrupt` button.

## Build

From the repository root:

```bash
./examples/chrome-extension/build.sh
```

This compiles `huggr-wasm` for `wasm32-unknown-unknown`, writes the wasm-bindgen glue into `examples/chrome-extension/pkg/`, and vendors the generic JS modules into `examples/chrome-extension/vendor/` (extensions can only load modules from inside their own folder; both folders are gitignored).

If the build fails with a `wasm-bindgen` schema mismatch, install the matching CLI version:

```bash
cargo install -f wasm-bindgen-cli --version 0.2.126
```

## Install locally in Chrome

1. Open `chrome://extensions`.
2. Enable `Developer mode`.
3. Click `Load unpacked`.
4. Select the `examples/chrome-extension` folder.
5. Click the extension icon to open the side panel.
6. Open `Settings` in the side panel and enter an API key, base URL, and model id.
7. Use the `Ask` tab to ask the agent to operate the browser.

The default base URL is `https://router.huggingface.co/v1` and the default model is `google/gemma-4-31B-it:cerebras`. Any OpenAI-compatible streaming chat-completions endpoint should work if it supports tool calls.

## Sharing

Build first, then zip the folder (it must include `pkg/` and `vendor/`):

```bash
./examples/chrome-extension/build.sh
cd examples
zip -r huggr-chrome-extension.zip chrome-extension
```

The recipient unzips, loads the folder via `Load unpacked`, and enters their own API key in `Settings`.

## Notes

- The extension stores downloaded files in IndexedDB, not in the user's real Downloads folder.
- File upload only supports files previously downloaded into the extension-local file store.
- This is a developer-mode extension, not a signed Chrome Web Store package.
- The current manifest uses broad `http://*/*` and `https://*/*` host permissions so the browser-use workflow works during development. Tightening this to optional host permissions is the next packaging step.
