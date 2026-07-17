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

| Concern           | The trap (what harnesses do)              | What Huggr does                                              |
| ----------------- | ----------------------------------------- | ----------------------------------------------------------- |
| **Durable state** | The flat `messages[]` list *is* the state | Append-only **event log** is the source of truth            |
| **Model context** | Same `messages[]` is sent to the model    | Context is a **projection** rendered from the log per turn  |
| **IO**            | The loop owns tokio, sockets, fs          | **Sans-IO** core emits commands; the **host** does IO       |
| **Permissions**   | `if dangerous { prompt() }` in the loop   | Sandbox is **what the host registers**, decided from config |

These separations provide the following behavior:

- **Trace = replay input and derived history made durable.** `trace_id` names a file containing the time-stamped input events, emitted commands, and consolidated log.
- **Resume = re-fold a trace.** Resume performs no IO beyond reading the file and makes no model calls, so it is immediate.
- **Fork = copy a log prefix.** Sibling explorations share a prefix and diverge.
- **Sandbox = what the host registers.** "This agent has no shell" is a fact about registration, not a policy hope.
- **Cost = arithmetic over the trace.** Per-op usage/latency lives on the log; answer metadata is a fold.

## Core and host contract

The entire surface between brain and host is two enums plus two methods: `submit(envelope)` folds a time-stamped event into state and queues commands, while `poll()` drains them. Both are synchronous and pure, with no `async` or IO. Awaiting effects and the next event belongs to the host.

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
    UserInput { content: Value, est_tokens: u32 },       // ignored if ops are in flight
    UserAbort,                                           // pure cancel, no new content
    ModelDelta { op: OpId, delta: ModelDelta },          // streaming transport, never durable
    ModelDone  { op: OpId, output: ModelOutput, usage: Usage, est_tokens: u32 },
    ModelError { op: OpId, error: Value },
    CapabilityChunk { op: OpId, chunk: Value },
    CapabilityDone  { op: OpId, result: Value, est_tokens: u32 },
    CapabilityError { op: OpId, error: Value, est_tokens: u32 },
    PermissionDecision { op: OpId, decision: Decision, est_tokens: u32 },
    OpCancelled { op: OpId },
}

/// The brain's input unit: every submitted event carries the host's injected
/// time. The brain has no clock; `at` is stamped onto everything durable.
pub struct Envelope { at: Timestamp, event: Event }
```

The host driver loop is the entire integration surface:

```rust
loop {
    for cmd in brain.poll() { host.dispatch(cmd) }        // spawn model/tool tasks, abort, persist…
    let event = host.next_event().await;                  // merged, ordered
    brain.submit(Envelope::new(host.now(), event));       // pure, instant
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
- **Model context is a projection, not the log.** Per turn, the policy produces a `ContextPlan` from the log. The plan identifies which blocks are included, truncated, dropped, or omitted, with token estimates. The reducer renders the `ModelRequest` from that plan.

  Projection keeps tool-call transcripts provider-valid. Tool results render immediately after their originating assistant tool-call block, and budget or forget compaction drops paired tool-call/result blocks together.

  The default static policy includes the log. The built-in `BudgetPolicy` performs deterministic truncate/drop compaction and tool-result forget rules in the projection only. The durable log remains complete and append-only.
- **Model-backed summaries are ordinary recorded model work.** When `BudgetPolicy` is configured with a summary selector and the projection needs a summary, the reducer issues a summarizer `StartModelCall` before the main call.

  Its `ModelDone` appends `Record::ContextSummary { replaces_up_to, text, est_tokens }` plus the normal `OpEnded`. Later projections render that summary block instead of records up to `replaces_up_to`, without deleting the original records.

  Replay remains deterministic because the summary text is another recorded model result.
- **Large file exchange uses content-addressed blobs.** Explicit inbound and outbound files are stored by SHA-256 through the host-layer `BlobBackend`; ordinary tool results remain opaque values in the log.

  The default `FsBlobStore` wraps `huggr-replay::BlobStore`, shards objects under the shared `~/.huggr/blobs/` store (or `HUGGR_BLOB_STORE`), and copies filesystem inputs into atomically installed objects. Existing and loaded objects are checked against their content address. `MemBlobStore` is the in-memory reference backend.

  The log holds the reference. Identical content deduplicates to one object.
- **Token counts come from the host, at ingestion.** The brain cannot tokenize (provider-specific, not sans-IO-friendly); the host annotates records with estimates and the brain's projection just sums them. Authoritative accounting comes from the returned `Usage` per call.

## In-flight operations and concurrency

- **The op table.** `StartModelCall`/`StartCapability` insert into `inflight`; each model delta appends to the model op's buffer cheaply, while capability chunks are transport-only and ignored by the reducer. `*Done`/`*Error`/`OpCancelled` remove the op and append a final `Record::OpEnded` carrying **`OpMeta`** `{ started_at, ended_at, model, usage, extra }`. Latency and spend are queryable from the trace itself without a side table.
- **Atomicity is automatic.** The brain processes one event at a time. The host provides concurrency by merging many sources into one ordered stream. The brain contains no locks.
- **Foreground vs background** is a policy answer (`is_background(capability)`): a foreground op blocks the turn, while a background op lets the model resume immediately and folds its result in at the next turn boundary. This distinction is invisible to the host.
- **Cancellation is first-class:** `Command::Cancel` → host aborts → `Event::OpCancelled` → the op is removed and its partial output logged explicitly (`OpOutcome::Cancelled { partial }`). Never an implicit gap.
- **User turns start only at idle boundaries.** A host that receives user input while an operation is live must buffer it outside the brain and submit it after the current turn. The reducer ignores mid-turn `UserInput` events so a message cannot be recorded and then stranded behind a terminal answer.
- **Deltas are transport, never durable.** A thousand-token response arrives as many `ModelDelta`s that update the live buffer and are then discarded.

  Exactly **one** consolidated `Record::ModelOutput` is appended per model call. Tool chunks follow the same rule and produce one `Record::ToolResult`.

  This keeps the durable log the size of a normal message history. The trace event stream still records transport deltas because replay verifies the full submitted event sequence.
- **Backpressure:** handlers stay O(1)-ish by appending to a buffer. Heavy work never happens in the reducer.

## Model provider abstraction

- **Canonical request/response.** `ModelRequest { blocks, tools, extra }` with structured `ContextBlock`s; `ModelOutput { text, tool_calls, stop }`. Provider-specific knobs the brain never reads ride the opaque `extra`.
- **A model call is a typed command, not a capability**, because the brain *reasons about model output* (tool calls drive the turn loop) but never about tool output (opaque leaves). At the host level a model adapter is still registered like any capability.
- **`ModelSelector` is a plain string newtype.** The toolkit's host-facing configuration restricts authors to `fast`, `balanced`, `powerful`, and `max`, then resolves each tier to a provider, model id, and pricing before registering adapters. The core still branches on no provider or catalog type. Each model op records its selector in `OpMeta`, so per-tier spend falls out of the trace.
- **Streaming is the only mode.** Adapters stream deltas live via the sink and return the consolidated output; there is no non-streaming path. Transport errors (429s, network blips, timeouts) are retried inside the adapter and never reach the brain; only the final outcome is recorded, so a replayed session doesn't re-suffer transient failures.
- **Transport vs semantic errors.** If retrying the same request unchanged might work, the error is transport-level and the host retries internally. If the model must change something to succeed, such as malformed tool args or a logical tool failure, the error is semantic and returns to the turn loop as a tool result so the model can correct it.

## Determinism, replay, and traces

All nondeterminism is injected: time via the `Envelope` stamp on every submitted event, model output and tool results as events. The brain never reads a clock or RNG. A pure fold over a recorded stream therefore reproduces every command bit-for-bit.

```rust
pub struct Trace {
    meta: TraceMeta,          // trace_id, depends_on, agent name/version, created_at, question, status
    events: Vec<Envelope>,    // the ordered, time-stamped host→brain stream, the replay INPUT
    log: Vec<LogEntry>,       // the consolidated record stream, the truth
    commands: Vec<Command>,   // the drained command sequence
    blobs: BlobManifest,      // refs to content-addressed payloads (not inlined)
    policy: Option<Value>,    // opaque TurnPolicy configuration
}
```

- **The log is the truth, not state.** `BrainState` is never stored, always rederivable.
- **`verify()`** re-folds the events into a fresh brain and asserts the reconstructed log **and** command sequence equal the recorded ones, bit-for-bit. This is the release gate: any new control-flow path ships with a replay test.
- **Policy config is replay input.** Traces carry the host-recorded policy config as opaque JSON. Built-in configs use `kind = "static"` or `kind = "budget"`. Custom host policies use their own open string kind plus a registered pure decoder.

  A trace with an unknown policy kind can still be replayed with an explicitly supplied policy. Faithful automatic replay and resume need a registry that knows the kind.
- **The `TraceBackend`** holds immutable traces keyed by content-derived `trace_id`, with `depends_on` lineage in the header. `head()` reads metadata without folding events.

  The default filesystem implementation is `FsTraceStore`/`TraceStore`, rooted at `<agent-home>/traces`. It uses atomic `create_new` reservation so parallel asks are collision-free. While an ask is live, the host maintains a mutable atomic snapshot under `traces/.checkpoints/`. The snapshot has a stable `trace_id`, appears in trace listings with status `interrupted`, and is removed only after the completed immutable trace and scratch state are durable. `MemTraceStore` is the in-memory reference implementation.
- **The `FeedbackBackend`** is a sidecar store keyed to existing trace ids. The default filesystem implementation appends JSON lines under `<agent-home>/feedback/<trace_id>.jsonl`; `MemFeedbackStore` is the in-memory reference implementation. Feedback is intentionally outside replay/verify.
- **Agent home** resolves the same for development and built surfaces. Resolution uses `HUGGR_AGENT_HOME` as a full override, then `HUGGR_HOME/<agent-name>`, then `$HOME/.huggr/<agent-name>`, and finally a temporary-directory fallback.

  Built artifacts install their embedded definition into a content-addressed `.definitions/<agent>/<hash>/` cache beside the agent homes. The cache is never unpacked over mutable traces, scratch, memory, or feedback state. Manifest-relative state roots resolve under the agent home, while definition resources and tool grants resolve against the cached definition.

  The default scratch root is `<agent-home>/scratch`, the default memory root is `<agent-home>/memory`, and the default feedback root is `<agent-home>/feedback`. `[traces].store` and `[scratchpad].root` remain explicit manifest overrides.

  The default blob store is shared across agents. Resolution uses `HUGGR_BLOB_STORE`, then `HUGGR_HOME/blobs`, then `$HOME/.huggr/blobs`, and finally a temporary-directory fallback.
- **Storage is pluggable at the host layer.** `huggr-agent` defines `TraceBackend`, `BlobBackend`, `ScratchBackend`, and `FeedbackBackend`.

  `Agent::new` is the convenience filesystem constructor. `Agent::with_storage` / `StorageOverrides` accepts custom `Arc<dyn ...>` implementations.

  A generated agent crate can opt in by exporting `pub fn storage() -> huggr_agent::StorageOverrides`. This needs no core type changes or manifest enum.
- **Resume after crash** uses the same machinery. The native host writes a checkpoint after recording each durable event and its derived commands, before it starts the next external effect. Model deltas and capability chunks do not trigger writes because they are transport, not completed steps. To continue, pass the interrupted checkpoint id through the existing `trace_id` input. Resume folds the checkpoint, appends `OpCancelled` for any effect that was in flight when the process stopped, and starts the new user turn without repeating completed model or tool calls. Filesystem scratch state uses the checkpoint id while the run is live, so completed tool writes remain available after restart.

  Automatic live checkpointing currently applies to filesystem-backed native agents, including `huggr run`, built CLI and MCP artifacts, and the native Python runtime. Custom `StorageOverrides` and the TypeScript/browser hosts must provide their own live persistence policy.

## Risks and mitigations

| Risk                                                | Mitigation                                                            |
| --------------------------------------------------- | --------------------------------------------------------------------- |
| Interface over-/under-engineered                    | Narrow waist: type only what the brain branches on              |
| Durable logs balloon from per-token deltas          | Deltas are transport-only in the reducer; persist consolidated records + blobs |
| Sans-IO makes the simple case painful               | `huggr run` on an agent crate folder is the ten-second loop            |
| Canonical model type too thin to use providers well | First-class streaming/tool-call fields + opaque `extra`               |
| Feature creep back toward a platform                | Small artifacts, explicit process grants, no enum without a branch   |
