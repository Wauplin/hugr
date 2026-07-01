# CLAUDE.md

Guidance for working in the Baton repository.

## What this is

Baton is a **runtime-free, sans-IO agent harness** in Rust. Read `docs/DESIGN.md` and `docs/ARCHITECTURE.md` before making non-trivial changes — the architecture is the product, and several decisions are deliberate one-way doors. `docs/ROADMAP.md` tracks the phased plan and per-phase exit criteria; `PROGRESS.md` tracks what is actually built.

## The one rule that matters most

**`baton-core` is sans-IO and pure.** It is a reducer: `submit(event)` folds an event into state and queues commands; `poll()` drains them. It must never do IO.

Hard invariants — do not break these:

- **No environmental dependencies in `baton-core`.** No `tokio`, no `reqwest`, no `std::fs`, no sockets, no clock, no RNG, no threads. Only pure-data crates (`serde`, `serde_json`). Verify with `cargo tree -p baton-core`.
- **The brain is single-threaded.** All concurrency lives in the host. The moment the brain is multithreaded, we lose sans-IO, replay, and easy bindings.
- **All nondeterminism is injected** as events (`Tick` for time; model output, tool results, user input as events). The brain never reads a clock or RNG. This is what makes replay bit-for-bit deterministic — protect it.
- **The log is the source of truth.** `BrainState` is a *fold* over the log and must stay rebuildable from it. Don't add un-derived state.
- **Deltas are transport, never durable.** `ModelDelta`/`CapabilityChunk` drive live UI and op buffers but are *never* written to the log. One consolidated `Record` is appended per logical message/tool-result.
- **Streaming is the only model mode.** Adapters stream deltas live via the sink and return the consolidated output; there is no non-streaming path.

## The narrow-waist rule (ARCHITECTURE §2.4)

> **Type only what the brain branches on. Everything else is an opaque payload.**

- Typed & stable: op lifecycle (start/delta/done/error/cancel), model *output structure* (text vs tool calls vs stop reason), turn control, permission outcomes.
- Opaque (`Value`): capability args/results, plugin payloads, provider knobs, prompts, answers. The brain stores and forwards these; it never interprets.

Consequences for code changes:

- `#[non_exhaustive]` on **every public enum** so adding a variant isn't breaking. Hosts always have a `_ => {}` arm.
- Host-facing **structs** that callers must construct (e.g. `ModelOutput`, `ToolCall`, `ToolSchema`, `Usage`, `ModelRequest`, `SamplingParams`) are also `#[non_exhaustive]`, so they need **constructors** (`::new`, builders) — external code can't use struct literals. Add a constructor when you add such a struct.
- Adding a new tool, provider knob, or plugin must touch **zero** core types. If a change forces a core type edit for a new *capability*, reconsider it.

## Where logic goes

- **Agent strategy** (which model, how to project context, whether permission is needed) lives in the pluggable `TurnPolicy` — never hardcoded in the reducer. The Phase 0 `StaticPolicy` is the trivial pass-through projection.
- **The reducer** (`brain.rs`) only: maintains the log + op table; drives the turn loop; asks the policy; routes opaque payloads; emits permission/UI events; decides done/checkpoint. If you're adding "smarts", it probably belongs in a policy, not the reducer.
- **Everything hard** (IO, HTTP, rendering, scheduling, model resolution, the atomic CAS check, storage) is the *host's* job — not in `baton-core`.

## Project layout

```
crates/baton-core/       # the sans-IO brain (NO tokio/reqwest/fs)
  src/primitives.rs  # OpId, Seq, Timestamp, Value, ObjectKey
  src/model.rs       # ModelRequest/Delta/Output, ToolCall, Usage, selectors
  src/command.rs     # Command (brain → host) + OutputEvent
  src/event.rs       # Event (host → brain) + SteerMode, Decision, VersionRef
  src/record.rs      # LogEntry, Record, OpOutcome, OpMeta (the durable log)
  src/state.rs       # BrainState + in-flight op table (derived; foldable)
  src/policy.rs      # TurnPolicy trait + StaticPolicy
  src/brain.rs       # Brain: poll() + submit() + the reducer

crates/baton-host/       # default native host (tokio, IO) — Phase 1
  src/engine.rs      # the tokio driver loop + EngineBuilder
  src/capability.rs  # Capability trait + ChunkSink + registry
  src/model.rs       # ModelAdapter trait + ModelSink + registry
  src/policy.rs      # host permission Policy: AllowAll/DenyAll/Interactive
  src/frontend.rs    # Frontend trait + StdoutFrontend (ANSI colors)
  src/capabilities/  # shell, fs_read, fs_write, http, blob (content-addressed store)

crates/baton-providers/  # model adapters — OpenAiAdapter (streaming)
crates/baton-cli/        # the `baton` binary (~10 lines on top of baton-host)

crates/baton-replay/     # versioned, portable trace format (save/load) — Phase 3
  src/lib.rs         # Trace { meta, events, log, blobs }; std::fs save/load
  src/blob.rs        # BlobStore: disk-backed content-addressed (sha256) store
```

`baton-plugin-abi` (Phase 5, the versioned plugin contract + subprocess transport) and `baton-wasm` (Phase 4, the browser/JS binding — the same brain compiled to WASM, driving the Chrome extension in `crates/baton-wasm/extension/`) now exist too; the remaining crates in `ARCHITECTURE.md` §10 (`baton-py`, `baton-js`) arrive in later phases. `baton-replay` is a host-side **persistence** crate — it may use `std::fs`, but it depends on `baton-core` as *pure data only* and never pulls IO into the core. **Never add environmental dependencies to `baton-core`** to make a host easier; put them in the host crate. All IO/HTTP/shell/clock work lives in `baton-host` (or another host), never in the core.

When extending the host: capabilities are uniform (no privileged built-ins — shell/fs/http are ordinary `Capability`s); a model call is "an effect the host provides" registered like a capability; transport errors (retries, 429s) are the adapter's job, semantic errors route back to the model as tool results.

## Commands

```bash
cargo test                  # all tests
cargo test -p baton-core    # core only
cargo clippy --all-targets  # lint (keep it clean)
cargo fmt --all             # format before committing
cargo tree -p baton-core    # audit: must stay free of tokio/reqwest/fs
```

## Conventions

- **Keep the docs in sync — it's part of "done".** After completing a task, update `PROGRESS.md` and any affected files in `docs/` (DESIGN / ARCHITECTURE / ROADMAP) so they always reflect what is actually built. A task isn't finished until the docs match reality.
- **Markdown is single-line.** Write every markdown file (`CLAUDE.md`, `PROGRESS.md`, `README.md`, `docs/`) with one physical line per paragraph or bullet — never hard-wrap prose at 80 columns; rely on the editor's soft-wrap. (Fenced code blocks and table rows are exempt.)
- Reference design sections in comments as `ARCHITECTURE §X` / `DESIGN §X` so code stays traceable to the rationale.
- Keep event handlers O(1)-ish (append to a buffer); no heavy work in the reducer (backpressure, ARCHITECTURE §4.4).
- When you add a `Command`/`Event`/`Record` variant, also: keep it `#[non_exhaustive]`, update the reducer's match, and add a scripted test that pins the resulting command sequence.
- Determinism is testable: any new control-flow path should have a replay test asserting identical commands on a re-fed event stream.
