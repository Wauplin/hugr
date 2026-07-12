# Guide plan

This file tracks the guides still to write under `docs/guides/`. Guides 1 through 9 cover the surfaces (CLI, typed responses, browser, Python, TypeScript), composition and cost, traces and replay, and context compaction. The features below have reference documentation but no hands-on guide yet. Each entry becomes one guide in the style of [context compaction](09-context-compaction.md): problem first, mechanism, configuration, worked example, limitations. Remove an entry when its guide lands.

## Planned guides

1. **Tool grants and jails** (`10-tool-grants-and-jails.md`). Sandbox-by-registration in practice: granting and scoping `fs_read`/`fs_write`, restricted versus full `shell`, `web_fetch` allowlists, `web_search`, `traces_read`, and how each jail holds against adversarial arguments.
2. **Skills** (`11-skills.md`). The Agent Skills folder format, progressive disclosure via `skill_read`, definition-owned versus runtime skills (`Ask.skills`, `--skill`), validation rules, and the trust model.
3. **Files and state: blobs, scratchpad, and memory** (`12-blobs-scratchpad-memory.md`). Inbound and outbound blob exchange, the shared content-addressed store, per-lineage scratch with copy-on-fork, and opt-in durable memory.
4. **Models, tiers, and pricing** (`13-models-tiers-pricing.md`). The `[models]` block, free-form tier names and selectors, per-tier pricing and cost accounting, adapter retry rules, and transport versus semantic errors.
5. **Limits and unattended runs** (`14-limits-and-cron.md`). Opt-in `[limits]`, errors as answers with partial traces, `[cron.<name>]` jobs, `fresh` versus `chain` lineage, per-job limit overrides, and the uncapped-job refusal.
6. **Serving and consuming MCP** (`15-mcp.md`). Exposing a built agent as an MCP server with `--mcp-serve`, the `ask` and `feedback` tools, and granting external MCP servers with `[tools.mcp.<name>]`.
7. **Runtime arguments** (`16-runtime-args.md`). Invocation-time configuration with `[runtime.args.<name>]`: manifest target patching, positional and required arguments, environment fallbacks, and how each surface exposes them.
8. **Streaming and events** (`17-streaming-and-events.md`). The shared `AgentEvent` vocabulary: `--stream` on the CLI binary, `ask_events` in Rust, `agent.run(...)` in Python and TypeScript, and why events are host-layer observations outside the trace.

## Covered elsewhere, no separate guide

- Typed response contracts and hooks: [guide 2](02-typed-responses-and-hooks.md).
- Agents as tools, delegation, feedback, `huggr stats`: [guide 7](07-composition-and-cost.md).
- Trace anatomy, replay, verify: [guide 8](08-traces-replay-debugging.md).
- Context compaction and pruning: [guide 9](09-context-compaction.md).
- The security model and per-capability threat notes stay in [the reference](../security.md); guide 10 links to them instead of restating.
- Custom storage backends and custom `TurnPolicy` implementations are advanced host extension points documented in [runtime](../runtime.md); a guide can follow if they stabilize.
