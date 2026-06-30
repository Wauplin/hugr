# CLAUDE.md

Guidance for working in the Baton repository.

## What this is

Baton is a **runtime-free, sans-IO agent harness** in Rust. Read
`docs/DESIGN.md` and `docs/ARCHITECTURE.md` before making non-trivial changes —
the architecture is the product, and several decisions are deliberate one-way
doors. `docs/ROADMAP.md` tracks the phased plan and per-phase exit criteria.

## The one rule that matters most

**`baton-core` is sans-IO and pure.** It is a reducer: `submit(event)` folds an
event into state and queues commands; `poll()` drains them. It must never do IO.

Hard invariants — do not break these:

- **No environmental dependencies in `baton-core`.** No `tokio`, no `reqwest`,
  no `std::fs`, no sockets, no clock, no RNG, no threads. Only pure-data crates
  (`serde`, `serde_json`). Verify with `cargo tree -p baton-core`.
- **The brain is single-threaded.** All concurrency lives in the host. The
  moment the brain is multithreaded, we lose sans-IO, replay, and easy bindings.
- **All nondeterminism is injected** as events (`Tick` for time; model output,
  tool results, user input as events). The brain never reads a clock or RNG.
  This is what makes replay bit-for-bit deterministic — protect it.
- **The log is the source of truth.** `BrainState` is a *fold* over the log and
  must stay rebuildable from it. Don't add un-derived state.
- **Deltas are transport, never durable.** `ModelDelta`/`CapabilityChunk` drive
  live UI and op buffers but are *never* written to the log. One consolidated
  `Record` is appended per logical message/tool-result.

## The narrow-waist rule (ARCHITECTURE §2.4)

> **Type only what the brain branches on. Everything else is an opaque payload.**

- Typed & stable: op lifecycle (start/delta/done/error/cancel), model *output
  structure* (text vs tool calls vs stop reason), turn control, permission
  outcomes.
- Opaque (`Value`): capability args/results, plugin payloads, provider knobs,
  prompts, answers. The brain stores and forwards these; it never interprets.

Consequences for code changes:

- `#[non_exhaustive]` on **every public enum** so adding a variant isn't
  breaking. Hosts always have a `_ => {}` arm.
- Host-facing **structs** that callers must construct (e.g. `ModelOutput`,
  `ToolCall`, `ToolSchema`, `Usage`) are also `#[non_exhaustive]`, so they need
  **constructors** (`::new`, builders) — external code can't use struct
  literals. Add a constructor when you add such a struct.
- Adding a new tool, provider knob, or plugin must touch **zero** core types.
  If a change forces a core type edit for a new *capability*, reconsider it.

## Where logic goes

- **Agent strategy** (which model, how to project context, whether permission
  is needed) lives in the pluggable `TurnPolicy` — never hardcoded in the
  reducer. The Phase 0 `StaticPolicy` is the trivial pass-through projection.
- **The reducer** (`brain.rs`) only: maintains the log + op table; drives the
  turn loop; asks the policy; routes opaque payloads; emits permission/UI
  events; decides done/checkpoint. If you're adding "smarts", it probably
  belongs in a policy, not the reducer.
- **Everything hard** (IO, HTTP, rendering, scheduling, model resolution, the
  atomic CAS check, storage) is the *host's* job — not in `baton-core`.

## Project layout

```
crates/baton-core/
  src/primitives.rs  # OpId, Seq, Timestamp, Value, ObjectKey
  src/model.rs       # ModelRequest/Delta/Output, ToolCall, Usage, selectors
  src/command.rs     # Command (brain → host) + OutputEvent
  src/event.rs       # Event (host → brain) + SteerMode, Decision, VersionRef
  src/record.rs      # LogEntry, Record, OpOutcome, OpMeta (the durable log)
  src/state.rs       # BrainState + in-flight op table (derived; foldable)
  src/policy.rs      # TurnPolicy trait + StaticPolicy
  src/brain.rs       # Brain: poll() + submit() + the reducer
  tests/             # scripted_session.rs, determinism.rs (+ common/)
```

The other crates in `ARCHITECTURE.md` §10 (`baton-host`, `baton-cli`,
`baton-wasm`, …) don't exist yet — they arrive in later phases. Don't add
environmental dependencies to `baton-core` to make a future host easier; put
them in that host's crate.

## Commands

```bash
cargo test                  # all tests
cargo test -p baton-core    # core only
cargo clippy --all-targets  # lint (keep it clean)
cargo fmt --all             # format before committing
cargo tree -p baton-core    # audit: must stay free of tokio/reqwest/fs
```

## Conventions

- Reference design sections in comments as `ARCHITECTURE §X` / `DESIGN §X` so
  code stays traceable to the rationale.
- Keep event handlers O(1)-ish (append to a buffer); no heavy work in the
  reducer (backpressure, ARCHITECTURE §4.4).
- When you add a `Command`/`Event`/`Record` variant, also: keep it
  `#[non_exhaustive]`, update the reducer's match, and add a scripted test that
  pins the resulting command sequence.
- Determinism is testable: any new control-flow path should have a replay test
  asserting identical commands on a re-fed event stream.
