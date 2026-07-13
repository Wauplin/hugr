# Agent framework comparison and Huggr gap analysis

This page compares Huggr with CrewAI, LangChain/LangGraph plus LangSmith, and eve, then identifies capabilities that would improve Huggr without weakening its sans-IO core or artifact-first model.

## Scope and snapshot

The comparison reflects public documentation and Huggr `main` at commit `3ae15de` on 2026-07-13, before this page was added. These projects have different boundaries, so direct feature counts are misleading:

- **CrewAI** combines an open-source Python framework for agents, crews, and event-driven flows with the hosted or self-hosted CrewAI AMP operations platform.
- **LangChain/LangGraph plus LangSmith** is a stack: LangChain supplies agent abstractions and integrations, LangGraph supplies stateful orchestration and durable execution, and LangSmith supplies observability, evaluation, prompt tooling, and deployment.
- **eve** is a beta, TypeScript-first, filesystem-authored agent application framework with a durable workflow runtime, sandbox, HTTP surface, channels, schedules, and evals.
- **Huggr** builds focused agents on a pure reducer and packages them as portable artifacts with a uniform ask/answer contract. It deliberately treats larger applications and orchestrators as callers.

The external claims below use official documentation. Hosted-only capabilities are called out because they are not equivalent to an open-source runtime feature.

## Executive recap

Huggr is not missing a basic agent loop. It already has several capabilities that are difficult to retrofit later: a sans-IO reducer, deterministic trace verification, immutable resume and fork lineage, manifest-scoped capabilities, typed response contracts, model-tier cost accounting, context projection, progressive skill disclosure, agent-as-tool composition, and standalone CLI/MCP/Python artifacts.

The most beneficial missing capabilities are operational rather than conversational:

1. **Durable mid-turn checkpoints and parked runs.** Huggr can reconstruct and reconcile a recorded partial trace, but the standard native ask path persists the trace at the end and `Command::Checkpoint` does not write a live checkpoint. A process cannot yet stop after a completed step and transparently continue the same turn.
2. **A real human-in-the-loop surface.** The core already models permission requests and decisions, but the native host automatically allows every registered capability. There is no standard way to pause an ask, present a request, and later resume with allow, deny, edited arguments, or user clarification.
3. **A first-party evaluation and regression harness.** Huggr has a worked judge-graded example and deterministic replay tests, but no generic dataset runner, assertions, experiment comparison, or CI-oriented `huggr eval` command.
4. **Production observability exports and trace querying.** Local immutable traces, replay, verification, feedback, and `huggr stats` are strong debugging primitives. Huggr lacks OpenTelemetry export, indexed trace search, dashboards, alerts, sampling, and online evaluators.
5. **A standard service and delivery edge.** Huggr ships CLI, stdio MCP, language bindings, and a browser example, but no supported HTTP/SSE server, auth middleware, webhook ingress, schedules, or channel adapters.

Provider breadth, middleware hooks, semantic retrieval, connection credential brokering, and higher-level workflow helpers are useful secondary gaps. They should remain host, toolkit, or adapter features rather than enter `huggr-core`.

## Positioning

| Project | Primary unit | Best fit | Main tradeoff relative to Huggr |
| --- | --- | --- | --- |
| Huggr | A focused, packaged huglet with a uniform ask/answer contract | Auditable specialists, local or embedded execution, portable tools for other orchestrators | Smaller integration and operations surface; no built-in application orchestrator |
| CrewAI | Python agents assembled into crews, tasks, processes, and flows | Role-based multi-agent automations and Python workflow applications | Less constrained runtime boundary; production UI, deployment, RBAC, and triggers are largely AMP concerns |
| LangChain/LangGraph + LangSmith | Composable model/tool abstractions and state graphs, operated through LangSmith | Custom stateful applications, broad integrations, experiments, and managed deployment | Larger stack and more application wiring; reproducibility depends on checkpointing and application discipline rather than a pure event reducer |
| eve | One filesystem-authored TypeScript agent application | Durable web and channel agents with sandboxed compute, schedules, and a frontend path | Broader application runtime with more infrastructure assumptions; beta APIs and a less minimal privilege model |

## Capability matrix

`Built in` means the main open-source runtime or toolkit has a supported path. `Partial` means the primitive exists but the end-to-end product path is incomplete. `Platform` means the documented path primarily belongs to a hosted or separately operated product.

| Capability | Huggr | CrewAI | LangChain/LangGraph + LangSmith | eve |
| --- | --- | --- | --- | --- |
| Basic tool-calling loop and streaming | Built in | Built in | Built in | Built in |
| Structured outputs | Rust schema contract plus Python/TypeScript mirrors | Pydantic structured outputs | Structured output strategies and typed graph state | Output schemas through the client and task APIs |
| Multi-agent composition | Agents as tools and isolated self-delegation | Crews, delegation, sequential and hierarchical processes | Subagents, routers, handoffs, and subgraphs | Root copies, declared subagents, remote agents, and fan-out |
| Deterministic workflow graph | External caller only | Flows with starts, listeners, routers, branching, and shared state | LangGraph state graphs and functional workflows | Durable workflow helpers and programmatic subagent orchestration |
| Conversation resume and fork | Immutable trace lineage with deterministic re-fold | Memory, persisted flows, checkpoint restore, and checkpoint fork | Thread checkpoints, time travel, and checkpoint forks | Durable sessions and continuations |
| Mid-turn crash recovery | Partial: reducer and trace format support it, standard ask persistence does not checkpoint live work | Event-driven checkpoints can skip completed tasks | Durable execution resumes from checkpoints and preserves successful work | Each step is checkpointed; completed steps do not rerun |
| Human approval and clarification | Partial: typed permission protocol, native host auto-allows | Human input and flow feedback; richer routing in AMP | Interrupts and HITL middleware with approve, edit, and reject | Durable approvals and questions, rendered by channels |
| Capability security | Strong manifest grants and root/host allowlists; full shell still relies on an outer OS sandbox | Tool-specific controls; MCP security guidance; hosted RBAC in AMP | Application and deployment policy, middleware, and sandbox integrations | Per-session sandbox backends, network policy, route auth, and credential brokering |
| Skills | Standard Agent Skills with progressive disclosure and jailed reads | Filesystem skills | Skills in LangChain multi-agent and Deep Agents surfaces | Markdown skills loaded on demand |
| Working and durable state | Per-lineage scratch, agent-wide memory, blobs, immutable traces | Flow state plus unified scoped semantic memory | Short-term thread state and long-term stores | Durable session state and persistent sandbox workspace |
| Knowledge and semantic retrieval | Tool-driven filesystem/search patterns; no first-party ingestion or vector retrieval layer | First-class knowledge sources and semantic retrieval | Broad document loader, embedding, vector-store, and retriever integrations | Connections, tools, and sandbox files; no central knowledge abstraction |
| Context management | Deterministic projection, truncation, summaries, forget rules, skills | Memory and knowledge injection plus agent controls | Middleware, state transforms, summaries, stores, and harnesses | Instructions, skills, dynamic capabilities, sandbox, and subagent isolation |
| Model providers | One OpenAI-compatible streaming adapter behind four stable tiers | Native major-provider integrations plus LiteLLM providers | Dedicated provider packages and routers behind a common API | AI SDK provider ecosystem and direct provider model objects |
| MCP and external APIs | MCP server output and stdio MCP client grants | stdio, SSE, and streamable HTTP MCP plus Apps | MCP across libraries and Agent Server; broad integration packages | MCP and OpenAPI connections with operation filters and token brokering |
| Traces and cost | Immutable local traces, exact replay verification, feedback, stats, mandatory per-answer cost | Local events plus AMP tracing and third-party observability integrations | LangSmith traces, dashboards, filters, feedback, annotation, and alerts | Streams, run metadata, OpenTelemetry, and platform dashboards |
| Evaluation | Example pipeline only; generic harness is planned | `crewai test` iteration scoring plus external integrations | Offline and online evaluation, datasets, evaluators, experiments, and production rules | First-party eval cases, deterministic fixtures, assertions, judges, reporters, and remote targets |
| Shipping | Standalone binary, stdio MCP server, typed Python wheel, Rust/Python/TypeScript embedding | Python package and AMP deployments | Python/TypeScript packages plus Agent Server and LangSmith deployments | Node server, Vercel deployment, self-hosting, and host-framework mounting |
| Ingress and delivery | CLI, MCP, programmatic calls, and a Chrome extension example | AMP REST API, webhooks, integrations, and triggers | Agent Server API, SDK clients, crons, A2A, and MCP | HTTP, web frontends, Slack, Discord, Teams, Telegram, Twilio, GitHub, Linear, custom channels, and cron schedules |
| Team operations | Repository and external CI conventions | AMP Studio, repositories, marketplace, RBAC, and team management | LangSmith workspaces, prompt management, datasets, reviewers, deployment, and RBAC | Source-controlled app plus deployment platform features |

## What each competitor contributes

### CrewAI

CrewAI's clearest advantage is that it treats multi-agent organization and deterministic application flow as first-class, adjacent concepts. [Crews and processes](https://docs.crewai.com/en/concepts/processes) cover sequential and hierarchical role-based execution, while [Flows](https://docs.crewai.com/en/concepts/flows) provide event-driven start, listen, routing, branching, shared state, and visualization around ordinary code and crews.

The current framework also exposes more batteries around agent applications: [event-driven checkpointing](https://docs.crewai.com/en/concepts/checkpointing) for restore and fork, a [unified semantic memory](https://docs.crewai.com/en/concepts/memory), [knowledge sources](https://docs.crewai.com/en/concepts/knowledge), LLM call hooks, a large tool catalog, several MCP transports, and broad model-provider support. Its built-in [`crewai test`](https://docs.crewai.com/en/concepts/testing) is much narrower than LangSmith or eve evals, but it still supplies a standard repeated-run scoring loop that Huggr lacks.

CrewAI AMP adds managed deployment, traces, triggers, integrations, team controls, RBAC, secrets, and visual editing. Those are product-platform advantages, not evidence that Huggr's reducer should own them. The reusable lesson is to expose stable host interfaces for checkpoint storage, telemetry, approval, and ingress so operators can build or adopt comparable services.

### LangChain, LangGraph, and LangSmith

This stack has the broadest extension ecosystem in the comparison. [LangChain](https://docs.langchain.com/oss/python/langchain/overview) standardizes models, tools, structured output, middleware, and integrations. Its [provider interface](https://docs.langchain.com/oss/python/concepts/providers-and-models) supports dedicated packages for provider-specific features as well as gateways and routers. Its [guardrail middleware](https://docs.langchain.com/oss/python/langchain/guardrails) can inspect or transform requests, model calls, tool calls, and outputs.

[LangGraph](https://docs.langchain.com/oss/python/langgraph/overview) supplies state graphs, durable execution, streaming, and human input. Its [persistence layer](https://docs.langchain.com/oss/python/langgraph/persistence) checkpoints graph state for fault recovery, memory, time travel, and forks. [Interrupts](https://docs.langchain.com/oss/python/langgraph/interrupts) can pause indefinitely and resume with external input. Multi-agent patterns include [subagents](https://docs.langchain.com/oss/python/langchain/multi-agent/subagents), routers, and [handoffs](https://docs.langchain.com/oss/python/langchain/multi-agent/handoffs).

[LangSmith observability](https://docs.langchain.com/langsmith/observability) and [Studio](https://docs.langchain.com/oss/python/langchain/studio) provide the operational layer that Huggr's local trace tools do not: indexed trace browsing, prompt and tool inspection, dashboards, feedback workflows, datasets, and interactive debugging. [LangSmith Evaluation](https://docs.langchain.com/langsmith/evaluation) supports offline datasets and experiments as well as sampled online evaluators, human review, code rules, LLM judges, comparisons, and feedback loops into new test cases. [Agent Server](https://docs.langchain.com/langsmith/server-api-ref) adds hosted or self-hosted APIs for assistants, threads, runs, crons, stores, MCP, and A2A.

Huggr should not imitate the size of this stack. It can adopt the high-value seams: telemetry export, trace indexing, reusable evaluators, durable interrupts, and provider adapters, while keeping one trace format and a much smaller authoring contract.

### eve

eve is the closest comparison for an artifact-like, filesystem-authored agent, but its artifact is an application rather than a small callee. A directory can contain instructions, tools, skills, a sandbox, channels, connections, subagents, schedules, hooks, state, and evals. The [project overview](https://eve.dev/) and [repository](https://github.com/vercel/eve) describe a TypeScript-first framework that can mount inside Next.js or run as its own Node service.

Its main advantage over Huggr is end-to-end durable operation. An eve [session checkpoints every step](https://eve.dev/docs/concepts/execution-model-and-durability), can survive restarts and deployments, and can park without holding compute. [Human-in-the-loop](https://eve.dev/docs/human-in-the-loop) approvals and questions use that same durable pause mechanism. [Subagents](https://eve.dev/docs/subagents) have their own contexts and sandboxes, and independent calls can fan out concurrently.

eve also covers the application edge directly: [channels](https://eve.dev/docs/channels/overview), [schedules](https://eve.dev/docs/schedules), a stable HTTP session and event-stream protocol, frontend hooks, deployment, route authentication, and [MCP or OpenAPI connections](https://eve.dev/docs/connections) with scoped credential resolution. Its [eval runner](https://eve.dev/docs/evals/overview) exercises the real HTTP application, supports datasets, deterministic model fixtures, tool and output assertions, LLM judges, remote targets, concurrency, CI exit codes, and reporters. OpenTelemetry and platform dashboards cover production traces.

The security comparison is mixed rather than one-sided. eve gives every agent a replaceable isolated compute sandbox and can broker credentials at the network boundary. Huggr gives focused agents a much smaller declared capability set with path and host jails, while full process access remains an explicit operator escape hatch. A Huggr service host could add a container or VM sandbox without changing the brain or making broad shell access the default.

## Huggr's current advantages

These should remain design constraints while filling the gaps:

- **Exact replay verification.** LangGraph, CrewAI, and eve checkpoint application or workflow state. Huggr records the nondeterministic event stream and verifies that re-folding emits the same command sequence. That is a stronger debugging and conformance property than ordinary checkpoint restore.
- **Immutable branching.** A follow-up always produces a new trace with `depends_on`, so fork lineage is explicit and concurrent branches do not mutate a shared checkpoint.
- **A narrow, pure core.** New tools, providers, grants, and payloads do not require new core variants. IO, persistence, telemetry, credentials, and deployment remain host concerns.
- **Auditable privilege manifests.** Optional capabilities do not exist unless registered from a grant, and built-in filesystem and network tools enforce concrete roots or host allowlists.
- **Portable specialist artifacts.** A built agent is a standalone executable and MCP server with the same contract used by generated Python and programmatic language surfaces.
- **Mandatory accounting.** Status, trace ID, token use, duration, and cost are part of every answer rather than optional telemetry.
- **Cross-surface core.** The same reducer runs in native Rust, embedded Python, Node, browsers, and a Chrome extension host.

## Recommended gaps to close

### P0: live checkpoints and durable pause/resume

Turn `Command::Checkpoint` into a real host contract without adding IO to `huggr-core`. The native host should accept a checkpoint sink that atomically persists the recorder's event prefix, emitted commands, policy config, lineage, and any host metadata required to locate scratch and blobs. The standard agent surface should reserve and return a trace or run ID before completion so a crashed or parked ask can be addressed later.

The resume contract should distinguish:

- a completed parent trace used for a new follow-up or fork;
- an incomplete run resumed from its last committed boundary;
- an in-flight model or tool operation that must be marked cancelled and retried only when the host's effect policy permits it;
- a parked run awaiting an external decision.

This work must document at-least-once behavior around interrupted tools. Huggr cannot promise exactly-once external side effects from a local trace alone. Tool authors need idempotency keys or an explicit no-retry classification for side-effecting operations.

Why P0: durable checkpoints are the prerequisite for reliable long-running work and a useful human-in-the-loop surface. CrewAI, LangGraph, and eve all treat recovery boundaries as an operating primitive.

### P0: human approval and user input

Replace the native host's unconditional allow path with an injected permission broker. A broker should be able to allow, deny with a reason, and eventually support argument edits. The synchronous CLI can prompt on a TTY; noninteractive surfaces should emit a typed pending-input answer or event and leave the run parked for a later resume call.

The existing `RequestPermission` and `PermissionDecision` core types are an appropriate narrow waist. The missing pieces belong in the host and surfaces:

- a pending request identifier and serialized request payload;
- a durable wait state tied to the run ID;
- CLI, MCP, Rust, Python, TypeScript, and browser methods for submitting a decision;
- policy choices such as always, once per lineage, conditional, or never;
- an agent-initiated clarification request that uses the same pause protocol;
- audit records for the request, actor, decision, edited arguments, and reason.

Why P0: Huggr currently labels capabilities as permission-gated but the standard native path auto-approves them. Closing that gap makes write, shell, web, MCP, and agent actions safer without widening the core.

### P0: first-party evaluation harness

Implement the planned `huggr eval` path as a normal consumer of the ask/answer contract. A useful first version needs:

- source-controlled cases with question, runtime args, blobs, skills, and optional resume lineage;
- deterministic assertions over status, typed response JSON paths, text, tool names, tool counts, cost, tokens, latency, and trace verification;
- custom code evaluators and a Huggr agent as an LLM judge;
- concurrency, repetitions, per-case limits, caching only when explicitly safe, and stable CI exit codes;
- a recorded-response model adapter for reducer and host tests without provider calls;
- JSON and Markdown reports that preserve trace IDs for every case;
- comparison against a named baseline so prompt, model, tool, quality, cost, and latency regressions are visible together.

Huggr already has the primitives: typed answers, traces, costs, feedback, a judge example, and deterministic replay. Packaging them into one test contract offers more value than adding another agent abstraction.

### P1: OpenTelemetry and indexed trace operations

Add a host-side telemetry exporter derived from `AgentEvent` and final trace metadata. Emit stable spans for asks, model calls, tool calls, delegated agents, retries, and summaries, with token, cost, latency, status, model tier, resolved provider/model, trace ID, parent trace ID, and capability name attributes. Prompt, arguments, and results should be opt-in because they can contain secrets or user data.

Keep local traces authoritative. OpenTelemetry is an export, not a second replay log. A small local trace index can then support filtering by time, status, cost, model, tool, feedback, and lineage without scanning every JSON file. Dashboards and alerts can remain external concerns.

Why P1: Huggr can explain one run well, but teams also need to find the expensive, failing, or anomalous runs across many invocations.

### P1: standard HTTP and streaming service host

Provide a thin, optional service crate or generated surface around the existing contract:

- `POST /asks`, `GET /asks/{id}`, cancellation, feedback, and decision endpoints;
- SSE or WebSocket streaming over `AgentEvent`;
- health, describe, and config endpoints with credential values omitted;
- injected authentication and authorization hooks;
- bounded queues, concurrency limits, request size limits, and backpressure;
- explicit trace, blob, scratch, and model catalog backends.

Schedules, webhooks, Slack, GitHub, and other channels can be separate adapters that normalize input into `Ask` and deliver `Answer` or events. They do not belong in `huggr-core` or the universal agent contract.

Why P1: every competitor offers a supported network-facing runtime. Huggr currently requires each operator to wrap a binary, MCP subprocess, or language binding before it can be run as a service.

### P1: provider-native adapters and routing policy

Keep the four public tiers, but add native adapters for providers whose streaming and tool semantics are not faithfully covered by Chat Completions. The planned Anthropic adapter is the right next proof. Google and a local inference adapter can follow based on demand.

Provider features should remain adapter data unless the brain branches on them. Host-side routing can add fallback, retry budgets, health-aware selection, and per-tier policy while recording the resolved provider, model, attempt, usage, and cost in the trace. Credential values must remain outside traces and introspection.

Why P1: an OpenAI-compatible seam offers broad reach but leaves provider-native features and error semantics behind. CrewAI, LangChain, and eve all make provider choice substantially easier.

### P2: lifecycle middleware and guardrails

Huggr has deterministic answer hooks, provider retries, response repair, policies, and capability permission flags, but no uniform lifecycle extension surface. Add narrowly scoped host hooks around ask admission, context projection, model dispatch/result, tool dispatch/result, and final answer. Hooks should declare whether they are pure or effectful, and any output that affects replay must enter the event stream or trace metadata.

Useful applications include PII redaction, tenant checks, rate limits, policy enforcement, prompt injection scanners, tool argument validation, output moderation, telemetry enrichment, caching, and model fallback. This should not become an untyped chain that silently mutates the reducer's durable log.

### P2: semantic knowledge and memory adapters

Huggr's filesystem tools and trace-aware context policy are sufficient for many focused retrieval agents. A reusable retrieval capability would still reduce repeated application code:

- an ingestion pipeline outside the brain;
- namespaced document and chunk identifiers;
- pluggable embedding and vector stores;
- metadata and source filters;
- a jailed `knowledge_search` capability with size and result caps;
- citations or stable source handles in opaque results;
- explicit lifecycle and deletion controls.

The existing `memory` grant could gain optional search backends, but semantic memory should not be inserted automatically into every prompt. Retrieval must remain a visible tool or pure context-policy decision so traces show what influenced an answer.

### P2: remote connections and credential brokering

Extend the existing stdio MCP client with remote streamable HTTP transport, allowlists, timeouts, response caps, and explicit auth providers. An OpenAPI-to-capability adapter could generate tools outside the core while preserving operation allowlists. Per-user OAuth and app credentials should be resolved by trusted host code and never exposed to the model or recorded in traces.

This closes a practical gap highlighted by all three competitors. It should preserve Huggr's grant rule: a remote server or OpenAPI document may describe many operations, but only explicitly allowed operations are registered.

### P2: orchestration helpers, not a second brain

Huggr does not need a graph DSL inside its reducer. It would benefit from a small host-side composition library for deterministic application code:

- sequential steps and typed data passing;
- parallel fan-out and fan-in with child cost aggregation;
- conditional routing over typed answers;
- retry and compensation policies;
- run-level limits and cancellation;
- a workflow trace that references immutable child traces rather than merging their logs.

This would cover common CrewAI Flow and LangGraph use cases while keeping the huglet as the callee and ordinary Rust, Python, or TypeScript code as the orchestrator.

## Features that should not move into `huggr-core`

- HTTP servers, databases, schedulers, queues, dashboards, OpenTelemetry exporters, OAuth, and secret stores.
- Provider SDKs, tokenizers, vector databases, embedding models, and document loaders.
- Channel adapters, SaaS integrations, deployment control planes, RBAC, marketplaces, and visual builders.
- A general graph engine or hosted workflow service.
- Mutable traces or a second durable state store that cannot be derived from recorded events.
- Tool-specific argument or result types that violate the narrow-waist rule.
- Default shell, network, or remote-process access merely to match a broader framework's tool catalog.

These can exist in hosts, toolkit crates, generated surfaces, or separate products. The core should gain a new type only when it must branch on a stable lifecycle distinction, such as a genuinely parked operation, not when a new integration needs configuration.

## Suggested sequence

1. Land `huggr eval` and a deterministic model fixture so every later behavior change has a regression gate.
2. Make host checkpoints durable and addressable during a live ask, then test crash recovery at each model, tool, summary, and cancellation boundary.
3. Add the permission broker and durable pending-input protocol across CLI, MCP, Rust, Python, TypeScript, and browser surfaces.
4. Export OpenTelemetry from the same host events and add a local trace index without changing trace authority.
5. Ship an optional HTTP/SSE host with injected auth, storage, and concurrency policy.
6. Add native provider adapters, beginning with the already planned Anthropic adapter.
7. Add remote MCP/OpenAPI connections and semantic retrieval as explicitly granted capabilities.
8. Add host-language orchestration helpers only after real compositions show repeated boilerplate.

This order first improves correctness and testability, then long-running safety, then operation and adoption. It also keeps every new source of IO outside `huggr-core`.

## Official sources

### Huggr

- [Overview](../concepts/overview.md)
- [Runtime](../concepts/runtime.md)
- [Security](../concepts/security.md)
- [Context management](../concepts/context-management.md)
- [Models and pricing](../concepts/models-and-pricing.md)
- [Agents and manifest](agents.md)
- [Capabilities](capabilities.md)
- [Trace inspection](../guides/inspect-traces.md)
- [Composition](../guides/compose-agents.md)
- [Dataset and evaluation example](../tutorials/docs-qa-dataset-pipeline.md)

### CrewAI

- [Documentation index](https://docs.crewai.com/)
- [Agents, crews, and flows](https://docs.crewai.com/en/concepts/flows)
- [Checkpointing](https://docs.crewai.com/en/concepts/checkpointing)
- [Agent capabilities](https://docs.crewai.com/en/concepts/agent-capabilities)
- [Memory](https://docs.crewai.com/en/concepts/memory)
- [Knowledge](https://docs.crewai.com/en/concepts/knowledge)
- [Testing](https://docs.crewai.com/en/concepts/testing)
- [Tracing](https://docs.crewai.com/en/observability/tracing)
- [CrewAI AMP](https://docs.crewai.com/en/enterprise/introduction)

### LangChain, LangGraph, and LangSmith

- [Framework, runtime, and harness boundaries](https://docs.langchain.com/oss/python/concepts/products)
- [LangChain agents](https://docs.langchain.com/oss/python/langchain/agents)
- [Providers and models](https://docs.langchain.com/oss/python/concepts/providers-and-models)
- [Guardrails](https://docs.langchain.com/oss/python/langchain/guardrails)
- [LangGraph overview](https://docs.langchain.com/oss/python/langgraph/overview)
- [Persistence](https://docs.langchain.com/oss/python/langgraph/persistence)
- [Interrupts](https://docs.langchain.com/oss/python/langgraph/interrupts)
- [Multi-agent handoffs](https://docs.langchain.com/oss/python/langchain/multi-agent/handoffs)
- [LangSmith observability](https://docs.langchain.com/langsmith/observability)
- [LangSmith evaluation](https://docs.langchain.com/langsmith/evaluation)
- [LangSmith Studio](https://docs.langchain.com/oss/python/langchain/studio)
- [Agent Server API](https://docs.langchain.com/langsmith/server-api-ref)

### eve

- [eve overview](https://eve.dev/)
- [GitHub repository](https://github.com/vercel/eve)
- [Execution and durability](https://eve.dev/docs/concepts/execution-model-and-durability)
- [Human-in-the-loop](https://eve.dev/docs/human-in-the-loop)
- [Subagents](https://eve.dev/docs/subagents)
- [Sandbox](https://eve.dev/docs/sandbox)
- [Connections](https://eve.dev/docs/connections)
- [Channels](https://eve.dev/docs/channels/overview)
- [Schedules](https://eve.dev/docs/schedules)
- [Evals](https://eve.dev/docs/evals/overview)
- [Deployment](https://eve.dev/docs/guides/deployment)
- [Instrumentation](https://eve.dev/docs/guides/instrumentation)
