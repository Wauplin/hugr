# Guides

Guides cover one task and assume you already know the basic Huggr workflow. For a first project, start with [Build your first agent](../tutorials/first-agent.md).

- [Define typed responses and answer hooks](typed-responses.md): use Rust response contracts, separate model-facing and public types, and post-process answers deterministically.
- [Package an agent for Python](package-agent-for-python.md): ship a built agent as a typed Python wheel, subprocess, or MCP server.
- [Compose agents and account for cost](compose-agents.md): grant agents as tools, pass blobs, file feedback, and inspect delegated cost.
- [Inspect, replay, and verify traces](inspect-traces.md): inspect trace anatomy, step through replay, verify determinism, and analyze feedback.
- [Serve and consume MCP](mcp.md): expose a built agent through MCP and grant external MCP servers to an agent.
- [Configure runtime arguments](runtime-arguments.md): patch supported manifest values at invocation time across CLI, MCP, and generated Python surfaces.
