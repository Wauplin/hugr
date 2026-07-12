# huggr-agents — the TypeScript runtime API

Define a huglet entirely in TypeScript — tools as functions, config as data — driving the WASM brain (`crates/huggr-wasm`) in Node or the browser. Same config keys as `huggr.toml` and the Python API, same `Answer` contract, same event vocabulary, same trace format.

```ts
import { createAgent } from "huggr-agents/node";

const agent = createAgent({
  name: "policy-helper",
  system: "Answer from the policy tools. Return JSON.",
  models: {
    default: "medium",
    base_url: "https://router.huggingface.co/v1",
    api_key_env: "HUGGR_API_KEY",
    medium: { model: "moonshotai/Kimi-K2-Instruct",
              input_usd_per_m_tokens: 1.0, output_usd_per_m_tokens: 1.5 },
  },
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

- `huggr-agents` (root export) — the platform-neutral `Agent`, contract types, the OpenAI-compatible fetch adapter (429/5xx retries), and in-memory reference stores.
- `huggr-agents/node` — fs `TraceStore`/`FeedbackStore` under `~/.huggr/<name>/` (same resolution and layout as the Rust runtime — `huggr verify`/`huggr traces` read TS-recorded traces directly), wasm loader from `./pkg`, `api_key_env` from `process.env`.
- `huggr-agents/browser` — IndexedDB stores and a fetch-based wasm loader.
- `agent.verify(traceId)` replays a stored trace bit-for-bit through the wasm `verify_trace_json` fold — the same gate as `huggr verify`, cross-language in both directions.
- `context` passes through to the core `BudgetPolicy`, so compaction runs inside the WASM brain.

Tool functions are **trusted host code**: Huggr jails what the model can invoke (sandbox-by-registration), not what your TS does once invoked.

The plain-JS extension host modules (`agent_driver.js`, `openai_adapter.js`, `indexed_db.js`) remain here for `examples/chrome-extension`, which vendors them at build time; the example migrates onto this typed package next.

## Development

```bash
cd bindings/typescript
npm install
npm run build:wasm   # cargo + wasm-bindgen → ./pkg
npm test             # tsc → dist, then node --test against a mock provider
```
