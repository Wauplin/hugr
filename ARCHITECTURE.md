# Hugr — Design & Architecture

> **Build your subagent, ship it anywhere.** Hugr is a toolkit for building tiny, self-contained, domain-specific subagents on a runtime-free, sans-IO Rust core. This is the one document to read top to bottom: the vision and user-facing model first, the internals second, the security model last.

## Part I — What Hugr is

### 1. Vision

Hugr builds **domain-specific subagents**: small, specialized agents that do one thing well — answer questions about a docs folder, read PDFs, fetch live data from an allowlisted API — and that an orchestrator (a human, a script, or a bigger agent) calls through **one uniform contract**: a question in, a structured response out, with cost, duration, and a resumable trace id attached.

The pitch in one sentence: **a subagent is a small Rust crate plus a system prompt and a set of tools with privileges; Hugr turns that crate folder into a self-contained binary** — with traces, forking, sandboxing, and cost accounting built in.

Why domain-specific subagents:

- **Token efficiency.** A subagent with 5 tools and a 200-line system prompt is dramatically cheaper and more reliable than a generalist with 50 tools. The orchestrator pays one tool-call's worth of context to invoke it, not the whole domain's.
- **Security by construction.** A subagent that never registers `shell` *cannot* run shell commands. Privileges are declared in the agent manifest and enforced by what the host registers — not by a runtime policy trying to say "no" fast enough.
- **Composability.** Every subagent exposes the same ask/answer contract, so orchestrators compose them without per-agent glue. And because that contract is tool-shaped, **a Hugr agent *is* a tool**: one agent grants another in its manifest (`[tools.agent.<name>]`) and calls it like any capability (§8).
- **No vendor lock-in.** Hugr subagents are artifacts *you* run — locally, in CI, in a container — because the runtime is a small library, not a service.

### 2. Goals & non-goals

Goals:

- **Trivial to define.** A new subagent is a human-readable, auditable Rust crate folder: a manifest, a system prompt, tool selections from a predefined library, and optional typed response/hooks/capability code in the same crate.
- **Self-contained to ship.** `hugr build` produces one standalone CLI binary per agent; the same binary is an MCP server via `--mcp-serve`. There is exactly one artifact kind.
- **One invocation contract.** Every subagent accepts a question + optional metadata and returns a structured response + mandatory metadata (status, cost, duration, tokens, trace id). Orchestrators never learn per-agent APIs.
- **Resumable and forkable by default.** Every run persists an immutable trace with a `trace_id` and an optional `depends_on` parent. Passing a `trace_id` back resumes that context; passing an older one forks a sibling branch.
- **Sandboxed by default.** A subagent gets a private scratchpad and exactly the tools it declares. Blob exchange with the caller is explicit.
- **Deterministic and replayable.** Any session can be replayed bit-for-bit for testing, debugging, and resume — a property of the sans-IO core (Part III).
- **One way to do each thing.** One artifact kind, one run path per stage (dev: `hugr run`; ship: the built binary), one external-tool escape hatch (MCP), one trace format. Breaking changes are acceptable; there is no backward-compatibility ceremony.

Non-goals:

- **A general-purpose coding or browser agent.** Hugr defines the *callee* side; generalists are orchestrators that call Hugr agents.
- **A hosted runtime or marketplace.** Hugr ships artifacts; where they run is your business.
- **A universal agent-to-agent wire protocol.** MCP is the adapter today; others (A2A) can be added at the edge if demanded, never as foundations.
- **Multimodal-first.** Text-in/text-out with blob attachments is the contract; images/audio ride as blobs a specific agent's tools may interpret.

### 3. What a subagent is

A subagent is **(1) a system prompt and (2) a list of tools with associated privileges**. That pair is what makes it domain-specific. Everything else is shared infrastructure every subagent gets for free:

1. **A scratchpad** — a private filesystem subtree the agent can freely read/write without permission round-trips and without escaping its root.
2. **Traces** — every run is stored as a replayable trace with a `trace_id`; follow-up questions resume it; older ids fork it (§5).
3. **The brain** — the same `hugr-core` reducer: turn loop, context projection, deterministic replay (Part III).
4. **A common API** — invocation (`ask`) plus introspection (`--describe`: name, tools, tiers, pricing, limits; `--config`: the parsed manifest as JSON with secrets redacted; `--traces`: stored lineage).
5. **Blob exchange** — a caller can hand the agent files and receive files back; large payloads ride the content-addressed blob store.
6. **Accounting** — every response carries cost (from per-tier pricing config) and duration, folded from the trace's per-op metadata.
7. **Composition** — any built Hugr agent can be granted to another as an ordinary tool (§8); the child's cost folds into the caller's metadata.

The manifest and prompt are data; the agent crate owns any typed contract or custom Rust wiring; the infrastructure is the toolkit.

### 4. The user journey: define → run → build

```
my-agent/
  Cargo.toml          # the agent crate; owns typed contracts and optional Rust wiring
  hugr.toml          # manifest: name, model tiers + pricing, tool grants, limits
  SYSTEM.md          # the system prompt (plain markdown)
  src/lib.rs          # optional typed response / hooks / compile-in capability registration
```

- `hugr new <name> [--template weather|blank]` scaffolds a working agent crate folder. The default `weather` template is the self-contained beginner example: it grants only the allowlisted `web_fetch` tool (scoped to the Open-Meteo API hosts) and needs no local data folder, so `hugr new` → set the provider key → `hugr run` answers immediately. `blank` is the tool-free starting point.
- `hugr run <agent-dir> "question" [--trace <id>]` is the development loop. Agents with a Rust response contract compile and reuse a cached dev shim that links the current agent crate, then run the same generated surface as the built binary; legacy manifest-schema agents can still run directly.
- `hugr build <agent-dir> [--surface cli|python]` embeds the manifest, prompt, and bundled agent files into a **single standalone binary** wrapping the shared runtime (the default `cli` surface), and can additionally emit a typed language binding (`--surface python`, §4.1). Building requires a Rust toolchain; running the CLI artifact requires nothing.
- `hugr traces <agent-dir>` lists the trace lineage tree; `hugr replay` / `hugr verify` point the replay machinery at a stored trace.

Every built agent binary has the same shape:

```
<agent> [runtime args...] "question" [--trace <id>] [--json|--pretty] [--blob <path>...]
<agent> --describe | --config | --traces
<agent> --mcp-serve          # stdio MCP server exposing one `ask` tool
```

Runtime args are declared in the manifest and generated by the toolkit; they patch manifest targets before the agent is assembled. One JSON `Answer` on stdout, logs on stderr, always exit 0 — errors are answers (`status: "error"`), so callers branch on data, not exceptions. Any language can always consume the CLI binary via subprocess or MCP; a native, typed binding is an optional convenience generated on demand (§4.1).

### 4.1 Language surfaces

`hugr build --surface python` generates, alongside the CLI binary, a native **PyO3 + maturin** package so an orchestrator can `import <agent>; <agent>.ask(...)` in-process instead of shelling out. Surfaces are additive and open-ended (`python` today; `kotlin`/`ts`/… later) — each is a *generated wrapper crate*, exactly like the CLI shim, so the agent crate stays clean (just its response contract) and the toolkit owns surface generation. There is still one runtime and one Ask/Answer contract; a surface is only a typed doorway to it.

The design rule that keeps this scalable: **validation stays on the Rust side; every surface is a thin typed-deserialization layer, never a second validator.** Hugr already casts model output into the agent's response type before it reaches `Answer.response` (§18.2), so a surface only needs to *deserialize* the already-valid JSON into native typed values. Re-validating per surface would mean re-implementing the same schema in Python, then Kotlin, then TS — exactly the duplication the narrow-waist rule (§14) rejects.

The Python surface makes this concrete:

- A compiled extension (`<agent>._native`, PyO3) embeds the same bundle, links the response-contract crate, and drives one ask in-process, returning the `Answer` as opaque JSON — the narrow waist crosses the FFI boundary unchanged.
- A pure-Python package wraps it with a typed `ask(...) -> Answer` whose declared runtime args become typed parameters (positional ones lead, before the question), and generates stdlib `@dataclass` models — the agent's response type plus the stable `Answer`/`AnswerMeta`/`BlobHandle` contract types — from the **schemars JSON Schema read out of the built artifact's `--config`** (one source of truth, so the Python types cannot drift from the Rust ones). `ok`/`status` branch the typed success response (`Answer.response`) from the error message (`Answer.error`), preserving errors-as-answers.
- The build is offline and self-contained: no runtime dependency beyond the wheel, and a static type checker (mypy/pyright) enforces both the input arguments and the response fields — the Python surface is as strict as the Rust one.

### 5. The contract: Ask / Answer, traces, resume, fork

```rust
// hugr-agent. Plain serde structs with public fields — no builder ceremony.
pub struct Ask {
    pub question: String,            // the one required field
    pub trace_id: Option<TraceId>,   // resume/fork anchor
    pub blobs: Vec<BlobHandle>,      // inbound files
    pub extra: Value,                // opaque caller metadata, echoed into the trace
}

pub struct Answer {
    pub status: String,              // "success" | "error" (open string set; nothing branches on it internally)
    pub response: Value,             // structured response object; error answers use response.error
    pub trace_id: TraceId,           // the NEW trace this run persisted
    pub blobs: Vec<BlobHandle>,      // outbound files, content-addressed
    pub metadata: AnswerMeta,        // MANDATORY accounting
    pub extra: Value,                // non-answer extras, opaque to the contract
}

pub struct AnswerMeta {
    pub duration_ms: u64,
    pub cost_micro_usd: u64,         // folded from per-op usage × per-tier pricing
    pub tokens_in: u64, pub tokens_out: u64,
    pub model_calls: u32, pub tool_calls: u32,
}
```

Design rules: `AnswerMeta` is never optional — an orchestrator can always account for a call. `response` is always a JSON object; without a declared response contract, plain model text is wrapped as `{ "text": ... }`. A typed response contract is a Rust `serde` + `schemars` type: Hugr derives JSON Schema from it, passes that schema to the model provider as `response_format`, and casts the final JSON into the Rust type before returning it. If that cast fails, the agent asks the model to repair the response for up to the contract's attempt limit. `extra` is only for non-answer extras and is never load-bearing for the contract. `BlobHandle { ref: Bytes | Path | Sha256, media_type }` — inbound blobs are materialized into the scratchpad before the turn starts; outbound blobs are returned by content-addressed ref into the blob store (dedup by hash).

The orchestration model:

- **New question, no `trace_id`** → fresh session; the answer carries the new `trace_id`.
- **Follow-up, with `trace_id`** → the agent loads that trace, re-folds it into a fresh brain (instant, deterministic, zero model calls), appends the new question as a live turn, and persists the result as a **new** trace with `depends_on = parent`. The parent is never mutated.
- **Fork = ask an old id twice.** Because every follow-up writes a new immutable trace, sibling branches are the default behavior: `root → t1 → {t2a, t2b}`. Lineage is a DAG recorded in trace headers; immutability makes parallel asks race-free by construction.

Scratch state follows the lineage: a resumed ask sees its ancestor's notes; a fork gets a copy-on-fork view, so sibling branches never observe each other's writes.

### 6. The manifest

```toml
[agent]
name = "policy-docs"
version = "0.1.0"
description = "Answers questions about the company travel policy."

[models]
base_url = "https://router.huggingface.co/v1"
api_key_env = "POLICY_DOCS_API_KEY"
[models.default]                      # tier names are free-form strings; one tier is the common case
model = "google/gemma-4-31B-it:cerebras"
input_usd_per_m_tokens = 1.0
output_usd_per_m_tokens = 1.5

[tools.fs_read]                       # a grant from the predefined library
root = "./policies"                   # scope; jailed by the capability

[runtime.args.docs_path]              # optional invocation-time config
target = "tools.fs_read.root"         # patch this manifest field before assembly
positional = true
required = true
env = "POLICY_DOCS_PATH"
help = "Folder containing policies to search."

[tools.mcp.github]                    # external tools: an MCP server (the one escape hatch)
command = "gh-mcp"

[tools.agent.receipts]                # another built Hugr agent as a tool (§8)
artifact = "./receipts-agent"

[limits]
max_model_calls = 20
max_cost_micro_usd = 50000
timeout_s = 120
```

`SYSTEM.md` beside it is the system prompt, with a small template-var set (`{{agent_name}}`, `{{tools}}`, `{{date}}`). Reviewing a subagent's blast radius = reading `hugr.toml`: a tool that is not granted is not registered, and an unregistered capability **cannot** be invoked — sandbox-by-registration, not sandbox-by-policy (Part IV). `[runtime.args.<name>]` is the only way to make invocation-time config part of the surface: the toolkit adds it to the built CLI and MCP `ask` schema, then patches the declared target before registering tools or model adapters. Runtime path values are resolved from the caller's current directory, so one docs binary can be used on a different folder per invocation without recompilation. The Rust response contract belongs to the current agent crate: `src/lib.rs` must expose `pub const RESPONSE_RUST_TYPE: &str = "crate_name::TypeName";` and define that `serde` + `schemars` type. `hugr run`/`hugr build` infer the crate from `Cargo.toml` beside `hugr.toml`, fail explicitly if `RESPONSE_RUST_TYPE` is missing, derive a provider schema name from the Rust type, derive JSON Schema from the type, pass it to the model provider, and cast final JSON with `serde`. Response repair uses the runtime's fixed default attempt limit for now, not manifest configuration. `[response].schema` remains the legacy manifest-owned JSON Schema path. `[limits]` are enforced host-side on every ask: an exceeded limit yields an ordinary `status: "error"` answer with a persisted, still-verifying partial trace.

### 7. The tool library

Vetted, parameterized capabilities selectable by manifest grant, each jailed to its declared scope and covered by a threat-model note (Part IV):

- **`fs_read`** — root-jailed read-only family: `fs_list` / `fs_search` / `fs_read` / `fs_read_range` / `fs_read_many` / `fs_outline`.
- **`scratchpad`** — ungated `scratch_read` / `scratch_write` / `scratch_list`, jailed to the ask's scratch subtree (provided by the runtime, always on).
- **`web_fetch`** — host-allowlisted GET-only fetch, fail-closed on an empty allowlist, no automatic redirects.

The library is **exec-free**: no shell tool exists, and nothing in the library spawns a process except granted child agents. A sandboxed `code_exec` (pinned interpreter, cwd = scratchpad, no network, capped) is a designed future addition; a general `shell` never enters the library.

Custom tools, in order of weight: **another Hugr agent** (`[tools.agent.<name>]`, §8), an **MCP server** (`[tools.mcp.<name>]`, stdio, tools appear namespaced), or a compile-in Rust `Capability` for those embedding the runtime directly. MCP is the *only* external-process escape hatch — there is no bespoke plugin protocol.

### 8. Agents as tools (composition)

Because every agent exposes the same ask contract, granting one agent to another is a manifest line. The grant registers **one ordinary capability** named `agent_<name>`: its args are an `Ask` (question, optional `trace_id` for follow-ups, blob handles); its result is the full `Answer`. To the calling model it looks like any other tool.

- **The child is a built artifact.** The grant points at a built agent binary; the parent spawns it as a subprocess speaking the standard CLI JSON contract. One composition mechanism, aligned with "the artifact is the product".
- **Privileges compose downward only.** The child runs under its *own* manifest — its own jail, tiers, limits. Granting an agent never leaks the parent's capabilities into it.
- **Cost folds up.** The child's `Answer.metadata` merges into the parent's `AnswerMeta`, so the orchestrator's cost line stays complete.
- **Determinism is preserved.** The child's `Answer` (with its `trace_id`) is recorded as the tool's result in the parent trace; replaying the parent never re-runs the child. Recursion depth is capped (`max_agent_depth`).

### 9. Crate layout

```
crates/hugr-core/       # the sans-IO brain (Part III). NO tokio, NO reqwest, NO fs.
crates/hugr-host/       # native tokio host: driver loop, capability/model registries, MCP client.
crates/hugr-providers/  # OpenAI-compatible streaming model adapter.
crates/hugr-replay/     # the trace format + content-addressed blob store + replay/verify/inspect.
crates/hugr-agent/      # the subagent runtime: Ask/Answer, TraceStore (trace_id/depends_on),
                        #   scratchpad, blob exchange, limits, cost accounting, agent-as-tool.
crates/hugr-toolkit/    # agent crate manifests (hugr.toml + SYSTEM.md), the tool library,
                        #   the `hugr` CLI (new / run / build / traces / replay / verify), and
                        #   the language-surface generators (CLI shim, PyO3/maturin — §4.1).
crates/hugr-docs/       # the reference subagent crate (docs Q&A): hugr.toml + SYSTEM.md plus
                        #   typed response contract, run/buildable by hugr-toolkit.
```

Dependency rules: **`hugr-core` depends on nothing environmental** (verify with `cargo tree -p hugr-core`). `hugr-replay` may use `std::fs` but consumes `hugr-core` as pure data. The layers stack strictly: `hugr-agent` on `hugr-host` + `hugr-replay`; `hugr-toolkit` on `hugr-agent`. Nothing reaches into `hugr-core` internals — they are all hosts.

### 10. Standards positioning

- **MCP** is how a Hugr agent is exposed *as a tool* to orchestrators (Claude Code and most frameworks speak it): every built binary serves `--mcp-serve` with one `ask` tool whose structured result carries the full `Answer`. Session continuity rides our `trace_id` in the tool arguments, not MCP session state; we never use MCP sampling (deprecated) — the agent owns its provider.
- **A2A** is the surviving agent↔agent standard for *remote* orchestration; an adapter is possible later (our `describe()` output is card-shaped) but is deliberately not a foundation.
- **The gap Hugr fills**, verified unowned: (a) a cross-process **forkable session contract** (`trace_id`/`depends_on` with bit-for-bit deterministic replay); (b) **mandatory cost/duration metadata on every response**; (c) **single-binary agent packaging**. That combination is the product.

## Part II — The runtime seen from the outside

### 11. The shape in one diagram

```
           ┌─────────────────────────────────────────────┐
           │                   HOST                       │
           │   (tokio: model streams, tools, timers)      │
   ask ───▶│  inbox  ◀── LLM stream ◀── tools ◀── timers  │   real concurrency
           │    │                          ▲              │   lives here
           │    │ submit(event)            │ exec command │
           │    ▼                          │              │
           │  ┌────────────────────────────┴───┐          │
           │  │            BRAIN (core)        │          │
           │  │   pure, single-threaded,       │          │
           │  │   sans-IO state machine        │          │
           │  │   poll() -> [Command]          │          │
           │  └────────────────────────────────┘          │
           └─────────────────────────────────────────────┘
```

The brain never does IO. It consumes one ordered event stream and produces commands. The host does everything else. An `Agent::ask` is: assemble an engine from the definition (registries, adapter, prompt), optionally re-fold a parent trace, submit the question, drive the loop until `Done`, fold the trace into an `Answer`, persist.

### 12. Why sans-IO: the core thesis

Most harness pain traces back to conflating four things that should be separate:

| Concern           | The trap (what harnesses do)              | What Hugr does                                              |
| ----------------- | ----------------------------------------- | ----------------------------------------------------------- |
| **Durable state** | The flat `messages[]` list *is* the state | Append-only **event log** is the source of truth            |
| **Model context** | Same `messages[]` is sent to the model    | Context is a **projection** rendered from the log per turn  |
| **IO**            | The loop owns tokio, sockets, fs          | **Sans-IO** core emits commands; the **host** does IO       |
| **Permissions**   | `if dangerous { prompt() }` in the loop   | Sandbox is **what the host registers**, decided from config |

Every headline feature is a direct payoff of these separations:

- **Trace = the log made durable.** `trace_id` is just a name for the saved file.
- **Resume = re-fold a trace.** Zero IO beyond the file read, no model re-calls, instant.
- **Fork = copy a log prefix.** Sibling explorations share a prefix and diverge.
- **Sandbox = what the host registers.** "This agent has no shell" is a fact about registration, not a policy hope.
- **Cost = arithmetic over the trace.** Per-op usage/latency lives on the log; answer metadata is a fold.

## Part III — The core, in depth

### 13. The core ↔ host contract

The entire surface between brain and host is two enums plus two methods: `submit(event)` folds an event into state and queues commands; `poll()` drains them. Both are synchronous and pure — no `async`, no IO. The only `await` in the system is the host's `next_event()`.

```rust
pub enum Command {
    /// Start a model completion. `model` is a logical selector string the host resolves.
    StartModelCall { op: OpId, model: ModelSelector, request: ModelRequest },
    /// Invoke a host capability (tool). There are NO privileged built-ins.
    StartCapability { op: OpId, name: CapabilityName, args: Value },
    /// Request permission for a gated action; the host decides.
    RequestPermission { op: OpId, request: PermissionRequest },
    /// Abort an in-flight operation.
    Cancel { op: OpId },
    /// Emit an observability event (side-effect-free for state).
    Emit(OutputEvent),
    /// Persist current durable state.
    Checkpoint,
    /// The turn/session reached a terminal state.
    Done { reason: DoneReason },
}

pub enum Event {
    UserInput { text: String },                          // queued if ops are in flight
    UserAbort,                                           // pure cancel, no new content
    ModelDelta { op: OpId, delta: ModelDelta },          // streaming transport, never durable
    ModelDone  { op: OpId, output: ModelOutput, usage: Usage },
    ModelError { op: OpId, error: ModelError },
    CapabilityChunk { op: OpId, chunk: Value },
    CapabilityDone  { op: OpId, result: Value },
    CapabilityError { op: OpId, error: CapabilityError },
    PermissionDecision { op: OpId, decision: Decision }, // Allow | Deny { reason }
    OpCancelled { op: OpId },
    Tick { now: Timestamp },                             // injected time — the brain has no clock
}
```

The host driver loop is the entire integration surface:

```rust
loop {
    for cmd in brain.poll() { host.dispatch(cmd) }       // spawn model/tool tasks, abort, persist…
    let event = host.next_event().await;                  // merged, ordered, stamped
    brain.submit(event);                                  // pure, instant
}
```

### 14. The narrow-waist rule

The single biggest design risk is the interface itself. Over-engineer it and every extension is a breaking change; under-engineer it and the brain can't reason about anything. The resolution, applied field by field:

> **Type only what the brain branches on. Everything else is an opaque payload.**

- The brain **branches on**: op lifecycle (start/delta/done/error/cancel), model output structure (text vs tool calls), turn control, permission outcomes → typed and stable. There are few of them and they rarely change.
- The brain **only stores/forwards**: capability args/results, provider knobs, prompts, answers → opaque (`Value`). The brain is a router and bookkeeper for them, never an interpreter.

Consequences: `StartCapability { name, args: Value }` keeps args opaque, so new tools never change the core. Adding a tool, a provider knob, or an agent grant touches **zero** core types. A corollary of the same taste: **an enum nobody branches on should be a string** — status labels, privilege classes, and selector names are open string sets, not variant lists.

### 15. What the brain actually does (and what it doesn't)

The reducer (`brain.rs`) does exactly:

1. **Bookkeeping** — maintain the append-only log and the in-flight op table.
2. **The turn loop** — drive `user → model → (tool calls?) → tools → model → … → done`.
3. **Ask the pluggable `TurnPolicy`** — which model selector to use, how to project context, whether a capability is gated. Strategy lives in the policy, never hardcoded in the reducer.
4. **Route opaque payloads** — turn a model's tool calls into `StartCapability` ops; feed results back as context.
5. **Decide lifecycle** — when a turn is `Done`; when to `Checkpoint`.

It does **not**: any IO or model calls; running tools; rendering; resolving what a selector maps to; storage; scheduling. The brain answers one question, repeatedly: *given the log and the event that just arrived, what should happen next?*

### 16. State model: event log + projection

- **Durable state is an append-only log** of `LogEntry { seq, at, record }` — user messages, consolidated model outputs, tool results, op endings. `BrainState` (including the op table) is a fold over the log and can always be rebuilt (`Brain::from_log`). Resume = replay the fold. Fork = copy a prefix.
- **Model context is a projection, not the log.** Per turn, the policy produces a `ContextPlan` from the log (which blocks are included, with token estimates), and the reducer renders the `ModelRequest` from it. Projection keeps tool-call transcripts provider-valid (tool results render immediately after their originating assistant tool-call block). Subagent sessions are short and bounded by `[limits]`, so there is no in-session summarization/compaction machinery — the projection includes the log.
- **Large payloads are content-addressed blobs.** Tool outputs and file exchange are stored by SHA-256 in `hugr-replay::BlobStore`; the log holds the reference. Identical content dedupes to one file.
- **Token counts come from the host, at ingestion.** The brain cannot tokenize (provider-specific, not sans-IO-friendly); the host annotates records with estimates and the brain's projection just sums them. Authoritative accounting comes from the returned `Usage` per call.

### 17. In-flight operations & concurrency

- **The op table.** `StartModelCall`/`StartCapability` insert into `inflight`; each `*Delta`/`*Chunk` appends to the op's buffer cheaply; `*Done`/`*Error`/`OpCancelled` remove the op and append a final `Record::OpEnded` carrying **`OpMeta`** `{ started_at, ended_at, model, usage, extra }`. Latency and spend are queryable from the trace itself — no side table.
- **Atomicity is automatic.** The brain processes one event at a time; concurrency is the host merging many sources into one ordered stream. No locks inside the brain.
- **Foreground vs background** is a policy answer (`is_background(capability)`): a foreground op blocks the turn; a background op lets the model resume immediately, with its result folded in at the next turn boundary. Invisible to the host.
- **Cancellation is first-class:** `Command::Cancel` → host aborts → `Event::OpCancelled` → the op is removed and its partial output logged explicitly (`OpOutcome::Cancelled { partial }`). Never an implicit gap.
- **Deltas are transport, never durable.** A thousand-token response arrives as many `ModelDelta`s that update the live buffer and are discarded; exactly **one** consolidated `Record::ModelOutput` is appended per model call (same for tool chunks vs one `Record::ToolResult`). This is what keeps traces the size of a normal message history, and what makes replay clean: replay feeds consolidated events only.
- **Backpressure:** handlers stay O(1)-ish (append to a buffer); heavy work never happens in the reducer.

### 18. Model provider abstraction

- **Canonical request/response.** `ModelRequest { blocks, tools, params, extra }` with structured `ContextBlock`s; `ModelOutput { text, tool_calls, stop }`. Provider-specific knobs the brain never reads ride the opaque `extra`.
- **A model call is a typed command, not a capability**, because the brain *reasons about model output* (tool calls drive the turn loop) but never about tool output (opaque leaves). At the host level a model adapter is still registered like any capability.
- **`ModelSelector` is a plain string newtype.** The manifest maps free-form tier names to concrete adapters (`[models.<tier>]` → endpoint, model id, pricing); the policy picks a selector; the host registry resolves it. Each model op records its selector in `OpMeta`, so per-tier spend falls out of the trace.
- **Streaming is the only mode.** Adapters stream deltas live via the sink and return the consolidated output; there is no non-streaming path. Transport errors (429s, network blips, timeouts) are retried inside the adapter and never reach the brain; only the final outcome is recorded, so a replayed session doesn't re-suffer transient failures.
- **Transport vs semantic errors.** If retrying the same request unchanged might work, it's transport (host retries internally). If the model must *change something* to succeed — malformed tool args, a tool's logical failure — it's semantic and routes back into the turn loop as a tool result so the model can correct itself.

### 19. Determinism, replay, and the trace format

All nondeterminism is injected: time via `Event::Tick`, model output and tool results as events. The brain never reads a clock or RNG. A pure fold over a recorded stream therefore reproduces every command bit-for-bit.

```rust
pub struct Trace {
    meta: TraceMeta,        // trace_id, depends_on, agent name/version, created_at, question, status
    events: Vec<Event>,     // the ordered host→brain stream — the replay INPUT
    log: Vec<LogEntry>,     // the consolidated record stream — the truth
    commands: Vec<Command>, // the drained command sequence
    blobs: BlobManifest,    // refs to content-addressed payloads (not inlined)
}
```

- **The log is the truth, not state.** `BrainState` is never stored — always rederivable.
- **`verify()`** re-folds the events into a fresh brain and asserts the reconstructed log **and** command sequence equal the recorded ones, bit-for-bit. This is the release gate: any new control-flow path ships with a replay test.
- **The `TraceStore`** (a directory under the agent's data dir) holds immutable traces keyed by content-derived `trace_id`, with `depends_on` lineage in the header; `head()` reads metadata without folding events; file creation is atomic (`create_new`) so parallel asks are collision-free.
- **Resume after crash** is the same machinery: fold the persisted log, append `OpCancelled` for ops that were in flight, continue live.

### 20. Risks & mitigations

| Risk                                                | Mitigation                                                            |
| --------------------------------------------------- | --------------------------------------------------------------------- |
| Interface over-/under-engineered                    | Narrow waist: type only what the brain branches on (§14)              |
| Traces balloon from per-token deltas                | Deltas are transport-only; persist consolidated records + blobs (§17) |
| Sans-IO makes the simple case painful               | `hugr run` on an agent crate folder is the ten-second loop            |
| Canonical model type too thin to use providers well | First-class streaming/tool-call fields + opaque `extra`               |
| Feature creep back toward a platform                | One artifact, one escape hatch (MCP), no enum without a branch        |

## Part IV — Security & threat model

### 21. The security model

**Sandbox-by-registration.** A subagent can only invoke a capability its manifest grants; an ungranted tool is never registered, so there is no code path to it. The manifest is the audit surface a human reviews. The threat actor is the **model** (and any content it reads): every tool argument is attacker-controlled, and each jail must hold against adversarial arguments. Tools return semantic errors to the model (never panics), so a rejected escape attempt is just another tool result.

Assumptions and non-goals: the manifest is trusted (a grant's scope is authored by the operator, not the model); resource exhaustion beyond documented caps, timing side channels, and anything the operator explicitly grants (pointing `fs_read` at `/`) are out of scope — granting broadly is a manifest review failure, not a jail bug. The process/OS boundary (running an untrusted binary) is the operator's responsibility.

### 22. Per-tool threat notes

**`fs_read`** (read-only, one canonicalized root):

- **Path traversal (`../`, absolute, prefix).** Rejected component-wise before any filesystem touch: caller paths must be relative with only `Normal`/`CurDir` components. Test: `jail_rejects_traversal_and_absolute_paths`.
- **Symlink escape.** A symlink inside the root pointing outside clears the component check — the defense is the **post-canonicalize `starts_with(root)` re-check** on every resolved target; recursive walks apply the same filter per entry. The root itself is canonicalized at construction. Test: `jail_rejects_symlink_that_escapes_the_root` (unix).
- **TOCTOU on canonicalize.** The window between canonicalization and read is accepted because the tool is read-only — worst case is reading a swapped file, not writing outside the jail. Documented, not defended.

**`scratchpad`** (per-lineage scratch subtree, ungated — the jail is the boundary):

- **Traversal & symlink escape.** Same discipline as `fs_read`; **writes canonicalize the (created) parent directory too**, so a symlinked parent can't redirect a write outside the jail. Tool results carry only relative paths, so scratch contents never enter the log. Tests: `crates/hugr-agent/tests/scratchpad.rs`.
- **Cross-ask / sibling leakage.** Each ask gets its own working copy, seeded copy-on-fork from the parent's finalized subtree — a fork sees ancestor notes but never a sibling's writes.

**`web_fetch`** (network; host allowlist + GET-only default + byte cap; empty allowlist ⇒ fail-closed):

- **Off-allowlist host.** The parsed host must equal an allowlisted host or be a dot-bounded subdomain. Userinfo tricks (`https://allowed@evil.com`) resolve to the real host and are rejected; suffix collisions (`notexample.com` vs `example.com`) are prevented by the `.` boundary.
- **Redirect bypass (SSRF).** Automatic redirects are disabled (`redirect::Policy::none()`); a `3xx` is returned to the model as-is, and following it is a *new* call whose target is re-checked.
- **Scheme confusion.** Only `http`/`https`; `file://` etc. cannot exfiltrate local files.
- **DNS-rebinding / internal-IP SSRF.** Not defended at v1: allowlisting a host that resolves internally reaches it. Mitigation is operator-side; resolve-and-pin is future work.

**External grants (`mcp`, `agent`).** `[tools.mcp.*]` runs an operator-declared external process; its jail is the process boundary plus whatever the server enforces — Hugr does not sandbox its filesystem/network. Granting one is equivalent to trusting that command; `--config` surfaces the command/args for audit. `[tools.agent.*]` spawns a built Hugr agent whose own manifest is its jail; privileges compose downward only.

## Part V — Reference

### 23. Open questions

- **Trace schema migration.** Long-lived traces need a migration story as `Record`/`Event` evolve (`format_version` exists; migrations do not).
- **Trace garbage collection.** Fork trees accumulate; pruning policy is undecided (delete by hand for now).
- **Concurrent asks on one agent.** Default: each ask is an independent session/process (traces make this safe); a serving mode with a session pool is future work.
- **Storage backends.** Scratchpad, traces, and blobs assume a local filesystem today; the store boundaries are narrow enough to swap (a database, object storage) when a real need appears.
- **WASM.** The core has no environmental dependencies and should stay WASM-compilable; whether an artifact ever targets the browser is deferred until a use case appears.

### 24. Glossary

- **Subagent / agent** — a packaged Hugr artifact: agent crate (prompt + tools + config + optional Rust wiring) + runtime, exposing the ask/answer contract.
- **Brain / core** — the pure, sans-IO state machine (`hugr-core`).
- **Host** — the environment-specific layer that performs IO and drives the brain (`hugr-host`).
- **Agent crate folder** — the auditable agent source folder (`Cargo.toml`, `hugr.toml`, `SYSTEM.md`, optional Rust code).
- **Ask / Answer** — the uniform invocation contract: question + metadata in; structured response + mandatory metadata out.
- **Trace** — the durable, replayable event log of one session; identified by `trace_id`, optionally rooted on a parent via `depends_on`.
- **Fork** — starting a new session from an existing trace's log; the parent is immutable.
- **Scratchpad** — the agent's private filesystem subtree, writable without gates, jailed to its root.
- **Capability / tool** — a host-provided implementation of an effect; granted to an agent in its manifest. A built Hugr agent can itself be granted as a tool.
- **Event / Command / Op / Projection / Policy** — the core vocabulary of Part III.

### 25. The name

**Hugr** is Old Norse for "mind, thought, inner intent": a small, portable agent mind that runs inside many bodies. Pronounced **HUG-er**. Crates follow `hugr-<area>`; the CLI reads naturally as `hugr run`.
