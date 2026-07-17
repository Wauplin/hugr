# Streaming and events

This page explains how a caller observes an ask while it runs: the shared `AgentEvent` vocabulary, `--stream` on a built binary, `Agent::ask_events` in Rust, and `agent.run(...)` in Python and TypeScript. It also explains what events deliberately are not: durable. The answer and the trace are the record; events are the live view.

## The problem

`ask()` is a black box until it returns: fine for an orchestrator that only wants the `Answer`, useless for a chat UI that needs tokens as they arrive, a progress line naming the tool currently running, or a log of what a slow ask is doing. The temptation is to bolt rendering into the runtime loop. Huggr instead emits a typed event stream beside the ask, and keeps it strictly separate from the durable log.

## The event vocabulary

Rust, Python, and TypeScript event streams use the same nine events, tagged by a `type` field in snake_case:

| Event | Payload | Meaning |
| --- | --- | --- |
| `ask_started` | `trace_parent` | the ask began; parent trace id if resuming |
| `model_started` | `op`, `tier` | a model call began on the named tier |
| `text_delta` | `op`, `text` | a streamed chunk of model text |
| `model_ended` | `op`, `usage` | the call finished, with token usage |
| `tool_started` | `op`, `name`, `args` | a capability was invoked |
| `tool_ended` | `op`, `name`, `is_error`, `result` | its result (or semantic error) came back |
| `notice` | `message` | a host-side observation worth showing |
| `done` | `reason` | the turn reached a terminal state |
| `answer_ready` | `answer` | the full `Answer`, last event of the stream |

A successful stream starts with `ask_started` and ends with `answer_ready`; an infrastructure failure can end after a `notice`. The `op` id correlates a start, its deltas, and its end, so an interleaved display can attribute every chunk. Tool args and results are the same opaque JSON the model saw; the events add no interpretation.

## Where events come from, and what is durable

`AgentEvent` values are host-layer observations. The brain already emits its command stream; the host's frontend hooks translate op lifecycle into `AgentEvent`s as they happen, and plain `ask()` uses a silent frontend that drops them all, producing an identical trace either way.

The distinction is between the live `AgentEvent` view and the core event stream. `AgentEvent` values are not stored. The trace does store every time-stamped host-to-brain `Envelope`, including `ModelDelta` and `CapabilityChunk`, because those envelopes are the replay input. Replay re-feeds them and verifies the resulting commands and log. The durable log remains compact: deltas and chunks never become `Record`s, and each completed model output or tool result produces one consolidated entry.

Watching through `ask_events`, `run(...)`, or `--stream` does not add anything to the trace. If you need to reconstruct the durable content and replay sequence after the fact, read the trace ([Inspect, replay, and verify traces](../guides/inspect-traces.md)); if you need the normalized UI lifecycle while the ask runs, subscribe to `AgentEvent`s.

## CLI: `--stream`

```bash
huglet-docs ./docs "Explain compaction" --stream
```

stdout becomes newline-delimited JSON: one compact `AgentEvent` per line, then the final `Answer` as the last line. The answer line is the bare `Answer` object, not wrapped in an event (the `answer_ready` event is skipped on this surface since the answer follows anyway), so a consumer can parse lines as "has a `type` field → event, otherwise → answer". `--json`/`--pretty` only affect the non-streaming path; streamed lines are always compact. Exit code is 0 and errors are answers, as everywhere.

This is the observation surface for subprocess callers: the same ask path, watched rather than awaited. It is not a second loop; the trace and answer are identical with or without the flag.

## Rust: `ask_events`

```rust
let (mut events, handle) = agent.ask_events(ask);
while let Some(event) = events.recv().await {
    if let AgentEvent::TextDelta { text, .. } = &event { print!("{text}"); }
}
let answer = handle.await??;
```

`ask_events` returns an unbounded receiver of `AgentEvent` plus a join handle resolving to the `Result<Answer, AskError>`. The channel ends after `answer_ready`; an infrastructure failure surfaces as a `notice` on the stream and an error from the handle.

## Python and TypeScript: `run(...)`

Python-defined agents, generated wheels for Rust-defined agents, and the TypeScript embedding expose `ask(...)` plus a streaming `run(...)` over the same vocabulary:

```python
async for event in agent.run("Explain compaction"):
    if isinstance(event, huggr.TextDeltaEvent):
        print(event.text, end="")
    elif isinstance(event, huggr.AnswerReadyEvent):
        answer = event.answer
```

Python yields typed dataclasses (`AskStartedEvent` through `AnswerReadyEvent`), cast from the same wire shapes; `DoneReason` is normalized to a `kind` plus optional `message`. Both Python surfaces use the same authoritative event model source and signal-aware native bridge. `Ctrl+C` interrupts blocking `ask()`, while task cancellation or iterator close aborts `run()`.

```ts
for await (const event of agent.run("Explain compaction")) {
  if (event.type === "text_delta") process.stdout.write(event.text);
}
```

TypeScript's `AgentEvent` is a discriminated union on `type` with the identical wire shapes; `ask()` is `run()` with a collector that returns the `answer_ready` payload. Model text deltas are yielded as the provider stream delivers them, while the other events retain their order ([Define an agent in TypeScript](../tutorials/typescript-agent.md)).

## Choosing the surface

- Orchestrators that only branch on results: plain `ask` / the default CLI output. `AnswerMeta` already summarizes calls, tokens, and cost.
- Interactive UIs: `run(...)` or `ask_events`, render `text_delta`, and use `tool_started`/`tool_ended` for activity indicators.
- Wrapping a built binary from another language: `--stream` and a line parser.
- Anything after the fact, including debugging a stream you already watched: the trace, not a saved event log.

## Limitations

- `AgentEvent` values are ephemeral by design. A trace stores the underlying time-stamped core envelopes and consolidated records, but not the normalized `ask_started` through `answer_ready` stream.
- The stream observes; it does not control. There is no event-level cancel or reply channel; aborting an ask is the caller's process-level concern.
- `tool_ended` carries the full result payload, which can be large; a UI should truncate for display rather than assume small events.
- MCP callers get no events: the MCP `ask` returns once, when the answer is ready ([Serve and consume MCP](../guides/mcp.md)).
- Delta granularity is whatever the provider streams; there is no flush cadence guarantee beyond event order.
