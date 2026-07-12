# Overview

## Vision

Huggr builds **domain-specific huglets**: small, specialized agents that handle focused tasks. Examples include answering questions about a documentation folder, reading PDFs, or fetching live data from an allowlisted API.

An orchestrator, whether a human, script, or larger agent, calls them through **one uniform contract**. A question goes in, and a structured response comes out with cost, duration, and a resumable trace id.

**A huglet is a small Rust crate plus a system prompt and a set of tools with privileges. Huggr turns that crate folder into a self-contained binary** with built-in traces, forking, sandboxing, and cost accounting.

Why domain-specific huglets:

- **Token efficiency.** A huglet with 5 tools and a 200-line system prompt is cheaper and more reliable than a generalist with 50 tools. The orchestrator pays one tool call's worth of context to invoke it instead of loading the entire domain.
- **Security by construction.** A huglet that never registers `shell` cannot run shell commands. Privileges are declared in the agent manifest and enforced by what the host registers, rather than by a runtime policy.
- **Composability.** Every huglet exposes the same ask/answer contract, so orchestrators can compose them without per-agent glue. Because that contract is tool-shaped, **a Huggr agent is a tool**: one agent grants another in its manifest (`[tools.agent.<name>]`) and calls it like any capability. See [Agents as tools](../reference/agents.md#agents-as-tools).
- **No vendor lock-in.** huglets are artifacts that you run locally, in CI, or in a container. The runtime is a small library, not a service.

## Goals and non-goals

Goals:

- **Trivial to define.** A new huglet is a human-readable, auditable Rust crate folder: a manifest, a system prompt, tool selections from a predefined library, and optional typed response/hooks/capability code in the same crate.
- **Self-contained to ship.** `huggr build` produces one standalone CLI binary per agent; the same binary is an MCP server via `--mcp-serve`, and `--surface python` also generates a typed wheel.
- **One invocation contract.** Every huglet accepts a question + optional metadata and returns a structured response + mandatory metadata (status, cost, duration, tokens, trace id). Orchestrators never learn per-agent APIs.
- **Resumable and forkable by default.** Every completed turn persists an immutable trace with a `trace_id` and an optional `depends_on` parent. Passing a `trace_id` back resumes that context; passing an older one forks a sibling branch.
- **Sandboxed by default.** A huglet gets a private scratchpad and only the optional tools it declares. Blob exchange with the caller is explicit.
- **Deterministic and replayable.** Any session can be replayed bit-for-bit for testing, debugging, and resume because the [core is sans-IO](runtime.md).
- **One way to do each thing.** One run path per stage (dev: `huggr run`; ship: a generated surface) and one trace format. External processes require explicit MCP, shell, delegation, or agent grants. Breaking changes are acceptable; there is no backward-compatibility ceremony.

Non-goals:

- **A general-purpose coding or browser agent as the core abstraction.** Huggr defines the callee side; generalists are usually orchestrators that call Huggr agents. Edge hosts may still package a concrete generalist experience when the runtime boundary stays clean. The Chrome extension in `examples/chrome-extension` is the browser-host example.
- **A hosted runtime or marketplace.** Huggr ships artifacts, while the operator chooses where to run them.
- **A universal agent-to-agent wire protocol.** MCP is the current adapter. Others, such as A2A, can be added at the edge if needed, but are not foundations.
- **Multimodal-first.** Text-in/text-out with blob attachments is the contract; images/audio ride as blobs a specific agent's tools may interpret.

## What a huglet is

A huglet consists of **(1) a system prompt and (2) a list of tools with associated privileges**. That pair makes it domain-specific. Every huglet also receives the following shared infrastructure:

1. **A scratchpad:** a private filesystem subtree that the agent can read and write without permission round trips or access outside its root.
2. **Traces:** every completed turn is stored as a replayable trace with a `trace_id`. Follow-up questions resume it, and older ids fork it. See [the Ask and Answer contract](../reference/agents.md#the-ask-and-answer-contract).
3. **The brain:** the same `huggr-core` reducer, including the turn loop, context projection, and deterministic replay. See [Runtime](runtime.md).
4. **A common API:** invocation (`ask`), asynchronous feedback (`feedback` keyed to a trace), and introspection (`--describe`: name, tools, tiers, pricing, context policy, limits; `--config`: effective runtime configuration and response schema without secret values; `--traces`: stored lineage).
5. **Blob exchange:** a caller can give files to the agent and receive files back. Large payloads use the content-addressed blob store.
6. **Accounting:** every response carries cost (from per-tier pricing config) and duration, folded from the trace's per-op metadata.
7. **Composition:** any built Huggr agent can be granted to another as an ordinary tool. The child's cost folds into the caller's metadata. See [Agents as tools](../reference/agents.md#agents-as-tools).

The manifest and prompt are data. The agent crate owns any typed contract or custom Rust wiring, and the toolkit provides the infrastructure.
