# Roadmap

> Companion to `DESIGN.md` and `ARCHITECTURE.md`. Phased plan with explicit exit criteria. Each phase is shippable and de-risks the next. The ordering is deliberate: prove the *pure core* first, then the *showcase*, then the *differentiators* (concurrency, portability), then *extensibility* and *advanced runtime* (sub-agents, forks, scheduling).

## Guiding principles for sequencing

1. **Prove the hard invariant first.** The sans-IO + deterministic-replay core is the foundation; if it isn't clean, nothing else matters. Build it before any IO exists.
2. **Always have a runnable showcase.** From Phase 1 on, there is a real CLI you can use, so the project is never just theory.
3. **Lead the public story with the portability demo** (Phase 4) — that's the attention moment.
4. **Defer extensibility and advanced runtime** until the contract is stable, so plugins don't ossify a half-baked ABI.

---

## Phase 0 — Pure core skeleton (no IO)

**Goal.** The brain exists as a pure state machine with zero IO.

- `Command` / `Event` enums, `OpId`, the reducer `(state, event) -> (state', [command])`.
- Append-only event log + `BrainState` with the in-flight op table.
- Context projection trait (trivial pass-through implementation for now).
- A scripted test harness that feeds canned events and asserts emitted commands.

**Exit criteria.**
- A scripted "user → model call → tool call → model call → done" session reduces to the expected command sequence.
- **Deterministic replay:** feeding the same event stream twice yields identical commands. No tokio, no reqwest, no fs anywhere in `hugr-core`.

---

## Phase 1 — Batteries-included CLI host (the showcase)

**Goal.** A real, usable terminal agent driven by the Phase 0 core.

- `hugr-host`: tokio driver loop (`poll` / `next_event` / `submit`).
- One provider adapter (OpenAI chat completion) in `hugr-providers`, streaming model deltas.
- Capabilities: `shell`, `fs read/write`, `http` — all via the uniform `Capability` interface (no privileged built-ins).
- Interactive `Policy` (prompts the user) + a `-y/--yes` style allow mode.
- Minimal TUI/stdout front-end consuming `OutputEvent`s.

**Exit criteria.**
- Run a genuine multi-turn coding session in the terminal end-to-end.
- "CLI on a laptop" host setup is ≈ 10 lines on top of `hugr-host`.

---

## Phase 2 — Concurrency & streaming (the differentiator)

**Goal.** Multiple in-flight operations; LLM is "just another stream."

- ✅ **P2-1 — Multiple concurrent ops.** The op table holds many simultaneously in-flight ops keyed by `OpId`; the host runs one task per op. A **background** capability (policy-designated, `TurnPolicy::is_background`) does not block the turn, so a model response streams **while** a background `shell` op runs — interleaved events, atomic per-event reduction, deterministic replay. `ProcessExited` is reacted to instantly (event-driven; no polling/`sleep`). Core stays sans-IO/single-threaded; no new `Command`/`Event` variants (background-ness is a brain-side scheduling decision invisible to the host).
- ✅ **P2-2 — First-class cancellation.** A `Cancel` (driven by `UserAbort`/ESC, or a steer-interrupt) aborts the op's host task; the brain records the partial work as a `Cancelled { partial }` outcome ("N tokens then cancelled", model `text_so_far` preserved) and, on a plain abort once the last op drains, emits the terminal `Done { Cancelled }`. A stale `OpCancelled` racing the op's real terminal event is idempotent (a no-op), so replay stays exact. Both model-stream ops and background capability ops cancel cleanly through the real engine (no leaked work). The host gained a cloneable `EventSender` (`Engine::event_sender()`) so a Ctrl-C / signal handler can inject `UserAbort` mid-turn. No new `Command`/`Event`/`Record` variants — the cancellation contract was already in core.
- ✅ **P2-3 — Delta coalescing with exact recording.** The host batches the *render* of consecutive streamed text (a `Coalescer` between `Command::Emit` and the `Frontend`), cutting per-token flush churn, while recording exactly **one** consolidated `Record` per message. Coalescing is render-only: the engine still submits *every* `ModelDelta` to the brain (so `text_so_far` stays complete and a cancelled op's partial loses no tokens), and deltas never hit the durable log — so replay stays bit-for-bit identical regardless of how the stream was chunked. Entirely host-side; `hugr-core` is untouched.

**Exit criteria.**
- ✅ Kick off a long `cargo build` and stream a model response simultaneously; react to `ProcessExited` instantly (no polling/`sleep`). Covered by `hugr-core/tests/concurrent_ops.rs` (scripted interleave + deterministic replay) and `hugr-host/tests/end_to_end.rs::model_stream_runs_while_background_op_is_in_flight` (real engine, proven overlap).
- ✅ Cancel an in-flight model stream cleanly; the log records "N tokens then cancelled"; replay reproduces it. Covered by `hugr-core/tests/cancellation.rs` (scripted command sequence + deterministic replay + the stale-cancel race) and `hugr-host/tests/end_to_end.rs::cancel_in_flight_model_stream_preserves_partial` / `::cancel_in_flight_background_op_cleanly` (real engine aborts the task, partial preserved, no leaked work).
- ✅ Delta coalescing records exactly one consolidated `Record` per message and replays bit-for-bit regardless of batching. Covered by `hugr-host` `coalesce` unit tests (chunking-invariant rendered text, ordering flushes) and `hugr-host/tests/end_to_end.rs::delta_coalescing_keeps_recording_exact` (per-char vs chunked vs single-delta streams yield identical logical records and a single `ModelOutput`, while the per-char stream coalesces to one render).

**Phase 2 is complete.**

---

## Phase 3 — Traces: save, replay, inspect

**Goal.** Sessions are first-class artifacts.

- ✅ **P3-1 — `hugr-replay` crate + trace format.** New host-side persistence crate owning the versioned, portable **trace** container: `Trace { meta, events, log, commands, blobs }` (`commands` added post-Phase-3 with a serde default, so old traces still load) — an integer `format_version` (rejects unknown future versions), the ordered host→brain `Event` stream (the replay *input*), the consolidated seq-stamped `LogEntry` log (the *truth*; `BrainState` is never stored, always rederivable by folding the log), and a `BlobManifest` of content-addressed `BlobRef`s (structure in place for the P3-2 blob store; bytes referenced, not inlined). `Trace::save`/`load` are the **only** fs touch in the trace story; `hugr-core` stays sans-IO (`hugr-replay` uses it as pure data only). Round-trips a Phase 1/2 session to disk and back, byte-for-byte equal.
- Trace file format (versioned, portable, shareable). ✅ (plain JSON; `FORMAT_VERSION`; forward-compatible).
- ✅ **P3-3 — `hugr replay <trace>` + inspector.** Replay re-feeds a trace's recorded `Event` stream into a *fresh* brain and reconstructs every `Command` it emitted bit-for-bit (`hugr_replay::replay`/`verify`; `verify` asserts the reconstructed log **and** the reconstructed command sequence equal the recorded ones, bit-for-bit — the exit criterion; a pre-commands trace falls back to log-only comparison). An `Inspector` steps through the session one event at a time (the commands + log tail each event produced). The CLI gains `hugr --record <path>` (capture the ordered event stream + log to a trace; the engine has an opt-in `Recorder` and serializes its `StaticPolicy` so replay reproduces the brain's permission/background branching) and `hugr replay <trace> [--step]`. `hugr-core` is untouched.
- ✅ **P3-2 — Blob store capability.** A content-addressed, disk-backed `BlobStore` (SHA-256 keys, `"sha256:<hex>"`) lives in `hugr-replay` and produces `BlobRef`s compatible with the trace's `BlobManifest`. `hugr-host` wraps it in an ordinary `blob` `Capability` (not a privileged built-in — registered like `shell`/`fs`/`http`, args/results opaque `Value`). Same content dedupes to one file; a large tool result is offloaded by digest and rehydrated on load. `hugr-core` stays sans-IO (the new `sha2` dep is host-side only).
- ✅ **P3-4 — CLI resume from a trace.** Resume = replay-as-a-starting-point: `EngineBuilder::resume(trace)` rebuilds the brain by re-feeding the saved trace's events into a fresh brain (with **zero IO** — recorded model/shell/http work is not re-run, only re-folded to reconstruct `BrainState`) and restores the trace's policy (`hugr_replay::policy_from_trace`, now public), then keeps recording (pre-loading the `Recorder` with the trace's events) so the continued session re-saves the full history (old + new) and still replays bit-for-bit. The CLI gains `hugr resume <trace> [prompt...]` (continue with a new one-shot turn or interactively; writes back to `<trace>` by default, or `--record <path>` to leave the original untouched; `-y`/`-m` mirror the live flags). `hugr-core` is untouched.

**Exit criteria.**
- ✅ Record a real Phase 1/2 session, replay it bit-for-bit. Covered by `hugr-host/tests/end_to_end.rs::record_then_replay_reconstructs_the_session_bit_for_bit` (record a shell-tool session through the engine → save → reload → replay through a fresh brain → reconstructed command sequence + log byte-identical to the recording; the inspector reassembles the same log step by step).
- ✅ Resume a saved session and add a new turn. Covered by `hugr-host/tests/end_to_end.rs::resume_from_trace_continues_the_session` (record a session → save → resume into a fresh engine that reconstructs the original log with no IO → add a new user turn → the grown log contains the original records as a prefix plus the new turn's → re-save yields a trace that still `verify()`s bit-for-bit, policy preserved).

**Phase 3 complete.**

---

## Phase 4 — Portability (the attention moment) 🔶 Chrome binding done; Python deferred

> Phases 5 and 6 were built first (by request). The **Chrome-extension leg** of this phase is now done: `hugr-wasm` + an installable MV3 side-panel agent. The `hugr-py` (PyO3) leg is still deferred, and the Phase 5 `WasmPlugin` *plugin* transport remains a stub (its wasmtime backend is a separate future item). Nothing in Phases 5/6 blocked Phase 4 — both stayed host-side and left `hugr-core` sans-IO.

**Goal.** Same brain, many environments.

- ✅ `hugr-wasm`: compile `hugr-core` to WASM (`wasm-bindgen`); a **Chrome extension** host with a `fetch`-based streaming model adapter, a DOM front-end, and tab/page capabilities (read + navigate, no click/form-submit). **No backend** — an MV3 page fetches the model endpoint cross-origin directly. The binding is JSON-in/JSON-out over `submit`/`poll` (every `Event`/`Command` is already `serde`, so zero marshalling). See `crates/hugr-wasm/` and its `extension/`. The brain is byte-for-byte the same reducer as the CLI; only the host differs.
- `hugr-py`: PyO3 bindings exposing `poll`/`submit`; a Python host script. **(deferred)** A narrower product-level `hugr-docs` binding now exists for one-question docs retrieval, but it does not replace the general brain binding.
- Size/start-up validation against Architecture §11 targets. The WASM module is **236 KB** (well under the < 2 MB target); formal cold-start benchmarking is **deferred**.

**Exit criteria.**
- 🔶 The *same* agent brain demonstrably running in (a) a Chrome extension / browser tab with no server ✅, (b) a Python script ⏳ (deferred), (c) the native CLI ✅.
- 🔶 WASM module within size target ✅ (236 KB); cold start within target ⏳ (not yet benchmarked).

---

## Phase 5 — Extensibility (Pi-like, runtime-free) ✅

**Goal.** Third parties add tools/behavior without recompiling the core.

- ✅ `hugr-plugin-abi`: the versioned, narrow plugin contract (`describe` / `invoke` / `on_event`, an integer `PROTOCOL_VERSION`, opaque `Value` payloads). Transport-agnostic behind a single `PluginTransport` trait. Implemented transport: **subprocess/stdio** (`SubprocessPlugin`) — a plugin is an external program exchanging JSON lines; language-agnostic, process-sandboxed, no core recompile. The **WASM component world** is scaffolded behind the `wasm` feature (`WasmPlugin` stub implementing the same trait) — its wasmtime backend lands with Phase 4. (`on_event` is defined in the protocol but reserved; the host does not yet deliver it — "narrow now, widen later", §8.1.)
- ✅ Plugins surface as ordinary `Capability`s through the registry (`hugr_host::plugins::{PluginCapability, load, load_subprocess}`); no privileged built-ins, no privileged plugins. Streamed chunks bridge to the brain as `CapabilityChunk`s. The CLI gains `--plugin <CMD>` to load one live.
- ✅ Secondary subprocess/MCP adapter path — this *is* the implemented path for now (the WASM path is the deferred primary; §8.2).

**Exit criteria.**
- ✅ A third-party plugin (separate repo, no core recompile) adds a working tool the agent can call. Covered by `hugr-example-plugin` — a standalone binary depending on **nothing** from Hugr (only `serde_json`) — and `hugr-example-plugin/tests/e2e.rs`: the real plugin process is loaded over the subprocess transport, its `uppercase` tool is called end-to-end through the real engine, and its result folds back into the turn loop.
- ✅ Plugin cannot touch core internals; contract is versioned and documented. The plugin only ever answers protocol messages (it links no Hugr crate); `PROTOCOL_VERSION` is checked on load (a newer version is rejected); the wire shape is pinned by `protocol` unit tests and documented in `hugr-plugin-abi`.

**Phase 5 is complete** (subprocess transport; WASM transport scaffolded for Phase 4).

---

## Phase 6 — Sub-agents & forks ✅

**Goal.** Cheap, portable sub-agents built on log forking.

- ✅ `Command::StartAgent { op, agent, config, seed }`: a child is just another `hugr-core` instance. A policy-designated tool (`TurnPolicy::agent_seed`, like `is_background`) makes the brain emit `StartAgent` instead of `StartCapability`; the child's `AgentDone`/`AgentError` result folds back into the turn loop exactly like a tool result (§13.1). No hardcoding in the reducer — spawning is a *strategy* decision in the policy.
- ✅ **Forking:** `AgentSeed` (`Fresh` / `ForkAt { seq }` / `ForkFull`) copies a log prefix to seed the child (shared context) or starts fresh (isolated). Resolving the seed is a pure operation on the brain's log; `Brain::from_log` re-derives a child's state by folding the inherited prefix (§14). The same primitive underlies branch/rewind.
- ✅ Aggregation: child results return to the parent as the op's result value (a text digest + aggregated token usage for per-agent attribution, §14.3); forks diverge, results flow back one-directionally.
- ✅ Isolation: the host runs the child **in-process** (a spawned task reusing a subset of the parent's model + capability registries; `hugr_host::agent`). Its ops live in a `JoinSet` so a parent `Cancel` tears down the whole subtree. Subprocess/worktree isolation are future host choices behind the same contract (§13.2). Nested sub-agents (a child spawning grandchildren) work with no special case.

**Exit criteria.**
- ✅ A parent agent fans out to N child agents (fork-shared context), collects results, and the whole tree replays deterministically from one trace. Covered by `hugr-core/tests/sub_agents.rs` (scripted delegate/fan-out + the fan-out join + deterministic replay) and `hugr-host/tests/end_to_end.rs::parent_fans_out_to_sub_agents_and_replays` (through the **real engine**: a parent spawns two child agents that run as their own brains, their digests fold back as `task` tool results, and the recorded parent trace `verify()`s bit-for-bit — the recorded `AgentDone` results drive the fold, §13.3).

**Phase 6 is complete.**

---

## Phase 7 — Durable resume & scheduling (cron) ✅

**Goal.** Survive crashes; fire on a schedule.

- ✅ **Resume-after-crash:** `EngineBuilder::checkpoint(path, cadence)` writes atomic trace checkpoints during the run (including `EveryEvent`, which captures mid-turn in-flight state), and `EngineBuilder::resume(trace)` now reconciles stale in-flight ops with the conservative recorded `CrashResumePolicy::CancelInflight` policy by appending `OpCancelled` events before going live. The choice is replayable because it is recorded in the trace; idempotent re-issue remains a future host policy.
- ✅ **Host-side scheduler:** `hugr_host::Schedule` / `TriggerTarget` / `fire_once` fire prompts into (a) a resumed existing trace, (b) a named persistent session, or (c) a fresh trace. The CLI gains `hugr schedule --cron ... --trace|--session|--fresh ... [prompt...]`, with `--once` for one-shot fires and a loop otherwise.
- ✅ **Checkpoint cadence + compaction policy:** checkpoint cadence is explicit (`OnCommand`, `EveryEvent`, `EveryNEvents`), trace writes are atomic (`Trace::save_atomic`), and the native compaction policy is explicit and lossless (`TraceCompaction::PreserveFull`) so durable traces keep the full event stream + consolidated log as the source of truth.

**Exit criteria.**
- ✅ Kill the process mid-turn; resume and continue correctly from the trace. Covered by `hugr-host/tests/end_to_end.rs::durable_checkpoint_resumes_after_mid_turn_crash`.
- ✅ A scheduled trigger fires a prompt into a session on a cron cadence. Covered by `hugr-host/tests/end_to_end.rs::scheduled_trigger_fires_into_named_persistent_session`.

**Phase 7 is complete.**

---

## Phase 8 — Multi-provider, accounting & hardening

**Goal.** Production-readiness breadth.

- Additional provider adapters (OpenAI, others) with cache/reasoning/tool-call fidelity preserved.
- Usage/cost accounting as events; per-op, per-sub-agent attribution.
- `hugr-js` (Node/Deno) and `hugr-py` bindings.
- Docs, examples, conformance tests for hosts and plugins.

**Exit criteria.**
- Swap providers without touching the core; cost reports are accurate per sub-agent; a non-Rust host (Python/JS) drives a full session.

---

## Cross-cutting tracks (run throughout)

- **Conformance suite:** scripted scenarios that any host/binding must pass (deterministic command sequences).
- **Benchmarks:** WASM size, cold start, per-event reduce latency, idle memory — tracked from Phase 0 so regressions are caught early.
- **Trace corpus:** accumulate real recorded sessions as regression fixtures.
