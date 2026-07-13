# Serve and consume MCP

This guide covers both directions of Huggr's MCP support: exposing a built huglet as an MCP server with `--mcp-serve`, so any MCP client can call it, and granting an external MCP server to an agent with `[tools.mcp.<name>]`, so its tools appear next to the built-in library. MCP is the one external-tool escape hatch by design; everything else composes through the ask/answer contract.

## The problem

The ask/answer contract makes huglets uniform for callers that speak it, but most agent hosts speak MCP. Without an adapter, every integration is bespoke glue in both directions: exposing an agent to Claude Code or another MCP client, and using an existing MCP server's tools from inside an agent. Huggr answers both with one protocol boundary: stdio JSON-RPC, subprocess on each side.

## Serving: the built binary as an MCP server

Every built agent binary is already an MCP server:

```bash
./dist/huglet-docs-cli/target/release/huglet-docs ./docs --mcp-serve
```

Register that command (binary plus its runtime args, then `--mcp-serve`) as a stdio server in any MCP client; the server name and version come from the manifest, and the agent description becomes the server instructions. The transport is newline-delimited JSON-RPC over stdio, and there is no state beyond the process: one registration, one agent.

The server exposes exactly two tools.

**`ask`** mirrors the CLI contract: a required `question`, an optional `trace_id` to resume or fork a stored trace, optional inbound `blobs` (handle objects with `bytes` or `sha256` refs; `path` refs are rejected because the MCP client counts as an untrusted caller), and optional `skills` (local skill folder paths for this ask). Every `[runtime.args.<name>]` from the manifest is also added to the schema as a string property, required ones included, so a client can re-point the same docs binary at a different folder per call. The result carries the full `Answer` as structured content plus a text rendering of the response, so both schema-aware and plain-text clients get something useful.

**`feedback`** takes a `trace_id` and an opaque `payload` and appends caller feedback for that trace, the same back-channel as `--feedback` on the CLI.

Two behaviors follow from the contract rather than from MCP:

- **Errors are answers.** A failed run returns a normal result whose `Answer` has `status: "error"`; the MCP-level error path is reserved for infrastructure problems such as an unknown parent trace id.
- **Conversation state rides `trace_id`, not the session.** A follow-up is another `ask` call carrying the previous answer's `trace_id`. The MCP client does not need session support, and killing and restarting the server loses nothing, because every trace is on disk.

The server does not use MCP sampling (the agent owns its model provider) and exposes no resources or prompts; the tool surface is the whole surface.

## Consuming: granting an MCP server to an agent

The other direction is a manifest grant:

```toml
[tools.mcp.github]
command = "gh-mcp"
args = ["--stdio"]      # optional
```

At assembly time the host starts the command as a subprocess, performs the MCP handshake, lists its tools, and registers each one as an ordinary capability named `mcp__<name>__<tool>`, with the schema the server advertised. The model sees them next to `fs_read` or `web_fetch` with no special status; the [narrow waist](../concepts/runtime.md#the-narrow-waist-rule) means their arguments and results are opaque payloads like any other tool's. A server that fails to start or answer the handshake fails agent assembly up front rather than surfacing mid-conversation.

Failure handling at call time follows the usual split: a tool result flagged as an error by the server, or a transport failure such as the server dying mid-call, comes back to the model as a structured tool error it can react to, not a host crash. Results arrive whole; MCP tool calls do not stream chunks.

Since every built huglet serves MCP, one huglet can consume another this way. Prefer `[tools.agent.<name>]` for that, though: the agent grant folds the child's cost into your `AnswerMeta`, forwards blob refs, and records the child's trace id, none of which a generic MCP grant knows how to do (see [composition and cost](compose-agents.md)).

## Trust and audit

An MCP grant is the widest line a manifest can carry. The server is an operator-declared external process: Huggr does not sandbox its filesystem or network, it inherits the agent process's full environment (including any secrets in it) and working directory, and there is no `env` scoping in the grant. Granting `[tools.mcp.<name>]` is precisely as trusting as running `command` yourself, which is why it sits with full shell and agent grants in the [security model](../concepts/security.md) as an explicit external-process escape hatch. `--config` on a built binary exposes the command and args for audit.

The same reasoning applies in reverse when serving: anyone who can call your agent's MCP `ask` can spend your model budget and exercise every granted tool, and runtime args in the schema let a caller re-scope what the manifest made patchable, for example pointing `fs_read` at a different directory. Serve an agent whose manifest you would be comfortable handing to the caller, set `[limits]`, and remember that `--mcp-serve` itself does not authenticate anyone; whoever owns the client registration owns the calls.

## Worked example

The docs huglet from [Build your first agent](../tutorials/first-agent.md) is built once, then registered in an MCP client with command `./dist/huglet-docs-cli/target/release/huglet-docs` and args `["./docs", "--mcp-serve"]`. The client calls `ask {"question": "How do runtime args work?"}` and receives an `Answer` with a `trace_id`; a follow-up passes that `trace_id` back and gets a resumed conversation; `feedback {"trace_id": ..., "payload": {"score": 1}}` files a review the insights workflow can mine later ([Inspect, replay, and verify traces](inspect-traces.md)). Meanwhile, the same binary could itself grant `[tools.mcp.github]` and call `mcp__github__*` tools during its turns. Both boundaries are subprocesses speaking the same protocol.

## Limitations

- Stdio only, in both directions: no HTTP/SSE transport, and one server process per client registration or per grant.
- The served surface is tools only (`ask`, `feedback`): no resources, prompts, sampling, or MCP-level streaming; a client sees the answer when the ask finishes, so give slow agents a generous client timeout.
- Consumed MCP tools return single-shot results; a server that streams has its output buffered by the protocol layer.
- The grant supports `command` and `args` only. There is no per-server environment or working-directory scoping; isolate an untrusted server with an OS-level sandbox around it, or do not grant it.
