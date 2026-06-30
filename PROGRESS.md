# Progress

Running log of what's implemented, phase by phase (see `docs/ROADMAP.md`).

## Phase 0 — Pure core skeleton (no IO) ✅

**Goal:** the brain exists as a pure state machine with zero IO.

Done:

- Workspace set up (`crates/baton-core`), ready to grow into the full layout.
- `baton-core` — the sans-IO reducer, split into modules:
  - `primitives.rs` — `OpId`, `Seq`, `Timestamp`, `Value`, `ObjectKey`.
  - `model.rs` — canonical `ModelRequest`/`ModelDelta`/`ModelOutput`, `ToolCall`,
    `ToolSchema`, `Usage`, `ModelSelector` (+ constructors).
  - `command.rs` / `event.rs` — the two-enum brain↔host contract,
    `#[non_exhaustive]` throughout.
  - `record.rs` — the append-only log (`LogEntry`, `Record`, `OpOutcome`, `OpMeta`).
  - `state.rs` — `BrainState` + in-flight op table (derived; foldable from the log).
  - `policy.rs` — pluggable `TurnPolicy` + `StaticPolicy` (trivial pass-through projection).
  - `brain.rs` — `Brain::poll()` / `submit()` + the turn-loop reducer.
- Tests (`crates/baton-core/tests`): scripted session, permission round-trip,
  parallel tool calls, projection contents, deterministic replay, delta-vs-log,
  JSON round-trip. **9 passing.**

**Exit criteria — met:**

- ✅ Scripted `user → model → tool → model → done` reduces to the expected
  command sequence (`scripted_session.rs`).
- ✅ Deterministic replay: same event stream twice → identical commands
  (`determinism.rs`).
- ✅ No `tokio`/`reqwest`/`fs` in `baton-core` (`cargo tree -p baton-core` shows
  only `serde`/`serde_json`).

Decisions:

- Single crate for Phase 0; model types kept in `baton-core` (move to
  `baton-model` later if needed).
- `#[non_exhaustive]` on enums **and** host-facing structs, with constructors on
  the structs (forward-compatible, narrow-waist).
- Dropped `panic = "abort"` from the release profile (conflicts with the test
  harness; belongs in a WASM-specific profile in Phase 4).

## Phase 1 — Batteries-included CLI host (the showcase) 🚧

**Goal:** a real, usable terminal agent driven by the Phase 0 core.

Planned:

- [ ] `baton-host`: tokio driver loop (`poll` / `next_event` / `submit`),
      capability + model-adapter traits, registries, host-side permission policy.
- [ ] Capabilities: `shell`, `fs read/write`, `http` via the uniform interface.
- [ ] `baton-providers`: OpenAI chat-completions adapter with streaming deltas.
- [ ] Interactive `Policy` (prompts) + `--yes` allow mode.
- [ ] Minimal stdout front-end consuming `OutputEvent`s.
- [ ] `baton-cli`: the showcase binary (~10 lines on top of `baton-host`).

**Exit criteria:**

- [ ] Genuine multi-turn coding session in the terminal, end-to-end.
- [ ] "CLI on a laptop" host setup ≈ 10 lines on top of `baton-host`.
