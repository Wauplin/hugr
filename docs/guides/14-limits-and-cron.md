# Limits and unattended runs

This guide explains how to bound what a huglet can spend and how to run it on a schedule: the `[limits]` block and how each limit is enforced, why an exceeded limit is an answer rather than an exception, and `[cron.<name>]` jobs with their lineage modes and cost-cap rule. Together they are what makes leaving an agent running without a human watching a bounded decision instead of an open tab.

## The problem

An agent loop spends money and time on every iteration, and a confused model can loop indefinitely: re-reading files, retrying a doomed plan, calling tools past any useful point. Attended, you notice and hit Ctrl-C. Unattended, on a schedule or behind an orchestrator, nothing notices for you. The bound has to be declared up front and enforced by the host.

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
- **`max_cost_micro_usd`** folds each completed call's cost (manifest pricing × returned usage, see [models, tiers, and pricing](13-models-tiers-pricing.md)) into a running total, and refuses the *next* model call once the total has crossed the cap. A call's cost is unknowable until its usage returns, so the cap is a threshold that stops further spending, not a hard ceiling on the final number: expect the total to overshoot by up to one call.
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

## Cron jobs

`[cron.<name>]` declares recurring asks in the manifest, one table per job:

```toml
[cron.daily]
schedule = "0 8 * * *"               # five-field cron, UTC
question = "Write the daily summary."
lineage = "fresh"                    # "fresh" (default) | "chain"

[cron.daily.limits]
max_cost_micro_usd = 10000           # per-job override of [limits]
```

`schedule` must be exactly five fields, `question` must be non-empty, and per-job `[cron.<name>.limits]` overrides `[limits]` key by key (an unset per-job key falls back to the base). Run the scheduler with `huggr cron <agent-dir>` during development or `<agent> --cron-serve` on a built binary; both run every configured job until the process stops, logging one line per fire to stderr.

Scheduling behavior is deliberately simple:

- Each job sleeps until its next cron occurrence, then fires one ordinary `Ask`.
- If a fire is still running when the next occurrence arrives, the new fire is **skipped**, not queued; long runs shed fires rather than pile up.
- Every cron trace is tagged in its metadata with the job name and fire time (`extra.cron`, `extra.fired_at`), so `huggr traces` and `huggr stats` can slice by job.

**Lineage** decides what each fire remembers. `fresh` starts every fire from nothing, which fits idempotent reports. `chain` passes the previous **successful** fire's `trace_id` as the next fire's parent, so the job is one growing conversation: a daily summarizer that chains can see what it already reported. An error fire does not advance the chain; the next fire resumes from the last success. At startup the scheduler recovers the anchor from the trace store (the most recent success tagged with the job name in `extra.cron`), so a restart continues the chain instead of starting a new one. Chained jobs also grow context over time, which is exactly the case [context compaction](09-context-compaction.md) exists for.

## The uncapped-job refusal

Unattended model calls spend money with no human watching, so the scheduler fails closed: at startup it requires every job's **effective `max_cost_micro_usd`** (per-job override or base `[limits]`) to be set, and refuses to start otherwise:

```
[cron.daily] has no max_cost_micro_usd; set one in [limits] or [cron.daily.limits], or pass --allow-uncapped
```

Only the cost cap satisfies the rule; `max_model_calls` or `timeout_s` alone does not, because neither bounds dollars directly. `--allow-uncapped` is the explicit operator override, and it should be exactly as rare as it sounds. Note the cap is per fire, not per day: a job firing hourly with a 10,000 micro-USD cap can spend up to $0.24 a day.

## Worked example

A monitoring huglet checks a status page and appends findings to a chained report:

```toml
[limits]
timeout_s = 300

[cron.hourly]
schedule = "0 * * * *"
question = "Check the status page and update the incident log."
lineage = "chain"

[cron.hourly.limits]
max_cost_micro_usd = 5000
max_model_calls = 10
```

Each fire resumes the previous one, so the model sees the incident log it has been building. A hung fetch dies at five minutes with a persisted partial trace; a runaway loop stops at ten model calls or half a cent, whichever comes first; a fire that overruns the hour causes the next one to be skipped and logged. The worst unattended day this configuration allows is 24 fires × $0.005.

## Limitations

- The cost cap is enforced between model calls, so the final spend can exceed the cap by the cost of the call in flight when it tripped. Size the cap with that margin.
- Limits do not bound tool work: there is no `max_tool_calls`, and a tool that runs long is only caught by `timeout_s`.
- A `timeout_s` abort drops the turn mid-flight; the trace is self-consistent up to the abort, but whatever the model was mid-way through is gone.
- The cron scheduler is in-process: no catch-up for fires missed while the process was down, and no distributed locking; running two schedulers for the same agent double-fires every job.
- Limits bound one ask. A caller that retries in a loop re-arms them each time; budget across asks belongs to the orchestrator (the caller can sum `AnswerMeta`, which is why it is mandatory).
