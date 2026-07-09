# hugr-wasm

`hugr-wasm` is a Chrome Manifest V3 extension that runs the Hugr brain in WebAssembly and performs browser-specific IO through extension JavaScript. `hugr-core` stays sans-IO: tabs, page reads, clicks, waits, downloads, and uploads are ordinary Hugr capabilities executed by the Chrome host layer.

## What works now

- The side panel runs a Hugr turn loop through `hugr-core` compiled to WASM.
- The extension always runs in YOLO mode: browser capabilities are auto-approved and no permission prompt is shown for tool calls.
- The browser model adapter calls an OpenAI-compatible streaming `/chat/completions` endpoint with the API key configured in the side panel.
- The browser model adapter auto-compacts provider requests after approximately 64k tokens by retaining the recent valid tool-call tail and replacing older messages with a deterministic compact transcript summary.
- The browser model adapter also drops stale heavyweight page observations before compaction: older `page_snapshot`, `page_read_text`, and `page_read_html` results are removed from provider context after a later browser action, navigation, or fresher page observation makes them obsolete. The durable trace still keeps the original tool results.
- Browser capabilities include tab listing/open/close/switch/reload/history, page HTML/text/snapshot, click/type/select/scroll/submit/focus, waits, local downloads, local file listing/metadata/delete, and upload from the extension-local file store into a page file input.
- Settings, sessions/traces, and downloaded files are stored in browser-local IndexedDB.
- Sessions are autosaved during the run: a partial session record is created at start and refreshed after model calls, tool calls, checkpoints, terminal states, and periodic streamed text. If the extension crashes, the `Sessions` tab should still contain the latest partial trace.
- The Ask view has an `Interrupt` button for stopping the current run. It aborts the active model request, stops waiting for the current tool result in the driver, saves the partial session as `interrupted`, and re-enables the composer so you can send a corrective message.

## Build

From the repository root:

```bash
./crates/hugr-wasm/extension/build.sh
```

This compiles `hugr-wasm` for `wasm32-unknown-unknown` and writes the generated WASM glue into `crates/hugr-wasm/extension/pkg/`.

If the build fails with a `wasm-bindgen` schema mismatch, install the matching CLI version:

```bash
cargo install -f wasm-bindgen-cli --version 0.2.126
./crates/hugr-wasm/extension/build.sh
```

## Install locally in Chrome

1. Open `chrome://extensions`.
2. Enable `Developer mode`.
3. Click `Load unpacked`.
4. Select `/home/wauplin/projects/hugr/crates/hugr-wasm/extension`.
5. Click the `hugr-wasm` extension icon to open the side panel.
6. Open `Settings` in the side panel and enter an API key, base URL, and model id.
7. Use the `Ask` tab to ask the agent to operate the browser.

The default base URL is `https://router.huggingface.co/v1` and the default model is `google/gemma-4-31B-it:cerebras`. Any OpenAI-compatible streaming chat-completions endpoint should work if it supports tool calls.

## Sharing with a friend on macOS

Build first so the zip includes `extension/pkg/hugr_wasm.js` and `extension/pkg/hugr_wasm_bg.wasm`.

```bash
./crates/hugr-wasm/extension/build.sh
cd crates/hugr-wasm
zip -r hugr-wasm-extension.zip extension
```

Send `crates/hugr-wasm/hugr-wasm-extension.zip` to your friend.

On macOS, they should:

1. Unzip `hugr-wasm-extension.zip`.
2. Open Chrome and go to `chrome://extensions`.
3. Enable `Developer mode`.
4. Click `Load unpacked`.
5. Select the unzipped `extension` folder, not the zip file itself.
6. Click the extension icon to open the side panel.
7. Enter their own API key in `Settings`.

If macOS creates an extra top-level folder when unzipping, they should choose the inner folder that directly contains `manifest.json`.

## Notes

- The extension stores downloaded files in IndexedDB, not in the user's real Downloads folder.
- File upload only supports files previously downloaded into the extension-local file store.
- This is a developer-mode extension, not a signed Chrome Web Store package.
- The current manifest uses broad `http://*/*` and `https://*/*` host permissions so the browser-use workflow works during development. Tightening this to optional host permissions is the next packaging step.
