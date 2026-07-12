# Models, tiers, and pricing

This page explains how a huglet talks to model providers: the `[models]` manifest block, what a tier is and how one is selected per call, how retries and errors are split between the adapter and the turn loop, and how per-tier pricing turns trace-recorded tokens into the mandatory cost line on every answer. The same configuration shape applies to the manifest, the Python and TypeScript runtime APIs, and the browser host.

## The problem

An agent needs a model endpoint, but hardcoding one couples three decisions that change at different speeds: which provider endpoint to hit, which concrete model to use for which kind of work, and what that work costs. Huggr separates them: the host resolves endpoints, the manifest names models under free-form **tiers**, and the policy picks a tier per call by logical name. Cost then falls out of the trace instead of being estimated after the fact.

## The `[models]` block

```toml
[models]
base_url = "https://router.huggingface.co/v1"   # one endpoint for all tiers
api_key_env = "HUGGR_API_KEY"                   # env var name; the key never enters the manifest
default = "medium"                              # optional: which tier the policy uses

[models.medium]
model = "google/gemma-4-31B-it:cerebras"
input_usd_per_m_tokens = 1.0
output_usd_per_m_tokens = 1.5

[models.small]
model = "google/gemma-4-9B-it"
input_usd_per_m_tokens = 0.2
output_usd_per_m_tokens = 0.3
```

The block has exactly three reserved keys (`base_url`, `api_key_env`, `default`); every other table is a tier. A tier has exactly three keys: a required `model` id plus the two optional prices, both in USD per million tokens. There are no per-tier endpoint overrides and no sampling knobs in the manifest; provider-specific parameters ride the opaque `extra` of a `ModelRequest`, set by the policy, never by the reducer.

`[models]` is required and must declare at least one tier. If `default` is set it must name a declared tier; otherwise the default is the tier named `medium` if one exists, else the first tier by name. `medium` is only a naming convention for that fallback, nothing else in the system treats it specially.

The same shape appears verbatim on the other surfaces: Python's `models={"base_url": ..., "default": ..., "medium": {...}}` and TypeScript's `ModelsConfig` use the same keys and the same default-tier rule (TypeScript additionally accepts an inline `api_key`, a browser convenience).

## Tiers and selection

Inside the core, a model is only ever named by a `ModelSelector`, a plain string. Each manifest tier registers one adapter in the host's model registry under its tier name; the `TurnPolicy` chooses a selector per call; the host resolves it at dispatch time. The built-in policies use one fixed tier per turn (the default tier), so today multiple tiers earn their keep in two places:

- `[context].summary_model` names the tier used for compaction summaries, so a long-lived agent can summarize with a cheap model and answer with a good one (see [context compaction](context-management.md)).
- A custom `TurnPolicy` can pick tiers dynamically; because the selector is an open string, adding a tier touches no core types.

A selector that resolves to no registered adapter is not a crash: the host injects a `ModelError` (`no adapter for model <tier>`), which ends the turn as an ordinary error answer with a persisted trace.

Every model op records its selector in the trace's `OpMeta`, so per-tier spend is a query over stored traces, not a separate metering system.

## Retries and the error split

Adapters follow one rule: if retrying the same request unchanged might work, retry inside the adapter; if the model must change something, return the error to the turn loop.

The OpenAI-compatible adapter in `huggr-providers` retries 429 and 5xx responses and transport failures with exponential backoff (250 ms doubling, capped at 10 s, four attempts total). Other 4xx responses are returned immediately. The brain never sees any of this: only the final outcome becomes an event, so a replayed trace does not re-suffer transient failures. Once a streaming response has started, a mid-stream failure is final rather than retried.

Semantic errors, such as malformed tool arguments or a logical tool failure, take the other path: they return to the model as tool results so it can correct itself within the same turn.

There is no per-request HTTP timeout in the adapter; wall-clock bounds belong to `[limits].timeout_s`, which caps the whole ask (see [limits](limits.md)).

## Cost accounting

`AnswerMeta` is mandatory on every answer, and its `cost_micro_usd` comes from arithmetic over the trace: for each ended model op, `input_tokens × input_usd_per_m_tokens + output_tokens × output_usd_per_m_tokens`, using the recorded selector's manifest prices, rounded to the nearest micro-USD. Tokens come from the provider's returned usage, not estimates.

Consequences worth knowing:

- **Omitted prices mean zero cost**, not an error. Tokens and call counts are still reported; only the dollars are missing. Set both prices on every tier you care to account for.
- **A resumed ask bills only its new work.** The fold starts at the resume baseline, so re-asking an old trace never re-bills its ancestry.
- **Delegated cost folds up.** A child agent's `AnswerMeta` merges into the parent's cost, tokens, and call counts (not duration, which the parent's wall clock already covers), so an orchestrator's cost line is complete. `huggr stats` separates own from delegated cost when reporting.
- The upstream router may also report its own cost figure; it is kept as host-side metrics in the usage `extra` and never feeds `AnswerMeta`. Your manifest prices are the accounting source of truth.

Micro-USD (1 USD = 1,000,000) keeps the arithmetic in integers; user-facing reports print USD and show `<$0.01` for nonzero dust.

## Worked example

A docs agent declares the two tiers above with `default = "medium"` and `summary_model = "small"` under `[context]`. A long session makes nine `medium` calls and one `small` summarization call. The trace records ten `OpEnded` entries, each with its selector and usage. The answer's `cost_micro_usd` is the sum of both tiers' token math; `huggr stats` breaks the same numbers out per trace; and switching the summarizer to a different model is a one-line manifest edit that no code, and no core type, notices.

## Limitations

- One provider endpoint per agent: `base_url` and `api_key_env` are block-level, so tiers cannot point at different providers. Front several providers with a router if you need that behind one URL.
- The adapter speaks the OpenAI-compatible streaming protocol only, and streaming is the only mode.
- Pricing is static manifest data; a provider price change is a manifest edit, and past traces are always priced at whatever the manifest said when their answers were folded.
- Built-in policies do not escalate tiers on failure or difficulty; tier strategy beyond the default and the summarizer needs a custom policy.
