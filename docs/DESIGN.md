# Design Document

> **Name:** `Hugr` (published as `hugr-rs`; see `BRANDING.md`). A lightweight, embeddable, runtime-free agent harness written in Rust.

## 1. Vision

Build the agent "brain" that can run **anywhere** — a Chrome extension, a mobile app, a Python or JS script via bindings, a serverless function, a long-lived server — from a *single, portable core* with a small memory footprint and fast startup.

The differentiator is not a feature list. It is an **architecture** that makes "the same agent loop running in your browser tab with no backend, as a small WASM module" a true, demonstrable statement. No existing harness can say that, because they all baked irreversible assumptions into their day-one design.

We keep the simplicity and extensibility that makes Pi pleasant, but remove the runtime weight and the structural traps that the current generation of harnesses (Claude Code, Codex/`codex-rs`, Pi-style tools, the various clones, Hermes-style agents) cannot escape without a full redesign.

## 2. Goals

- **Run anywhere.** Browser/extension (WASM), mobile, native CLI, server, embedded in Python/JS via bindings. One core, many hosts.
- **Tiny footprint, fast start.** No async runtime in the core. Cold start measured in single-digit milliseconds; binary/WASM module measured in low MBs.
- **Easy to bind.** Binding the core to a new language/environment should be a small "poll / submit event" loop, not a fight against an embedded runtime.
- **Easy to extend, Pi-style.** Third parties add tools and behavior without recompiling the core, without reaching into its internals.
- **Deterministic and replayable.** Any session can be recorded and replayed bit-for-bit for testing, debugging, and resume-after-crash.
- **First-class concurrency & streaming.** The core handles multiple in-flight operations and bidirectional streaming (LLM deltas *and* shell logs *and* user input, in parallel) as its default mode — not as a bolted-on special case.

## 3. Non-Goals (for now)

- Being a batteries-included product on day one. The first deliverable is a **thin CLI host** that *showcases* the core, not a polished end-user app.
- A universal lowest-common-denominator model abstraction that hides provider-specific features. We support provider specifics as first-class optional fields, not by erasing them.
- Distributed/multi-machine orchestration. We design so it's *possible* later, but it is not a v1 concern.
- A plugin marketplace, GUI, or hosted service.

## 4. The core thesis: separate state, context, IO, and policy

Most harness pain traces back to **conflating four things that should be separate**. The whole design is organized around keeping them apart.

| Concern           | The trap (what harnesses do)                      | What we do                                                 |
| ----------------- | ------------------------------------------------- | ---------------------------------------------------------- |
| **Durable state** | The flat `messages[]` list *is* the state         | Append-only **event log** is the source of truth           |
| **Model context** | Same `messages[]` is sent to the model            | Context is a **projection** rendered from the log per turn |
| **IO**            | The loop owns tokio, sockets, fs, shell           | **Sans-IO** core emits commands; the **host** does IO      |
| **Permissions**   | `if dangerous { prompt() }` scattered in the loop | Policy is **externalized data**, decided outside the core  |

These four separations are the entire architecture. Everything else (resume, replay, multi-front-end, multi-provider, sub-agents, parallel streaming) falls out of them rather than being engineered separately.

## 5. Pain points in current harnesses (and our response)

These are the **one-way doors** — decisions cheap to make on day one and ruinously expensive to reverse once users and plugins depend on them.

### 5.1 "The conversation is the state"

**Pain.** The session is an append-only list of `messages` that is simultaneously the durable record, the thing sent to the model, and the thing the UI renders. Consequences:

- Compaction/summarization is destructive surgery on the only source of truth — irreversible, silently lossy.
- Branching, rewind, edit-and-resume, sub-agent context sharing are near impossible to add later.
- Large tool outputs live in context forever because there's nowhere else to put them.

**Response.** Event-sourced state. The durable thing is an append-only log of events ("user said X", "tool op 8 returned Y", "model op 7 produced Z"). What we send to the model is a *projection* rendered from that log through a policy that decides what is included, summarized, evicted, or replaced with a reference. Compaction-without-loss, rewind, branching, and replay then come for free.

### 5.2 IO baked into the loop

**Pain.** A tokio-driven loop assuming a local filesystem and shell cannot ship to a browser or bind cleanly into another runtime (asyncio vs tokio is a two-event-loop fight).

**Response.** **Sans-IO** core (the `rustls`/`quinn`/`h2` pattern). The core is a pure state machine: events in, commands out, no sockets, no tokio, no fs. The host performs all IO. The *same* core compiles to WASM, links into a Python/JS binding, or runs on a server — only the host differs.

### 5.3 Privileged built-in tools

**Pain.** `Bash`/`Read`/`Write` are hardwired into the loop while "extensions" (MCP, plugins) go through a separate, lesser path. Now you can't run where there is no shell, can't sandbox uniformly, and built-ins/plugins diverge forever.

**Response.** **There are no privileged tools.** The core only knows "invoke capability X with these args." The host supplies *all* implementations, including filesystem and shell, through one uniform capability interface. A browser host simply doesn't register `shell`.

### 5.4 Permissions as control flow

**Pain.** Approval logic is imperative `if` statements scattered through the loop, hardcoding human-in-the-loop and making headless/CI/autonomous/mobile modes a special-cased mess.

**Response.** The core *emits* a permission request as an event; an external, pluggable **policy** (data) decides allow/deny/ask. The same core runs interactively, in CI (allowlist), or autonomously (capability set) with no loop changes.

### 5.5 Resume and replay as afterthoughts

**Pain.** When the process dies (or a phone backgrounds the app, or a server request times out), can you resume mid-turn? Codex and Claude Code both added resume *after the fact*, painfully, because the loop side-effects everywhere. And because the loop isn't pure, sessions can't be replayed deterministically — hence thin, flaky test suites.

**Response.** Both come free from the design. Persist the event log → rehydrate the state machine to resume. Inject *all* nondeterminism (model calls, clock, randomness, IO) at the edge → record the event stream → replay bit-for-bit.

### 5.6 Provider wire-format leakage

**Pain.** Either the core speaks one provider's wire format natively (adding others = surgery), or it over-abstracts into a lowest-common-denominator message type that can't express prompt-cache breakpoints, reasoning/thinking blocks, or streaming tool calls — losing exactly the features that matter for cost/quality.

**Response.** A canonical internal representation rich enough to carry cache markers, reasoning content, and structured tool calls as **first-class optional fields**, with thin adapters at the edge. Cache breakpoints and token budgets are structured metadata on context blocks, never string concatenation.

### 5.7 The synchronous, LLM-centric loop (the "sleep 120" anti-pattern)

**Pain.** The LLM call is the privileged blocking center of the universe; everything else waits behind it or is *polled* (`sleep 120; check; sleep`). Polling is the smell: the harness has no way to be *told* something happened, so it periodically asks. Background tasks, cancellation, and reacting to a process finishing the instant it finishes are all clumsy or impossible.

**Response.** **The LLM is not special.** It is one event source among many (shell stdout, user input, sub-agent results, timers, file watchers). All produce timestamped events into a single inbox. See §6.

### 5.8 UI coupled to the loop

**Pain.** Rendering interleaved with the loop → exactly one front-end forever.

**Response.** The core emits an event/command stream; any front-end (TUI, browser, mobile, headless) subscribes. Rendering is never inside the core.

### 5.9 Over-wide plugin contracts

**Pain.** If plugins can mutate arbitrary internal state, the core can never evolve without breaking the ecosystem (versioning hell).

**Response.** A **narrow** plugin contract: plugins are pure-ish reactions over the event stream plus capability requests — never deep mutation. Narrow now, widen later; never the reverse.

### 5.10 Cost/usage accounting and silent truncation

**Pain.** Per-turn / per-tool / per-sub-agent attribution is nearly impossible to retrofit. Eviction that drops data without a reference reads as "we covered everything" when we didn't.

**Response.** Usage is an event. Evicted content is *referenced, not deleted* — rehydratable on demand from the log. Any bounded coverage is logged, never silent.

## 6. Concurrency & streaming model

This is the most differentiating part. Current harnesses are structurally incapable of clean parallel streaming because the LLM call is a blocking center.

### 6.1 The reframe: the LLM is just another stream

Shell stdout, user keystrokes, a sub-agent finishing, a timer, a file-watcher, and LLM token deltas all produce the **same kind of thing**: timestamped events dropped into the brain's inbox. The LLM gets no special status.

### 6.2 Concurrency lives in the host; the brain stays single-threaded

- The **host** runs N things concurrently (LLM stream, shell process, user input) with real async/OS parallelism. As each produces a chunk, the host pushes it as an event into the brain's **inbox** (one ordered queue), stamped with a sequence number.
- The **brain** processes the inbox **one event at a time, atomically**. It is a reducer: `(state, event) -> (state', commands)`. No locks, no interleaving — "atomic events" satisfied for free by single-threaded processing.

Result: true parallel I/O *and* a deterministic, replayable, easily-bindable brain. Replay works because we recorded the *actual* interleaving (sequence numbers); the merge itself need not be deterministic, only recorded.

> **Hard rule: the brain is never multithreaded.** The moment it is, we lose sans-IO, replay, and easy bindings. All concurrency belongs to the host.

### 6.3 Multiple in-flight operations, correlated by ID

The one real upgrade over a naive request/response loop: the brain supports **many concurrent operations**, correlated by operation ID.

- Commands carry an op ID: `StartModelCall(op=7, …)`, `StartProcess(op=8, …)`.
- Events reference it: `ModelDelta(op=7, …)`, `ProcessStdout(op=8, …)`, `ProcessExited(op=8, code=0)`.
- The brain holds a table of in-flight ops in its state and reacts to deltas from any of them, in any arrival order.

This enables what polling-based harnesses cannot:

- Run a long `cargo build` (op 8) **and** stream a model response (op 7) concurrently, interleaving both.
- React to `ProcessExited(op=8)` the instant it happens — no `sleep`, because the host *told* the brain via an event.
- **First-class cancellation:** the brain emits `Cancel(op=7)`; the host aborts that HTTP request / kills that process. This is the thing polling harnesses can't do cleanly, and here it's free.

### 6.4 Honest caveats

1. **Provider APIs are mostly half-duplex with the model.** You can stream tokens *out* of a normal completion, but cannot inject input *into* a generation already in flight (except realtime/voice-style APIs). So "bidirectional with the LLM mid-generation" is limited; design around **cancel + re-issue**, not whispering into a running generation. "Bidirectional at the *harness* level" (concurrent ops, background streams feeding state, cancel-and-restart) is fully feasible today and is where the value is.
2. **Backpressure / coalescing.** Token deltas and chatty shell output can flood the inbox. Event handlers must be cheap (append to a buffer, not real work). Decide whether the brain sees every token delta or the host **coalesces** (e.g. batch every ~16ms). Coalescing is usually right — but the host must record what it actually fed, so replay matches.
3. **Partial/aborted operations in the log.** When an LLM stream is cancelled at token 50, the log needs a clean representation ("op 7 produced 50 tokens then cancelled"), or compaction/replay gets confused. Easy by design, annoying to retrofit.

## 6.5 Traces, sub-agents, forks, and scheduling — one mechanism

A deliberate payoff of "the conversation is *not* the state": four capabilities that other harnesses build as separate, hard-to-retrofit subsystems collapse into operations on the append-only event log. See `ARCHITECTURE.md` §§12–16 for the concrete mechanics.

- **Saving a trace** = the event log made durable. We persist the ordered event stream (not derived state), with large payloads referenced as content-addressed blobs. Traces are portable (record on a server, replay in a browser), and are the same substrate used for replay, debugging, test fixtures, and resume. Saving is a *host capability* (disk, IndexedDB, HTTP, a Hub repo) — the core never decides where a trace goes.
- **Sub-agents ("agent subprocess")** = *another `hugr-core` instance*. Spawning one is a `StartAgent` op that behaves like any other in-flight op (stream, observe, cancel, attribute cost). The host chooses isolation — in-process, worktree, or subprocess/remote — over the *same* brain↔host contract, so sub-agents reuse the entire run-anywhere story.
- **Forks** = copying a log prefix (copy-on-write). This single primitive powers sub-agent context sharing, branching/"what-if", rewind/edit-resume, and speculative execution. Results flow back as *values*, not log merges — forks diverge, results return one-directionally (no CRDT pain).
- **Cron / scheduling** = a *host-side* scheduler (the core has no clock; time is injected) that fires a trigger by injecting an event into a session — either resuming an existing trace, targeting a named persistent session, or starting fresh per fire. No special core support beyond resume + event injection, both of which already exist.

The unifying idea: **trace = durable log; resume = re-fold a trace; fork = copy a log prefix; sub-agent = a forked log in its own brain; cron = a scheduler that injects an event.** Every advanced runtime feature is the event log viewed from a different angle.

## 7. Extensibility: the hardest real tension

"Lightweight, no-runtime Rust binary" pulls against "dynamically extensible like Pi." How we resolve it is itself a one-way door. Three plugin mechanisms, each with a real cost:

| Mechanism                                 | Pros                                                | Cons                                                                |
| ----------------------------------------- | --------------------------------------------------- | ------------------------------------------------------------------- |
| **Compile-time** (traits, cargo features) | Zero overhead, smallest binary                      | No third-party plugins without recompiling — not Pi-like            |
| **Subprocess + protocol** (MCP model)     | Fully dynamic, language-agnostic                    | Heavy (process + JSON-RPC each), bad for mobile/browser, slow start |
| **WASM components**                       | Dynamic *and* portable to browser/mobile, sandboxed | Adds a WASM runtime to the binary, FFI/serialization cost           |

**Decision (provisional): WASM components as the primary extension ABI**, with a **narrow event/hook contract**. It is the only option that keeps *lightweight + run-anywhere + dynamically extensible* simultaneously true, and its sandbox aligns with the capability model. We still support subprocess/MCP as a secondary path for heavy, language-agnostic tools where weight is acceptable (server hosts), and compile-time tools for the batteries-included defaults.

The contract stays narrow: plugins react to events and request capabilities; they never touch core internals.

## 8. Over-engineering guardrails

The sans-IO purist failure mode is real. We explicitly guard against it:

- **Ship a batteries-included default host.** "I just want a CLI on my laptop" must be ~10 lines (native + reqwest + local shell/fs). The bare sans-IO core stays available for exotic targets; most users never see it. Sans-IO at the bottom, ergonomic wrapper on top.
- **Don't abstract what no host will vary.** Abstract the clock and RNG (needed for replay — cheap, worth it). Do *not* abstract the allocator or invent config knobs nobody uses.
- **Keep the host trait small.** If the smallest possible host is 500 lines, the "run anywhere" promise is dead. Minimal host = a `poll`/`submit` loop plus a handful of capability impls.

## 9. What earns attention

The architecture alone earns nothing — nobody stars "sans-IO." What earns attention is the **payoff demonstrated concretely**: the same agent brain running in a Chrome extension, a Python script, and a server, as a small WASM module with sub-10ms startup, *with no backend*. Codex going Rust got attention for speed; nobody has shipped a *truly portable* agent core. Lead with the "here it is running in your browser tab" moment.

## 10. Open questions & known blind spots

Smaller open questions (defaults leaning one way, decide during implementation):

- Streaming granularity: per-token vs coalesced chunks as the default the brain sees (leaning coalesced, host-recorded).
- WASM component model maturity vs a simpler custom ABI for v1 plugins.
- How rich the canonical model representation needs to be for v1 (which provider-specific fields are first-class from the start).
- Sub-agent context sharing semantics (fork the log vs reference-shared log).
- Whether/when to add the `hugr-hub` (Hugging Face integration) crate from `BRANDING.md` to the crate layout and roadmap.

**Resolved** (designed; see the referenced architecture sections):

- **Compaction is itself a model call** → projection stays pure/synchronous (reads existing summaries only); compaction is a **separate `small`-tier model op** whose result is appended as a summary `Record` the next projection consumes. Adds a compaction sub-loop. (ARCHITECTURE §3.4)
- **Token counting** → **host tokenizes at ingestion** and stores the count on the record; the brain's projection just **sums** stored counts against the budget (arithmetic, not tokenization). Estimate for projection; authoritative accounting from returned `Usage`. (ARCHITECTURE §3.5)
- **User steering / interruption mid-turn** → conversational `UserInput` can arrive any time; the reducer consults a `SteerMode` (Queue / Interrupt / AppendAndContinue). Two events: `UserInput { content, mode }` and `UserAbort` (pure cancel). **Decided:** default is `Queue` (interrupt is reversible via ESC; an accidental interrupt would throw away in-flight work), with Interrupt always available. (ARCHITECTURE §4.6)
- **Transport vs semantic errors** → transport (429/network/timeout/cache) is the **host's** (retry/backoff internally, surface only final outcome); semantic (malformed tool JSON, schema-invalid args, stale-edit conflict) is the **brain's** (route back into the turn loop). Rule: if retrying *unchanged* might work → host; if the model must *change* something → brain. (ARCHITECTURE §5.4)

**Still open** — none block starting:

- **Trace / log schema migration.** `#[non_exhaustive]` + a `schema_version` in `TraceMeta` are necessary but not sufficient. Long-lived traces need a migration story as `Record`/`Event` evolve, or the resume/replay promise erodes over time.
- **Fan-out concurrency cap.** When the model emits N tool calls at once, the brain emits N `StartCapability` commands; the **host** caps how many run concurrently. Confirm the cap lives host-side and the brain imposes no ordering it doesn't mean to.
- **System prompt / agent configuration.** Where the base system prompt, instructions, and the capability→tool-schema advertisement come from, and how the projection assembles them. Currently implicit in `ContextPolicy`; worth making explicit.
- **Capability sandboxing beyond permission.** Permission decides *whether*; it doesn't *sandbox* execution (filesystem jail, network egress, resource limits). WASM plugins are sandboxed by construction, but native `shell`/`fs` are not. Out of scope for v1, but a "run anywhere safely" gap to track.

## 11. Glossary

- **Brain / core** — the pure, sans-IO state machine.
- **Host** — the environment-specific layer that performs IO and drives the brain.
- **Event** — something that happened, fed into the brain's inbox.
- **Command** — something the brain wants the host to do.
- **Operation (op)** — a unit of in-flight work (a model call, a process), identified by an op ID, correlating commands and events.
- **Event log** — append-only durable source of truth.
- **Projection** — the model context rendered from the log for a given turn.
- **Capability** — a host-provided implementation of a tool/effect.
- **Policy** — externalized data deciding allow/deny/ask for capability requests.
