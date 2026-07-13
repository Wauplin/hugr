# huggr-agents, the TypeScript runtime API

Define a huglet entirely in TypeScript, with tools as functions and config as data, driving the WASM brain (`crates/huggr-wasm`) in Node or the browser. The config corresponds to `huggr.toml` with flattened context forget maps and an inline browser API key; it uses the same `Answer` fields, event vocabulary, and trace format.

```ts
import { createAgent } from "huggr-agents/node";

const agent = createAgent({
  name: "policy-helper",
  system: "Answer from the policy tools. Return JSON.",
  models: { default: "balanced" },
  tools: [{
    name: "lookup_policy",
    description: "Search policy text.",
    schema: { type: "object", properties: { query: { type: "string" } }, required: ["query"] },
    invoke: async (args) => ({ matches: await searchPolicyText(args.query) }),
  }],
});

const answer = await agent.ask("Can I expense a train ticket?");
for await (const event of agent.run("Follow-up?", { traceId: answer.trace_id })) { /* stream */ }
```

- `huggr-agents` (root export): the platform-neutral `Agent`, contract types, the OpenAI-compatible fetch adapter (transport and 429/5xx retries before streaming starts), and in-memory reference stores.
- `huggr-agents/node`: fs `TraceStore`/`FeedbackStore` under `~/.huggr/<name>/` with the Rust runtime's layout, wasm loader from `./pkg`, and `api_key_env` from `process.env`. `huggr verify` and `huggr traces` can read those traces when the supplied agent crate resolves to the same store.
- `huggr-agents/browser`: IndexedDB stores and a fetch-based wasm loader.
- `agent.verify(traceId)` replays a stored trace bit-for-bit through the wasm `verify_trace_json` fold, the same gate as `huggr verify`, across compatible trace stores.
- `context` passes through to the core `BudgetPolicy`, so compaction runs inside the WASM brain.

Model selection uses the fixed `fast`, `balanced`, `powerful`, and `max` tiers. Pass a `modelCatalog` in the optional runtime argument to override the built-in provider, model, and pricing mappings.

Tool functions are **trusted host code**: Huggr jails what the model can invoke (sandbox-by-registration), not what your TS does once invoked.

The plain-JS extension host modules (`agent_driver.js`, `openai_adapter.js`, `indexed_db.js`) remain here for `examples/chrome-extension`, which vendors them at build time; the example migrates onto this typed package next.

## Development

```bash
cd bindings/typescript
npm install
npm run build:wasm   # cargo + wasm-bindgen → ./pkg
npm test             # tsc → dist, then node --test against a mock provider
```
