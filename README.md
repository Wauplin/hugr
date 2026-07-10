# Hugr

> **Build your subagent, ship it anywhere.** A toolkit for tiny, self-contained, domain-specific agents on a runtime-free, sans-IO Rust core.

A **subagent** is, at its essence, a small Rust crate plus a system prompt and a set of tools with declared privileges. Hugr turns that agent crate folder into **one self-contained binary** — which also serves MCP via `--mcp-serve` — with the shared infrastructure every subagent needs built in:

- **One invocation contract.** A question (string) in; a structured response object + mandatory metadata out — status, **cost**, **duration**, tokens, and a **trace id**. Every Hugr agent, every surface, same shape. Errors are answers (`status: "error"`, exit 0), so callers branch on data, not exceptions.
- **Resumable & forkable traces.** Every run persists an immutable trace. Pass its `trace_id` back to continue the conversation; pass an *older* id to fork a sibling branch. Orchestrators explore many directions without ever growing one shared context — replay is instant and bit-for-bit deterministic.
- **Sandboxed by construction.** An agent registers exactly the tools its manifest grants. No shell granted = no shell exists in the binary (and the tool library is exec-free). Plus a private, jailed scratchpad and blob exchange with the caller.
- **Token-efficient by design.** A handful of domain tools and a focused prompt, not fifty generic ones. Small agents are cheaper, faster, and more reliable — and an orchestrator pays one tool call to use them.
- **Agents compose.** A built Hugr agent *is* a tool: grant one to another with a manifest line (`[tools.agent.<name>] artifact = "..."`) and it's called like any capability — delegation never widens privileges, and the child's cost folds into the caller's metadata.

The reference agent, [`hugr-docs`](examples/hugr-docs/), is a checked-in docs-Q&A agent crate: `hugr.toml` + `SYSTEM.md` live beside the Rust response contract. `hugr-toolkit` does not depend on it. The generic `hugr run` path still works for typed agents by compiling a cached dev shim that links the current agent crate.

See [`ARCHITECTURE.md`](ARCHITECTURE.md) (design, architecture, threat model — the spec) for more details, or start with the [tutorials](docs/tutorials/README.md) for a guided tour of every surface.

## Quickstart

```bash
cargo run -p hugr-toolkit --bin hugr -- new my-agent            # scaffold an agent crate
export HUGR_API_KEY=hf_...                                       # whatever the manifest's api_key_env names
cargo run -p hugr-toolkit --bin hugr -- run my-agent "question" # interpret it (dev loop)
cargo run -p hugr-toolkit --bin hugr -- build my-agent          # ship it: one standalone binary
./my-agent/dist/my-agent "question"                              # answers; --trace <id> resumes; --mcp-serve serves MCP
```

Every built binary self-describes: `--describe` (tools, privileges, tiers, pricing, limits), `--config` (the parsed manifest, secrets redacted), `--traces` (stored lineage).

By default, both `hugr run` and built binaries store agent state under `~/.hugr/<agent-name>/`: immutable traces in `traces/` and per-lineage scratch state in `scratch/`. Override the full home with `HUGR_AGENT_HOME`, or the base with `HUGR_HOME`.

## What An Agent Crate Looks Like

```
my-agent/
  Cargo.toml          # Rust crate metadata; typed contracts and hooks live here
  hugr.toml          # name, model tiers + pricing, tool grants + scopes, limits
  SYSTEM.md          # the system prompt
  src/lib.rs          # optional typed response / hooks / custom Rust wiring
```

```toml
[agent]
name = "policy-docs"
description = "Answers questions about the company travel policy."

[models.medium]
model = "google/gemma-4-31B-it:cerebras"
input_usd_per_m_tokens = 1.0
output_usd_per_m_tokens = 1.5

[tools.fs_read]
root = "./policies"        # read-only, jailed to this folder
```

Runtime invocation config can patch manifest targets before the agent is assembled. For example, `hugr-docs` declares `docs_path` once and the toolkit exposes it in both the CLI and MCP `ask` schema:

```toml
[runtime.args.docs_path]
target = "tools.fs_read.root"
positional = true
required = true
env = "HUGR_DOCS_PATH"
help = "Folder containing the documentation to search."
```

Auditable by reading: the manifest *is* the blast radius. Unknown keys are hard errors, so a typo can't silently widen or narrow it. The tool library today: `fs_read` (six read-only `fs_*` tools), `web_fetch`, `memory`, `traces_read` (read-only trace/feedback mining), the scratchpad — plus `[tools.mcp.<name>]` (the one external-process escape hatch) and `[tools.agent.<name>]` (another built agent as a tool).

## The core underneath

The runtime is built on `hugr-core`, a pure, **sans-IO**, single-threaded brain — a reducer over an append-only event log. The entire brain ↔ host surface is two enums and two methods:

```rust
loop {
    for cmd in brain.poll() {        // drain commands the brain wants performed
        host.perform(cmd);
    }
    let event = host.next_event().await;  // the only await — host-side only
    brain.submit(event);             // pure, instant, no IO
}
```

Four separations most harnesses conflate — durable state (event log) vs model context (projection) vs IO (host) vs permissions (externalized policy) — are why the subagent features fall out for free: a **trace** is the log made durable, **resume** is re-folding a trace, a **fork** is copying a log prefix, and **cost** is arithmetic over per-op metadata already in the log. All nondeterminism is injected as events, so replay is bit-for-bit deterministic.

## Crate layout

```
crates/
  hugr-core/          # the sans-IO brain — log, projection, op table, reducer. NO tokio, NO reqwest, NO fs.
  hugr-host/          # native tokio host: engine driver loop, capability/model registries, MCP stdio client, JSON-line framing.
  hugr-providers/     # OpenAI-compatible streaming adapter (retries inside).
  hugr-replay/        # trace format + content-addressed blob store + replay/verify/inspect.
  hugr-agent/         # the subagent runtime: Ask/Answer, trace store with trace_id/depends_on + fork, scratchpad, blobs, limits, cost accounting, agent-as-tool (subprocess).
  hugr-toolkit/       # agent manifests (hugr.toml + SYSTEM.md), the tool library, and the `hugr` CLI: new/run/build/traces/replay/verify.
  hugr-wasm/          # generic WASM bindings around hugr-core for browser/JS hosts.
  hugr-python/        # PyO3 runtime embedding: define agents and tools in Python (built via bindings/python).
bindings/
  python/             # the `hugr-agents` Python package: typed layer + tests over hugr-python.
  typescript/         # the `hugr-agents` TS package: typed Agent over the WASM brain (node + browser).
examples/
  hugr-docs/          # the reference subagent crate (docs Q&A): manifest, prompt, and typed response contract.
  hugr-weather/       # the self-contained beginner agent; source of the `hugr new --template weather` scaffold.
  hugr-insights/      # offline self-improvement agent: mines an agent's traces + feedback via `traces_read`.
  chrome-extension/   # a browser host built on hugr-wasm + bindings/typescript: chrome.* capabilities, side-panel UI.
```

## The reference subagent: `hugr-docs`

One folder in, one question in, one JSON response out — with cost metadata. No shell, no writes, no network tool; the read-only, folder-jailed `fs_*` tools.

```bash
export HUGR_DOCS_API_KEY=hf_...   # or any OpenAI-compatible endpoint key
cargo run -p hugr-toolkit --bin hugr -- run examples/hugr-docs ./docs "What is the narrow-waist rule?" | jq
```

```json
{
  "status": "success",
  "response": {
    "response": "The narrow-waist rule is ...",
    "related_documents": ["ARCHITECTURE.md"]
  },
  "trace_id": "1e4f7d0a9b2c3d44",
  "metadata": { "duration_ms": 1234, "tokens_in": 1000, "tokens_out": 200, "cost_micro_usd": 1300, "model_calls": 2, "tool_calls": 3 }
}
```

The docs root is runtime config, not a compiled-in scope: `hugr run examples/hugr-docs ./docs "..."` and `hugr run examples/hugr-docs ./other-docs "..."` use the same agent crate with different read jails. Because `hugr-docs` exposes `RESPONSE_RUST_TYPE` and a typed Rust response contract, generic `hugr run` compiles and reuses a cached dev shim under the temp dir so `hugr-toolkit` still does not depend on `hugr-docs`. Build it with `hugr build examples/hugr-docs`; the generated standalone shim links the current agent crate inferred from `Cargo.toml`, then Python and other languages consume the built binary via subprocess or `--mcp-serve`.

Runs for this reference agent land in `~/.hugr/hugr-docs/traces` unless `HUGR_AGENT_HOME`, `HUGR_HOME`, or an explicit `[traces].store` override is set.

## Define an agent in Python or TypeScript

The `hugr-agents` Python package (`bindings/python`) embeds the same runtime: fixed-shape config inputs are `TypedDict`s, tools are sync/async Python callables with explicit JSON schemas, `agent.ask(...)` returns the standard `Answer` dataclass, and `async for event in agent.run(...)` streams a union of event dataclasses. Introspection, trace, feedback, and stats outputs are recursive dataclass graphs too; only domain-owned opaque JSON stays as JSON. Traces land in `~/.hugr/<name>/` and verify with the Rust CLI. See `bindings/python/README.md`.

The `hugr-agents` TypeScript package (`bindings/typescript`) is the same shape for Node and the browser, driving the WASM brain: tools as `{name, description, schema, invoke}` objects, the same config keys and events, node-fs or IndexedDB trace stores, and cross-language `verify` in both directions. See `bindings/typescript/README.md`.

## Building & testing

```bash
cargo build --workspace
cargo test                  # unit + scripted/determinism + end-to-end tests
cargo clippy --all-targets
cargo fmt --all
cargo tree -p hugr-core     # audit: must stay free of tokio/reqwest/fs
```

Notable tests: `hugr-core/tests` (scripted sessions + deterministic replay), `hugr-host/tests/end_to_end.rs` (real engine, tools, MCP, record/replay/resume), and the ignored slow gates `cargo test -p hugr-toolkit --test conformance -- --ignored` / `--test build_cli -- --ignored` (compile a real agent binary and check every surface agrees).

## License

Licensed under Apache-2.0.
