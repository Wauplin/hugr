# Hugr tutorials

Narrative, hands-on on-ramps — one per surface. Each tutorial is standalone and runnable top to bottom; read them in order for the full tour, or jump to the surface you care about. These teach; [`ARCHITECTURE.md`](../../ARCHITECTURE.md) is the spec and holds the rationale.

1. [Your first agent from the CLI](01-first-agent-cli.md) — scaffold, manifest anatomy, run, resume/fork, build one standalone binary.
2. [Typed responses and hooks](02-typed-responses-and-hooks.md) — Rust response contracts, model-facing types, answer hooks, with `hugr-docs` as the worked example.
3. [Your first Chrome extension](03-first-chrome-extension.md) — build your own browser host from the reusable WASM + TypeScript packages.
4. [An agent binary from Python](04-agent-binary-from-python.md) — ship a built agent as a typed Python wheel, subprocess, or MCP server.
5. _(pending)_ An agent entirely in Python — define agents and tools in Python on the same Rust runtime.
6. _(pending)_ An agent entirely in TypeScript — the TS runtime API in Node and the browser.
7. [Composition and cost](07-composition-and-cost.md) — agents as tools, zero-copy blob passing, feedback, `hugr stats`.
8. [Traces, replay, and debugging](08-traces-replay-debugging.md) — trace anatomy, `hugr replay --step`, `verify`, cron, and the insights workflow.
