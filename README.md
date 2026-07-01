# Hugr

> A lightweight, embeddable, runtime-free agent harness written in Rust.

**Hugr** is the agent "brain" that can run **anywhere** — a browser tab (as WASM, with no backend), a Python or JS script via bindings, a serverless function, or a long-lived server — from a *single, portable core* with a small footprint and fast startup.

The differentiator is not a feature list; it is an **architecture**. Hugr keeps four things separate that most harnesses conflate:

| Concern           | What Hugr does                                                |
| ----------------- | ------------------------------------------------------------- |
| **Durable state** | An append-only **event log** is the source of truth           |
| **Model context** | A **projection** rendered from the log per turn               |
| **IO**            | A **sans-IO** core emits *commands*; the **host** does all IO |
| **Permissions**   | **Externalized policy** (data), decided outside the core      |

From those separations, resume, replay, multi-front-end, multi-provider, sub-agents, forks, and parallel streaming all *fall out* rather than being engineered as separate subsystems.

See [`docs/DESIGN.md`](docs/DESIGN.md) for the rationale, [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the concrete contract, [`docs/ROADMAP.md`](docs/ROADMAP.md) for the original phased plan, and [`docs/ROADMAP_2.md`](docs/ROADMAP_2.md) for the current Rust CLI + Chrome extension product roadmap.

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

- [`Command`] — what the brain wants the host to do (start a model call, invoke a capability, request permission, cancel, checkpoint, …).
- [`Event`] — what happened (user input, model deltas/result, tool results, permission decisions, injected ticks, …).

`poll()` and `submit()` are **synchronous and pure**. All concurrency and IO live in the host; the brain is a single-threaded reducer.

[`Command`]: crates/hugr-core/src/command.rs
[`Event`]: crates/hugr-core/src/event.rs

## Status ✅

Per the roadmaps:

- **Phase 0** — the **pure core skeleton (no IO)**: the `Command`/`Event` vocabulary, the append-only log and in-flight op table, the turn loop (`user → model → tool → model → done`), a trivial pass-through projection policy, and **deterministic replay**.
- **Phase 1** — the **batteries-included CLI host**: a tokio driver loop, the uniform capability + model-adapter interfaces, `shell`/`fs`/`http` capabilities, a streaming OpenAI-compatible adapter, and the `hugr` CLI.
- **Roadmap 2 Phase 0** — product foundations: shipped `small`/`medium`/`big` tiers, `medium` default turns, host-supplied durable `est_tokens`, default auto-approve judge permissions, and explicit `--yolo` allow-all.

See [`PROGRESS.md`](PROGRESS.md) for the detailed status.

## Crate layout

The workspace grows into the full layout from [`ARCHITECTURE.md` §10](docs/ARCHITECTURE.md). Today:

```
crates/
  hugr-core/          # the sans-IO brain — state, log, projection, op table,
                       #   reducer. NO tokio, NO reqwest, NO fs.
  hugr-host/          # default native host: tokio driver loop, Capability +
                       #   ModelAdapter traits, shell/fs/http, policy, front-end.
  hugr-providers/     # model adapters — OpenAI chat completions (streaming).
  hugr-cli/           # the `hugr` showcase binary.
  hugr-replay/        # versioned, portable trace format + replay/inspect + blobs.
  hugr-plugin-abi/    # versioned plugin contract + subprocess (stdio) transport.
  hugr-example-plugin/# a standalone third-party plugin (no Hugr dependency).
  hugr-wasm/          # the browser/JS binding + a Chrome-extension host (WASM;
                       #   same brain, no backend). See crates/hugr-wasm/extension/.
```

Planned (later phases): `hugr-py`, `hugr-js`.

## Running the CLI

By default `hugr` talks to the **Hugging Face router** (an OpenAI-compatible endpoint), so if you're logged in with the `hf` CLI it works with no setup:

```bash
hf auth login                         # once; hugr reads the stored token

cargo run -p hugr-cli -- "list the rust files and summarise the workspace"
cargo run -p hugr-cli                # interactive REPL
cargo run -p hugr-cli -- --yolo "..." # allow all gated tool calls, skipping the judge
```

By default gated tools are judged by the configured `small` tier and denied reasons are routed back to the model. `--yolo` / `-y` switches to allow-all.

In the interactive REPL, `/context` prints the live `ContextPlan` (budget, included/summarized/referenced/omitted blocks, and reasons), `/compact` runs one lossless manual compaction pass through the `small` tier, and `/status` shows tier spend plus connected MCP servers/tools.

Configuration (all optional) via environment and `HUGR_CONFIG`:

| Variable             | Default                                                                                                                                             | Notes                                                                                                                                |
| -------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| API key              | `HUGR_API_KEY`, else `HF_TOKEN`, else the HF token file (`HF_TOKEN_PATH` / `$HF_HOME/token` / `~/.cache/huggingface/token`), else `hf auth token` | token file is read directly — no `hf` binary required                                                                                |
| `HUGR_MODEL_SMALL`   | `google/gemma-4-31B-it:cerebras`                                                                                                                    | model id for the `small` tier; must support tool calling                                                                             |
| `HUGR_MODEL_MEDIUM`  | `google/gemma-4-31B-it:cerebras`                                                                                                                    | model id for the `medium` tier (the CLI default tier); must support tool calling                                                     |
| `HUGR_MODEL_BIG`     | `google/gemma-4-31B-it:cerebras`                                                                                                                    | model id for the `big` tier; must support tool calling                                                                               |
| `HUGR_BASE_URL`      | `https://router.huggingface.co/v1`                                                                                                                  | set to `https://api.openai.com/v1` for OpenAI                                                                                        |
| `HUGR_CONFIG`        | unset                                                                                                                                               | JSON config with `models` and optional `mcp` / `mcp_servers` sections                                                               |
| `HUGR_FULL_OUTPUT`   | unset (collapse)                                                                                                                                    | truthy ⇒ show full tool output; same as the `--full-output` flag                                                                     |

MCP stdio servers can be loaded either from config or with repeatable `--mcp <cmd>` flags. Their tools are advertised as ordinary capabilities named `mcp__<server>__<tool>`, so they go through the same permission, tracing, and tool-result path as built-ins.

Example config:

```json
{
  "models": {
    "base_url": "https://router.huggingface.co/v1",
    "small": { "model": "google/gemma-4-31B-it:cerebras", "temperature": 0.0, "max_tokens": 512 },
    "medium": { "model": "google/gemma-4-31B-it:cerebras", "temperature": 0.2 },
    "big": { "model": "google/gemma-4-31B-it:cerebras", "temperature": 0.2, "max_tokens": 4096 }
  },
  "mcp": [
    { "name": "fs", "command": "mcp-filesystem", "args": ["."] },
    "python3 -m my_mcp_server"
  ]
}
```

> The model must support **function calling**, since hugr always advertises its tools. Small models that don't (e.g. some 8B instruct variants) return `model features function calling not support`.

The engine setup is ~10 lines on top of `hugr-host` (see the marked block in [`crates/hugr-cli/src/main.rs`](crates/hugr-cli/src/main.rs)).

## Running in the browser (Chrome extension)

The **same** brain, compiled to WASM, drives an installable Chrome side-panel agent that reads pages and navigates tabs — with **no backend**. Build the module and load the unpacked extension:

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.100
./crates/hugr-wasm/build-extension.sh
# then: chrome://extensions → Developer mode → Load unpacked → crates/hugr-wasm/extension/
```

A prebuilt `extension/wasm/` is committed, so you can skip the build and load it directly. See [`crates/hugr-wasm/extension/README.md`](crates/hugr-wasm/extension/README.md) and [`DEMOS.md`](crates/hugr-wasm/extension/DEMOS.md).

## Building & testing

```bash
cargo build --workspace
cargo test                  # unit + scripted/determinism + end-to-end tests
cargo clippy --all-targets
cargo fmt --all
cargo tree -p hugr-core    # audit: must stay free of tokio/reqwest/fs
```

Notable tests:

- `hugr-core/tests` — the Phase 0 exit criteria: a scripted session reduces to the expected command sequence; the same event stream replays to identical commands.
- `hugr-host/tests/end_to_end.rs` — a real multi-turn session through the tokio driver loop using the real `shell` capability.
- `hugr-providers` — request building, SSE accumulation, and a streaming run against a local mock SSE server.

## License

Licensed under either of Apache-2.0 or MIT at your option.
