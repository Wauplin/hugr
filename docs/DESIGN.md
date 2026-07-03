# Design Document

> **Name:** `Hugr` (published as `hugr-rs`; see `BRANDING.md`). **Build your subagent, ship it anywhere.** A toolkit for building tiny, self-contained, domain-specific agents on a runtime-free, sans-IO Rust core.

## 1. Vision

Hugr is a toolkit for building **domain-specific subagents**: small, specialized agents that do one thing well — answer questions about a docs folder, read PDFs, query a SQLite database, edit images — and that an orchestrator (a human, a script, or a bigger agent) can call through **one uniform contract**: a question in, an answer out, with cost, duration, and a resumable trace id attached.

The pitch in one sentence: **a subagent is a system prompt plus a set of tools with privileges; Hugr turns that definition into a self-contained artifact you can ship anywhere** — a single binary, a Rust crate, a Python module, or an MCP server — with traces, forking, sandboxing, and cost accounting built in and identical across all of them.

This is a deliberate pivot from "general agent harness with showcase apps" (a coding-agent CLI, a Chrome extension) to "the best way to build and ship *specialized* subagents". The `hugr-docs` crate proved the shape: a single binary, no shell access, seven read-only tools, one JSON answer with cost metadata, callable from a CLI or Python. Hugr generalizes that shape into a toolkit so the *next* subagent — a PDF expert, a SQLite reader, an image editor — costs a config file, not a Rust project.

### Why domain-specific subagents

- **Token efficiency.** A subagent with 5 tools and a 200-line system prompt is dramatically cheaper and more reliable than a generalist with 50 tools. The orchestrator pays one tool-call's worth of context to invoke it, not the whole domain's.
- **Security by construction.** A subagent that never registers `shell` *cannot* run shell commands. Privileges are declared in the agent definition and enforced by what the host registers — not by a runtime policy trying to say "no" fast enough.
- **Composability.** Because every subagent exposes the same ask/answer contract, orchestrators compose them without per-agent glue: same invocation, same metadata, same trace semantics. And because that contract is tool-shaped, **a Hugr agent *is* a tool**: one agent grants another in its manifest (`[tools.agent.<name>]`) and calls it like any capability — privileges compose downward only, and the child's cost folds into the caller's answer (`ARCHITECTURE.md` §20.5).
- **No vendor lock-in.** Platform agent frameworks solve packaging by owning the runtime (their cloud). Hugr subagents are self-contained artifacts *you* run — locally, in CI, in a container, behind an API — because the runtime is a small library, not a service.

## 2. Goals

- **Trivial to define.** A new subagent is a human-readable, auditable config folder: a manifest, a system prompt, tool selections from a predefined library, model tiers + pricing. No Rust required for the common case.
- **Self-contained to ship.** One `hugr build` produces the chosen surfaces — standalone CLI binary, Rust crate, Python module, MCP server — all wrapping the same agent. Surface selection is a build-time choice, not part of the agent's definition.
- **One invocation contract.** Every subagent accepts a question (string) + optional metadata and returns an answer (string) + mandatory metadata (status, cost, duration, tokens, trace id). Orchestrators never learn per-agent APIs.
- **Resumable and forkable by default.** Every run persists a trace with a `trace_id` and an optional `depends_on` parent. Passing a `trace_id` back resumes that context; passing an older one forks a sibling branch. Replay is instant and bit-for-bit deterministic because of the sans-IO core.
- **Sandboxed by default.** A subagent gets a private scratchpad filesystem and exactly the tools it declares. Blob exchange with the caller is explicit and permissioned.
- **Deterministic and replayable.** Unchanged from the original design: any session can be recorded and replayed bit-for-bit for testing, debugging, and resume-after-crash.
- **Runtime-free core.** The brain stays sans-IO, tiny, and embeddable — this is what makes "ship it anywhere" (including WASM later) true rather than aspirational.

## 3. Non-Goals (for now)

- **A general-purpose coding agent or browser agent.** The `hugr-cli` coding agent and the `hugr-wasm` Chrome extension are **parked** — kept compiling as regression hosts for the core, but receiving no product investment. They may return later as "just another packaged agent".
- **A hosted runtime or marketplace.** Hugr ships artifacts; where they run is your business.
- **A universal agent-to-agent network protocol.** We expose adapters to existing standards (MCP today; ACP/A2A-shaped surfaces when they stabilize — see §8) rather than inventing a wire protocol.
- **Multimodal-first.** Text-in/text-out with blob attachments is the v1 contract; images/audio ride as blobs a specific agent's tools may interpret.
- **Distributed orchestration.** Hugr defines the *callee* side (the subagent). Orchestrators are out of scope beyond a reference example in the demo.

## 4. The core thesis: separate state, context, IO, and policy

Unchanged, and it is exactly why the pivot is cheap. Most harness pain traces back to conflating four things that should be separate:

| Concern           | The trap (what harnesses do)                      | What we do                                                 |
| ----------------- | ------------------------------------------------- | ---------------------------------------------------------- |
| **Durable state** | The flat `messages[]` list *is* the state         | Append-only **event log** is the source of truth           |
| **Model context** | Same `messages[]` is sent to the model            | Context is a **projection** rendered from the log per turn |
| **IO**            | The loop owns tokio, sockets, fs, shell           | **Sans-IO** core emits commands; the **host** does IO      |
| **Permissions**   | `if dangerous { prompt() }` scattered in the loop | Policy is **externalized data**, decided outside the core  |

Every headline feature of the subagent pivot is a direct payoff of these separations:

- **Trace = the log made durable.** A subagent's session is already an ordered event stream; `trace_id` is just a name for the saved file.
- **Resume = re-fold a trace.** Passing `trace_id` back means loading the log and folding it into a fresh brain — zero IO, no model re-calls, instant.
- **Fork = copy a log prefix.** `depends_on` is exactly the fork primitive (`AgentSeed::ForkAt`) already built in Phase 6. Sibling explorations share a prefix and diverge.
- **Sandbox = what the host registers.** No privileged built-ins means "this agent has no shell" is a fact about registration, not a policy hope.
- **Cost = fold over `OpMeta`.** Per-op usage/latency already lives on the log; an answer's cost metadata is arithmetic over the trace.

Nothing in the pivot requires bending the core. That is the strongest possible validation of the original architecture.

## 5. What a subagent is

Trimmed to its essence, a subagent is **(1) a system prompt and (2) a list of tools with associated privileges**. That pair is what makes it domain-specific. Everything else is shared infrastructure every subagent gets for free:

1. **A scratchpad** — a private, self-contained filesystem subtree the agent can freely read/write (notes, intermediate artifacts) without permission round-trips and without escaping its root.
2. **Traces** — every run is stored as a replayable trace with a `trace_id`; follow-up questions resume it; older ids fork it (§6).
3. **The brain** — the same `hugr-core` reducer: turn loop, projection, compaction, deterministic replay.
4. **A common API** — introspection (`describe`: role, tools, model tiers, config; `config`/`env`: effective settings; `traces`: list stored traces) and invocation (`ask`), identical across every subagent and every surface.
5. **Blob exchange** — a caller can hand the agent files (bytes or paths) and receive files back, each with an explicit permission set (read/write/execute); large payloads ride the existing content-addressed blob store.
6. **Accounting** — every answer carries cost (from per-tier pricing config) and duration, aggregated from the trace's `OpMeta`.
7. **Resource grants** — an orchestrator can define named **resource groups** (a folder, a blob set, a database, a network host) and pass each subagent a grant per group — read, read-write, or nothing — with the grants riding the ask and recorded in the trace, so the same agent definition serves differently-privileged callers deterministically (`ARCHITECTURE.md` §18.5).
8. **Composition** — any Hugr agent can be granted to another as an ordinary tool (`ARCHITECTURE.md` §20.5); delegation attenuates privileges and aggregates cost, so agent trees stay auditable end-to-end.

The definition is data; the infrastructure is the toolkit. See `ARCHITECTURE.md` §18–§21 for the concrete contracts.

## 6. Traces, resume, and forking (the orchestration contract)

The interaction model between an orchestrator and a subagent:

- **New question, no `trace_id`** → the agent starts a fresh session, answers, persists the trace, and returns its new `trace_id` in the answer metadata.
- **Follow-up, with `trace_id`** → the agent loads that trace, re-folds it (instant, deterministic, no model calls), appends the new question as a new turn, answers, and persists the result as a **new trace** whose header records `depends_on: <parent trace_id>`. The parent is never mutated.
- **Fork** → because every follow-up produces a new immutable trace, the orchestrator can branch freely: ask from `trace_1` twice and get sibling traces `trace_2a`, `trace_2b`. The orchestrator explores many directions **without ever growing a single shared context** — each branch pays only for its own divergence, and storage shares the common prefix.

From the subagent's point of view this is trivially cheap: traces are stored independently with a `depends_on` field in their metadata, and "load context" is the existing replay fold. There is no session server, no lock, no shared mutable conversation. Immutability is what makes fan-out safe.

This model also gives orchestrators time-travel debugging for free: any intermediate trace is a first-class artifact that can be inspected, replayed, or used as a fork point later.

## 7. The toolkit: from config to artifact

The part that must become *easy* is defining and shipping a new subagent. Today it means writing a Rust host, glue, and build scripts (what `hugr-docs` did by hand). The toolkit collapses that to:

```
my-agent/
  hugr.toml          # manifest: name, description, model tiers + pricing, tool grants, limits
  SYSTEM.md          # the system prompt (plain markdown)
  tools/             # optional: extra tools beyond the built-in library
```

- **Predefined tool library.** The toolkit ships vetted, parameterized tools (scoped fs read/search/outline — generalized from `hugr-docs`; scratchpad; http fetch; sqlite query; more over time). Granting one is a manifest line with its scope/privileges (`[tools.fs_read] root = "./docs"`), not code.
- **Predefined tools stay exec-free by default.** There is no `shell` in the library; the one exec-class tool is a planned sandboxed `code_exec` (subprocess jail v1 — interpreter pinned in the manifest, cwd = scratchpad, no network, capped; WASM/WASI target) so "this agent can run code" is always a visible, reviewable manifest grant (`ARCHITECTURE.md` §20.2).
- **Custom tools without recompiling the toolkit.** Escape hatches in order of weight: **another Hugr agent** (`[tools.agent.<name>]` — composition as a config line, §5.8), an MCP server (config line), a subprocess plugin (the existing `hugr-plugin-abi`), or a Rust `Capability` impl for compile-in tools.
- **Auditable by reading.** The whole definition is human-readable text. Reviewing a subagent's blast radius = reading `hugr.toml`. If a tool isn't granted, the binary literally does not contain a path to it.
- **Build-time surface selection.** `hugr build --surface cli,python,mcp` bundles the agent definition with the runtime into the chosen artifacts. The agent definition never mentions surfaces — one definition, N packagings — because the common Rust API (§5.4) is the single thing every surface wraps.

The toolkit is a separate crate (`hugr-toolkit`) with its own CLI (`hugr new`, `hugr run`, `hugr build`, `hugr traces`, `hugr replay`). `hugr-core` stays a pure library underneath and never learns about manifests or bundling.

Two capstones follow from "definitions are data", designed now and deliberately built last (ROADMAP T6, `ARCHITECTURE.md` §22): a **machine-level agent registry** (installed agents publish their `AgentCard` to a well-known directory; `hugr agents list` and a gateway MCP server give an orchestrator the whole local fleet from one entry point — the registry is a cache over `describe()`, never an authority or a daemon), and **`hugr-builder`** — the Pi-style self-extension move: an ordinary subagent whose tools are the toolkit's own operations (scaffold/edit/validate/test-run/register), able to build new subagents on demand. Its v1 safety constraint: it emits pure-data definitions only (no native tools), may never grant a tool class it doesn't itself hold, and every generated agent carries `built_by` provenance — so generation never weakens the audit story.

## 8. Standards & prior art (what we align with, what we fill)

Researched July 2026 (web survey with sources); see `ROADMAP.md` for adapter sequencing.

- **MCP (Model Context Protocol)** is the pragmatic standard for exposing a subagent *as a tool* to orchestrators (Claude Code and most frameworks speak it). "Agent-as-an-MCP-tool" is an established pattern (e.g. Microsoft Agent Framework's `as_mcp_server()`). A Hugr subagent's MCP surface advertises one `ask` tool whose structured result carries the answer + metadata. Two spec facts to design against: the 2026-07-28 release candidate makes the protocol core **stateless** and adds a **Tasks** primitive for long-running work (map long asks onto it), and **sampling is deprecated** — a Hugr agent always calls its own provider, which we do anyway. MCP standardizes neither sessions/forking nor cost metadata; we carry both in the tool result payload.
- **ACP is two different things — and one of them is dead.** The IBM/BeeAI *Agent Communication Protocol* (Linux Foundation) **merged into A2A in August 2025** and is winding down; do not target it. The surviving ACP is Zed's *Agent Client Protocol* — JSON-RPC over stdio between an agent subprocess and an **editor/UI client** ("LSP for agents"; Rust-native reference implementation). Its `session/load`/`session/resume` overlaps with our resume but it has no fork primitive, no cost metadata, and no blob channel; it is an attractive *editor surface* adapter someday, not the orchestrator↔subagent contract.
- **A2A (Agent2Agent, Linux Foundation)** is the surviving agent↔agent standard: Agent Cards for discovery, a typed task lifecycle over JSON-RPC/SSE, first-class Artifacts (text/file/data parts — a clean match for our blob exchange). Its `contextId` is continuation, not forking, and **cost metadata is an open issue** (a2aproject/A2A#1155) pointed at the extensions mechanism — meaning Hugr can ship a usage extension rather than wait. A2A is the natural future surface for *remote* orchestration.
- **Claude Code subagents** (markdown files with YAML frontmatter: name, description, tools, model; body = system prompt) are the de facto *authoring* convention for "an agent is a prompt + tools as data" — no wire protocol, single-vendor, widely imitated. Our `hugr.toml` + `SYSTEM.md` adopts the same shape with typed scopes/pricing that frontmatter can't express; a frontmatter import path is a cheap compatibility win. In the Rust ecosystem (Rig, Swiftide, etc.) everything is code-first and tokio-bound; the closest declarative attempt (ai-agents.rs YAML) is early. Nobody is sans-IO.
- **The gap we fill** — verified unowned: (a) a **cross-process forkable session contract** (`trace_id`/`depends_on` with deterministic, bit-for-bit replay — LangGraph's checkpoint forking is in-process/framework-internal; OpenAI's `previous_response_id` forking is vendor-hosted with 30-day retention); (b) **mandatory cost/duration metadata on every answer** (MCP: absent; A2A: open issue; Zed-ACP: absent); (c) **single-binary agent packaging** (llamafile proved single-file distribution for *models*; nobody ships prompt+tools+harness subagents as standalone binaries). That combination *is* the product. Our own contract is the Rust API; protocols are adapters at the edge, per the narrow-waist rule.

## 9. Pain points the architecture already solved

(Condensed from the original design; the mechanisms are built and tested — see `ARCHITECTURE.md` for details.)

- **"The conversation is the state"** → event-sourced log + projection; lossless compaction; branching/forking. (§4)
- **IO baked into the loop** → sans-IO core; the same brain runs native, in WASM, or behind bindings.
- **Privileged built-in tools** → uniform capabilities; the sandbox story of §5 depends on this.
- **Permissions as control flow** → externalized policy; a docs agent with only read-only tools needs no judge at all.
- **Resume/replay as afterthoughts** → traces, deterministic replay, crash resume, all shipped (Phases 3/7).
- **Provider lock-in** → canonical model types with first-class cache/reasoning/tool-call fields; OpenAI-compatible streaming adapter shipped.
- **The synchronous LLM-centric loop** → op table, background ops, first-class cancellation (Phase 2).
- **Cost attribution** → `OpMeta` per op (usage, latency, routing) queryable from the trace alone — now surfaced as the answer's mandatory metadata.

## 10. Over-engineering guardrails

- **The common case is a config folder.** If defining a read-only docs agent requires writing Rust, the toolkit has failed. Measure: the demo agents (§ROADMAP Phase T4) must be definable in < 50 lines of manifest + a prompt.
- **Don't invent a protocol.** The contract is a Rust API + a JSON answer schema. Wire protocols (MCP, ACP, A2A) are adapters, added when demanded, never load-bearing.
- **Keep the answer schema small.** Status, message, trace_id, cost, duration, tokens, related refs, opaque extras. Resist per-agent structured-output schemas in v1 — that's what `extra` is for.
- **Don't abstract surfaces prematurely.** Ship CLI + Rust crate first (T2), Python next, MCP after; each surface is thin because it wraps the same `hugr-agent` API.

## 11. What earns attention

The demo moment is: **define four specialized agents in four config folders, `hugr build` them, and watch an orchestrator answer a cross-domain question by delegating — with per-agent cost lines and a fork tree you can replay**. Nobody else can show "here is my PDF expert as a 5 MB binary with no shell access, here is the same expert as `pip install`, and here is yesterday's conversation forked three ways for free."

## 12. Open questions

- **Trace schema migration.** Long-lived traces need a migration story as `Record`/`Event` evolve (`format_version` exists; migrations do not). Sharpened by the pivot: orchestrators will hold `trace_id`s across subagent upgrades. Tracked in ROADMAP T5.
- **Trace garbage collection.** Fork trees accumulate; `depends_on` forms a DAG whose shared blobs are content-addressed, but pruning policy (LRU? pinned roots?) is undecided.
- **Blob permission enforcement.** Read/write/execute grants on exchanged blobs are declared in the contract; enforcement depth (copy-on-share vs bind-mount vs advisory) is host-dependent and needs a per-surface decision (T3).
- **Concurrent asks on one agent.** The brain is single-session; the artifact may receive parallel `ask`s. Default: each ask is an independent session/process (traces make this safe); a serving mode with a session pool is future work.
- **How much of `TurnPolicy` to expose in the manifest.** Tier routing and compaction knobs as TOML vs "sane defaults only" — start with defaults, expose knobs on demand.
- **WASM surface.** The core still compiles to WASM; whether a *subagent artifact* targets WASM (e.g. an in-browser expert) is deferred until a real use case appears.
- **Resource-group enforcement depth.** V1 enforcement is registration + jail (a granted `FsRoot` becomes the tool's jail root); whether/when to add OS-level enforcement (read-only bind mounts, mount namespaces) per host is undecided — same axis as blob permission enforcement above.
- **Agent-as-tool schema surface.** How much of a child's `AgentCard` (extras schema, tool list) to inline into the generated tool schema — richer helps the calling model, but bloats the caller's context, against the token-efficiency budget (ROADMAP X3).
- **Self-extension limits.** How far to let `hugr-builder` go beyond pure-data definitions (generated MCP configs? generated Rust tool crates gated on human review?), and whether builder-made agents need a distinct trust tier in the registry beyond `built_by` provenance. Deliberately unanswered until T6.

## 13. Glossary

- **Subagent / agent** — a packaged Hugr artifact: definition (prompt + tools + config) + runtime, exposing the ask/answer contract.
- **Brain / core** — the pure, sans-IO state machine (`hugr-core`).
- **Host** — the environment-specific layer that performs IO and drives the brain (`hugr-host` natively).
- **Agent definition** — the auditable config folder (`hugr.toml`, `SYSTEM.md`, `tools/`).
- **Surface** — a build-time packaging of an agent: CLI, Rust crate, Python module, MCP server.
- **Ask / Answer** — the uniform invocation contract: question + metadata in; message + mandatory metadata (status, trace_id, cost, duration) out.
- **Trace** — the durable, replayable event log of one session; identified by `trace_id`, optionally rooted on a parent via `depends_on`.
- **Fork** — starting a new session from an existing trace's log prefix; the parent is immutable.
- **Scratchpad** — the agent's private filesystem subtree, writable without permission gates, jailed to its root.
- **Capability / tool** — a host-provided implementation of an effect; granted to an agent in its manifest. Another Hugr agent can itself be granted as a tool (`ARCHITECTURE.md` §20.5).
- **Resource group** — a named set of orchestrator-owned resources (folders, blobs, databases, hosts) passed with an ask; a per-group **grant** (read / read-write / absent) determines which manifest tools bound to that group get registered (`ARCHITECTURE.md` §18.5).
- **Event / Command / Operation / Projection / Policy** — unchanged from the core architecture; see `ARCHITECTURE.md`.
