# Reference

## Open questions

- **Trace schema migration.** Long-lived traces need a migration story as `Record`/`Event` evolve (`format_version` exists; migrations do not).
- **Trace garbage collection.** Fork trees accumulate. The pruning policy is undecided; delete traces manually for now.
- **Concurrent asks on one agent.** By default, each ask is an independent session or process, which traces make safe. A serving mode with a session pool is future work.
- **Browser packaging.** The split is done (generic `hugr-wasm` bindings + `bindings/typescript` + the Chrome-extension example with a vendor/pkg build script); what remains open is typed TS packaging and store-signed distribution.

## Glossary

- **Subagent / agent:** a packaged Hugr artifact: agent crate (prompt + tools + config + optional Rust wiring) + runtime, exposing the ask/answer contract.
- **Brain / core:** the pure, sans-IO state machine (`hugr-core`).
- **Host:** the environment-specific layer that performs IO and drives the brain (`hugr-host`).
- **Agent crate folder:** the auditable agent source folder (`Cargo.toml`, `hugr.toml`, `SYSTEM.md`, optional Rust code).
- **Ask / Answer / Feedback:** the uniform invocation contract: question + metadata in; structured response + mandatory metadata out; optional opaque caller feedback appended later by trace id.
- **Trace:** the durable, replayable event log of one session, identified by `trace_id` and optionally rooted on a parent via `depends_on`.
- **Fork:** starting a new session from an existing trace's log. The parent is immutable.
- **Scratchpad:** the agent's private filesystem subtree, writable without gates and jailed to its root.
- **Capability / tool:** a host-provided implementation of an effect, granted to an agent in its manifest. A built Hugr agent can itself be granted as a tool.
- **Event / Command / Op / Projection / Policy:** the core vocabulary described in [Runtime](runtime.md).

## Name

**Hugr** is Old Norse for "mind, thought, inner intent": a small, portable agent mind that runs inside many bodies. Pronounced **HUG-er**. Crates follow `hugr-<area>`; the CLI reads naturally as `hugr run`.
