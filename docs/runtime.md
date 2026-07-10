# Runtime

## Runtime and host

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

The brain never performs IO. It consumes one ordered event stream and produces commands. The host handles everything else. `Agent::ask` assembles an engine from the definition (registries, adapter, prompt), optionally re-folds a parent trace, submits the question, drives the loop until `Done`, folds the trace into an `Answer`, and persists it.

## Why the core is sans-IO

Most harness pain traces back to conflating four things that should be separate:

| Concern           | The trap (what harnesses do)              | What Hugr does                                              |
| ----------------- | ----------------------------------------- | ----------------------------------------------------------- |
| **Durable state** | The flat `messages[]` list *is* the state | Append-only **event log** is the source of truth            |
| **Model context** | Same `messages[]` is sent to the model    | Context is a **projection** rendered from the log per turn  |
| **IO**            | The loop owns tokio, sockets, fs          | **Sans-IO** core emits commands; the **host** does IO       |
| **Permissions**   | `if dangerous { prompt() }` in the loop   | Sandbox is **what the host registers**, decided from config |

These separations provide the following behavior:

- **Trace = the log made durable.** `trace_id` names the saved file.
- **Resume = re-fold a trace.** Resume performs no IO beyond reading the file and makes no model calls, so it is immediate.
- **Fork = copy a log prefix.** Sibling explorations share a prefix and diverge.
- **Sandbox = what the host registers.** "This agent has no shell" is a fact about registration, not a policy hope.
- **Cost = arithmetic over the trace.** Per-op usage/latency lives on the log; answer metadata is a fold.

## Core and host contract

The entire surface between brain and host is two enums plus two methods: `submit(event)` folds an event into state and queues commands, while `poll()` drains them. Both are synchronous and pure, with no `async` or IO. The only `await` in the system is the host's `next_event()`.

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

## The narrow-waist rule

The interface must provide enough structure for the brain without making every extension a breaking change. Apply this rule field by field:

> **Type only what the brain branches on. Everything else is an opaque payload.**

- The brain **branches on**: op lifecycle (start/delta/done/error/cancel), model output structure (text vs tool calls), turn control, permission outcomes → typed and stable. There are few of them and they rarely change.
- The brain **only stores/forwards**: capability args/results, provider knobs, prompts, answers → opaque (`Value`). The brain is a router and bookkeeper for them, never an interpreter.

Consequently, `StartCapability { name, args: Value }` keeps args opaque, so new tools never change the core. Adding a tool, a provider knob, or an agent grant touches **zero** core types. The same rule applies to enums: **an enum nobody branches on should be a string**. Status labels, privilege classes, and selector names are open string sets, not variant lists.

## Reducer responsibilities

The reducer (`brain.rs`) handles the following responsibilities:

1. **Bookkeeping:** maintain the append-only log and the in-flight op table.
2. **The turn loop:** drive `user → model → (tool calls?) → tools → model → … → done`.
3. **Consult the pluggable `TurnPolicy`:** choose a model selector, project context, and determine whether a capability is gated. Strategy lives in the policy, never hardcoded in the reducer. Hosts can pass a custom policy to `EngineBuilder::policy`, record its opaque `{"kind": ...}` config in the trace, and register a pure decoder in `PolicyRegistry` so replay/resume can rebuild it.
4. **Route opaque payloads:** turn a model's tool calls into `StartCapability` ops and feed results back as context.
5. **Manage lifecycle:** decide when a turn is `Done` and when to `Checkpoint`.

It does not perform IO or model calls, run tools, render output, resolve selectors, store data, or schedule work. Given the log and the latest event, the brain determines what should happen next.

## State model

- **Durable state is an append-only log** of `LogEntry { seq, at, record }` values containing user messages, consolidated model outputs, tool results, and op endings. `BrainState` (including the op table) is a fold over the log and can always be rebuilt (`Brain::from_log`). Resume replays the fold, while fork copies a prefix.
- **Model context is a projection, not the log.** Per turn, the policy produces a `ContextPlan` from the log (which blocks are included, truncated, dropped, or omitted, with token estimates), and the reducer renders the `ModelRequest` from it. Projection keeps tool-call transcripts provider-valid (tool results render immediately after their originating assistant tool-call block, and budget/forget compaction drops paired tool-call/result blocks together). The default static policy includes the log, while the built-in `BudgetPolicy` performs deterministic truncate/drop compaction and tool-result forget rules in the projection only; the durable log remains complete and append-only.
- **Model-backed summaries are ordinary recorded model work.** When `BudgetPolicy` is configured with a summary selector and the projection wants a summary, the reducer issues a summarizer `StartModelCall` before the main call. Its `ModelDone` appends `Record::ContextSummary { replaces_up_to, text, est_tokens }` plus the normal `OpEnded`; later projections render that summary block instead of records up to `replaces_up_to`, without deleting the original records. Replay is deterministic because the summary text is just another recorded model result.
- **Large payloads are content-addressed blobs.** Tool outputs and file exchange are stored by SHA-256 through the host-layer `BlobBackend`; the default `FsBlobStore` wraps `hugr-replay::BlobStore`, shards objects under the shared `~/.hugr/blobs/` store (or `HUGR_BLOB_STORE`), and hardlinks filesystem paths when possible. `MemBlobStore` is the in-memory reference backend. The log holds the reference. Identical content dedupes to one object.
- **Token counts come from the host, at ingestion.** The brain cannot tokenize (provider-specific, not sans-IO-friendly); the host annotates records with estimates and the brain's projection just sums them. Authoritative accounting comes from the returned `Usage` per call.

## In-flight operations and concurrency

- **The op table.** `StartModelCall`/`StartCapability` insert into `inflight`; each `*Delta`/`*Chunk` appends to the op's buffer cheaply; `*Done`/`*Error`/`OpCancelled` remove the op and append a final `Record::OpEnded` carrying **`OpMeta`** `{ started_at, ended_at, model, usage, extra }`. Latency and spend are queryable from the trace itself without a side table.
- **Atomicity is automatic.** The brain processes one event at a time. The host provides concurrency by merging many sources into one ordered stream. The brain contains no locks.
- **Foreground vs background** is a policy answer (`is_background(capability)`): a foreground op blocks the turn, while a background op lets the model resume immediately and folds its result in at the next turn boundary. This distinction is invisible to the host.
- **Cancellation is first-class:** `Command::Cancel` → host aborts → `Event::OpCancelled` → the op is removed and its partial output logged explicitly (`OpOutcome::Cancelled { partial }`). Never an implicit gap.
- **Deltas are transport, never durable.** A thousand-token response arrives as many `ModelDelta`s that update the live buffer and are discarded; exactly **one** consolidated `Record::ModelOutput` is appended per model call (same for tool chunks vs one `Record::ToolResult`). This is what keeps traces the size of a normal message history, and what makes replay clean: replay feeds consolidated events only.
- **Backpressure:** handlers stay O(1)-ish by appending to a buffer. Heavy work never happens in the reducer.

## Model provider abstraction

- **Canonical request/response.** `ModelRequest { blocks, tools, params, extra }` with structured `ContextBlock`s; `ModelOutput { text, tool_calls, stop }`. Provider-specific knobs the brain never reads ride the opaque `extra`.
- **A model call is a typed command, not a capability**, because the brain *reasons about model output* (tool calls drive the turn loop) but never about tool output (opaque leaves). At the host level a model adapter is still registered like any capability.
- **`ModelSelector` is a plain string newtype.** The manifest maps free-form tier names to concrete adapters (`[models.<tier>]` → endpoint, model id, pricing); the policy picks a selector; the host registry resolves it. Each model op records its selector in `OpMeta`, so per-tier spend falls out of the trace.
- **Streaming is the only mode.** Adapters stream deltas live via the sink and return the consolidated output; there is no non-streaming path. Transport errors (429s, network blips, timeouts) are retried inside the adapter and never reach the brain; only the final outcome is recorded, so a replayed session doesn't re-suffer transient failures.
- **Transport vs semantic errors.** If retrying the same request unchanged might work, the error is transport-level and the host retries internally. If the model must change something to succeed, such as malformed tool args or a logical tool failure, the error is semantic and returns to the turn loop as a tool result so the model can correct it.

## Determinism, replay, and traces

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

- **The log is the truth, not state.** `BrainState` is never stored, always rederivable.
- **`verify()`** re-folds the events into a fresh brain and asserts the reconstructed log **and** command sequence equal the recorded ones, bit-for-bit. This is the release gate: any new control-flow path ships with a replay test.
- **Policy config is replay input.** Traces carry the host-recorded policy config as opaque JSON; built-in configs use `kind = "static"` or `kind = "budget"`, and custom host policies use their own open string kind plus a registered pure decoder. A trace with an unknown policy kind can still be replayed with an explicitly supplied policy, but faithful automatic replay/resume needs the registry that knows that kind.
- **The `TraceBackend`** holds immutable traces keyed by content-derived `trace_id`, with `depends_on` lineage in the header; `head()` reads metadata without folding events. The default filesystem implementation is `FsTraceStore`/`TraceStore` rooted at `<agent-home>/traces`, using atomic `create_new` reservation so parallel asks are collision-free. `MemTraceStore` is the in-memory reference implementation.
- **The `FeedbackBackend`** is a sidecar store keyed to existing trace ids. The default filesystem implementation appends JSON lines under `<agent-home>/feedback/<trace_id>.jsonl`; `MemFeedbackStore` is the in-memory reference implementation. Feedback is intentionally outside replay/verify.
- **Agent home** resolves the same for dev and built surfaces: `HUGR_AGENT_HOME` as a full override, else `HUGR_HOME/<agent-name>`, else `$HOME/.hugr/<agent-name>`, else a temp-dir fallback. The default scratch root is `<agent-home>/scratch`; the default memory root is `<agent-home>/memory`; the default feedback root is `<agent-home>/feedback`; `[traces].store` and `[scratchpad].root` remain explicit manifest overrides. The default blob store is shared across agents: `HUGR_BLOB_STORE`, else `HUGR_HOME/blobs`, else `$HOME/.hugr/blobs`, else a temp-dir fallback.
- **Storage is pluggable at the host layer.** `hugr-agent` defines `TraceBackend`, `BlobBackend`, `ScratchBackend`, and `FeedbackBackend`; `Agent::new` is the convenience filesystem constructor, while `Agent::with_storage` / `StorageOverrides` accepts custom `Arc<dyn ...>` implementations. A generated agent crate can opt in by exporting `pub fn storage() -> hugr_agent::StorageOverrides`; no core type changes and no manifest enum are needed.
- **Resume after crash** is the same machinery: fold the persisted log, append `OpCancelled` for ops that were in flight, continue live.

## Risks and mitigations

| Risk                                                | Mitigation                                                            |
| --------------------------------------------------- | --------------------------------------------------------------------- |
| Interface over-/under-engineered                    | Narrow waist: type only what the brain branches on              |
| Traces balloon from per-token deltas                | Deltas are transport-only; persist consolidated records + blobs |
| Sans-IO makes the simple case painful               | `hugr run` on an agent crate folder is the ten-second loop            |
| Canonical model type too thin to use providers well | First-class streaming/tool-call fields + opaque `extra`               |
| Feature creep back toward a platform                | One artifact, one escape hatch (MCP), no enum without a branch        |
