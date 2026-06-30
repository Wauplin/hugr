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

## Status — Phases 0 & 1 ✅

Per the [roadmap](docs/ROADMAP.md):

- **Phase 0** — the **pure core skeleton (no IO)**: the `Command`/`Event`
  vocabulary, the append-only log and in-flight op table, the turn loop
  (`user → model → tool → model → done`), a trivial pass-through projection
  policy, and **deterministic replay**.
- **Phase 1** — the **batteries-included CLI host**: a tokio driver loop, the
  uniform capability + model-adapter interfaces, `shell`/`fs`/`http`
  capabilities, an interactive permission policy, a streaming OpenAI adapter,
  and the `baton` CLI.

See [`PROGRESS.md`](PROGRESS.md) for the detailed status.

## Crate layout

The workspace grows into the full layout from
[`ARCHITECTURE.md` §10](docs/ARCHITECTURE.md). Today:

```
crates/
  baton-core/       # the sans-IO brain — state, log, projection, op table,
                    #   reducer. NO tokio, NO reqwest, NO fs.
  baton-host/       # default native host: tokio driver loop, Capability +
                    #   ModelAdapter traits, shell/fs/http, policy, front-end.
  baton-providers/  # model adapters — OpenAI chat completions (streaming).
  baton-cli/        # the `baton` showcase binary.
```

Planned (later phases): `baton-wasm`, `baton-py`, `baton-js`,
`baton-plugin-abi`, `baton-replay`.

## Running the CLI

```bash
export OPENAI_API_KEY=sk-...          # required
export OPENAI_MODEL=gpt-4o-mini       # optional (this is the default)

cargo run -p baton-cli -- "list the rust files and summarise the workspace"
cargo run -p baton-cli                # interactive REPL
cargo run -p baton-cli -- -y "..."    # approve all tool calls (no prompts)
```

The engine setup is ~10 lines on top of `baton-host` (see the marked block in
[`crates/baton-cli/src/main.rs`](crates/baton-cli/src/main.rs)).

## Building & testing

```bash
cargo build --workspace
cargo test                  # unit + scripted/determinism + end-to-end tests
cargo clippy --all-targets
cargo fmt --all
cargo tree -p baton-core    # audit: must stay free of tokio/reqwest/fs
```

Notable tests:

- `baton-core/tests` — the Phase 0 exit criteria: a scripted session reduces to
  the expected command sequence; the same event stream replays to identical
  commands.
- `baton-host/tests/end_to_end.rs` — a real multi-turn session through the
  tokio driver loop using the real `shell` capability.
- `baton-providers` — request building, SSE accumulation, and a streaming run
  against a local mock SSE server.

## License

Licensed under either of Apache-2.0 or MIT at your option.
