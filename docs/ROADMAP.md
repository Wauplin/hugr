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
- **Deterministic replay:** feeding the same event stream twice yields identical commands. No tokio, no reqwest, no fs anywhere in `baton-core`.

---

## Phase 1 — Batteries-included CLI host (the showcase)

**Goal.** A real, usable terminal agent driven by the Phase 0 core.

- `baton-host`: tokio driver loop (`poll` / `next_event` / `submit`).
- One provider adapter (OpenAI chat completion) in `baton-providers`, streaming model deltas.
- Capabilities: `shell`, `fs read/write`, `http` — all via the uniform `Capability` interface (no privileged built-ins).
- Interactive `Policy` (prompts the user) + a `-y/--yes` style allow mode.
- Minimal TUI/stdout front-end consuming `OutputEvent`s.

**Exit criteria.**
- Run a genuine multi-turn coding session in the terminal end-to-end.
- "CLI on a laptop" host setup is ≈ 10 lines on top of `baton-host`.

---

## Phase 2 — Concurrency & streaming (the differentiator)

**Goal.** Multiple in-flight operations; LLM is "just another stream."

- ✅ **P2-1 — Multiple concurrent ops.** The op table holds many simultaneously in-flight ops keyed by `OpId`; the host runs one task per op. A **background** capability (policy-designated, `TurnPolicy::is_background`) does not block the turn, so a model response streams **while** a background `shell` op runs — interleaved events, atomic per-event reduction, deterministic replay. `ProcessExited` is reacted to instantly (event-driven; no polling/`sleep`). Core stays sans-IO/single-threaded; no new `Command`/`Event` variants (background-ness is a brain-side scheduling decision invisible to the host).
- ✅ **P2-2 — First-class cancellation.** A `Cancel` (driven by `UserAbort`/ESC, or a steer-interrupt) aborts the op's host task; the brain records the partial work as a `Cancelled { partial }` outcome ("N tokens then cancelled", model `text_so_far` preserved) and, on a plain abort once the last op drains, emits the terminal `Done { Cancelled }`. A stale `OpCancelled` racing the op's real terminal event is idempotent (a no-op), so replay stays exact. Both model-stream ops and background capability ops cancel cleanly through the real engine (no leaked work). The host gained a cloneable `EventSender` (`Engine::event_sender()`) so a Ctrl-C / signal handler can inject `UserAbort` mid-turn. No new `Command`/`Event`/`Record` variants — the cancellation contract was already in core.
- ✅ **P2-3 — Delta coalescing with exact recording.** The host batches the *render* of consecutive streamed text (a `Coalescer` between `Command::Emit` and the `Frontend`), cutting per-token flush churn, while recording exactly **one** consolidated `Record` per message. Coalescing is render-only: the engine still submits *every* `ModelDelta` to the brain (so `text_so_far` stays complete and a cancelled op's partial loses no tokens), and deltas never hit the durable log — so replay stays bit-for-bit identical regardless of how the stream was chunked. Entirely host-side; `baton-core` is untouched.

**Exit criteria.**
- ✅ Kick off a long `cargo build` and stream a model response simultaneously; react to `ProcessExited` instantly (no polling/`sleep`). Covered by `baton-core/tests/concurrent_ops.rs` (scripted interleave + deterministic replay) and `baton-host/tests/end_to_end.rs::model_stream_runs_while_background_op_is_in_flight` (real engine, proven overlap).
- ✅ Cancel an in-flight model stream cleanly; the log records "N tokens then cancelled"; replay reproduces it. Covered by `baton-core/tests/cancellation.rs` (scripted command sequence + deterministic replay + the stale-cancel race) and `baton-host/tests/end_to_end.rs::cancel_in_flight_model_stream_preserves_partial` / `::cancel_in_flight_background_op_cleanly` (real engine aborts the task, partial preserved, no leaked work).
- ✅ Delta coalescing records exactly one consolidated `Record` per message and replays bit-for-bit regardless of batching. Covered by `baton-host` `coalesce` unit tests (chunking-invariant rendered text, ordering flushes) and `baton-host/tests/end_to_end.rs::delta_coalescing_keeps_recording_exact` (per-char vs chunked vs single-delta streams yield identical logical records and a single `ModelOutput`, while the per-char stream coalesces to one render).

**Phase 2 is complete.**

---

## Phase 3 — Traces: save, replay, inspect

**Goal.** Sessions are first-class artifacts.

- ✅ **P3-1 — `baton-replay` crate + trace format.** New host-side persistence crate owning the versioned, portable **trace** container: `Trace { meta, events, log, blobs }` — an integer `format_version` (rejects unknown future versions), the ordered host→brain `Event` stream (the replay *input*), the consolidated seq-stamped `LogEntry` log (the *truth*; `BrainState` is never stored, always rederivable by folding the log), and a `BlobManifest` of content-addressed `BlobRef`s (structure in place for the P3-2 blob store; bytes referenced, not inlined). `Trace::save`/`load` are the **only** fs touch in the trace story; `baton-core` stays sans-IO (`baton-replay` uses it as pure data only). Round-trips a Phase 1/2 session to disk and back, byte-for-byte equal.
- Trace file format (versioned, portable, shareable). ✅ (plain JSON; `FORMAT_VERSION`; forward-compatible).
- `baton replay <trace>` reconstructs commands bit-for-bit; an inspector to step through a session.
- ✅ **P3-2 — Blob store capability.** A content-addressed, disk-backed `BlobStore` (SHA-256 keys, `"sha256:<hex>"`) lives in `baton-replay` and produces `BlobRef`s compatible with the trace's `BlobManifest`. `baton-host` wraps it in an ordinary `blob` `Capability` (not a privileged built-in — registered like `shell`/`fs`/`http`, args/results opaque `Value`). Same content dedupes to one file; a large tool result is offloaded by digest and rehydrated on load. `baton-core` stays sans-IO (the new `sha2` dep is host-side only).
- Update CLI to resume from a trace

**Exit criteria.**
- Record a real Phase 1/2 session, replay it bit-for-bit. (P3-1 lands the trace format + round-trip; bit-for-bit *replay* through the brain is P3-3.)

---

## Phase 4 — Portability (the attention moment)

**Goal.** Same brain, many environments.

- `baton-wasm`: compile `baton-core` to WASM; browser host with `fetch`-based model adapter and DOM front-end. **No backend.**
- `baton-py`: PyO3 bindings exposing `poll`/`submit`; a Python host script.
- Size/start-up validation against Architecture §11 targets.

**Exit criteria.**
- The *same* agent brain demonstrably running in (a) a Chrome extension / browser tab with no server, (b) a Python script, (c) the native CLI.
- WASM module within size target; cold start within target.

---

## Phase 5 — Extensibility (Pi-like, runtime-free)

**Goal.** Third parties add tools/behavior without recompiling the core.

- `baton-plugin-abi`: WASM component world (`describe` / `invoke` / `on_event`), narrow hook contract, sandboxed.
- Plugins surface as `Capability`s through the registry; host loads them.
- Secondary subprocess/MCP adapter path (server hosts only).

**Exit criteria.**
- A third-party plugin (separate repo, no core recompile) adds a working tool the agent can call.
- Plugin cannot touch core internals; contract is versioned and documented.

---

## Phase 6 — Sub-agents & forks

**Goal.** Cheap, portable sub-agents built on log forking.

- `StartAgent` op kind: a child is just another `baton-core` instance.
- **Forking:** copy a log prefix to seed a child (shared context) or start fresh (isolated). Branch/rewind on the parent uses the same mechanism.
- Aggregation: child results return to the parent as op results; usage/cost attributed per agent.
- Isolation options (in-process task vs subprocess vs worktree) chosen by host.

**Exit criteria.**
- A parent agent fans out to N child agents (fork-shared context), collects results, and the whole tree replays deterministically from one trace.

---

## Phase 7 — Durable resume & scheduling (cron)

**Goal.** Survive crashes; fire on a schedule.

- Resume-after-crash: load the persisted log, replay the fold, re-issue or cancel ops that were in-flight at crash time (recorded policy choice).
- Host-side scheduler firing triggers into: (a) a resumed existing session, (b) a named persistent session, (c) a fresh session per fire.
- Checkpoint cadence + compaction-of-log policy.

**Exit criteria.**
- Kill the process mid-turn; resume and continue correctly from the trace.
- A scheduled trigger fires a prompt into a session on a cron cadence.

---

## Phase 8 — Multi-provider, accounting & hardening

**Goal.** Production-readiness breadth.

- Additional provider adapters (OpenAI, others) with cache/reasoning/tool-call fidelity preserved.
- Usage/cost accounting as events; per-op, per-sub-agent attribution.
- `baton-js` (Node/Deno) bindings.
- Docs, examples, conformance tests for hosts and plugins.

**Exit criteria.**
- Swap providers without touching the core; cost reports are accurate per sub-agent; a non-Rust host (Python/JS) drives a full session.

---

## Cross-cutting tracks (run throughout)

- **Conformance suite:** scripted scenarios that any host/binding must pass (deterministic command sequences).
- **Benchmarks:** WASM size, cold start, per-event reduce latency, idle memory — tracked from Phase 0 so regressions are caught early.
- **Trace corpus:** accumulate real recorded sessions as regression fixtures.
