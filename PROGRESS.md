# Progress

Running log of what's implemented, phase by phase (see `docs/ROADMAP.md`).

## Phase 0 — Pure core skeleton (no IO) ✅

**Goal:** the brain exists as a pure state machine with zero IO.

Done:

- Workspace set up (`crates/baton-core`), ready to grow into the full layout.
- `baton-core` — the sans-IO reducer, split into modules:
  - `primitives.rs` — `OpId`, `Seq`, `Timestamp`, `Value`, `ObjectKey`.
  - `model.rs` — canonical `ModelRequest`/`ModelDelta`/`ModelOutput`, `ToolCall`, `ToolSchema`, `Usage`, `ModelSelector` (+ constructors). `Usage` carries `input_tokens`/`output_tokens` plus an **opaque `extra: Value`** (narrow-waist passthrough) for provider extras such as cost — the brain never reads it; only the host does.
  - `command.rs` / `event.rs` — the two-enum brain↔host contract, `#[non_exhaustive]` throughout.
  - `record.rs` — the append-only log (`LogEntry`, `Record`, `OpOutcome`, `OpMeta`).
  - `state.rs` — `BrainState` + in-flight op table (derived; foldable from the log).
  - `policy.rs` — pluggable `TurnPolicy` + `StaticPolicy` (trivial pass-through projection).
  - `brain.rs` — `Brain::poll()` / `submit()` + the turn-loop reducer.
- Tests (`crates/baton-core/tests`): scripted session, permission round-trip, parallel tool calls, projection contents, deterministic replay, delta-vs-log, JSON round-trip. **9 passing.**

**Exit criteria — met:**

- ✅ Scripted `user → model → tool → model → done` reduces to the expected command sequence (`scripted_session.rs`).
- ✅ Deterministic replay: same event stream twice → identical commands (`determinism.rs`).
- ✅ No `tokio`/`reqwest`/`fs` in `baton-core` (`cargo tree -p baton-core` shows only `serde`/`serde_json`).

Decisions:

- Single crate for Phase 0; model types kept in `baton-core` (move to `baton-model` later if needed).
- `#[non_exhaustive]` on enums **and** host-facing structs, with constructors on the structs (forward-compatible, narrow-waist).
- Dropped `panic = "abort"` from the release profile (conflicts with the test harness; belongs in a WASM-specific profile in Phase 4).

## Phase 1 — Batteries-included CLI host (the showcase) ✅

**Goal:** a real, usable terminal agent driven by the Phase 0 core.

Done:

- `baton-host`: the tokio [`Engine`] driver loop (drain `poll()` → perform commands as concurrent tasks → await next event → `submit()`), plus:
  - [`Capability`] + [`ModelAdapter`] traits and their registries.
  - Host-side permission [`Policy`]: `AllowAll`, `DenyAll`, `Interactive` (prompts).
  - [`Frontend`] trait + streaming `StdoutFrontend`.
  - `EngineBuilder` that assembles the brain's `StaticPolicy` from registered capabilities (their schemas → advertised tools; sensitive ones → gated set).
- Capabilities (`baton-host::capabilities`): `shell` (streams stdout), `fs_read` (read-only, no permission), `fs_write`, `http`.
- `baton-providers`: `OpenAiAdapter` — chat completions with streaming SSE, tool-call assembly, usage accounting (including **real cost from the router**: the adapter reads `usage.cost`/`total_cost`/`cost_details.total_cost` from the response and surfaces it verbatim in `Usage.extra` as `{ "cost", "cost_source": "router" }`; when the response omits cost it falls back to a tiny static per-token price table, tagged `"cost_source": "estimated"`, and emits no cost at all for unknown models), configurable base URL/model. Defaults target the **Hugging Face router** (`https://router.huggingface.co/v1`, `google/gemma-4-31B-it:together`); the API key resolves from `OPENAI_API_KEY` → `HF_TOKEN` → the Hugging Face token file read directly (`HF_TOKEN_PATH`, else `$HF_HOME/token`, else `~/.cache/huggingface/token`) → `hf auth token` (last resort, only if no token file is present). Reading the token file directly means a logged-in user needs no `hf` binary on `PATH`. Transport-level **retry with exponential backoff** (the adapter's job, per CLAUDE.md): transient failures — network/connect errors, HTTP 429, and 5xx — are retried with capped exponential backoff up to a configurable `max_attempts` (`with_max_attempts`, default 4); non-429 4xx are semantic errors and are never retried.
- `baton-cli`: the `baton` binary. One-shot (`baton "prompt"`) or interactive REPL; `-y/--yes` for allow-all. Prints a startup banner (model · endpoint · mode).
- CLI observability: the `Frontend` trait gained lifecycle hooks (model start/end + token usage, tool start with args, tool result, permission decision, session end); `StdoutFrontend` renders them with ANSI colors (auto-disabled off a TTY / under `NO_COLOR`).
- CLI metrics: `StdoutFrontend` renders per-call metric lines and a session-totals footer. Per model call it shows **cost** (read from `Usage.extra` — the narrow-waist passthrough the adapter fills, ARCHITECTURE §2.4), **input/output tokens**, and **elapsed time**; per tool call it shows elapsed time. Elapsed below `0.01s` is treated as zero and omitted. At session end (`Frontend::on_session_end`, driven by `Engine::session_end` after a one-shot run or interactive exit) it prints a `Σ` footer with total elapsed, total in/out tokens, and total cost. All timing is **host-side** (`Instant` in the front-end); `baton-core` stays clock-free / sans-IO. The accumulation + formatting live in a pure, unit-tested `Metrics` struct (folding model/tool calls into totals; tiny-cost precision; empty-session yields no footer).
- Collapsed tool output: `StdoutFrontend` renders large tool results as a head (first `RESULT_HEAD_LINES` = 8 lines) plus a "… +N lines" summary, so a 1000-line shell result stays compact. Full output is restored by `BATON_FULL_OUTPUT` (truthy env var; honoured by `StdoutFrontend::default`) or the CLI's `--full-output` flag (`StdoutFrontend::with_full_output`). Object results expand multiline string fields (e.g. a shell `stdout`) so the line count reflects real output.
- Streaming is the **only** model mode (explicit contract on `ModelAdapter`): adapters stream deltas live via the sink, then return the consolidated output. No non-streaming path exists.

Refinement to `baton-core` made for real providers: the durable `ToolResult` now carries the originating model `tool_call` id, so projection emits provider-correct `tool_call_id` correlation. Added `ModelOutput::new`, `ModelRequest::new` and `SamplingParams` builders (host-facing structs are `#[non_exhaustive]`).

Tests (40 total across the workspace):

- `baton-host/tests/end_to_end.rs` — a real multi-turn session driven through the tokio loop with a scripted model + the **real shell capability**; a denied-permission round-trip; plus a metrics flow test (a cost-reporting scripted model drives `on_model_end` with tokens + cost from `Usage.extra`, tool ends fire, and `Engine::session_end` triggers `on_session_end` once).
- `baton-host` `frontend` unit tests — tool-result collapse/full-output, and the `Metrics` accumulation + footer formatting (token/cost folding, tiny-cost precision, elapsed floor, empty-session = no footer).
- `baton-providers` — unit tests for request building + SSE accumulation + retry classification/backoff, `tests/streaming.rs` driving the adapter against a **local mock SSE server** (real reqwest streaming path), and `tests/retry.rs` driving retries against a **local mock HTTP server** (transient 429/5xx retried to success, persistent 5xx gives up after `max_attempts`, 4xx not retried).

**Exit criteria:**

- ✅ "CLI on a laptop" host setup ≈ 10 lines on top of `baton-host` (see the marked block in `crates/baton-cli/src/main.rs`).
- ✅ Genuine multi-turn session end-to-end. Verified **live** against the HF router: `baton -y "Use the shell tool to run 'echo baton-live-test', then tell me what it printed."` — the model called the shell tool, the host ran it and streamed the output, and the model produced a final answer. Also covered by the driver-loop + mock-SSE tests for CI (no key needed).

## Phase 2 — Concurrency & streaming (the differentiator) ✅

**Goal:** multiple in-flight operations; the LLM is "just another stream."

### P2-1 — Multiple concurrent ops ✅

The op table already held many in-flight ops keyed by `OpId`, and the host already ran one task per op (one `tokio::spawn` per `StartModelCall`/`StartCapability`, feeding a single inbox channel — the brain reduces interleaved events one at a time, atomically). The missing piece for "a model stream **and** a background `shell` op run simultaneously" was a way for an op to *not* hold the turn open. Added:

- `baton-core`: `OpKind::Capability` gained a `background: bool` flag; `OpKind::blocks_turn()` returns `false` for background capabilities (and was rewritten as an exhaustive match so a future op kind can't silently default to "blocks"). `TurnPolicy` gained `is_background(capability) -> bool` (default `false`), implemented by `StaticPolicy` via a configurable background set (`with_background`). The reducer (`brain.rs`): after a model turn's tool fan-out, if nothing blocks the turn it resumes the model immediately so it streams alongside the background op(s); a granted-permission background op resumes likewise; and `on_model_done` **defers `Done`** while a background op is still in flight (the turn isn't over while work runs — the background result is folded in and a fresh turn picks it up). No new `Command`/`Event`/`Record` variants — background-ness is a brain-side scheduling decision the host never sees.
- `baton-host`: `Capability` gained `runs_in_background()` (default `false`); `CapabilityRegistry::background_names()`; `EngineBuilder::build()` threads those into the brain's `StaticPolicy` (`.with_background(...)`), mirroring the existing permissioned-names wiring. The `Engine` driver loop needed **no** change — it already spawns one task per op and reacts event-driven (the shell capability awaits `wait_with_output()`, so `ProcessExited` is instant with no polling/`sleep`).

Tests (44 total across the workspace, +4):

- `baton-core/tests/concurrent_ops.rs` — the headline scripted interleave (model stream + background shell, pinning the exact command sequence including the deferred `Done`); deterministic replay over the interleaved stream (identical commands **and** identical log); and a mixed background + foreground fan-out asserting only the *foreground* op gates the turn.
- `baton-host/tests/end_to_end.rs::model_stream_runs_while_background_op_is_in_flight` — through the **real tokio engine**: a background op blocks on a channel while the next model call provably runs (true overlap, not "both ran eventually"), then releases it; the final turn picks up the result and ends with exactly one `EndTurn`.

**Exit criteria:**

- ✅ Kick off a long background op and stream a model response simultaneously; react to its completion instantly (no polling/`sleep`).
- ✅ Cancel an in-flight model stream cleanly; the log records "N tokens then cancelled"; replay reproduces it (P2-2 below).
- ✅ Delta coalescing with exact recording (P2-3 below).

### P2-2 — First-class cancellation ✅

The brain already had the cancellation *shape* (`Command::Cancel`, `Event::OpCancelled`, `Brain::on_op_cancelled` logging a `Cancelled { partial }` outcome that preserves a model op's `text_so_far`); the host already aborted the tokio task on `Cancel` and emitted `OpCancelled`. P2-2 closed the end-to-end path and hardened the reducer:

- `baton-core` (`brain.rs`): `on_op_cancelled` now (1) **ignores a cancel confirmation for an op that already resolved** — the host aborts the task *and* emits `OpCancelled`, but the task may have queued its real terminal event (`ModelDone`) a hair before the abort; that event is folded first and removes the op, so the late `OpCancelled` must be a no-op or it would append a spurious `Cancelled` `OpEnded` and break replay (cancellation is idempotent, ARCHITECTURE §6.4); and (2) emits the terminal `Done { reason: Cancelled }` once the **last** in-flight op drains on a plain abort (`UserAbort`/ESC) with nothing to resume — previously a bare abort left the brain silently idle and the front-end (which already renders `DoneReason::Cancelled`) never saw it. The existing steer-interrupt path (`pending_resume` → fresh turn) is unchanged, and a single-op cancel while other work is still in flight does **not** force `Done` (the turn only ends when the brain is idle). No new `Command`/`Event`/`Record` variants — the cancellation contract was already in place.
- `baton-host` (`engine.rs`): added a cloneable `EventSender` handle (`Engine::event_sender()`) for injecting events into the running loop from *outside* a turn — the realistic wiring for a Ctrl-C / signal handler sending `UserAbort` while `user_turn` is awaiting the model stream. `EventSender::abort()` is the `UserAbort` convenience. The driver loop itself was already correct (it aborts the per-op `JoinHandle` on `Command::Cancel` and confirms with `OpCancelled`); nothing else changed there.

Tests (50 total across the workspace, +6):

- `baton-core/tests/cancellation.rs` — the headline scripted "stream N tokens then `UserAbort`" pinning the command sequence (`StartModelCall` → `Cancel` → `Done { Cancelled }`) and asserting the partial (`"Hello, wor"`) is in the log; deterministic replay (identical commands **and** identical log — partial reproduced *then* the cancel); the stale-`OpCancelled`-after-`ModelDone` race is a no-op (exactly one `Ok` `OpEnded`, no spurious `Cancelled`); and cancelling one background op mid-stream does **not** end the turn (the model op still gates it → `EndTurn`, not `Cancelled`).
- `baton-host/tests/end_to_end.rs` — through the **real tokio engine**: `cancel_in_flight_model_stream_preserves_partial` (a model that streams two tokens then hangs forever; a `UserAbort` injected via `event_sender()` aborts the task; the turn ends `Cancelled`, the partial `"Hello, wor"` is in the durable log, and **no** consolidated `ModelOutput` was recorded); and `cancel_in_flight_background_op_cleanly` (a never-finishing background op is aborted on `UserAbort`, logged `Cancelled`, with the engine fully drained — `inflight_len() == 0`, no leaked work).

**Exit criteria:**

- ✅ Cancel an in-flight model stream cleanly (host aborts the task; partial text preserved). Background capability ops cancel cleanly too (no leaked work).
- ✅ Replay reproduces the partial output then the cancel, deterministically.
- ✅ Delta coalescing with exact recording (P2-3 below).

### P2-3 — Delta coalescing with exact recording ✅

The host coalesces high-frequency streamed deltas for the **render** while still recording exactly **one** consolidated `Record` per message — deltas are transport, never durable (ARCHITECTURE §4.4/§4.5), so replay stays bit-for-bit identical regardless of how the stream was batched. Implemented entirely host-side; `baton-core` is untouched (no new `Command`/`Event`/`Record` variants — coalescing is invisible to the brain):

- `baton-host` (`coalesce.rs`): a small, pure, IO-free [`Coalescer`] that buffers *consecutive same-op streamed text* (`ModelText` / `ModelReasoning`, kept separate since they render differently) and merges it into one larger `OutputEvent`. Any other event — a different op, a tool chunk, a tool start, a notice — first flushes the pending buffer (preserving order), then passes through. It takes `OutputEvent`s in and yields the `OutputEvent`s the front-end should render, so it is fully unit-testable without stdout.
- `baton-host` (`engine.rs`): the `Engine` routes `Command::Emit` through the coalescer (`push` → render the merged result), and `flush_render`es it at every boundary where order matters — before any lifecycle hook (model/tool start, permission, done, notice; a single guard at the top of `perform` for every command except `Emit`), before a completion event in `observe` (`ModelDone`/`CapabilityDone`/`CapabilityError`, so the metric line follows its text), at the end of each turn (`drive_to_idle`), and in `session_end`. **Critically, the engine still submits *every* `ModelDelta` to the brain** (the `perform`/`observe` submit path is unchanged) — so the brain's `text_so_far` stays complete and a cancelled op's partial loses no tokens; coalescing batches only the front-end render, never the brain's event stream.

Tests (57 total across the workspace, +7):

- `baton-host` `coalesce` unit tests — consecutive same-op text merges on flush; a non-text event flushes first (order preserved); switching op flushes the previous op; text vs reasoning never merge; empty flush is a no-op; and the headline **chunking-invariant** property (per-char vs few-chunk vs single-chunk streams all render identical text, and per-char churn collapses to one render event).
- `baton-host/tests/end_to_end.rs::delta_coalescing_keeps_recording_exact` — through the **real tokio engine**: the same answer streamed per-character, in 5-char chunks, and as a single delta yields byte-for-byte identical *logical* records (`UserMessage`/`ModelOutput`/`ToolResult`) and exactly **one** consolidated `ModelOutput` per call (no per-delta log entries), while the per-character stream is coalesced to a single render call.

[`Coalescer`]: crates/baton-host/src/coalesce.rs

[`Engine`]: crates/baton-host/src/engine.rs
[`Capability`]: crates/baton-host/src/capability.rs
[`ModelAdapter`]: crates/baton-host/src/model.rs
[`Policy`]: crates/baton-host/src/policy.rs
[`Frontend`]: crates/baton-host/src/frontend.rs
