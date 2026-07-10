# hugr TypeScript bindings

The generic JS/TS host layer for the Hugr WASM brain (`crates/hugr-wasm`). Nothing in this folder touches `chrome.*` — a concrete host injects its own capability dispatcher, storage, and prompt.

- `agent_driver.js` — `runAgent(question, host, hooks)`: drives the WASM brain's submit/poll loop. `host` provides `loadWasm()`, `invokeCapability(name, args)`, `loadSettings()`, `saveSession(record)`, `systemPrompt`, and optional `defaults`.
- `openai_adapter.js` — OpenAI-compatible streaming `/chat/completions` client over `fetch`, including the current request-side compaction/prune POC (to be replaced by built-in policy compaction, see `plan.md` 2.1).
- `indexed_db.js` — IndexedDB-backed settings/sessions/files stores for browser hosts.

`examples/chrome-extension/` is the reference host: it implements the capability dispatcher over Chrome APIs and vendors these files at build time (extensions can only load modules from inside their own folder).

This package becomes the full typed TypeScript runtime API (define agents and tools in TS, in Node and the browser) in plan step 3.2.
