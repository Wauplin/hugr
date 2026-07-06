# Hugr

> **Build your subagent, ship it anywhere.** A toolkit for tiny, self-contained, domain-specific agents on a runtime-free, sans-IO Rust core.

A **subagent** is, at its essence, a system prompt plus a set of tools with declared privileges. Hugr turns that definition into a self-contained artifact — a single binary, a Rust crate, a Python module, or an MCP server — with the shared infrastructure every subagent needs built in:

- **One invocation contract.** A question (string) in; an answer (string) + mandatory metadata out — status, **cost**, **duration**, tokens, and a **trace id**. Every Hugr agent, every surface, same shape.
- **Resumable & forkable traces.** Every run persists an immutable trace. Pass its `trace_id` back to continue the conversation; pass an *older* id to fork a sibling branch. Orchestrators explore many directions without ever growing one shared context — replay is instant and bit-for-bit deterministic.
- **Sandboxed by construction.** An agent registers exactly the tools its manifest grants. No shell granted = no shell exists in the binary. Plus a private, jailed scratchpad and permissioned blob exchange with the caller.
- **Token-efficient by design.** Five domain tools and a focused prompt, not fifty generic ones. Small agents are cheaper, faster, and more reliable — and an orchestrator pays one tool call to use them.
- **Agents compose.** A Hugr agent *is* a tool: grant one to another with a manifest line (`[tools.agent.<name>]`) and it's called like any capability — privileges only narrow on the way down, and the child's cost folds into the caller's answer.
- **Orchestrator-defined resource grants.** An orchestrator declares named resource groups (a folder, blobs, a database) and passes each subagent a per-group grant — read, read-write, or nothing. Grants ride the ask, are recorded in the trace, and deterministically decide which tools even get registered.

The first such agent, [`hugr-docs`](crates/hugr-docs/) (a self-contained docs-Q&A agent with a CLI and Python binding), is the template the toolkit generalizes.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the vision, design, and threat model, and [`docs/ROADMAP.md`](docs/ROADMAP.md) for progress and the work plan.

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

Auditable by reading: the manifest *is* the blast radius. Then (per the roadmap): `hugr run my-agent "question"` to interpret it, `hugr build --surface cli,python,mcp` to ship it.

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
  hugr-host/          # native host: tokio driver, capability/model registries,
                       #   policies, MCP client, skills, scheduler.
  hugr-providers/     # OpenAI-compatible streaming adapter (HF router default).
  hugr-replay/        # versioned trace format + content-addressed blobs +
                       #   replay/verify/inspect/resume.
  hugr-plugin-abi/    # versioned plugin contract + subprocess transport.
  hugr-example-plugin/# a standalone third-party plugin (no Hugr dependency).
  hugr-docs/          # the prototype subagent (docs Q&A; CLI + Python).

  hugr-agent/         # NEW (roadmap T0): the common subagent API — Ask/Answer,
                       #   trace store with trace_id/depends_on, scratchpad, blobs.
  hugr-toolkit/       # NEW (roadmap T1–T2): declarative definitions, the
                       #   predefined tool library, and `hugr new/run/build`.

  hugr-cli/           # PARKED: the general coding-agent CLI (regression host).
  hugr-wasm/          # PARKED: browser/WASM host + Chrome extension (regression host).
```

`hugr-cli` and `hugr-wasm` proved the core runs anywhere (the same brain drove a terminal coding agent and a no-backend Chrome extension); they stay compiling as regression hosts but receive no product work during the subagent pivot.

## The prototype subagent: `hugr-docs`

One folder in, one question in, one JSON answer out — with cost metadata. No shell, no writes, no network tool; seven read-only, folder-jailed docs tools.

```bash
export HUGR_DOCS_API_KEY=hf_...   # or any OpenAI-compatible endpoint key
cargo run -p hugr-docs -- ./archive-light-2026-07-01 "Which repositories do I watch by default?" | jq
```

```json
{
  "status": "success",
  "message": "By default, you'll be watching all the organizations you are a member of...",
  "related_documents": ["hub/notifications.md"],
  "metadata": { "elapsed_ms": 1234, "tokens_in": 1000, "tokens_out": 200, "estimated_cost_micro_usd": 1300, "model_calls": 2, "tool_calls": 3, "...": "..." }
}
```

It also ships a Python binding: `hugr_docs.answer("...", docs_path="...")` returns the same dict and never raises for run failures. See [`crates/hugr-docs/README.md`](crates/hugr-docs/README.md).

## The demo target (roadmap T4)

Four differently-privileged subagents answering one cross-domain question — *"Which of last month's expenses violate our travel policy, and by how much?"*:

| Agent          | Tools (privileges)                          |
| -------------- | ------------------------------------------- |
| `policy-docs`  | `fs_read` jailed to the policy folder       |
| `receipts`     | `pdf_read` on blobs handed in by the caller |
| `ledger`       | `sqlite_query` (read-only) on `expenses.db` |
| `report-writer`| scratchpad only — no external reads at all  |

A ~200-line orchestrator delegates to them, resumes one thread by `trace_id`, forks a what-if branch, and prints a per-agent cost table.

## Building & testing

```bash
cargo build --workspace
cargo test                  # unit + scripted/determinism + end-to-end tests
cargo clippy --all-targets
cargo fmt --all
cargo tree -p hugr-core     # audit: must stay free of tokio/reqwest/fs
```

Notable tests: `hugr-core/tests` (scripted sessions + deterministic replay), `hugr-host/tests/end_to_end.rs` (real engine, capabilities, sub-agents, resume, MCP), `hugr-replay` (trace round-trips, recursive child verification).

## License

Licensed under either of Apache-2.0 or MIT at your option.
