# Agent framework comparison

This page gives a high-level comparison of Huggr, CrewAI, the LangChain ecosystem, and eve. It then lists the main features Huggr does not yet provide.

The comparison reflects public documentation and Huggr `main` at commit `ee9b0e9` on 2026-07-15, plus the live-checkpoint implementation under review. Hosted products are included because they affect what a user can do, even when the feature is not part of the open-source framework.

## High-level comparison

| Project                                 | What it is                                                                                       | Good fit                                                                                                          | Compared with Huggr                                                                                                       |
| --------------------------------------- | ------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| **Huggr**                               | A toolkit for building small, focused agents as standalone programs                              | Portable specialist agents that need explicit permissions, typed answers, local traces, and reproducible behavior |                                                                                                                           |
| **CrewAI**                              | A Python framework for coordinating several agents and workflow steps                            | Multi-agent automations built as Python applications                                                              | More built-in workflow and team coordination features, but less emphasis on a small pure runtime and standalone artifacts |
| **LangChain, LangGraph, and LangSmith** | A large ecosystem for building, running, testing, and monitoring agent applications              | Custom applications that need many model, data, and service integrations                                          | Much broader ecosystem and production tooling, but more components and application setup                                  |
| **eve**                                 | A TypeScript framework for complete agent applications, including web and messaging entry points | Agents that run as durable web services or connect directly to user channels                                      | More of the application stack is included, but the runtime is broader and its APIs are still beta                         |

In practical terms:

- Choose **Huggr** when the agent should be a focused, portable component with a small permission set and a trace that can be replayed exactly.
- Choose **CrewAI** when the main problem is assigning roles and steps across several agents in a Python workflow.
- Choose the **LangChain ecosystem** when integration breadth, configurable workflows, evaluation, and production monitoring matter more than keeping the stack small.
- Choose **eve** when building a TypeScript agent as a complete service with web, schedule, or messaging support.

## What Huggr already covers

Huggr already provides the basic agent loop, streaming model output, tool calls, structured answers, agent-to-agent delegation, conversation resume and fork, native live checkpoints, local traces, cost accounting, feedback, CLI and MCP access, and Rust, Python, TypeScript, and browser embedding.

Its distinguishing features are:

- **Exact replay verification:** a recorded run can be replayed to check that the same inputs produce the same actions.
- **Explicit permissions:** an agent receives only the tools and resource access declared in its manifest.
- **Portable artifacts:** a built agent is a standalone program with the same ask/answer contract on every supported surface.
- **A pure core:** model calls, files, networks, clocks, and other external effects stay outside the decision-making reducer.
- **Interrupted-run recovery:** filesystem-backed native agents atomically save each completed step and resume the checkpoint through the existing trace id contract.

## Missing features

The following are gaps in the standard Huggr experience. Some underlying pieces exist, but users cannot yet rely on a complete built-in workflow.

### Highest priority

1. **Ask a person before acting.** Huggr has internal permission request types, but its standard native host currently approves every registered tool automatically. It needs a way to pause, show the proposed action, accept or reject it, and then continue. The same mechanism should let an agent ask the user a clarifying question.
2. **Run repeatable evaluations.** There is an evaluation example, but no general `huggr eval` command for running a set of test questions, checking answers and tool use, comparing cost and quality with a baseline, and failing CI when behavior regresses.

### Production operation

3. **Search and monitor runs.** Huggr can inspect individual local traces and calculate aggregate statistics. It lacks indexed search across runs, standard telemetry export, dashboards, alerts, sampling, and automatic checks on live traffic.
4. **Run as a web service.** Huggr provides a CLI, MCP server, language bindings, and a browser example. It does not provide a supported HTTP API with streaming, authentication hooks, queues, cancellation, health checks, and limits for concurrent requests.
5. **Use more model providers directly.** The current adapter works with OpenAI-compatible APIs. Native adapters would better support providers whose streaming, tool calling, errors, or model options differ from that API.

### Integration and application features

6. **Apply checks around every operation.** Huggr lacks one standard extension point for input checks, sensitive-data removal, rate limits, tool validation, output moderation, caching, and similar policies around model and tool calls.
7. **Search document collections.** Agents can use granted file and search tools, but Huggr has no built-in pipeline for importing documents, creating a semantic index, and retrieving relevant passages with source references.
8. **Connect to remote tools and credentials.** Huggr supports local MCP subprocesses. It does not yet provide a standard remote MCP or OpenAPI connection layer with operation allowlists, authentication, timeouts, and credential handling outside the model.
9. **Compose multi-step applications.** Agents can call other agents, but application authors do not have first-party helpers for sequential steps, parallel work, conditional routing, retries, and cancellation. These helpers should live outside the pure core.
10. **Receive schedules, webhooks, and messages.** Huggr does not ship adapters for scheduled runs or services such as Slack and GitHub. Such adapters can translate incoming events into the existing ask/answer contract.

## What should remain outside `huggr-core`

Closing these gaps should not add IO or application infrastructure to the pure reducer. Web servers, databases, schedulers, provider SDKs, telemetry exporters, document indexes, credential stores, channel adapters, and workflow orchestration belong in hosts, toolkit crates, or separate services.

## Sources

- Huggr: [overview](../concepts/overview.md), [runtime](../concepts/runtime.md), [security](../concepts/security.md), [agents](agents.md), [capabilities](capabilities.md), [trace inspection](../guides/inspect-traces.md), and [composition](../guides/compose-agents.md)
- CrewAI: [documentation](https://docs.crewai.com/), [flows](https://docs.crewai.com/en/concepts/flows), [checkpointing](https://docs.crewai.com/en/concepts/checkpointing), and [testing](https://docs.crewai.com/en/concepts/testing)
- LangChain ecosystem: [product boundaries](https://docs.langchain.com/oss/python/concepts/products), [LangGraph](https://docs.langchain.com/oss/python/langgraph/overview), [observability](https://docs.langchain.com/langsmith/observability), and [evaluation](https://docs.langchain.com/langsmith/evaluation)
- eve: [overview](https://eve.dev/), [durability](https://eve.dev/docs/concepts/execution-model-and-durability), [human input](https://eve.dev/docs/human-in-the-loop), [channels](https://eve.dev/docs/channels/overview), and [evaluations](https://eve.dev/docs/evals/overview)
