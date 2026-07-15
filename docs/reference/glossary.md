# Glossary and open questions

## Open questions

- **Trace schema migration.** Long-lived traces need a migration story as `Record`/`Event` evolve (`format_version` exists; migrations do not).
- **Trace garbage collection.** Fork trees accumulate. The pruning policy is undecided; delete traces manually for now.
- **Concurrent asks on one agent.** By default, each ask is an independent session or process, which traces make safe. A serving mode with a session pool is future work.
- **Browser packaging.** The split and typed TypeScript package are done; store-signed extension distribution remains open.

## Glossary

- **Huglet / agent:** a packaged Huggr artifact: agent crate (prompt + tools + config + optional Rust wiring) + runtime, exposing the ask/answer contract.
- **Brain / core:** the pure, sans-IO state machine (`huggr-core`).
- **Host:** the environment-specific layer that performs IO and drives the brain (`huggr-host`).
- **Agent crate folder:** the auditable agent source folder (`Cargo.toml`, `huggr.toml`, `SYSTEM.md`, optional Rust code).
- **Ask / Answer / Feedback:** the uniform invocation contract: question + metadata in; structured response + mandatory metadata out; optional opaque caller feedback appended later by trace id.
- **Trace:** the durable, replayable event log of one session, identified by `trace_id` and optionally rooted on a parent via `depends_on`.
- **Fork:** starting a new session from an existing trace's log. The parent is immutable.
- **Scratchpad:** the agent's private filesystem subtree, writable without gates and jailed to its root.
- **Tool:** the model-facing view of an effect: a manifest grant that advertises one or more named, schema-described functions to the model. A built Huggr agent can itself be granted as a tool.
- **Capability:** the host-side implementation behind a tool, registered in the host's capability registry and invoked when the brain emits `StartCapability`. See [Tools and capabilities](agents.md#tools-and-capabilities).
- **Event / Command / Op / Projection / Policy:** the core vocabulary described in [Runtime](../concepts/runtime.md).
