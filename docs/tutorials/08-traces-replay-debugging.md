# Traces, replay, and debugging

## What you'll build

Every Hugr ask writes an immutable trace to `~/.hugr/<agent>/traces`. You'll read one, replay it event-by-event with `hugr replay --step`, verify it replays bit-for-bit with `hugr verify`, schedule recurring asks with `[cron.<name>]`, and then close the loop: hand those traces to an offline agent that mines them for improvement suggestions. By the end you'll know the trace is the source of truth and how to use it.

This assumes [01](01-first-agent-cli.md) (you can run/build an agent) and [07](07-composition-and-cost.md) (you know where cost and feedback live). The trace format is specified in [`ARCHITECTURE.md`](../../ARCHITECTURE.md) §19; this is the hands-on tour.

## Where traces live

Every surface — `hugr run`, a built binary, a Python or TS agent — resolves to the one per-agent home `~/.hugr/<sanitized-name>/traces/`. A resumed ask writes a **new** trace whose `depends_on` points at the parent, so lineage is a DAG recorded entirely in headers. List it:

```bash
hugr traces ./examples/hugr-weather
```

The built binary has the same view behind `--traces`. Output is a lineage tree: each head shows its `trace_id`, parent (`depends_on`), the question, the status wire string (`success` / `off_topic` / `error`), and a feedback count. The storage default and path resolution are documented in `crates/hugr-toolkit/src/surface.rs` and the home resolution in `crates/hugr-agent/src/store.rs`; env overrides are `HUGR_AGENT_HOME`, `HUGR_HOME`, and `HUGR_BLOB_STORE`.

## Trace anatomy

A trace is one JSON file keyed by a content-derived `trace_id` (sha256 of the trace, truncated; see `crates/hugr-agent/src/store.rs`). Its top-level shape lives in `crates/hugr-replay/src/lib.rs`:

- **`meta`** — the header: codename, `format_version`, `trace_id`, `depends_on`, `agent_name`/`agent_version`, `question`, `status`, opaque `extra`.
- **`events`** — the ordered host→brain event stream: the *input* to replay (`Tick`s, model output, tool results, user input).
- **`commands`** — the ordered brain→host command sequence the live host drained: the recorded *output* `verify` checks against (empty in older traces → falls back to log-only comparison).
- **`log`** — the consolidated, `seq`-stamped durable log: the *truth*. One `Record` per logical thing (user message, consolidated model output, tool result, op-ended) — never per streaming delta.
- **`blobs`** — references to content-addressed payloads; the bytes live in the blob store, inlined never.

`BrainState` (the live brain's state) is a *fold* over the log, so a trace plus `meta.events` is everything needed to reconstruct a brain.

## Replay one, step by step

`hugr replay` re-feeds a stored trace's events into the brain and walks it forward. Step mode prints every event and the commands and log entries it produced:

```bash
hugr replay ./examples/hugr-weather <trace_id> --step
```

You'll see, per event:

```
[3/12] event=ToolResult → 0 command(s), 1 log entr(ies)
```

— one line per replayed event (event kind, commands emitted, log entries appended), then a final `replayed N event(s)`. In inspection order you see exactly how each event — a streamed model output, a tool result, a timeout tick — changed state and output. The `Inspector` driving this is in `crates/hugr-replay/src/replay.rs`.

Wrap a `replay` call in a script and diff outputs across runs: the same trace bytes always replay to the same commands. That is the determinism guarantee you're debugging against.

## Verify: the determinism gate

`verify` replays a recorded event stream and asserts the re-derived brain produces the *same* command sequence (or the same log, on older traces without `commands`). It is the release gate and the cheapest end-to-end check:

```bash
hugr verify ./examples/hugr-weather <trace_id>
# <trace_id> verified ✓ (replays bit-for-bit)
```

A `verify` failure means the recorded input now produces different output — typically a brain change that forgot a reducer arm or dropped an event field. `hugr-core` is **sans-IO and pure**: no clock, no RNG, no IO. All nondeterminism is *injected* as events (`Tick` for time, model output and tool results as events), so the brain's output is a pure function of its input log. Anything that breaks that is a bug — see the ground rule in `AGENTS.md`.

## Schedule recurring asks with cron

A cron job is one manifest section, scheduled host-side (the brain never sees a clock):

```toml
[cron.daily-summary]
schedule = "0 8 * * *"              # 5-field cron: minute hour dom month dow
question = "Summarize today's watch list."
lineage = "chain"                   # resume from the last run; "fresh" is default
# optional limits override [limits] for these unattended asks:
[cron.daily-summary.limits]
max_cost_micro_usd = 20000
```

`schedule` is parsed with `croner` at load time, so a typo is a manifest error before anything runs. Run the scheduler:

```bash
hugr cron ./examples/hugr-weather --allow-uncapped
# or on a built binary:
my-weather --cron-serve --allow-uncapped
```

The process *is* the scheduler: there is no daemonization, no persisted schedule — `systemd`/`launchd` own keeping it running. Each fire is an ordinary `Ask` (with `extra: {"cron": "<name>", "fired_at": …}`), the trace is persisted like any other, and overlap of the same job is skipped (asks can be slow). The cost cap is load-bearing: the scheduler **refuses** to start a job with no effective `max_cost_micro_usd` — unattended model calls spend money with no one watching. Pass `--allow-uncapped` only if you really mean it. The scheduler and config are in `crates/hugr-toolkit/src/cron.rs`.

## Close the loop with the insights agent

Traces (always) plus feedback (filed in 07) are exactly the material for offline self-improvement. The `examples/hugr-insights` agent is granted the read-only `traces_read` tool family — `trace_list`, `trace_ops`, `trace_transcript`, `feedback_list` — jailed to a target agent's home. Point it at one:

```bash
hugr run ./examples/hugr-insights ~/.hugr/hugr-weather "What should hugr-weather improve?"
```

The agent's method (in its `SYSTEM.md`): `trace_list` for an overview, `trace_ops` for the model/tool call sequence without content, `trace_transcript` only when it needs the actual text to explain a pattern, and `feedback_list` for the themes others recorded. Results are **summaries and paged, size-capped excerpts**, never raw trace JSON — a full trace would blow any context budget. The tool family and its jailing live in `crates/hugr-toolkit/src/tools/traces_read.rs`.

Two things to keep in mind about this kind of agent (full threat note in [`ARCHITECTURE.md`](../../ARCHITECTURE.md) Part IV):

- **Trace content and feedback payloads are untrusted.** They contain other models' output and caller-supplied text — attacker-controlled. The insights agent must treat everything it reads as *data to analyze*, never instructions to follow. Its `SYSTEM.md` says so explicitly.
- **It only ever reports.** Suggestions are for a human or an orchestrator to review; nothing is auto-applied. There is deliberately no self-mutation loop.

The `InsightsResponse` it returns (`patterns` with evidence trace ids, `prompt_suggestions`, `tool_suggestions`, `feedback_themes`) is a structured report you can triage and promote into the agent crate or its manifest.

## That's the tour

From [01](01-first-agent-cli.md) you built an agent; from here you've seen the whole loop: run → trace → replay/verify → analyze → improve. For everything the tutorials deliberately don't repeat — the sans-IO contract, the narrow-waist rule, the storage and policy seams — the reference is always [`ARCHITECTURE.md`](../../ARCHITECTURE.md) and [AGENTS.md](../../AGENTS.md). Back to the [tutorial index](README.md).