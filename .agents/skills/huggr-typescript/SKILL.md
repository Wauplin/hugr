---
name: huggr-typescript
description: Define and run huglets in TypeScript or JavaScript through the huggr-agents WASM runtime, with Node or browser storage, callable tools, streaming events, feedback, and cross-language trace verification. Use when building a Node agent, a browser-hosted agent, or a custom TypeScript host around huggr-core.
---

# Build Huggr agents with TypeScript

Use the root package for platform-neutral types and in-memory stores. Use `huggr-agents/node` for filesystem-backed Node agents and `huggr-agents/browser` for IndexedDB-backed browser agents. Read [Define an agent in TypeScript](../../../docs/tutorials/typescript-agent.md) for the complete surface.

## Build the package

```bash
cd bindings/typescript
npm install
npm run build:wasm
npm run build
```

`build:wasm` requires Rust, the `wasm32-unknown-unknown` target, and `wasm-bindgen`. Node usage requires Node 18 or newer.

## Define a Node agent

```ts
import { createAgent } from "huggr-agents/node";

const agent = createAgent({
  name: "policy-helper",
  system: "Use lookup_policy and return a JSON object.",
  models: { default: "balanced" },
  tools: [{
    name: "lookup_policy",
    description: "Search policy text by keyword.",
    schema: {
      type: "object",
      properties: { query: { type: "string" } },
      required: ["query"],
    },
    invoke: async (args) => ({ matches: await searchPolicyText((args as { query: string }).query) }),
  }],
});

const answer = await agent.ask("Can I expense a train ticket?");
await agent.verify(answer.trace_id);
```

Tool functions are trusted host code. Registration limits what the model can invoke but does not sandbox the implementation body. An exception from `invoke` becomes a semantic tool error returned to the model.

## Stream and resume

```ts
for await (const event of agent.run("What receipt is needed?", { traceId: answer.trace_id })) {
  if (event.type === "text_delta") process.stdout.write(event.text);
  if (event.type === "answer_ready") console.log(event.answer);
}
```

Model text deltas are yielded as the provider stream delivers them; other events retain their order.

Pass `extra` for trace metadata and an `AbortSignal` as `signal` for cancellation. A resumed ask restores the parent trace's recorded policy, writes a new trace with `depends_on`, and never mutates the parent.

## Choose Node or browser storage

Node resolves `HUGGR_AGENT_HOME`, then `HUGGR_HOME/<name>`, then `~/.huggr/<name>`, and writes portable trace/feedback files. Browser agents use `createAgent` from `huggr-agents/browser`, load WASM over fetch, and persist through IndexedDB.

The runtime resolves the fixed `fast`, `balanced`, `powerful`, and `max` tiers from its built-in catalog. Pass `modelCatalog` in the optional runtime object to override providers, model ids, and prices. Browsers have no environment variables, so put `api_key` on the runtime provider from a user-controlled settings store and never bake a production secret into a published bundle. Supply custom `TraceStore` and `FeedbackStore` implementations through the runtime when the built-in fs, IndexedDB, or memory stores do not fit.

Provider-reported usage cost is authoritative for answer metadata and `max_cost_micro_usd`; configured token prices are the fallback when the provider reports no cost.

## Context policy

Pass `context` using manifest-shaped keys:

```ts
context: {
  compaction: "truncate",
  budget_tokens: 64000,
  trigger_tokens: 56000,
  keep_recent_tokens: 8000,
  max_block_tokens: 2000,
  keep_last_per_tool: { page_snapshot: 1 },
}
```

Compaction runs inside the WASM brain. Do not add unrecorded summarizer or request-pruning calls in the host adapter.

## Validate and troubleshoot

```bash
cd bindings/typescript
npm test
```

- Missing `pkg/`: run `npm run build:wasm` before `npm test` or browser packaging.
- Missing target: run `rustup target add wasm32-unknown-unknown`.
- `wasm-bindgen` schema mismatch: install the version required by the repository build script.
- Provider auth in Node: set the variable named by the resolved provider's `api_key_env` (`HF_TOKEN` for the built-in catalog); in browsers, inject `api_key` through a runtime `modelCatalog` from a user-controlled settings store.
- Trace drift: call `agent.verify(id)`. Use `$huggr-debug-traces` or the Rust CLI when a matching manifest-defined agent resolves to the same Node trace store.
