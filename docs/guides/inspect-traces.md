# Inspect, replay, and verify traces

## What you'll build

Every completed Huggr ask writes an immutable trace to `~/.huggr/<agent>/traces`. Filesystem-backed native asks also keep an atomic live checkpoint after every completed step. This guide reads a trace, resumes interrupted work, replays it event by event with `huggr replay --step`, verifies it bit-for-bit with `huggr verify`, and passes traces to an offline agent for improvement suggestions. It explains how the trace acts as the source of truth.

This assumes you can run and build an agent and know where cost and feedback live. See [Build your first agent](../tutorials/first-agent.md) and [Compose agents and account for cost](compose-agents.md). The trace format is specified in [the runtime documentation](../concepts/runtime.md#determinism-replay-and-traces). This guide provides the hands-on workflow.

## Where traces live

Every surface (`huggr run`, a built binary, or a Python or TypeScript agent) resolves to the same per-agent home `~/.huggr/<sanitized-name>/traces/`. A resumed ask writes a **new** trace whose `depends_on` points at the parent, so lineage is a DAG recorded entirely in headers. List it:

```bash
huggr traces ./examples/huglet-weather
```

The built binary provides the same view through `--traces`. Output is a lineage tree. Each head shows its `trace_id`, parent (`depends_on`), question, status wire string (`success` / `off_topic` / `error` / `interrupted`), and feedback count.

An ask whose process stopped before producing an answer remains in the listing with status `interrupted`. Its mutable snapshot lives under `traces/.checkpoints/`, outside the immutable completed-trace namespace. Resume it through the normal trace argument:

```bash
huggr run ./examples/huglet-weather --trace <interrupted-trace-id> "continue from the last completed step"
```

The new ask cancels any effect that was in flight at the crash boundary and does not repeat completed model or tool steps. When the new ask completes, it writes a new immutable child trace. The interrupted parent remains available for inspection and sibling recovery attempts.

The storage default and path resolution are documented in `crates/huggr-toolkit/src/surface.rs`. Home resolution is implemented in `crates/huggr-agent/src/store.rs`. Environment overrides are `HUGGR_AGENT_HOME`, `HUGGR_HOME`, and `HUGGR_BLOB_STORE`.

## Trace anatomy

A trace is one JSON file keyed by a content-derived `trace_id` (sha256 of the trace, truncated; see `crates/huggr-agent/src/store.rs`). Its top-level shape lives in `crates/huggr-replay/src/lib.rs`:

- **`meta`:** the header: codename, `format_version`, `trace_id`, `depends_on`, `agent_name`/`agent_version`, `question`, `status`, opaque `extra`. Live checkpoints use the open status string `interrupted`.
- **`events`:** the ordered host→brain event stream and the input to replay (`Tick`s, model output, tool results, user input).
- **`commands`:** the ordered brain→host command sequence drained by the live host and the recorded output checked by `verify` (empty in older traces → falls back to log-only comparison).
- **`log`:** the consolidated, `seq`-stamped durable log and source of truth. It contains one `Record` per logical item (user message, consolidated model output, tool result, op-ended), never one per streaming delta.
- **`blobs`:** references to content-addressed payloads. The bytes live in the blob store and are never inlined.

`BrainState` (the live brain's state) is a *fold* over the log, so a trace plus `meta.events` is everything needed to reconstruct a brain.

## Replay one, step by step

`huggr replay` re-feeds a stored trace's events into the brain and walks it forward. Step mode prints every event and the commands and log entries it produced:

```bash
huggr replay ./examples/huglet-weather <trace_id> --step
```

You'll see, per event:

```
[3/12] event=ToolResult → 0 command(s), 1 log entr(ies)
```

This is one line per replayed event (event kind, commands emitted, log entries appended), then a final `replayed N event(s)`. In inspection order, you see how each event (a streamed model output, a tool result, or a timeout tick) changed state and output. The `Inspector` driving this is in `crates/huggr-replay/src/replay.rs`.

Wrap a `replay` call in a script and diff outputs across runs: the same trace bytes always replay to the same commands. That is the determinism guarantee you're debugging against.

## Verify: the determinism gate

`verify` replays a recorded event stream and asserts the re-derived brain produces the *same* command sequence (or the same log, on older traces without `commands`). It is the release gate and the cheapest end-to-end check:

```bash
huggr verify ./examples/huglet-weather <trace_id>
# <trace_id> verified ✓ (replays bit-for-bit)
```

A `verify` failure means the recorded input now produces different output. The usual cause is a brain change that omitted a reducer arm or dropped an event field.

`huggr-core` is **sans-IO and pure**: no clock, RNG, or IO. All nondeterminism is injected as events, including `Tick` for time and events for model output and tool results. The brain's output is therefore a pure function of its input log.

Anything that breaks this property is a bug. See the ground rule in `AGENTS.md`.

## Close the loop with the insights agent

Traces and the feedback filed while [composing agents](compose-agents.md) provide the input for offline improvement analysis. The `examples/huglet-insights` agent is granted the read-only `traces_read` tool family (`trace_list`, `trace_ops`, `trace_transcript`, `feedback_list`), jailed to a target agent's home. Point it at one:

```bash
huggr run ./examples/huglet-insights ~/.huggr/huglet-weather "What should huglet-weather improve?"
```

The agent's method is defined in its `SYSTEM.md`. It uses `trace_list` for an overview and `trace_ops` for the model/tool call sequence without content. It calls `trace_transcript` only when it needs the text behind a pattern, and `feedback_list` for recorded themes.

Results are **summaries and paged, size-capped excerpts**, never raw trace JSON. A full trace would exceed the context budget.

The tool family and its jailing live in `crates/huggr-toolkit/src/tools/traces_read.rs`.

Two things to keep in mind about this kind of agent (full threat note in [the security documentation](../concepts/security.md)):

- **Trace content and feedback payloads are untrusted.** They contain attacker-controlled model output and caller-supplied text. The insights agent must treat everything it reads as data to analyze, never as instructions to follow. Its `SYSTEM.md` says so explicitly.
- **It only ever reports.** Suggestions are for a human or an orchestrator to review; nothing is auto-applied. There is deliberately no self-mutation loop.

The `InsightsResponse` it returns (`patterns` with evidence trace ids, `prompt_suggestions`, `tool_suggestions`, `feedback_themes`) is a structured report you can triage and promote into the agent crate or its manifest.

## Next

Together with [Build your first agent](../tutorials/first-agent.md), this guide completes the workflow: run → trace → replay/verify → analyze → improve. The [reference documentation](../README.md) and [AGENTS.md](../../AGENTS.md) cover the sans-IO contract, narrow-waist rule, storage, and policy details that the guides do not repeat. Back to the [guide index](README.md).
