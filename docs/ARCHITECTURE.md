# Technical Architecture

> Companion to `DESIGN.md`. This document gets concrete: the core ↔ host contract, state model, streaming/concurrency mechanics, replay, plugins, and crate layout. Rust types below are **illustrative sketches**, not final.

## 1. The shape in one diagram

```
           ┌─────────────────────────────────────────────┐
           │                   HOST                       │
           │  (per-environment: native / WASM / binding)  │
           │                                              │
   user ──▶│  inbox  ◀── LLM stream ◀── shell ◀── timers  │   real concurrency
           │    │                          ▲              │   lives here
           │    │ submit(event)            │ exec command │
           │    ▼                          │              │
           │  ┌────────────────────────────┴───┐         │
           │  │            BRAIN (core)         │         │
           │  │   pure, single-threaded,        │         │
           │  │   sans-IO state machine         │         │
           │  │                                 │         │
           │  │   poll() -> [Command]           │         │
           │  └─────────────────────────────────┘         │
           └─────────────────────────────────────────────┘
```

The brain never does IO. It consumes one ordered event stream and produces commands. The host does everything else.

## 2. The core ↔ host contract

The entire surface between brain and host is two enums plus two methods. This is what keeps bindings trivial.

### 2.1 Commands (brain → host)

```rust
/// Stable, serializable. Every effectful command carries an OpId so its
/// results can be correlated and it can be cancelled.
pub enum Command {
    /// Start a model completion. `model` is a logical *selector* (currently
    /// "small", "medium", or "big"), NOT a concrete endpoint — the host resolves it (§5.3).
    /// Host streams deltas back as Events.
    StartModelCall { op: OpId, model: ModelSelector, request: ModelRequest },

    /// Invoke a host capability (tool). Covers shell, fs, http, plugins —
    /// there are NO privileged built-ins.
    StartCapability { op: OpId, name: CapabilityName, args: Value },

    /// Request permission for a pending action; host's policy decides.
    RequestPermission { op: OpId, request: PermissionRequest },

    /// Abort an in-flight operation (HTTP request, process, etc.).
    Cancel { op: OpId },

    /// Emit a UI/observability event for front-ends. Side-effect-free for state.
    Emit(OutputEvent),

    /// Persist current durable state (checkpoint for resume).
    Checkpoint,

    /// The turn/session reached a terminal state.
    Done { reason: DoneReason },
}
```

### 2.2 Events (host → brain)

```rust
pub enum Event {
    /// New user input arrived.
    UserInput { text: String /* or richer */ },

    /// Host-injected request for one lossless compaction pass.
    CompactContext,

    /// Host-injected one-shot tier override for the next normal model turn.
    ModelOverride { selector: Option<ModelSelector> },

    /// Streaming model output. Many of these per StartModelCall.
    ModelDelta { op: OpId, delta: ModelDelta },
    ModelDone  { op: OpId, usage: Usage, stop: StopReason },
    ModelError { op: OpId, error: ModelError },

    /// Capability (tool) results — may stream (e.g. shell stdout) or be one-shot.
    CapabilityChunk { op: OpId, chunk: Value },         // e.g. a line of stdout
    CapabilityDone  { op: OpId, result: Value },
    CapabilityError { op: OpId, error: CapabilityError },

    /// Policy decided a permission request.
    PermissionDecision { op: OpId, decision: Decision },

    /// An operation the host aborted (in response to Cancel, or externally).
    OpCancelled { op: OpId },

    /// Injected nondeterminism (see §6 Determinism).
    Tick { now: Timestamp },
    Random { bytes: [u8; 32] },
}
```

### 2.3 The driver loop (host-side)

This is the *entire* integration surface a binding must implement. Everything hard lives in the host's async runtime; the brain stays synchronous.

```rust
// Pseudocode. `brain` is the sans-IO core; `host` is environment-specific.
loop {
    // 1. Drain commands the brain wants performed.
    for cmd in brain.poll() {
        match cmd {
            Command::StartModelCall { op, request } => host.spawn_model(op, request),
            Command::StartCapability { op, name, args } => host.spawn_capability(op, name, args),
            Command::RequestPermission { op, request } => host.spawn_policy(op, request),
            Command::Cancel { op } => host.abort(op),
            Command::Emit(ev) => host.render(ev),
            Command::Checkpoint => host.persist(brain.snapshot()),
            Command::Done { .. } => return,
        }
    }

    // 2. Block until the next event from ANY source (merged, ordered, stamped).
    let event = host.next_event().await;   // the only `await` — host-side only

    // 3. Feed it in. Pure, instant, no IO.
    brain.submit(event);
}
```

Key properties:

- `brain.poll()` and `brain.submit()` are **synchronous and pure** — no `async`, no IO. A WASM/Python/JS binding calls them directly.
- The only `await` is `host.next_event()`, entirely on the host side.
- The host is free to have many concurrent tasks (one per in-flight op) all feeding the same `next_event()` channel.

### 2.4 The data-interface trap, and the one rule that avoids it

The single biggest design risk is the interface itself. Over-engineer it (rich types for everything) and writing a new host becomes a huge chore, and every extension is a breaking change. Under-engineer it (everything is an opaque blob) and the brain can't reason about anything, capabilities are weak, and you bolt on hacks later. The resolution is a **narrow-waist** interface (like IP in the network stack: a small, stable middle; rich variability at the edges), governed by one rule:

> **Type only what the brain branches on. Everything else is an opaque payload.**

Apply it field by field:

- The brain **branches on** op lifecycle (start/delta/done/error/cancel), model *output structure* (text vs tool calls vs stop reason), turn control, and permission outcomes → these are **typed and stable**. There are few of them and they rarely change.
- The brain **only stores/forwards** capability arguments, capability results, plugin payloads, provider-specific params, prompts, and answers → these are **opaque** (`Value`/bytes). The brain is a *router and bookkeeper* for them, never an interpreter. Adding a new tool, a new provider knob, or a new plugin therefore touches **zero** core types.

Concretely: `StartCapability { name, args: Value }` keeps `args` opaque, so new tools never change the core. `ModelOutput.tool_calls` is typed, because the brain must decide "are there tool calls? then run them." `PermissionRequest` carries a typed *outcome channel* but an opaque *detail* blob the policy interprets. `ModelRequest` is typed for the parts the brain assembles (blocks, cache hints) but carries an `extra: Value` for provider knobs it never reads.

Two more guardrails against breakage on extension:

- **`#[non_exhaustive]` on every public enum**, so adding a `Command`/`Event`/`Record` variant is not a breaking change for hosts (they already have a `_ => {}` arm).
- **Forward-compatible passthrough.** A newer host or plugin may carry data an older core doesn't understand; because such data rides in opaque payloads, the core stores and replays it untouched rather than rejecting it.

This is what lets the interface stay small enough to bind in ~an afternoon, yet never block a future capability: the brain's *vocabulary* is fixed and tiny; the *content* it carries is unbounded.

### 2.5 What the brain actually does (and what it doesn't)

A fair worry is that the brain, described abstractly, sounds like it "does everything." It does not — it is small and its job is precise. See `draft/brain_sketch.rs` for an annotated, end-to-end sketch of the reducer; the exhaustive list is:

1. **Bookkeeping** — maintain the append-only log and the in-flight op table.
2. **The turn loop** — drive `user → model → (tool calls?) → tools → model → … → done`. This is the agentic control flow, and it is the brain's core reason to exist.
3. **Ask the pluggable `TurnPolicy`** — which model to call (multi-model routing), how to project context from the log, whether a capability needs permission. *Strategy* lives in the policy, not hardcoded in the reducer.
4. **Route opaque payloads** — turn a model's tool calls into `StartCapability` ops; feed results back as context. The brain never interprets the args/results.
5. **Emit** permission requests (the host's policy decides) and cosmetic UI events.
6. **Decide lifecycle** — when a turn/session is `Done`; when to `Checkpoint`.

What the brain explicitly does **not** do: any IO, HTTP, or model calls; running tools; rendering; deciding permissions; resolving which concrete model a selector maps to; storage; scheduling. All of that is the host. The brain answers exactly one question, repeatedly: *"given the log and the event that just arrived, what should happen next?"*

## 3. State model: event log + projection

### 3.1 Durable state is an append-only log

```rust
pub struct Session {
    log: Vec<LogEntry>,        // append-only source of truth
    state: BrainState,         // derived; rebuildable by folding `log`
}

pub struct LogEntry {
    seq: u64,                  // host-assigned global order (also replay key)
    at: Timestamp,             // from injected Tick, never a syscall
    record: Record,            // user msg, model output, op start/chunk/done, ...
}
```

- The log is the truth. `BrainState` (including the in-flight op table, see §4) is a fold over the log and can always be rebuilt.
- Resume = load the log, replay the fold. Branch = copy a log prefix. Rewind = truncate to a `seq`.

### 3.2 Model context is a projection, not the log

Per turn, the brain asks the turn policy for a pure, inspectable context plan from the log, then renders the actual model request from that plan:

```rust
pub trait TurnPolicy {
    fn project_context(&self, log: &[LogEntry], budget: TokenBudget) -> ContextPlan;
}
```

`ContextPlan` carries one entry per source block with its disposition (`Included`, `Referenced`, `Summarized`, or `Omitted`), a reason, the recorded token estimate, budget totals, and cache hints. The reducer derives `ModelRequest` from the plan by rendering only included/referenced/summarized entries. Projection decides, per block, whether to include verbatim, summarize, evict-to-reference, or drop. Crucially:

- **Evicted content is referenced, not deleted.** A large tool output becomes `{ ref: "op:8", summary: "...", tokens: 12000 }` in context, with the full bytes still in the log (or a content-addressed blob store, §3.3). It can be rehydrated on demand.
- **Tool-call transcripts stay provider-valid.** The log may contain host hook records or op metadata between a model `tool_calls` output and the matching durable `ToolResult`; projection renders the matching tool-result blocks immediately after the originating assistant tool-call block, then marks the later log-position tool result as already represented. This preserves the append-only source of truth while satisfying strict OpenAI-compatible chat formats.
- Compaction is a *projection choice*, never mutation of the log. Nothing is ever lost.

### 3.3 Large payloads: content-addressed blobs

Tool outputs and large inputs are stored by hash; the log holds the reference. The host provides the blob store as a capability (in-memory for browser, disk for native). This keeps the log small and context assembly cheap.

```rust
pub struct BlobRef { hash: Hash, len: u64, media: MediaType }
```

Implemented (P3-2): `hugr-replay::BlobStore` is the disk-backed, content-addressed store (SHA-256 keys, `"sha256:<hex>"`; identical content dedupes to one file). It produces `BlobRef`s in the exact shape the trace's `BlobManifest` carries, so a large payload offloaded by digest rehydrates on load. `hugr-host` exposes it as an ordinary `blob` capability (not a privileged built-in; opaque `Value` args/results) — a browser host can swap in a different store. The store's `std::fs` IO lives in the host-side persistence crate; `hugr-core` stays sans-IO.

### 3.4 Compaction is a model op, not a function

`ContextPolicy::project` is **pure and synchronous** — `log -> ModelRequest`. It only *reads* what's in the log (including any existing summaries) and decides include/evict/reference. It must never block or call a model, or the brain stops being a pure state machine.

But real compaction (summarizing old turns to reclaim budget) requires a model call. So compaction is **not** part of projection — it is a **separate model op the brain triggers**, exactly like any other:

1. When projection would exceed the budget (or a watermark is crossed), the brain emits a `StartModelCall` over the span to compact, using the selector `TurnPolicy::choose_model` returns for `RoutingPhase::Compaction` (the shipped `RoutingPolicy` picks `small`; `StaticPolicy` falls back to its default model). The selected span never splits a tool_use/tool_result group — the boundary is extended so a `ModelOutput` carrying tool calls and its answering `ToolResult`(s) are summarized together. The summarization prompt and per-record rendering are provided `TurnPolicy` methods (`compaction_request` / `render_summary_record`) with core defaults, so hosts override them without a reducer edit.
2. Its `ModelDone` result is appended to the log as a **summary `Record`** that references the span it replaces.
3. The *next* projection sees that summary and evicts the underlying entries to references (§3.2) — nothing is lost; the originals remain in the log/blobs.

This keeps projection pure while compaction stays an ordinary, replayable, cost-attributed op (it shows up in the trace with its own `OpMeta`). The cost: a small **compaction sub-loop** in the brain (decide-when, span-selection, "don't compact while a turn depends on those entries") — straightforward, but it is real logic to design, not free.

Manual compaction is the same mechanism with a different trigger: a host injects `Event::CompactContext`, the reducer selects one span through the same pure `TurnPolicy::select_compaction_span` hook, emits one policy-routed compaction model call, and appends the returned summary. Because the trigger and summarizer result are events, replay never re-runs the summarizer or re-decides the span.

### 3.5 Token counts come from the host, at ingestion

Projection decides what fits a `TokenBudget`, but the brain **cannot tokenize** (provider-specific, potentially heavy, not sans-IO-friendly). Split:

- The **host tokenizes once, at ingestion** — when a `ModelDone`/`CapabilityDone` content enters the log, the host annotates the record with its token count (e.g. on `OpMeta`/content metadata).
- The brain's projection then just **sums stored counts** against the budget — arithmetic, not tokenization.

The stored count is an estimate (necessarily approximate across model families — a count for one model ≠ another); it's good enough for projection *decisions*. Authoritative accounting still comes from the returned `Usage` after each call.

### 3.6 Routing inputs are derived, never observed

Model-tier routing is another policy decision over projected state, not host state. Before a normal model call the reducer builds a pure `RoutingInputs` snapshot from `BrainState`, the durable log, and the current `ContextPlan`: routing phase, recent tool-risk signal, context pressure, recorded recent failures, and any recorded one-shot override. `TurnPolicy::choose_model(state, inputs)` receives that snapshot and returns a logical selector. Because every input is reconstructed from the same recorded event stream and stored token estimates, replay re-derives the same selector without tokenizing or consulting the environment.

## 4. In-flight operations & concurrency

### 4.1 The op table

```rust
pub struct BrainState {
    inflight: BTreeMap<OpId, OpState>,  // every started, not-yet-finished op; ordered so cancel fan-out is deterministic
    // ... projection caches, counters, etc.
}

pub enum OpState {
    Model { buffer: PartialModelOutput },   // accumulates ModelDelta
    Capability { kind: CapabilityName, buffer: Vec<Value> },
    AwaitingPermission { request: PermissionRequest },
}
```

- `StartModelCall`/`StartCapability` insert into `inflight`.
- Each `*Delta`/`*Chunk` updates the buffer **cheaply** (append only).
- `*Done`/`*Error`/`OpCancelled` remove from `inflight` and append a final `Record::OpEnded` to the log, carrying **per-op metadata** (`OpMeta`).

Every op records `OpMeta` when it ends. Cost is just *one* field — **timing matters at least as much**: `started_at`/`ended_at` give wall-clock latency per op (which model call was slow, how long a tool ran), useful for observability, scheduling, and debugging. `OpMeta` holds `{ started_at, ended_at, model: Option<ModelSelector>, routing: Option<RoutingDecision>, usage: Option<Usage>, extra: Value }`, where `routing` records the chosen selector, pure routing-input snapshot, and reasons, and `extra` is an opaque bag (provider request-id, cache-hit info, retry count, …) the brain stores but never interprets (narrow-waist, §2.4). Because it lives on the log record, latency, spend, and escalation reasons are queryable from the trace itself — no side table — and aggregate per op, per model selector, or per sub-agent.

### 4.2 Atomicity & ordering

The brain processes one event at a time. Concurrency is the host merging many sources into one ordered, sequence-stamped stream. Therefore:

- No locks inside the brain.
- "Atomic events" is automatic: an event is fully reduced before the next.
- A model stream (op 7) and a shell stream (op 8) interleave in the inbox in real arrival order; the brain handles whichever event is next.

**Foreground vs background ops.** Whether an op *holds the turn open* is a policy decision, not a host one. The `TurnPolicy` answers `is_background(capability)`; the reducer marks the in-flight op accordingly. A **foreground** op (the default) blocks the turn: the model only resumes once every foreground op of the turn has resolved (the fan-out join, §6.3). A **background** op does **not** block the turn: the brain resumes the model immediately, so the model stream and the background op (e.g. a long `cargo build`) run *simultaneously*, their events interleaving atomically. When a background op finishes, its result is folded into the log and picked up at the next turn boundary; if the model already produced its final answer while the background op was still running, the brain defers `Done` until the background op resolves (the turn isn't over while work is in flight). The host runs every op — foreground or background — identically: one task per op feeding the shared inbox. Background-ness is invisible to the host; it never reaches a `Command` variant.

### 4.3 Cancellation

```
brain emits Command::Cancel { op: 7 }
  → host aborts the op 7 HTTP request / kills process
  → host emits Event::OpCancelled { op: 7 }
  → brain removes op 7 from inflight, logs "op 7 cancelled after N tokens"
```

First-class, no polling. The brain decides *when* to cancel based on any event (e.g. a background build failing makes the in-flight response moot).

### 4.4 Backpressure & coalescing

- Handlers must stay O(1)-ish (append to buffer). No heavy work in the reducer.
- The **host** may coalesce high-frequency deltas (e.g. batch model tokens every ~16ms, or shell output per line/Nms). The brain need not know coalescing happened.
- Optional: the brain can signal a soft backpressure hint via `Emit`, but the authoritative throttling is host-side.

### 4.5 Deltas are transport, not durable state (this is what keeps traces small)

A response of thousands of tokens arrives as thousands of `ModelDelta` events. If each were persisted, traces would be enormous JSONL relative to a normal message list. They are not, because **deltas are ephemeral transport, never durable records**:

- A `ModelDelta` does two cheap things and is then discarded: it appends to the op's live buffer (for streaming UI) and triggers a cosmetic `Command::Emit`. It is **never** written to the log.
- The **authoritative** result arrives once, in `ModelDone { output }` (the consolidated message). The brain's *logic* keys off this, and exactly **one** `Record::ModelOutput` is appended to the log per model call. Same for tools: many `CapabilityChunk`s stream, but one `Record::ToolResult` is persisted.

So the durable trace holds roughly **one record per logical message / tool result** — comparable in size to a conventional `messages[]` history, not to the raw delta stream. Large payloads inside those records are further offloaded to content-addressed blobs (§3.3) and deduplicated.

This also keeps replay (§6) cheap and clean: replay feeds the consolidated `ModelDone` directly (no deltas), and the brain produces identical commands because its logic never depended on individual deltas. The only thing not reproduced is the token-by-token *visual* — which is cosmetic. If you ever want pixel-faithful streaming replay (e.g. for a demo recording), that is an **opt-in, separately stored delta journal**, never part of the default trace.

### 4.6 User input & mid-turn steering

"User input" is broader than a chat message. Four categories, only the first has a mid-turn question:

1. **Conversational input** — a new instruction, possibly rich (text + images + file refs + pasted blobs).
2. **Control signals** — abort/interrupt, pause/resume. No new content, just "stop."
3. **Responses to brain asks** — `PermissionDecision`; it only arrives when the brain is waiting for it.
4. **Session operations** — rewind/fork-at-seq, edit-and-resume, switch model/policy/permission-mode. These are *host actions on the log* (built on fork, §14), not ordinary reducer events.

Because conversational input can arrive **while a model/tool op is in flight**, the reducer has an explicit "input while ops in flight" arm. Three steering mechanisms, all supported by the brain; the *choice* is a flag/policy, never hardcoded:

- **Queue** (default) — append the input; process it at the next turn boundary once current ops resolve. Non-disruptive.
- **Interrupt/steer** — `Cancel` the in-flight ops, append the input, start a fresh model turn that sees both the partial work and the new instruction. Reuses cancellation (§4.3); no new mechanism.
- **Append-and-continue** — add to context, let the current op finish, the *next* model call picks it up.

Modeled as two events (mirroring the common UX of *type = queue, ESC = interrupt*):

```rust
Event::UserInput { content: Value, mode: SteerMode }   // mode defaults to Queue
Event::UserAbort                                        // pure cancel, no new content
enum SteerMode { Queue, Interrupt, AppendAndContinue }
```

Because a cancelled op's partial output is logged (§6.4), an interrupt hands the model genuinely useful context: *"you had started saying X when the user interrupted with Y."*

## 5. Model provider abstraction

### 5.1 Canonical request/response with first-class optional fields

```rust
pub struct ModelRequest {
    blocks: Vec<ContextBlock>,     // structured, NOT a concatenated string
    tools: Vec<ToolSchema>,
    params: SamplingParams,
    cache_hints: Vec<CacheBreakpoint>,   // first-class, provider-mapped
    reasoning: Option<ReasoningConfig>,  // thinking/extended reasoning
}

pub struct ContextBlock {
    role: Role,
    content: Vec<ContentPart>,     // text, tool_use, tool_result, image, ref...
    cacheable: bool,
    evictable: bool,
    priority: u8,
    est_tokens: u32,
}

pub enum ModelDelta {
    Text(String),
    Reasoning(String),             // thinking deltas, kept separate
    ToolCallStart { id: String, name: String },
    ToolCallArgsDelta { id: String, json_fragment: String },
    ToolCallEnd { id: String },
}
```

### 5.2 Adapters at the edge

Provider adapters live in the host layer (or a host-side crate), translating `ModelRequest`/`ModelDelta` to/from Anthropic, OpenAI, etc. The brain is provider-agnostic but never lowest-common-denominator: cache breakpoints, reasoning blocks, and streaming tool calls survive the round trip because they're first-class in the canonical type. Provider-specific knobs the brain doesn't reason about ride in an opaque `extra: Value` on `ModelRequest` (per the narrow-waist rule, §2.4), so adding a provider feature never changes a core type.

### 5.3 Why a model call is a typed command — not just a capability — and how multi-model works

This is deliberate, and it follows directly from §2.4. A model call and a tool call play *different roles*:

- The brain **reasons about model output**: it assembles deltas, inspects the result for tool calls, and the result *drives the turn loop*. That output structure must be **typed** (`ModelOutput` with `tool_calls`, `stop`, …). The model call is the *engine* of the control loop.
- The brain **does not reason about tool output**: capability results are opaque `Value`s it merely routes back as context. Tools are *leaves*.

Different role, different typing → they're different commands. (At the *host* level a model is still "an effect the host provides," registered much like a capability — the split is purely about whether the brain interprets the result.)

**Multi-model** falls out cleanly because `StartModelCall` names a **logical `ModelSelector`**, not a concrete endpoint:

```rust
pub enum ModelSelector {
    Named(String),   // product tiers: "small" | "medium" | "big"; type stays open
}
```

Two layers, cleanly split:

- **Which logical model to use** for a given step is an *agent-strategy* decision → it lives in the pluggable `TurnPolicy::choose_model` in the brain. The current product ships exactly three configured tiers (`small`, `medium`, `big`), with `StaticPolicy` defaulting normal turns to `medium`; Phase B adds real routing over those tiers. The type remains open so future hosts can experiment without changing the core.
- **What each selector resolves to** (concrete provider, model id, endpoint, key, adapter) is *host configuration* → a host-side **model registry** maps `selector → adapter`.

```rust
// Host-side. The brain never sees any of this.
struct ModelRegistry { /* "small" -> OpenAIAdapter{…}, "medium" -> OpenAIAdapter{…}, "big" -> OpenAIAdapter{…} */ }
```

So binding the three shipped tiers is: register three entries in the host, and let the policy pick among those role names. The brain stays a handful of selector strings; all the wiring, cost, and provider specifics live in the host. Each model op records its selector in `OpMeta` (§4.1), so per-role spend *and* per-role latency fall out of the trace for free.

### 5.4 Error handling & retries: transport (host) vs semantic (brain)

Errors split cleanly along the same line as everything else — *did the brain need to reason about it?*

- **Transport errors → host.** Rate limits (429), network blips, timeouts, 5xx, TLS, connection resets, and provider-specific concerns like prompt-cache handling are the host's job. The host does retry/backoff internally and only surfaces an event to the brain when it has either succeeded (`ModelDone`/`CapabilityDone`) or genuinely given up (`ModelError`/`CapabilityError`). The brain never sees the intermediate retries — they're not part of the logical session. (Replay note: only the *final* outcome is recorded, so a replayed session doesn't re-suffer transient failures.)
- **Semantic errors → brain.** Malformed tool-call JSON, schema-invalid arguments, a tool that ran but returned a logical failure, a stale-edit `Conflict` (§7.3) — these are *part of the conversation*. The brain routes them back into the turn loop as a tool/error result so the model can correct itself and retry. This is ordinary turn-loop flow (`maybe_resume_model_turn`), not a special path.

The rule of thumb: if retrying the exact same request unchanged might work, it's transport (host). If the model has to *change something* to succeed, it's semantic (brain).

## 6. Determinism & replay

### 6.1 All nondeterminism is injected

- **Time:** the brain never calls a clock. The host injects `Event::Tick`. Log entries get `at` from the tick.
- **Randomness:** injected via `Event::Random` (or seeded per session). The brain never calls an RNG.
- **Model output, tool results, user input, IO:** all arrive as events.

### 6.2 Record

Recording a session = persisting the ordered, consolidated record stream (with `seq`) — one record per logical message/tool-result, **not** the raw deltas (§4.5). Because the brain is a pure fold, this stream + the initial state fully determine every command the brain ever emitted.

### 6.3 Replay

Replay = feed the recorded events back in `seq` order to a fresh brain. The brain emits identical commands. Uses:

- **Testing:** assert the command sequence for a recorded scenario.
- **Debugging:** step through a real session deterministically.
- **Resume after crash:** replay the persisted log up to the last `seq`, then resume live (re-issue any ops that were in-flight at crash time, or mark them cancelled — a policy choice recorded in the log).

### 6.4 Partial-op representation

A cancelled/interrupted op is logged explicitly:

```
Record::OpEnded { op: 7, kind: Model, outcome: Cancelled { produced_tokens: 50 } }
```

so projection and replay treat it consistently. Never an implicit gap.

## 7. Capabilities (tools) & policy

### 7.1 Uniform capability interface

```rust
// Host-side registry. Shell, fs, http, and plugin tools are ALL capabilities.
pub trait Capability {
    fn name(&self) -> CapabilityName;
    fn schema(&self) -> ToolSchema;
    // Streaming-capable: yields chunks then a final result.
    fn invoke(&self, args: Value, sink: &mut dyn ChunkSink) -> CapResult;
}
```

- The brain only emits `StartCapability { name, args }`. It has no idea whether `name` is a local shell or a remote service.
- A browser host registers `http`, `fetch`, maybe a sandboxed `eval`; it simply does not register `shell`. The brain adapts because tool availability is data (the tool schemas in `ModelRequest`).

### 7.2 Externalized policy

```rust
pub enum Decision { Allow, Deny { reason: String } }

pub trait Policy {
    fn decide(&self, req: &PermissionRequest, ctx: &PolicyCtx) -> Decision;
}
```

- Default native/browser product mode: `AutoApprove` asks the configured `small` tier for a yes/no safety verdict and returns `Allow` or `Deny { reason }`; the reason is routed back to the model as a tool-result-shaped denial.
- `yolo` host mode: `AllowAll` returns `Allow` for every gated action.
- CI/locked-down hosts can still use allowlist or deny-all data policies, returning `Allow`/`Deny` without prompting.

The brain's loop is identical in all modes. Permission is an op like any other (`RequestPermission` → `PermissionDecision`), and the decision event is recorded so replay never re-runs a judge.

### 7.3 Stateful capabilities & the stale-edit problem (optimistic concurrency)

The classic case: the model reads a file, the file changes externally, and the model's edit is now based on a stale view. The same shape appears for any capability over **external mutable state** — editing a remote doc, patching a PR, updating a DB row. This is optimistic concurrency control (compare-and-swap), and we split it deliberately into two halves:

- **The *check* always stays at the host.** Only the host sees the live external state, and the comparison must be **atomic with the write** or a TOCTOU race lets a concurrent writer slip in between check and write. So the brain can never perform the check itself.
- **The *bookkeeping* (the read-set) lives in the brain.** "What version did we last see for object O" is pure derived state, it belongs in the log, and centralizing it removes per-capability duplication. The brain keeps it; the host just receives an `expected_version` instead of remembering one.

#### The mechanism

The brain maintains a generic optimistic-concurrency table as a projection folded from capability results:

```rust
// In BrainState. Values are OPAQUE to the brain: it uses only Eq/Hash, and
// NEVER parses a path or a hash. This is what keeps it within the narrow waist
// (§2.4) — the brain legitimately branches on version *equality*, nothing more.
versions: HashMap<ObjectKey, Version>,

pub struct VersionRef { object: ObjectKey, version: Version }
pub type ObjectKey = String;  // host-canonicalized identity, e.g. abs path / "pr:org/repo#42"
pub type Version   = String;  // opaque token: content hash / etag / git sha / row xmin / ...
```

Flow:

1. A read-like capability returns a `VersionRef` in a **standard typed slot** of its result (not buried in the opaque blob). The brain records `versions[object] = version`.
2. When the model emits a mutating tool call, the brain looks up the target object's last-seen version and **stamps `expected_version` onto the op** — the model never sees or supplies the token.
3. The host performs an atomic CAS. On mismatch it returns `CapabilityError::Conflict { current_version, current_content_ref }`.
4. The brain treats `Conflict` like any other capability error: it routes it back into the turn loop as a tool result ("file changed since you read it, here is the current content, redo your edit"), and the model re-reads/retries. Same machinery as §3b `maybe_resume_model_turn`.

Because both the read-set and the conflict outcome live in the log, this is replay- and resume-safe by construction.

#### The one capability-specific bit, handled declaratively

Knowing that `fs.edit { path: "/x" }` targets object `"/x"` is capability-specific knowledge the brain must not hardcode. Rather than put parsing logic in the brain, the **tool schema declares it**: e.g. "my object-key is the `path` argument; my version is returned in the result's `version` field." The brain (or a shared helper) generically plucks the declared field. That declarative metadata is the (small, opt-in) cost this design adds to the data interface.

#### Default with an opt-out

This brain-centralized table is the **well-paved default**, opt-in per capability. Stateless capabilities (an HTTP GET, a calculator) simply omit the envelope and pay nothing. Two cases **opt out** and keep concurrency host-side:

1. **Native concurrency primitives** — ETags, DB transactions, git refs. The host uses them directly; the brain's table would be redundant.
2. **Sub-object / mergeable concurrency** — two edits to *different* functions in the same file shouldn't conflict, but a whole-file content hash says they do. Region-level or CRDT-style merge is inherently capability-specific and belongs at the host.

The honest limit: the generic table expresses only **whole-object** optimistic concurrency. That covers the majority case (it is what Claude Code does for files), which is why it is the default — not the only mechanism.

#### Division of responsibility, summarized

| Concern                                       | Owner                        |
| --------------------------------------------- | ---------------------------- |
| Canonical object identity (`ObjectKey`)       | Host (produces it)           |
| Version token meaning (hash/etag/sha)         | Host (produces & interprets) |
| Read-set: last-seen version per object        | Brain (folded from the log)  |
| Stamping `expected_version` onto a mutation   | Brain                        |
| The atomic compare-and-swap check             | Host                         |
| Reacting to a `Conflict` (loop back to model) | Brain                        |

## 8. Plugins

### 8.1 Primary ABI: WASM components, narrow contract

A plugin is a WASM component that the host loads and exposes as one or more capabilities, plus optional event hooks:

```
plugin exports:
  - describe() -> [ToolSchema]          // what capabilities it provides
  - invoke(name, args) -> stream<chunk> // capability implementation
  - on_event(event_view) -> [hook_action]  // optional, NARROW reactions only
host provides to plugin (imports):
  - request_capability(name, args)      // plugins can use other capabilities
  - log/emit
```

- Plugins **never** touch core internals or mutate `BrainState`. They react to an event *view* and request capabilities. Narrow now, widen later.
- Sandboxed by default (WASM) — aligns with the capability/policy model.

### 8.2 Secondary paths

- **Subprocess/MCP** for heavy or language-agnostic tools where weight is acceptable (server hosts only). Adapted into the same `Capability` interface.
- **Compile-time** capabilities for the batteries-included defaults (native shell/fs/http), shipped with the default host.

Implemented (Phase 5): `hugr-plugin-abi` owns the versioned, narrow contract (`describe`/`invoke`/`on_event` as tagged JSON, an integer `PROTOCOL_VERSION`, opaque `Value` payloads) behind a single transport-agnostic `PluginTransport` trait. The **subprocess** transport (`SubprocessPlugin`, stdio JSON) is the working default — a plugin is any external program, in any language, in its own repo, needing no core recompile and unable to touch core internals. The **WASM component** transport (the primary ABI above) is scaffolded behind the `wasm` feature (`WasmPlugin`) against the same trait; its wasmtime backend lands with Phase 4. The host wraps a loaded plugin's tools as ordinary `Capability`s (`hugr_host::plugins`) — no privileged plugins, mirroring "no privileged built-ins". `on_event` is defined but not yet delivered by the host (narrow now, widen later).

## 9. Front-ends

The core emits `OutputEvent`s via `Command::Emit`. Any number of front-ends subscribe:

- **TUI/CLI** (the first showcase host).
- **Browser/extension** renders the same event stream in DOM.
- **Headless** ignores most events, logs the rest.

Rendering is never inside the core; multiple front-ends can attach to one session simultaneously.

Implemented CLI decision (ROADMAP_2 D9): the native CLI stays on the stdout-streaming front-end for now, rather than adopting a TUI framework. This keeps logs copyable, works in dumb terminals and CI, and avoids taking the TUI dependency/API one-way door before the agent loop stabilizes. The stdout front-end owns readable status lines, compact tool cards, active background-op lists, token/cost/context counters surfaced by `/status`, and calm idle states; a future TUI can still subscribe to the same `OutputEvent`/lifecycle hooks without changing `hugr-core`.

## 10. Crate layout (proposed)

```
hugr-core         # sans-IO brain: state, log, projection, op table, reducer.
                   # NO tokio, NO reqwest, NO fs. #![no_std]-friendly if feasible.
hugr-model        # canonical ModelRequest/Delta + provider adapter traits.
hugr-providers    # Anthropic/OpenAI/... adapters (host-side, behind features).
hugr-host         # default native host: tokio driver, reqwest, shell/fs/http
                   # capabilities, disk blob store, interactive policy.
hugr-cli          # the batteries-included showcase CLI (≈ thin wrapper).
hugr-docs         # specialized read-only docs retrieval host/CLI.
hugr-wasm         # wasm-bindgen host glue for browser/extension.
hugr-py           # PyO3 bindings (poll/submit exposed).
hugr-js           # napi/wasm bindings for Node/Deno.
hugr-plugin-abi   # WASM component world definition + host loader.
hugr-replay       # versioned, portable TRACE format (save/load) + replay/inspect.
                   # Host-side persistence: depends on hugr-core as pure data,
                   # may use std::fs — never pulls IO into the core. (Phase 3.)
```

Dependency rule: **`hugr-core` depends on nothing environmental.** Everything async/IO/provider-specific lives outside it.

Implemented showcase host: `hugr-docs` demonstrates that host shape is not tied to the batteries-included terminal agent. It reuses `hugr-core`, `hugr-host`, and the OpenAI-compatible streaming adapter, but registers only folder-scoped read-only documentation capabilities (`docs_list`, `docs_search`, `docs_read`, `docs_read_range`, `docs_read_many`, `docs_read_range_many`, `docs_outline`) and emits a single machine-parseable JSON answer. It has no shell, no write/edit capability, no interactive policy surface, and no docs-specific core types; all retrieval arguments/results stay opaque `Value`s under the narrow-waist rule (§2.4).

## 11. Sizing & performance targets (initial)

- `hugr-core` compiled to WASM: low single-digit MB, ideally < 2 MB gzipped.
- Cold start (instantiate + first `poll`): single-digit ms.
- Steady-state per-event reduce: microseconds (append-only buffer updates).
- Memory per idle session: dominated by the log/blobs, not the runtime.

These are aspirations to validate early (see `ROADMAP.md` Phase 0/1 exit criteria), not guarantees.

## 12. Traces (saving & loading sessions)

A **trace** is the saved form of a session. Because the brain is a pure fold over an ordered event stream, a trace is just *that stream made durable* — there is no separate "save format" to invent.

### 12.1 What a trace contains

```rust
pub struct Trace {
    meta: TraceMeta,          // codename/version, schema version, created-at(seq 0 tick)
    events: Vec<Event>,       // the ordered host→brain stream — the replay INPUT
    log: Vec<LogEntry>,       // the ordered, seq-stamped CONSOLIDATED record stream (the truth)
    commands: Vec<Command>,   // the ordered commands the driver drained (serde-default; old traces load without it)
    blobs: BlobManifest,      // refs to content-addressed payloads (not inlined)
    children: Vec<ChildTrace>, // recorded sub-agent sessions, each a nested Trace tied to its parent op (§13.3; serde-default, old traces load without it)
    // NOTE: BrainState is NOT stored — it is always rederivable by folding the log.
}
```

Key points:

- **The log is the truth, not state.** We persist the consolidated record stream (§4.5) — one entry per logical message/tool-result — never the derived `BrainState`. This keeps traces small, forward-compatible (a newer core can re-fold an old trace), and impossible to desync from reality.
- **Deltas are not in the trace.** Raw `ModelDelta`/`CapabilityChunk` transport events are discarded after folding (§4.5); only the consolidated outcome is recorded. This is what makes a trace comparable in size to a normal `messages[]` history rather than a multi-thousand-line delta dump. (Pixel-faithful streaming replay, if ever wanted, is an opt-in separate delta journal.)
- **Blobs are referenced, not inlined.** Large tool outputs / inputs live in the content-addressed blob store (§3.3); the trace carries `BlobRef`s. A trace can be shipped with or without its blobs (e.g. share just the skeleton, or a full bundle).

### 12.2 Saving is a host capability, not core logic

The brain emits `Command::Checkpoint`; the host serializes the current trace (append-only, so checkpointing is cheap — usually just flushing new events). Implemented in the native host: `EngineBuilder::checkpoint(path, cadence)` writes atomic trace checkpoints (`Trace::save_atomic`) either on `Command::Checkpoint`, after every submitted host event, or after every N events; writes run in `spawn_blocking` off the driver loop and are single-flight (a checkpoint due mid-write marks dirty and rewrites the latest state when the writer finishes, guarded by a monotone generation), skip when nothing changed, and flush synchronously on session end and `Drop`. The core never decides *where* a trace goes (disk, IndexedDB in a browser, an HTTP endpoint, a Hub repo) — that's a host capability. Same core, any storage.

### 12.3 Loading / replay / portability

- **Replay** (§6) folds the events into a fresh brain → identical commands. `verify()` asserts the reconstructed durable log **and** the reconstructed command sequence equal the recorded ones, bit-for-bit and in order (a pre-commands trace falls back to log-only comparison). It then recursively verifies every recorded **child session** (`children`, §13.3) the same way — re-seeding a fresh brain from the child's recorded fork prefix under the child's recorded policy — and a failing child fails the whole verify with an error naming the op that spawned it.
- A trace is **portable**: record on a server, replay in the browser, because neither the brain nor the trace depends on the environment. (Caveat: replaying *live* — re-issuing real model/tool calls — needs those capabilities present; pure replay of the recorded run needs nothing.)
- Traces double as the substrate for **resume** (§15), **debugging**, **test fixtures** (§Roadmap cross-cutting), and **sharing reproductions**.

## 13. Sub-agents (the "agent subprocess")

A sub-agent is **not a special subsystem** — it is *another `hugr-core` instance*. Because the core is tiny, pure, and runtime-free, spawning one is cheap, and an arbitrarily deep tree of agents is just a tree of brains.

Implemented (Phase 6): `Command::StartAgent { op, agent, config, seed }` is emitted (instead of `StartCapability`) when the pluggable `TurnPolicy::agent_seed(capability)` designates a tool as a sub-agent spawner — *strategy* in the policy, not hardcoded in the reducer. `agent` is the typed agent-kind name (the capability name; serde-default for old traces); `config` is the model's opaque tool-call args passed through **untouched** (the brain never injects keys into them, §2.4); `seed` is the forked log prefix (§14). Nesting depth is a host concern: `EngineBuilder::max_agent_depth` (default 1) caps it, and exceeding the cap routes back to the model as an `agent_depth_exceeded` semantic tool result. Per-kind defaults (model tier, tool allowlist) are host registration data (`AgentDefaults` via `EngineBuilder::agent_with_defaults`), not reducer knowledge. The child runs **in-process** as a spawned host task (`hugr_host::agent::run_agent`) reusing a subset of the parent's model + capability registries; its ops live in a `JoinSet` so a parent `Cancel` tears down the subtree. Its digest returns as `Event::AgentDone { op, result }` (a text answer + aggregated usage), folded back like any tool result. Nested agents work with no special case. Replay of the parent stays flattened (§13.3): the parent trace records each child's `AgentDone`, so re-feeding it reconstructs the parent bit-for-bit without re-running children — and a recording host additionally nests each child's **own** recorded session into the parent trace (`Trace::children`, a `ChildTrace` per completed child), so children are visible to the trace, replay, and verification too.

### 13.1 A sub-agent is an op

```rust
Command::StartAgent {
    op: OpId,
    agent: String,              // typed agent-kind name (the spawning capability)
    config: AgentConfig,        // model, policy, tools subset, system prompt (opaque, untouched)
    seed: AgentSeed,            // how to initialize the child's log (see §14 forks)
}
```

Its lifecycle mirrors a model call or a process op:

- The parent emits `StartAgent { op, .. }`; it goes into the parent's in-flight op table as `OpState::Agent { .. }`.
- The child runs as its own brain. Its progress streams back to the parent as ordinary events (`CapabilityChunk`-style) keyed by the parent's `op` — e.g. intermediate output, then a final `CapabilityDone { op, result }`.
- The parent reacts like any other op: interleave, observe, **`Cancel`** the whole subtree, attribute usage/cost per agent.

### 13.2 Where the child actually runs (host's choice)

The core doesn't care; the host picks isolation per `AgentConfig`:

| Mode                    | How                                                                           | When                                          |
| ----------------------- | ----------------------------------------------------------------------------- | --------------------------------------------- |
| **In-process**          | Child brain reduced on the same thread, interleaved via the host's task merge | Cheapest; default for most fan-out            |
| **Worktree**            | Child host runs in an isolated git worktree                                   | Parallel file mutation that would conflict    |
| **Subprocess / remote** | Child brain in another process or machine, events over a transport            | Heavy isolation, language-agnostic, scale-out |

Crucially, the brain ↔ host contract (§2) is identical in all three — a remote sub-agent is the same enums over a different transport. This is exactly why "run anywhere" pays off internally: sub-agents reuse the whole portability story.

### 13.3 Determinism with sub-agents

The whole tree replays from one trace because the parent's event stream already records everything the children sent back. Hugr implements **both** views, with the flattened one canonical:

- **Flattened (canonical):** the parent's event stream records each child's digest (`AgentDone`/`AgentError`) keyed by the parent `op`, so re-feeding the parent trace reconstructs the parent bit-for-bit without re-running any child.
- **Nested (implemented too):** a *recording* host also captures each completed child session as a `ChildTrace { op, agent, seed, trace }` in the parent trace's `children` — the child's own event stream (in submission order, `Tick`s included), drained command sequence, consolidated log, captured policy, and the fork prefix (§14) it was seeded with. The nesting is recursive: a grandchild's `ChildTrace` lives inside its parent child's trace. `verify()` recursively verifies every child by re-seeding a fresh brain from the recorded seed (`Brain::from_log`) under the child's recorded policy and re-feeding its events; any mismatch fails the whole verification with an error naming the child's op. Both `children` and `seed` are serde-defaulted and skipped when empty, so pre-children traces load unchanged and childless traces stay byte-identical to the old format. Non-recording hosts skip all of this (children run unrecorded, zero overhead).

## 14. Forks

A **fork** is the primitive underneath sub-agents, branching, rewind, and speculative execution. Because durable state is an append-only log, forking is *copying a prefix*.

Implemented (Phase 6): `AgentSeed` (`Fresh` / `ForkAt { seq }` / `ForkFull`) is resolved to the actual log prefix by the brain (a pure operation on its own log), and `Brain::from_log` re-derives a child's `BrainState` by folding that inherited prefix with zero IO. Results flow back one-directionally as the `StartAgent` op's value — no log merge (§14.3).

```rust
pub enum AgentSeed {
    Fresh,                       // empty log — isolated child
    ForkAt { seq: u64 },         // copy parent's log[..=seq] — shared context, then diverge
    ForkFull,                    // copy the entire current log
}
```

### 14.1 Mechanics

- `ForkAt { seq }` creates a new log initialized with the parent's entries up to `seq`. The fork then evolves independently; the parent is untouched.
- **Copy-on-write** is the obvious optimization: forks share the immutable prefix (the log is append-only, so the prefix never changes) and only diverging entries cost memory. This keeps fan-out of N children over a large shared context cheap — central to the low-memory / many-agents goal.

### 14.2 What forks give you (all the same mechanism)

- **Sub-agent context sharing** — `ForkAt`/`ForkFull` seeds a child with the parent's context.
- **Branching / "what-if"** — fork, try a different path, compare.
- **Rewind / edit-resume** — fork at an earlier `seq`, drop later entries, resume with edited input.
- **Speculative execution** — fork and run multiple candidate continuations, keep the best.

### 14.3 Merging back

Returning results is *not* a log merge (that way lies CRDT pain). A child returns a **result value** (and optionally a summarized digest of its log) to the parent as the `StartAgent` op's result. The parent appends that result to its own log as one entry. The child's full log remains available as a referenced sub-trace if needed. Keep it one-directional: forks diverge, results flow back as values.

## 15. Durable resume & scheduling (cron)

### 15.1 Resume after crash

Resume is replay (§6.3) followed by going live:

1. Load the persisted trace; fold its events into a fresh brain → exact pre-crash `BrainState`, including the in-flight op table.
2. Reconcile ops that were in-flight at crash time. Two recorded policies:
   - **Re-issue:** the host restarts those ops (idempotent tools only).
   - **Cancel:** append `OpCancelled` for them and let the brain decide next. The choice is itself logged, so the resumed session stays replayable.
3. Continue the live driver loop.

Implemented native-host policy: `CrashResumePolicy::CancelInflight` is the conservative default and appends recorded `OpCancelled` events for stale in-flight ops before going live; the commands those reconcile submissions queue are drained (and recorded) so a resumed engine starts quiescent, with no stale pre-crash commands firing into the next live turn. Idempotent re-issue remains a future host policy. `CheckpointCadence::EveryEvent` is the crash-safe recording mode because the checkpoint captures the event that created the in-flight op before the op produces a terminal result.

This is why resume is *not* a feature to bolt on later — it's the same machinery as replay, available from Phase 3.

### 15.2 Scheduling / cron

Scheduling lives entirely in the **host** (the core has no clock — time is injected, §6.1). A scheduler fires a trigger by injecting an event, in one of three modes (mirroring how a mature trigger system works):

| Mode                 | Mechanism                                                    | Use                                                      |
| -------------------- | ------------------------------------------------------------ | -------------------------------------------------------- |
| **Resume existing**  | Load session trace (§15.1), then `submit(UserInput/Trigger)` | Recurring work you pick back up in the same conversation |
| **Named persistent** | Same, targeting a specific stored session id                 | Wake a specific sibling session                          |
| **Fresh per fire**   | New empty log, submit the trigger as the first event         | Each firing starts from a clean slate                    |

```rust
// Host-side scheduler (NOT in hugr-core).
struct Schedule { cron: CronExpr, target: TriggerTarget, prompt: String }
enum TriggerTarget { ResumeSession(SessionId), Persistent(SessionId), FreshSession }
```

A fire is just: (optionally load a trace →) inject a `UserInput`/trigger event → run the driver loop → checkpoint. Because the durable session is a trace, a cron job that "continues a conversation" and one that "starts fresh" differ only in whether a trace is loaded first. No special core support is needed beyond resume + event injection — both of which already exist.

Implemented native-host surface: `hugr_host::Schedule` pairs a `CronExpr` (`@every 10s`, `@every 5m`, `* * * * *`, `*/N * * * *`) with a `TriggerTarget` (`ResumeExisting`, `NamedPersistent`, or `FreshSession`) and a prompt; `fire_once` performs one fire by building or resuming an engine, running one user turn, and checkpointing the trace. The CLI exposes this as `hugr schedule --cron ... --trace|--session|--fresh ... [prompt...]` with `--once` for a single fire.

## 16. How §§12–15 reinforce each other

These four features are not four subsystems — they are **one mechanism (the event log) viewed four ways**:

- **Trace** = the log made durable.
- **Resume** = re-fold a trace, then go live.
- **Fork** = copy a log prefix (CoW).
- **Sub-agent** = a forked log running in its own brain instance.
- **Cron** = a host scheduler that injects an event into a (optionally resumed) session.

That is the payoff of "the conversation is *not* the state": every advanced runtime capability collapses into operations on an append-only log, instead of each one needing its own bespoke, hard-to-retrofit machinery.

## 17. Risks & mitigations

| Risk                                                                          | Mitigation                                                                                                |
| ----------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| Interface over-/under-engineered (hard to host, weak, or breaks on extension) | Narrow waist: type only what the brain branches on; opaque payloads elsewhere; `#[non_exhaustive]` (§2.4) |
| Traces balloon from per-token deltas                                          | Deltas are transport-only; persist consolidated records + blobs (§4.5)                                    |
| Sans-IO makes the simple case painful                                         | Ship `hugr-host` + `hugr-cli`; "CLI on laptop" ≈ 10 lines                                               |
| Streaming re-entrancy complexity                                              | Op table + cheap append handlers; coalesce host-side                                                      |
| WASM component model immaturity                                               | Start with a simpler custom WASM ABI; migrate when stable                                                 |
| Canonical model type too thin to use providers well                           | First-class cache/reasoning/tool-call fields + opaque `extra` from v1                                     |
| Plugin ABI ossifies too early                                                 | Keep contract narrow; version it; no internal access                                                      |
