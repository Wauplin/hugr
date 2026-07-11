# Hugr documentation

Hugr is a toolkit for building small, self-contained, domain-specific subagents on a runtime-free, sans-IO Rust core.

## Concepts and architecture

- [Overview](overview.md): vision, goals, non-goals, and the subagent model.
- [Agents](agents.md): defining, running, building, composing, and embedding agents.
- [Project structure](project-structure.md): crate boundaries, dependency rules, and standards positioning.
- [Runtime](runtime.md): the sans-IO design, core and host contract, state, concurrency, providers, replay, and risks.
- [Security](security.md): the security model and threat notes for each capability and host extension point.
- [Reference](reference.md): open questions, glossary, and naming.

## Tutorials

The [tutorials](tutorials/README.md) provide runnable introductions to each supported surface.

## Guides

Self-contained, end-to-end walkthroughs that compose multiple agents into working pipelines. Start with [a docs Q&A dataset, published to the Hub](guides/docs-qa-dataset-pipeline.md): a Rust data-synthesis specialist, a jailed Python publisher, and a judge-graded eval, with real outputs from a full run.
