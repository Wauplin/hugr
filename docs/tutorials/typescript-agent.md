# Define an agent in TypeScript

In this tutorial, you will define a huglet entirely in TypeScript, with config as a plain object and tools as functions. It drives the same sans-IO brain as every other surface, compiled to WebAssembly and running in Node or the browser.

Topics include the `Agent` class, the `ToolSpec` shape, the `ask`/`run` pair and event stream, Node and browser entry points, and cross-compatible trace verification with the Rust CLI.

The config keys correspond to `huggr.toml`, with flattened context forget maps; the `Answer` contract uses the same wire fields as the Rust and Python surfaces. The [runtime documentation](../concepts/runtime.md) explains why the brain is sans-IO and every effect is injected. This tutorial covers assembly.

## What the package is

The `huggr-agents` npm package in `bindings/typescript/` is a typed layer over the WASM brain in `crates/huggr-wasm`. It exports three entry points:

- **`huggr-agents`** (root): the platform-neutral pieces: the `Agent` class, contract types (`Answer`, `AgentEvent`, `ToolSpec`, `AgentConfig`, …), the OpenAI-compatible fetch adapter `callOpenAiCompatible`, and in-memory reference stores `MemTraceStore` / `MemFeedbackStore`.
- **`huggr-agents/node`:** the Node runtime: `createAgent(config)`, `loadWasm()` from `./pkg`, `FsTraceStore` / `FsFeedbackStore` under `~/.huggr/<name>/`, and `api_key_env` resolved from `process.env`.
- **`huggr-agents/browser`:** the browser runtime: `createAgent(config)`, `loadWasm(pkgUrl?)` over `fetch`, `IndexedDbTraceStore` / `IndexedDbFeedbackStore`.

The brain never touches IO. The TS `Agent` is the host: it loads the wasm, drives the submit/poll loop, fetches the model, invokes tools, and persists traces, following the documented runtime boundary.

## Prerequisites

From `bindings/typescript/`:

```bash
npm install
npm run build:wasm   # cargo + wasm-bindgen → ./pkg (needs the rust toolchain + wasm32-unknown-unknown)
npm run build        # tsc → ./dist
```

You need a working Rust toolchain with `wasm32-unknown-unknown` for `build:wasm` (or copy a prebuilt `pkg/`). An OpenAI-compatible API key for the provider you point at. Use Node 18 or newer for the Node path and a modern browser for the browser path.

## Config: the same keys as huggr.toml

The `AgentConfig` interface is the typed mirror of the manifest's `[agent]`, `[models]`, `[models.<tier>]`, `[limits]`, and `[context]` sections:

```ts
import type { AgentConfig } from "huggr-agents";

const config: AgentConfig = {
  name: "policy-helper",
  version: "0.1.0",
  system: "Answer from the policy tools. Return JSON.",
  models: { default: "balanced" },
};
```

- `name` names the agent's state home (`~/.huggr/<name>/` by default), just like `[agent]` name.
- `models` chooses a default from the fixed `fast`, `balanced`, `powerful`, and `max` tiers, and may contain concrete author overrides.
- `limits` (`LimitsConfig`) is optional and caps `max_model_calls`, `max_cost_micro_usd`, and `timeout_s`; the same three knobs as `[limits]`. An agent has no limits by default; each unset key is unbounded.
- `context` (`ContextConfig`) is optional and passes through to the core `BudgetPolicy` inside the WASM brain, so compaction (`"none"` | `"truncate"` | `"summarize"`), `budget_tokens`, `trigger_tokens`, `keep_recent_tokens`, `max_block_tokens`, `summary_model`, `tool_ttl`, and `keep_last_per_tool` all run in the brain, not the host. The forget maps `tool_ttl` and `keep_last_per_tool` sit directly on `ContextConfig` here, matching the WASM brain's decoder; this is intentionally flatter than the TOML manifest, which nests them under `[context.forget]`.
- The built-in catalog uses the Hugging Face router and `HF_TOKEN`. Pass `{ modelCatalog: { providers, models } }` as the second argument to `createAgent` for an explicit Node or browser host override. A browser provider may include `api_key` directly; Node normally resolves `api_key_env` from `process.env`. Key values never appear in output.

The `default` tier is what the brain selects when no component requests another tier. `Agent.resolvedModels()` returns the effective four-tier mapping after runtime and `HUGGR_MODEL_<TIER>` overrides. See [Models, providers, and pricing](../concepts/models-and-pricing.md).

## Tools: { name, description, schema, invoke }

A `ToolSpec` contains an explicit name, description, a JSON Schema for model arguments, and the invoke function:

```ts
import type { ToolSpec } from "huggr-agents";

const lookupPolicy: ToolSpec = {
  name: "lookup_policy",
  description: "Search policy text for a query.",
  schema: {
    type: "object",
    properties: { query: { type: "string" } },
    required: ["query"],
  },
  invoke: async (args) => {
    const results = await searchPolicyText(args.query as string);
    return { matches: results };
  },
};
```

The `invoke` signature is `invoke(args: Json): Promise<Json> | Json`. Its return value is JSON-serialized and fed back to the brain as a capability result.

If it throws, the exception message becomes a semantic tool error routed back to the model as `{ error: <message> }`. This matches the Rust runtime instead of crashing.

An unknown tool name (one you did not register) yields `unknown tool: <name>` and follows the same path.

Registration *is* the sandbox: tools you list in `config.tools` are the only ones the model can invoke. There is no privileged built-in. `requiresPermission?: boolean` is an opt-in flag on permissioned tools, but the TS `Agent` auto-allows every tool at registration (the embedding code was the grant), so it currently behaves as YOLO mode, following the same discipline as the Chrome extension host.

## Create the agent and ask

In Node, `createAgent` wires the default runtime for you:

```ts
import { createAgent } from "huggr-agents/node";

const agent = createAgent({
  ...config,
  tools: [lookupPolicy],
});

const answer = await agent.ask("Can I expense a train ticket?");
console.log(answer.status);              // "success"
console.log(answer.response);             // { answer: "yes, up to €200" }
console.log(answer.trace_id);             // "<16 hex chars>"
console.log(answer.metadata.cost_micro_usd);   // number
console.log(answer.metadata.model_calls);      // number
```

`agent.ask(question, options?): Promise<Answer>` drains the run and returns the final `Answer`.

The `Answer` has the same shape on every surface. It contains `status` (`"success"` or `"error"`), `response` (a `Record<string, Json>` object), `trace_id`, optional `blobs`, and `metadata: AnswerMeta`. Metadata contains `duration_ms`, `cost_micro_usd`, `tokens_in`, `tokens_out`, `model_calls`, and `tool_calls`.

Run errors are answers, not exceptions: a blown limit, missing final model text, or timeout ends `ask`/`run` with an error answer (`status: "error"`, `response.error` set) rather than throwing. Failures outside a run still throw as ordinary exceptions: an invalid config, storage or WASM-loading errors, a runaway session, a `feedback` call for an unknown trace, and `verify` on a drifting trace.

## Stream with `agent.run`

When you want the event timeline, use the async generator instead:

```ts
for await (const event of agent.run("Can I expense a train ticket?")) {
  switch (event.type) {
    case "ask_started":   console.log("ask started, parent:", event.trace_parent); break;
    case "model_started": console.log("model turn", event.op, event.tier); break;
    case "text_delta":    process.stdout.write(event.text); break;
    case "model_ended":   console.log("tokens:", event.usage); break;
    case "tool_started":  console.log("tool", event.name, event.args); break;
    case "tool_ended":    console.log("tool done", event.name, event.is_error, event.result); break;
    case "notice":        console.log("notice:", event.message); break;
    case "done":          console.log("done:", event.reason); break;
    case "answer_ready":  console.log("answer:", event.answer); break;
  }
}
```

The full `AgentEvent` union is:

```ts
type AgentEvent =
  | { type: "ask_started"; trace_parent: string | null }
  | { type: "model_started"; op: number; tier: string }
  | { type: "text_delta"; op: number; text: string }
  | { type: "model_ended"; op: number; usage: Usage }
  | { type: "tool_started"; op: number; name: string; args: Json }
  | { type: "tool_ended"; op: number; name: string; is_error: boolean; result: Json }
  | { type: "notice"; message: string }
  | { type: "done"; reason: Json }
  | { type: "answer_ready"; answer: Answer };
```

This is the same wire vocabulary as the Rust `--stream` surface and the Python `agent.run(...)` events, so a UI rendering these is portable across all three. `ask` is `run` with a collector: it yields every event, captures `answer_ready`, and returns it. TypeScript currently buffers a model call's text deltas until that call finishes; tool and turn events retain their order.

## AskOptions: resume, abort, extra

The optional second argument to both `ask` and `run` is `AskOptions`:

```ts
interface AskOptions {
  traceId?: string;
  extra?: Json;
  signal?: AbortSignal;
}
```

- `traceId` resumes/forks: the parent trace is loaded, re-folded into a fresh session via `session.resume_trace(...)`, and the *new* ask writes a new trace with `depends_on` set. Resuming never mutates the old trace; resuming the same id twice forks into two branches.
- `signal` cancels the run; an aborted signal drains the brain via `session.abort(...)` and produces an error answer (`status: "error"`, `response.error: "aborted by caller"`) rather than throwing.
- `extra` is arbitrary JSON stamped into the trace's meta; for tagging, correlation, anything you want to filter on later.

```ts
const first = await agent.ask("first question");

const followUp = await agent.ask("follow-up", { traceId: first.trace_id });
await agent.verify(followUp.trace_id);
```

## Node vs browser entry points

The split is entirely in the `AgentRuntime` injected at construction; the `Agent` class itself is platform-neutral. `AgentRuntime` is:

```ts
interface AgentRuntime {
  loadWasm(): Promise<WasmModule>;
  traces: TraceStore;
  feedback?: FeedbackStore;
  env?: (name: string) => string | undefined;
}
```

The Node entry (`huggr-agents/node`) provides defaults via `nodeRuntime(name)`:

```ts
import { createAgent, FsTraceStore, FsFeedbackStore, nodeRuntime } from "huggr-agents/node";

const agent = createAgent(config, { traces: new FsTraceStore("/custom/traces") });
```

This wires `loadWasm()` (which reads `./pkg` bytes without fetch), `FsTraceStore` under `<home>/traces/`, `FsFeedbackStore` under `<home>/feedback/`, and `env: process.env[name]`.

The home resolves in the same order as the Rust runtime: `$HUGGR_AGENT_HOME`, then `$HUGGR_HOME/<name>`, then `~/.huggr/<name>`.

Traces land as `<home>/traces/<id>.json` in the portable `huggr-replay` format. The Rust runtime writes the same layout, so `huggr verify` and `huggr traces` can read Node-written TypeScript traces when the supplied agent crate resolves to the same store.

`FsTraceStore.put` stamps a content-derived id (sha256 of the headed trace JSON, first 16 hex chars) and writes atomically (`flag: "wx"` claims the name, a collision bumps a `-N` suffix; the body goes to a `.tmp` and renames into place); so a put never overwrites, preserving trace immutability.

The browser entry (`huggr-agents/browser`) provides `browserRuntime(name, pkgUrl?)`:

```ts
import { createAgent, IndexedDbTraceStore } from "huggr-agents/browser";

const agent = createAgent(config, { traces: new IndexedDbTraceStore("my-agent") });
```

This wires `loadWasm(pkgUrl?)` (imports `huggr_wasm.js` and initializes the wasm bytes over `fetch`), `IndexedDbTraceStore` (one IndexedDB database per agent, keyed by trace id), `IndexedDbFeedbackStore`, and no `env`; browsers have no environment, so point at `models.api_key` directly.

The root export's in-memory stores (`MemTraceStore`, `MemFeedbackStore`) are the reference implementation of the storage seam and double as the "how to write a backend" example; if you want to store traces somewhere neither fs nor IndexedDB covers (a remote service, your app's database), implement `TraceStore`:

```ts
interface TraceStore {
  put(trace: Json, header: TraceHeader): Promise<string>;
  get(id: string): Promise<Json>;
  list(): Promise<TraceHead[]>;
}

interface FeedbackStore {
  append(feedback: Feedback): Promise<void>;
  list(traceId: string): Promise<Feedback[]>;
}
```

`put` stamps the `TraceHeader` into the trace's meta and returns the id. Traces are immutable, so a put never overwrites.

## Feedback

Append-only feedback is a sidecar keyed to a trace, never inside it:

```ts
await agent.feedback(answer.trace_id, { score: 5, note: "good" });
const feedback = await agent.feedbackFor(answer.trace_id);
// → [{ trace_id, payload: { score: 5, note: "good" }, created_at_ms }]
```

`agent.feedback(traceId, payload): Promise<Feedback>` throws if the trace doesn't exist. The storage layout matches the Rust side (`<home>/feedback/<trace_id>.jsonl` on Node, one JSON line per event), so `huggr stats` aggregates it across surfaces. Listing traces is `agent.traces(): Promise<TraceHead[]>`.

## The trace and verify story

Every ask is recorded as an immutable trace in the portable `huggr-replay` format (meta, events, log, commands, policy). The wasm brain's `trace_json()` returns it; the TS runtime's `TraceStore.put` stamps the header and persists it. Because Node writes the same `.json` layout the Rust runtime does, the cross-language story works in both directions:

```bash
huggr verify <agent-dir> <trace-id>   # reads the TS-recorded trace directly
huggr traces <agent-dir>             # lists TS-recorded traces
```

And from TS, you can re-verify a trace through the exact same wasm fold `huggr verify` uses:

```ts
await agent.verify(traceId);   // replays bit-for-bit; throws on drift
```

`agent.verify(traceId): Promise<void>` loads the stored trace and calls `verify_trace_json`, the wasm export of `huggr-replay::verify` and the same fold as the CLI. A compatible trace placed in the runtime's store verifies without rerunning its original tools; this is the release gate on new control-flow paths and your check after a change.

## In the browser: the chrome-extension example

`examples/chrome-extension/` is one concrete browser host. It currently vendors the *plain-JS* extension host modules (`agent_driver.js`, `openai_adapter.js`, `indexed_db.js`) rather than the typed `huggr-agents` package; the typed package is the same driver factored into typed TS, and the example is migrating onto it. The wiring is still instructive for the platform pieces:

- `host.js` assembles a host object (`loadWasm`, `invokeCapability`, settings, system prompt); the same five-key shape the typed `AgentRuntime` formalizes.
- `chrome_api.js` is the capability dispatcher: one `switch` on tool name mapping `tabs_list`, `page_snapshot`, `page_click`, `file_download_url`, … onto `chrome.*` calls. Unknown names throw, routing back to the model as a tool error.
- The manifest needs `content_security_policy.extension_pages` with `'wasm-unsafe-eval'` (required to instantiate the WASM brain), and the build vendors the generic modules because extensions can only load modules from inside their own folder.

To run a typed browser agent, use `createAgent` and `IndexedDbTraceStore` from `huggr-agents/browser`.

Chrome-specific capabilities still need a dispatcher, equivalent to `invokeCapability`, and registration in `config.tools`. The typed `Agent` is platform-neutral; only its runtime knows about Chrome.

See [Build a Chrome extension](chrome-extension.md) for the full extension build.

## Putting it together

A complete, runnable Node script:

```ts
import { createAgent } from "huggr-agents/node";

const agent = createAgent({
  name: "policy-helper",
  system: "Answer from the lookup_policy tool. Return { answer: string } as JSON.",
  models: { default: "balanced" },
  tools: [{
    name: "lookup_policy",
    description: "Search policy text for a query.",
    schema: { type: "object", properties: { query: { type: "string" } }, required: ["query"] },
    invoke: async (args) => ({ matches: [`policy line about ${(args as any).query}`] }),
  }],
});

const answer = await agent.ask("Can I expense a train ticket?");
if (answer.status === "success") {
  console.log(answer.response);
} else {
  console.log("error:", answer.response.error);
}
console.log(`cost: ${answer.metadata.cost_micro_usd / 1_000_000} USD, ${answer.metadata.model_calls} model calls, trace ${answer.trace_id}`);

await agent.verify(answer.trace_id);

const followUp = await agent.ask("Up to what amount?", { traceId: answer.trace_id });
console.log("forked trace:", followUp.trace_id, "depends on", answer.trace_id);
```

Run it with `node --experimental-vm-modules` (or a `.mjs` extension) after `npm run build` in `bindings/typescript/`. The same config, tools, and `ask`/`run` code works unchanged in the browser if you swap the import to `huggr-agents/browser` and point storage at `IndexedDbTraceStore`.

## Next

You can define and run agents in TypeScript. To compose agents as tools, pass blobs, and aggregate feedback and cost with `huggr stats`, continue with [Compose agents and account for cost](../guides/compose-agents.md).
