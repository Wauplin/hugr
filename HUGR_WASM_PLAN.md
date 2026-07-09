# hugr-wasm implementation plan

## Goal

`hugr-wasm` is a Chrome-extension host for Hugr: it runs the Hugr brain in WebAssembly, exposes browser navigation as ordinary Hugr capabilities, stores traces and downloaded files in browser-local storage, and keeps all Chrome-specific behavior outside `hugr-core` and `hugr-agent`.

## Architecture rule

`hugr-core` remains unchanged and sans-IO. Browser operations are `StartCapability { name, args }` calls with opaque JSON arguments; the Chrome extension host interprets those calls. Any shared changes needed below `hugr-wasm` must be runtime-agnostic traits or feature splits, not Chrome APIs leaking into shared crates.

## Target layout

```text
crates/hugr-wasm/
  Cargo.toml
  hugr.toml
  SYSTEM.md
  src/
    lib.rs
    capabilities.rs
    config.rs
    exports.rs
  extension/
    manifest.json
    service_worker.js
    sidepanel.html
    sidepanel.js
    content_script.js
    chrome_api.js
    indexed_db.js
```

## Phase 1: Repository scaffold

- Add `crates/hugr-wasm` to the workspace as a browser-specific crate.
- Add a `hugr.toml` and `SYSTEM.md` so the browser agent has the same auditable source shape as other Hugr agents.
- Add Rust definitions for the browser tool schemas; these are the contract between the model and the Chrome bridge.
- Gate `wasm-bindgen` exports behind `wasm32` so ordinary workspace checks stay useful on the host machine.

## Phase 2: Extension shell

- Add a Manifest V3 extension with a side panel, service worker, and content script.
- Keep the service worker thin: open the side panel, relay browser events, and own long-lived Chrome API listeners.
- Run interactive agent UI in the side panel because it is a better fit for long user-visible sessions than the MV3 service worker lifecycle.
- Store settings in `chrome.storage.local`; store traces and file blobs in IndexedDB.

## Phase 3: Browser capabilities

- Tabs/navigation: `tabs_list`, `tab_open_url`, `tab_close`, `tab_switch`, `tab_reload`, `tab_back`, `tab_forward`, `wait_for_navigation`, `wait_for_tab_opened`, `wait_for_url`, `wait_for_selector`, `wait_for_text`.
- Page reading: `page_read_html`, `page_read_text`, `page_snapshot`.
- Interaction: `page_click`, `page_type`, `page_select`, `page_scroll`, `page_submit`, `page_focus`.
- Files: `file_download_url`, `file_list_downloads`, `file_read_metadata`, `file_upload_to_input`, `file_delete`.
- Prefer `page_snapshot` node ids over model-authored CSS selectors for click/type/upload operations.

## Phase 4: Local file store

- Store downloaded files in IndexedDB, not the user's real downloads folder.
- Each file record has `file_id`, filename, media type, source URL, SHA-256 if available, byte length, and creation time.
- `file_upload_to_input` can only upload files from this Hugr local file store in v1.
- Optional later feature: import/export files through explicit user UI.

## Phase 5: Model adapter

- Implement an OpenAI-compatible streaming adapter using browser `fetch()`.
- Keep provider settings in extension settings: API key, base URL, model id, sampling limits, and budget limits.
- Convert streaming chunks into Hugr model deltas and return one consolidated model output.
- Transport retry logic belongs in the browser adapter, matching the native provider rule.

## Phase 6: Trace and replay

- Store traces immutably in IndexedDB with the same event/log/command structure as `hugr-replay`.
- If `hugr-replay` filesystem assumptions block reuse, split pure trace data from filesystem storage behind a runtime-agnostic store trait.
- Preserve resume/fork semantics: a resumed ask writes a new trace with `depends_on`, never mutates the parent.

## Phase 7: UI

- Minimal side panel with four views: Ask, Settings, Sessions, Files.
- Ask view shows the current question box, streamed answer/status, and the active trace id.
- Settings view edits API key, base URL, model id, and limits.
- Sessions view lists traces and supports resume/fork.
- Files view lists downloaded local files and supports deletion.

## Phase 8: Security defaults

- Start from `activeTab`, `tabs`, `scripting`, `storage`, `webNavigation`, and `sidePanel`.
- Prefer optional host permissions over unconditional `<all_urls>` for production packaging.
- Treat the model as attacker-controlled: every browser capability validates arguments and returns semantic errors instead of panicking.
- Avoid Chrome `debugger` API in v1 because it grants too much authority for the sandbox-by-registration philosophy.

## Phase 9: Checks

- Keep `cargo check -p hugr-wasm` clean on the native host.
- Add `cargo check -p hugr-core --target wasm32-unknown-unknown` once the target is available in the local toolchain.
- Later add browser E2E coverage for a local page: read text, click, type, wait, download a fixture into IndexedDB, upload it back into a file input, and inspect the persisted trace.

