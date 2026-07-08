# Hugr

> **Build your subagent, ship it anywhere.** A toolkit for tiny, self-contained, domain-specific agents on a runtime-free, sans-IO Rust core.

A **subagent** is, at its essence, a system prompt plus a set of tools with declared privileges. Hugr turns that definition folder into **one self-contained binary** — which also serves MCP via `--mcp-serve` — with the shared infrastructure every subagent needs built in:

- **One invocation contract.** A question (string) in; a structured response object + mandatory metadata out — status, **cost**, **duration**, tokens, and a **trace id**. Every Hugr agent, every surface, same shape. Errors are answers (`status: "error"`, exit 0), so callers branch on data, not exceptions.
- **Resumable & forkable traces.** Every run persists an immutable trace. Pass its `trace_id` back to continue the conversation; pass an *older* id to fork a sibling branch. Orchestrators explore many directions without ever growing one shared context — replay is instant and bit-for-bit deterministic.
- **Sandboxed by construction.** An agent registers exactly the tools its manifest grants. No shell granted = no shell exists in the binary (and the tool library is exec-free). Plus a private, jailed scratchpad and blob exchange with the caller.
- **Token-efficient by design.** A handful of domain tools and a focused prompt, not fifty generic ones. Small agents are cheaper, faster, and more reliable — and an orchestrator pays one tool call to use them.
- **Agents compose.** A built Hugr agent *is* a tool: grant one to another with a manifest line (`[tools.agent.<name>] artifact = "..."`) and it's called like any capability — delegation never widens privileges, and the child's cost folds into the caller's metadata.

The reference agent, [`hugr-docs`](crates/hugr-docs/), is a checked-in docs-Q&A definition folder run and built by `hugr-toolkit`.

There are exactly two docs: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) (design, architecture, threat model — the spec) and [`docs/ROADMAP.md`](docs/ROADMAP.md) (progress log + work plan).

## Quickstart

```bash
cargo run -p hugr-toolkit --bin hugr -- new my-agent            # scaffold a definition
export HUGR_API_KEY=hf_...                                       # whatever the manifest's api_key_env names
cargo run -p hugr-toolkit --bin hugr -- run my-agent "question" # interpret it (dev loop)
cargo run -p hugr-toolkit --bin hugr -- build my-agent          # ship it: one standalone binary
./my-agent/dist/my-agent "question"                              # answers; --trace <id> resumes; --mcp-serve serves MCP
```

Every built binary self-describes: `--describe` (tools, privileges, tiers, pricing, limits), `--config` (the parsed manifest, secrets redacted), `--traces` (stored lineage).

## What a subagent definition looks like

```
my-agent/
  hugr.toml          # name, model tiers + pricing, tool grants + scopes, limits
  SYSTEM.md          # the system prompt
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

Auditable by reading: the manifest *is* the blast radius. Unknown keys are hard errors, so a typo can't silently widen or narrow it. The tool library today: `fs_read` (six read-only `fs_*` tools), `http_fetch`, `sqlite_query`, the scratchpad — plus `[tools.mcp.<name>]` (the one external-process escape hatch) and `[tools.agent.<name>]` (another built agent as a tool).

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
  hugr-core/          # the sans-IO brain — log, projection, op table, reducer.
                      #   NO tokio, NO reqwest, NO fs.
  hugr-host/          # native tokio host: engine driver loop, capability/model
                      #   registries, MCP stdio client, JSON-line framing.
  hugr-providers/     # OpenAI-compatible streaming adapter (retries inside).
  hugr-replay/        # trace format + content-addressed blob store +
                      #   replay/verify/inspect.
  hugr-agent/         # the subagent runtime: Ask/Answer, trace store with
                      #   trace_id/depends_on + fork, scratchpad, blobs, limits,
                      #   cost accounting, agent-as-tool (subprocess).
  hugr-toolkit/       # definitions (hugr.toml + SYSTEM.md), the tool library,
                      #   and the `hugr` CLI: new/run/build/traces/replay/verify.
  hugr-docs/          # the reference subagent (docs Q&A): definition folder
                      #   only; run/build it with hugr-toolkit.
```

## The reference subagent: `hugr-docs`

One folder in, one question in, one JSON response out — with cost metadata. No shell, no writes, no network tool; the read-only, folder-jailed `fs_*` tools.

```bash
export HUGR_DOCS_API_KEY=hf_...   # or any OpenAI-compatible endpoint key
cargo run -p hugr-toolkit --bin hugr -- run crates/hugr-docs/definition ./docs "What is the narrow-waist rule?" | jq
```

```json
{
  "status": "success",
  "response": {
    "response": {
      "summary": "The narrow-waist rule is ..."
    },
    "related_documents": ["docs/ARCHITECTURE.md"]
  },
  "trace_id": "1e4f7d0a9b2c3d44",
  "metadata": { "duration_ms": 1234, "tokens_in": 1000, "tokens_out": 200, "cost_micro_usd": 1300, "model_calls": 2, "tool_calls": 3 }
}
```

The docs root is runtime config, not a compiled-in scope: `hugr run crates/hugr-docs/definition ./docs "..."` and `hugr run crates/hugr-docs/definition ./other-docs "..."` use the same definition with different read jails. Build it with `hugr build crates/hugr-docs/definition`; Python and other languages consume the built binary via subprocess or `--mcp-serve`.

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

Licensed under either of Apache-2.0 or MIT at your option.
