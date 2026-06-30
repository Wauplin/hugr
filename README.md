# Baton

> A lightweight, embeddable, runtime-free agent harness written in Rust.

**Baton** is the agent "brain" that can run **anywhere** — a browser tab (as
WASM, with no backend), a Python or JS script via bindings, a serverless
function, or a long-lived server — from a *single, portable core* with a small
footprint and fast startup.

The differentiator is not a feature list; it is an **architecture**. Baton
keeps four things separate that most harnesses conflate:

| Concern | What Baton does |
|---|---|
| **Durable state** | An append-only **event log** is the source of truth |
| **Model context** | A **projection** rendered from the log per turn |
| **IO** | A **sans-IO** core emits *commands*; the **host** does all IO |
| **Permissions** | **Externalized policy** (data), decided outside the core |

From those separations, resume, replay, multi-front-end, multi-provider,
sub-agents, forks, and parallel streaming all *fall out* rather than being
engineered as separate subsystems.

See [`docs/DESIGN.md`](docs/DESIGN.md) for the rationale,
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the concrete contract, and
[`docs/ROADMAP.md`](docs/ROADMAP.md) for the phased plan.

## The core ↔ host contract

The entire surface between the brain and a host is two enums plus two methods:

```rust
loop {
    for cmd in brain.poll() {        // drain commands the brain wants performed
        host.perform(cmd);
    }
    let event = host.next_event().await;  // the only await — host-side only
    brain.submit(event);             // pure, instant, no IO
}
```

- [`Command`] — what the brain wants the host to do (start a model call, invoke
  a capability, request permission, cancel, checkpoint, …).
- [`Event`] — what happened (user input, model deltas/result, tool results,
  permission decisions, injected ticks, …).

`poll()` and `submit()` are **synchronous and pure**. All concurrency and IO
live in the host; the brain is a single-threaded reducer.

[`Command`]: crates/baton-core/src/command.rs
[`Event`]: crates/baton-core/src/event.rs

## Status — Phase 0 ✅

Per the [roadmap](docs/ROADMAP.md), Phase 0 ships the **pure core skeleton (no
IO)**: the `Command`/`Event` vocabulary, the append-only log and in-flight op
table, the turn loop (`user → model → tool → model → done`), a trivial
pass-through projection policy, and **deterministic replay**.

Only [`baton-core`](crates/baton-core) exists today. Later phases add the
native host, provider adapters, the CLI, WASM/Python/JS bindings, traces,
plugins, sub-agents and scheduling (see the roadmap).

## Crate layout

Phase 0 is a single crate; the workspace is structured to grow into the full
layout from [`ARCHITECTURE.md` §10](docs/ARCHITECTURE.md):

```
crates/
  baton-core/     # the sans-IO brain (this is all that exists in Phase 0)
                  #   — state, log, projection, op table, reducer.
                  #   NO tokio, NO reqwest, NO fs.
```

Planned (later phases): `baton-model`, `baton-providers`, `baton-host`,
`baton-cli`, `baton-wasm`, `baton-py`, `baton-js`, `baton-plugin-abi`,
`baton-replay`.

## Building & testing

```bash
cargo build          # build the workspace
cargo test           # run the unit + scripted/determinism tests
cargo clippy --all-targets
cargo fmt --all
```

The two Phase 0 exit criteria are covered by tests in
[`crates/baton-core/tests`](crates/baton-core/tests):

- `scripted_session.rs` — a scripted `user → model → tool → model → done`
  session reduces to the expected command sequence.
- `determinism.rs` — feeding the same event stream twice yields identical
  commands; deltas never touch the durable log; the log round-trips through
  JSON.

## License

Licensed under either of Apache-2.0 or MIT at your option.
