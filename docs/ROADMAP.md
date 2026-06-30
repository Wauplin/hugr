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

- Multiple concurrent ops in the op table; host runs one task per op.
- Parallel: stream a model response **while** a background `shell` op streams stdout. Interleaved events, atomic reduction.
- **First-class cancellation** (`Cancel` → `OpCancelled`), partial-op logging.
- Host-side delta **coalescing** with exact recording for replay.

**Exit criteria.**
- Kick off a long `cargo build` and stream a model response simultaneously; react to `ProcessExited` instantly (no polling/`sleep`).
- Cancel an in-flight model stream cleanly; the log records "N tokens then cancelled"; replay reproduces it.

---

## Phase 3 — Traces: save, replay, inspect

**Goal.** Sessions are first-class artifacts.

- `baton-replay`: persist a **trace** (ordered event stream + log + blob refs).
- Trace file format (versioned, portable, shareable).
- `baton replay <trace>` reconstructs commands bit-for-bit; an inspector to step through a session.
- Blob store capability (disk for native) with content-addressed payloads.

**Exit criteria.**
- Record a real Phase 1/2 session, replay it bit-for-bit on another machine.
- Hand a trace file to a teammate; they reproduce the exact run offline.

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
