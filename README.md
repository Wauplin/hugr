# Huggr

> Build a huglet and ship it anywhere as a small, self-contained artifact on a runtime-free, sans-IO Rust core.

Huggr is a toolkit for building **small, domain-specific huglets**. A huglet is a small Rust crate: a `huggr.toml` manifest (model tiers, tool grants, limits), a `SYSTEM.md` system prompt, and optionally a typed Rust response contract. Huggr turns that folder into one standalone binary that answers questions over a JSON contract and serves MCP through `--mcp-serve`.

The idea is that a specialist with a focused prompt and five jailed tools is cheaper, faster, and safer than a generalist with fifty, and that an orchestrator (a human, a script, or a larger agent) should pay one tool call to use it.

## Quickstart

```bash
cargo run -p huggr-toolkit --bin huggr -- new my-agent            # scaffold an agent crate
export HUGGR_API_KEY=hf_...                                       # the provider key named by the manifest
cargo run -p huggr-toolkit --bin huggr -- run my-agent "question" # interpret it (dev loop)
cargo run -p huggr-toolkit --bin huggr -- build my-agent          # ship it: one standalone binary
./my-agent/dist/my-agent "question"                              # answers; --trace <id> resumes; --mcp-serve serves MCP
```

Every built binary self-describes: `--describe` (tools, privileges, tiers, pricing, limits), `--config` (the parsed manifest, secrets redacted), `--traces` (stored lineage).

Agent state lives under `~/.huggr/<agent-name>/` by default: immutable traces in `traces/`, per-lineage scratch state in `scratch/`. Override with `HUGGR_AGENT_HOME` or `HUGGR_HOME`.

## What an agent crate looks like

```
my-agent/
  Cargo.toml          # Rust crate metadata; typed contracts and hooks live here
  huggr.toml           # name, model tiers + pricing, tool grants + scopes, limits
  SYSTEM.md           # the system prompt
  src/lib.rs          # optional typed response / hooks / custom Rust wiring
```

```toml
[agent]
name = "policy-docs"
description = "Answers questions about the company travel policy."

[models]
base_url = "https://router.huggingface.co/v1"
api_key_env = "HUGGR_API_KEY"

[models.medium]
model = "google/gemma-4-31B-it:cerebras"
input_usd_per_m_tokens = 1.0
output_usd_per_m_tokens = 1.5

[tools.fs_read]
root = "./policies"        # read-only, jailed to this folder
```

The manifest defines the agent's blast radius and is the document to audit. A tool that is not granted is not registered, so there is no code path to it. Unknown keys are hard errors, so a typo cannot silently widen or narrow the grant.

The built-in library includes jailed filesystem reads and writes, restricted or full shell execution, allowlisted web fetch, Exa web search, memory, trace inspection, scratch state, and isolated self-delegation. High-privilege tools remain opt-in grants: restricted shell commands execute without shell syntax, while full shell and full-disk roots explicitly hand sandboxing to the operator. See the [built-in capability reference](docs/capabilities.md).

## What every agent gets

- **One invocation contract.** The input is a question string. The output is a structured response with mandatory status, cost, duration, token, and trace-id metadata. Errors are answers (`status: "error"`, exit 0), so callers branch on data instead of exceptions.
- **Resumable and forkable traces.** Every run persists an immutable trace. Pass its `trace_id` back to continue the conversation, or pass an older id to fork a sibling branch. Replay is bit-for-bit deterministic.
- **Sandboxing by construction.** An agent registers only the tools granted by its manifest, each jailed to its declared scope. Every agent also gets a private, jailed scratchpad and explicit blob exchange with the caller.
- **Progressively disclosed skills.** Add standard `SKILL.md` folders to `skills = [...]` in the manifest or pass `--skill <path>` for one ask. The model sees the skill catalog and loads matching instructions or referenced files through a jailed reader only when needed.
- **Cost accounting.** Every response carries cost (from per-tier pricing config), duration, and token counts, folded from the trace. `huggr stats` aggregates them across runs.
- **Composition.** A built Huggr agent is a tool: grant it with `[tools.agent.<name>] artifact = "..."` and call it like any capability. Delegation never widens privileges, and the child's cost folds into the caller's metadata.
- **Isolated self-delegation.** Grant `[tools.delegate]` when an agent should call itself in a fresh context window. Recursion is depth-capped and child cost folds into the parent.

## Example: the reference docs agent

[`examples/huglet-docs`](examples/huglet-docs/) answers questions about a documentation folder. It has no shell, write, or network tools; only the read-only, folder-jailed `fs_*` family.

```bash
export HUGGR_API_KEY=hf_...   # or any OpenAI-compatible endpoint key
cargo run -p huggr-toolkit --bin huggr -- run examples/huglet-docs ./docs "What is the narrow-waist rule?" | jq
```

```json
{
  "status": "success",
  "response": {
    "response": "The narrow-waist rule is ...",
    "related_documents": ["docs/README.md"]
  },
  "trace_id": "1e4f7d0a9b2c3d44",
  "metadata": { "duration_ms": 1234, "tokens_in": 1000, "tokens_out": 200, "cost_micro_usd": 1300, "model_calls": 2, "tool_calls": 3 }
}
```

The docs folder is runtime config, not a compiled-in scope: the same agent crate runs against `./docs` or any other folder, each invocation jailed to the folder it was given. Build it with `huggr build examples/huglet-docs` to get a standalone binary that any language can call as a subprocess or through `--mcp-serve`.

## Python and TypeScript

The same runtime is available without writing Rust:

- **Consume a built agent from Python.** `huggr build <agent> --surface python` wraps the agent into a typed wheel: `ask()` in-process, dataclasses out. See [guide 4](docs/guides/04-agent-binary-from-python.md).
- **Define an agent in Python.** The [`huggr-agents` package](bindings/python/README.md) embeds the runtime: tools are decorated callables, config is data, `agent.ask(...)` returns the standard `Answer`. See [guide 5](docs/guides/05-agent-entirely-in-python.md).
- **Define an agent in TypeScript.** The [`huggr-agents` TS package](bindings/typescript/README.md) drives the same brain compiled to WASM, in Node and the browser. See [guide 6](docs/guides/06-agent-entirely-in-typescript.md).

Traces written from any surface verify with the Rust CLI.

## The core underneath

The runtime is built on `huggr-core`, a pure, sans-IO, single-threaded reducer over an append-only event log. The brain and host communicate through two enums and two methods:

```rust
loop {
    for cmd in brain.poll() {        // drain commands the brain wants performed
        host.perform(cmd);
    }
    let event = host.next_event().await;  // the only await; host-side only
    brain.submit(event);             // pure, instant, no IO
}
```

All nondeterminism (time, model output, tool results) is injected as events, so any session replays bit-for-bit. A trace is the durable log; resume re-folds it, a fork copies a prefix, and cost is computed from per-op metadata in it. This is what lets the same brain run natively, in Python, and in the browser.

## What Huggr is not

- **Not a general-purpose coding or browser agent.** Huggr defines the callee side; generalists are usually the orchestrators that call Huggr agents.
- **Not a hosted runtime or marketplace.** Huggr ships artifacts; you choose where to run them (locally, CI, a container).
- **Not an agent-to-agent wire protocol.** MCP is the adapter for exposing an agent to orchestrators; A2A and others could be added at the edge but are not foundations.
- **Not multimodal-first.** Text in, text out, with blob attachments that a specific agent's tools may interpret.
- **Not stable.** This is a hobby prototype with no external users. Breaking changes land without deprecation shims or compatibility ceremony.

## Repository layout

```
huggr/
├── crates/
│   ├── huggr-core/          # the sans-IO brain: log, projection, op table, reducer (no tokio, reqwest, or fs)
│   ├── huggr-host/          # native tokio host: driver loop, capability/model registries, MCP client
│   ├── huggr-providers/     # OpenAI-compatible streaming model adapter
│   ├── huggr-replay/        # trace format, content-addressed blob store, replay/verify/inspect
│   ├── huggr-agent/         # huglet runtime: Ask/Answer, resume/fork, scratchpad, blobs, limits, cost
│   ├── huggr-toolkit/       # manifests, the tool library, and the `huggr` CLI (new/run/build/traces/replay/verify)
│   ├── huggr-wasm/          # WASM bindings around huggr-core for browser/JS hosts
│   └── huggr-python/        # PyO3 runtime embedding (built by maturin from bindings/python)
├── bindings/
│   ├── python/             # the `huggr-agents` Python package
│   └── typescript/         # the `huggr-agents` TypeScript package (Node + browser)
├── examples/
│   ├── huglet-docs/          # the reference docs-Q&A agent crate with a typed response contract
│   ├── huglet-weather/       # the beginner agent; source of the `huggr new --template weather` scaffold
│   ├── huglet-insights/      # offline self-improvement agent over traces and feedback
│   ├── huglet-datasmith/     # docs-QA dataset synthesizer with a typed QaDataset contract
│   ├── hf-librarian/       # Python pipeline: datasmith wheel, jailed Hub publisher, judge-graded eval
│   └── chrome-extension/   # a concrete browser host: chrome.* capabilities, side-panel UI, MV3
└── docs/                   # reference documentation, guides, and tutorials
```

## Documentation

- [Overview](docs/overview.md): vision, goals, non-goals, and the huglet model.
- [Agents](docs/agents.md): defining, running, building, composing, and embedding agents; the manifest; tools vs capabilities.
- [Runtime](docs/runtime.md): the sans-IO design, core and host contract, determinism, and replay.
- [Security](docs/security.md): the security model and threat notes for each capability.
- [Built-in capabilities](docs/capabilities.md): every toolkit grant, option, limit, and trust boundary.
- [Project structure](docs/project-structure.md): crate boundaries, dependency rules, and standards positioning.
- [Reference](docs/reference.md): open questions, glossary, and naming.

The [guides](docs/guides/README.md) are runnable introductions to each surface: [the CLI](docs/guides/01-first-agent-cli.md), [typed responses](docs/guides/02-typed-responses-and-hooks.md), [a Chrome extension](docs/guides/03-first-chrome-extension.md), [an agent binary from Python](docs/guides/04-agent-binary-from-python.md), [agents in pure Python](docs/guides/05-agent-entirely-in-python.md), [agents in TypeScript](docs/guides/06-agent-entirely-in-typescript.md), [composition and cost](docs/guides/07-composition-and-cost.md), and [traces and replay](docs/guides/08-traces-replay-debugging.md).

The [tutorials](docs/tutorials/README.md) are self-contained, end-to-end walkthroughs with real outputs. Start with [a docs Q&A dataset, published to the Hub](docs/tutorials/docs-qa-dataset-pipeline.md).

## Building and testing

```bash
cargo build --workspace
cargo test                  # unit + scripted/determinism + end-to-end tests
cargo clippy --all-targets
cargo fmt --all
cargo tree -p huggr-core     # audit: must stay free of tokio/reqwest/fs
```

Notable tests: `huggr-core/tests` (scripted sessions + deterministic replay), `huggr-host/tests/end_to_end.rs` (real engine, tools, MCP, record/replay/resume), and the ignored slow gates `cargo test -p huggr-toolkit --test conformance -- --ignored` / `--test build_cli -- --ignored` (compile a real agent binary and check the in-process crate, built CLI, and MCP surfaces agree; the generated Python, typed Node/browser, and Chrome surfaces are not yet in this gate).

## License

Licensed under Apache-2.0.
