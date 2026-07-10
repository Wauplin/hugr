---
name: hugr-chrome-extension
description: Build or modify a Chrome Manifest V3 extension that hosts the Hugr WASM brain, maps browser capability schemas to chrome.* implementations, persists settings and traces in IndexedDB, and calls an OpenAI-compatible model. Use for examples/chrome-extension, custom browser-agent UI, capability dispatch, extension packaging, or WASM/Chrome debugging.
---

# Build a Hugr Chrome extension

Start from `examples/chrome-extension` and preserve the three-layer boundary: `crates/hugr-wasm` owns the generic brain and browser tool schemas, `bindings/typescript` owns generic driver/model/storage modules, and the extension folder owns every `chrome.*` call and UI choice. Read [tutorial 03](../../../docs/tutorials/03-first-chrome-extension.md).

## Copy and narrow the example

Copy the example for a new host, then remove permissions, content scripts, and capability cases the product does not need. Registration is the sandbox; the prompt, advertised schemas, dispatcher, and manifest permissions must agree.

Keep the host contract:

```js
export const host = {
  async loadWasm() { /* load ./pkg/hugr_wasm.js */ },
  invokeCapability,
  loadSettings,
  saveSession,
  systemPrompt,
  defaults,
};
```

The generic driver calls only this interface. Keep Chrome APIs in the extension layer.

## Implement capabilities

Map each advertised tool name to one narrow Chrome operation:

```js
export async function invokeCapability(name, args) {
  switch (name) {
    case "tabs_list":
      return (await chrome.tabs.query({})).map(({ id, title, url }) => ({ id, title, url }));
    case "tab_close":
      await chrome.tabs.remove(args.tab_id);
      return { closed: true };
    default:
      throw new Error(`capability not implemented yet: ${name}`);
  }
}
```

Route privileged tab operations through the MV3 service worker and page DOM operations through the content script. Unknown or invalid calls must become semantic tool errors, not silent success.

A genuinely new browser tool needs both its model-facing schema in `crates/hugr-wasm/src/capabilities.rs` and an extension dispatcher implementation. Never add Chrome APIs, IndexedDB, fetch, clocks, or permissions to `hugr-core`.

## Keep the MV3 requirements

- Use a module service worker and include `'wasm-unsafe-eval'` in `content_security_policy.extension_pages` so the WASM brain can instantiate.
- Request only Chrome permissions and host permissions used by registered capabilities.
- Vendor `pkg/` and generic modules into the extension folder; MV3 extensions cannot import remote code.
- Store browser traces, settings, and extension-local files in IndexedDB. Do not imply that extension-local downloads are the user's real Downloads folder.
- Treat API keys as user settings. Never commit or bundle one.

## Build and load

```bash
./examples/chrome-extension/build.sh
```

Then open `chrome://extensions`, enable Developer mode, choose Load unpacked, and select `examples/chrome-extension`. Configure the API key, base URL, and model in the side panel.

The build compiles `hugr-wasm`, runs `wasm-bindgen`, and vendors the generic modules. If the schema versions mismatch, install the exact `wasm-bindgen-cli` version printed by the error or pinned in the example README.

## Validate changes

```bash
cargo check -p hugr-wasm
cd bindings/typescript
npm test
```

Exercise every added dispatcher case in Chrome, confirm interrupt produces an error answer rather than a hung session, and confirm saved sessions reload from IndexedDB. Review `manifest.json` after every capability change; broad host permissions are a security decision, not a convenience default.

For a typed Node/browser agent without Chrome-specific APIs, use `$hugr-typescript` instead. For trace drift or saved-session diagnosis, use `$hugr-debug-traces`.
