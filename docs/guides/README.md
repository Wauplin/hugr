# Huggr guides

These hands-on guides cover one surface each. Every guide is standalone and runnable from start to finish. Read them in order for the full sequence, or choose the surface you need. The [reference documentation](../README.md) contains the design rationale.

1. [Your first agent from the CLI](01-first-agent-cli.md); scaffold, manifest anatomy, run, resume/fork, build one standalone binary.
2. [Typed responses and hooks](02-typed-responses-and-hooks.md); Rust response contracts, model-facing types, answer hooks, with `huglet-docs` as the worked example.
3. [Your first Chrome extension](03-first-chrome-extension.md); build your own browser host from the reusable WASM + TypeScript packages.
4. [An agent binary from Python](04-agent-binary-from-python.md); ship a built agent as a typed Python wheel, subprocess, or MCP server.
5. [An agent entirely in Python](05-agent-entirely-in-python.md); define agents and tools in Python on the same Rust runtime.
6. [An agent entirely in TypeScript](06-agent-entirely-in-typescript.md); the TS runtime API in Node and the browser.
7. [Composition and cost](07-composition-and-cost.md); agents as tools, zero-copy blob passing, feedback, `huggr stats`.
8. [Traces, replay, and debugging](08-traces-replay-debugging.md); trace anatomy, `huggr replay --step`, `verify`, cron, and the insights workflow.
9. [Context compaction and pruning](09-context-compaction.md); why contexts grow, forget rules, the deterministic budget pass, summarization, and every `[context]` knob.

For a self-contained, end-to-end walkthrough that composes several agents into a working pipeline, see [the tutorials](../tutorials/README.md).
