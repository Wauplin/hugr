# Context compaction and pruning

This guide explains how a Hugr agent keeps its model context small as a conversation grows: what problem compaction solves, how the mechanism works, how it is implemented in the core, and every knob you can configure. It applies to all surfaces that accept a `[context]` block: the manifest, the Python and TypeScript runtime APIs, and browser hosts on the WASM brain.

## The problem

Every turn, the agent sends the model a context assembled from its history: the system prompt, user messages, previous model output, and tool results. Left alone, that context only grows. Three things go wrong, in order of appearance:

- **Cost.** Input tokens dominate the bill for tool-heavy agents. An agent that re-sends a 30k-token file listing on every turn pays for it on every turn.
- **Quality.** Models weigh recent and relevant context better than a haystack. Stale tool results (an old DOM snapshot, a superseded file read) actively mislead.
- **Hard limits.** Eventually the context exceeds the model's window and the ask fails.

Compaction addresses all three by shrinking what is *sent* per turn. Pruning is the part of compaction that drops stale tool results outright.

## Log versus projection

The key design fact: Hugr never shortens its history. The durable log in `hugr-core` is append-only, and the trace written from it is immutable. What shrinks is the **projection**: the per-turn rendering of the log into a `ModelRequest`.

Projection is owned by the `TurnPolicy` (see [runtime](../runtime.md)). Each turn, the policy's pure `project_context(log, budget)` produces a `ContextPlan` that records, for every log entry, a disposition:

- `Included`: sent verbatim.
- `Truncated`: sent, but cut down to a token cap.
- `Dropped`: not sent, with a note explaining why.
- `Omitted`: not sent because it never contributes to context (bookkeeping records, or entries covered by a summary).

The plan also totals used, truncated, and dropped tokens, so you can always see what the model did and did not receive. Because the policy is pure (no IO, no clock), the same log always projects to the same plan, which is what keeps traces replayable bit-for-bit after compaction kicks in.

Token counts are estimates supplied by the host when records enter the log (roughly four characters per token); the brain only sums them and never tokenizes.

## What happens by default

Nothing. `compaction = "none"` is the default: the `StaticPolicy` pass-through projection renders the whole log one-to-one every turn. For short-lived, single-question agents this is exactly right, and it is the cheapest thing to reason about. Configure compaction when an agent runs long conversations, loops over large tool results, or lives in a browser session that never restarts.

With compaction enabled, the toolkit swaps in the core `BudgetPolicy`, which layers three deterministic mechanisms and one model-backed one over the same pass-through base.

## Mechanism 1: forget rules (pruning)

Forget rules drop tool results that age out, and they apply on every turn regardless of context size. They are declared per tool name under `[context.forget]`:

- `tool_ttl = { web_fetch = 2 }`: a `web_fetch` result is dropped once two or more user turns have happened after it. Time-to-live is measured in user turns, not wall time.
- `keep_last_per_tool = { page_snapshot = 1 }`: only the newest `page_snapshot` result stays; older ones are dropped as soon as a newer one exists.

This is the right tool for capabilities whose output is a *view* that supersedes itself: DOM snapshots, directory listings, search results. The model only ever needs the latest one.

When a tool result is dropped, the assistant message that called it is dropped with it (and vice versa), because chat APIs reject a tool call without its result. The plan marks these as `dropped with paired tool transcript block`.

## Mechanism 2: the budget pass (truncate)

With `compaction = "truncate"`, the policy enforces a token budget once the projection crosses a trigger:

1. Project everything, apply forget rules, and total the estimate. If it is at or under `trigger_tokens`, send as is.
2. Otherwise, walk entries oldest-first, skipping the system prompt and a protected recent window of `keep_recent_tokens`.
3. An oversized block (over `max_block_tokens`) is truncated to that cap; other blocks are dropped, until the total fits `budget_tokens`.
4. A one-line system note is inserted so the model knows compaction happened: `Context compacted deterministically: dropped approximately N token(s), truncated approximately M token(s).`

Everything here is plain arithmetic over the log, so it costs nothing, needs no model, and replays identically.

## Mechanism 3: summarization

With `compaction = "summarize"`, the policy prefers a durable summary over silently dropping the middle of the conversation. When the projection crosses the trigger, it asks the host for one extra model call before the main turn: the blocks older than the recent window are sent to the `summary_model` tier with a fixed instruction to preserve goals, decisions, constraints, tool findings, and unresolved work.

The result is appended to the log as a `ContextSummary` record that states which entries it replaces. From then on, projection renders the summary as a system block (`Context summary through log seq N: ...`) and omits everything it covers. The summary is part of the log, so it lands in the trace, survives resume and fork, and replays deterministically; only its *creation* was a model call, and that call is a recorded event like any other.

If a summary already covers the cutoff, the deterministic budget pass from mechanism 2 applies instead, so the context stays within budget either way. A summarizer call that fails after the adapter's retries fails the ask like any other model error: the answer has `status: "error"` and the partial trace persists.

## Configuration

Everything lives in the manifest's `[context]` block; each key is optional:

```toml
[context]
compaction = "summarize"          # "none" (default) | "truncate" | "summarize"
budget_tokens = 64000             # target projection size (default 128000)
trigger_tokens = 56000            # start compacting past this (default: budget_tokens)
keep_recent_tokens = 8000         # protected tail window (default: budget_tokens / 3)
max_block_tokens = 2000           # per-block truncation cap (default: budget_tokens / 4)
summary_model = "small"           # summarize only; defaults to the default tier

[context.forget]
tool_ttl = { web_fetch = 2 }
keep_last_per_tool = { fs_read = 1, page_snapshot = 1 }
```

Setting `trigger_tokens` below `budget_tokens` gives the agent headroom: compaction starts before the budget is actually exceeded, so the turn that crosses the line still fits. A `keep_recent_tokens` window keeps the model grounded in the current exchange; the compactor never touches it or the system prompt.

The same shape works on every surface:

```python
agent = hugr.Agent(
    name="researcher",
    models={...},
    context={
        "compaction": "summarize",
        "budget_tokens": 64_000,
        "forget": {"keep_last_per_tool": {"fs_read": 1}},
    },
)
```

```ts
const agent = createAgent({
  name: "researcher",
  models: {...},
  context: { compaction: "truncate", budget_tokens: 64000, keep_last_per_tool: { page_snapshot: 1 } },
});
```

The TypeScript `ContextConfig` flattens the forget maps to top-level `tool_ttl` / `keep_last_per_tool` keys; the manifest and Python nest them under `forget`. In all cases the config decodes into the same core `BudgetPolicy`, so compaction behaves identically in a CLI binary, a Python process, and the WASM brain in a browser.

One surface opts in for you: the browser `BrowserAgentConfig` defaults to `compaction = "summarize"` with a 64k budget and `keep_last_per_tool = 1` for the page-view tools, because browser sessions are long-lived and page snapshots are large and self-superseding. Pass an explicit `context` to change that.

## Worked example

A docs agent with a 64k budget reads files in a loop. Turn by turn:

1. Turns 1 to 6 project everything; the estimate stays under the 56k trigger. Forget rules already drop all but the newest `fs_list` result.
2. Turn 7 crosses the trigger. With `summarize`, the host first calls the summarizer over everything older than the last 8k tokens, appends the `ContextSummary`, and only then runs the main turn: system prompt, one summary block, and the recent window.
3. Turn 8 projects the summary plus new turns. Nothing is re-summarized until the projection crosses the trigger again, at which point a new summary replaces a larger prefix.

The log still contains every file read and every model reply; `hugr traces` and `hugr replay` show them all, and the trace verifies bit-for-bit.

## Observing and debugging

- The `ContextPlan` is the explanation: every entry carries its disposition and estimated tokens, and drops carry a note naming the rule that removed them (`dropped by deterministic forget rule`, `dropped by deterministic budget policy`, `dropped with paired tool transcript block`).
- The injected compaction note and any `ContextSummary` blocks are visible in the trace, so a model that "forgot" something can be diagnosed by reading what it was actually sent.
- Because policies are pure and the summary is a recorded event, `hugr verify` remains the gate: a trace recorded under compaction replays and verifies exactly like any other. See [traces, replay, and debugging](08-traces-replay-debugging.md).

## Limitations

- Token counts are estimates, not tokenizer output; budgets are approximate by design, so leave headroom against the model's real window.
- Summarization is lossy and costs one extra model call whenever a new summary is produced; details the summary omits are gone from future context (never from the log).
- Forget rules key on exact tool names and drop whole results; there is no partial expiry within a result.
- Truncation cuts text blocks by character count, so a truncated block can end mid-sentence; the marker `[...truncated...]` makes that visible to the model.
