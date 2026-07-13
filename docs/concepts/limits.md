# Limits

This page explains how to bound what a huglet can spend: the `[limits]` block, how each limit is enforced, and why an exceeded limit is an answer rather than an exception.

## The problem

An agent loop spends money and time on every iteration, and a confused model can loop indefinitely: re-reading files, retrying a doomed plan, calling tools past any useful point. Behind an orchestrator, the bound has to be declared up front and enforced by the host.

## The `[limits]` block

Limits are opt-in. An agent has none by default, and every unset key is unbounded:

```toml
[limits]
max_model_calls = 20          # refuse the 21st model call
max_cost_micro_usd = 50000    # micro-USD; 50000 = $0.05
timeout_s = 120               # wall clock for the whole ask
```

These are the only three keys. Enforcement is entirely host-side, wrapped around the model adapter and the ask, so `huggr-core` never learns about limits and replay determinism is untouched:

- **`max_model_calls`** counts calls at the adapter boundary and refuses the call past the cap.
- **`max_cost_micro_usd`** folds each completed call's authoritative cost (provider-reported cost when present, otherwise manifest pricing × returned usage; see [models, tiers, and pricing](models-and-pricing.md)) into a running total, and refuses the *next* model call once the total has crossed the cap. A call's cost is unknowable until its usage returns, so the cap is a threshold that stops further spending, not a hard ceiling on the final number: expect the total to overshoot by up to one call.
- **`timeout_s`** races the whole turn against a deadline; on expiry the in-flight work is aborted and drained.

Note that `max_agent_depth`, which stops runaway recursive delegation, is a separate guard on the delegation capability, not a `[limits]` key.

## Errors are answers

A tripped limit does not throw. The refusal flows through the normal machinery, the turn ends, and the caller gets an ordinary answer:

```json
{
  "status": "error",
  "response": { "error": "limit exceeded: max_cost_micro_usd (50000)" },
  "trace_id": "…",
  "metadata": { "cost_micro_usd": 51231, "model_calls": 7, "…": "…" },
  "extra": { "limit_exceeded": { "limit": "max_cost_micro_usd", "value": 50000 } }
}
```

Three things make this shape useful. The **partial trace persists** and verifies, so you can replay exactly what the agent did before the cap and resume from it with a follow-up ask if the work was on track. The **metadata is complete**, so the orchestrator's accounting includes the failed attempt. And `extra.limit_exceeded` carries a stable key (`max_model_calls`, `max_cost_micro_usd`, `timeout_ms`), so a caller can branch on *which* bound tripped, for example by retrying a timeout with a fresh ask but treating a cost trip as a design problem.

Exit code is 0 on the CLI, as for every answer: callers branch on data, not on process status.

## Limitations

- The cost cap is enforced between model calls, so the final spend can exceed the cap by the cost of the call in flight when it tripped. Size the cap with that margin.
- Limits do not bound tool work: there is no `max_tool_calls`, and a tool that runs long is only caught by `timeout_s`.
- A `timeout_s` abort drops the turn mid-flight; the trace is self-consistent up to the abort, but whatever the model was mid-way through is gone.
- Limits bound one ask. A caller that retries in a loop re-arms them each time; budget across asks belongs to the orchestrator (the caller can sum `AnswerMeta`, which is why it is mandatory).
