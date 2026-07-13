# Models, providers, and pricing

This page explains the four model tiers, how Huggr resolves them, and how a built huglet stays self-contained while remaining configurable on another host.

## The author chooses capability, the operator chooses models

A huglet author selects one of four fixed tiers for each model-using component:

- `fast`: low-latency work such as classification and summarization
- `balanced`: the default for ordinary agents
- `powerful`: harder reasoning and generation
- `max`: the strongest configured option

```toml
[models]
default = "powerful"

[context]
compaction = "summarize"
summary_model = "fast"
```

The manifest does not need to repeat provider endpoints, credentials, concrete model ids, or prices. The recommended home for that operator-owned mapping is `~/.huggr/models.toml`:

```toml
[providers.hf]
base_url = "https://router.huggingface.co/v1"
api_key_env = "HF_TOKEN"

[models.fast]
provider = "hf"
model = "Qwen/Qwen3-4B-Instruct-2507:nscale"
input_usd_per_m_tokens = 0.01
output_usd_per_m_tokens = 0.03

[models.powerful]
provider = "hf"
model = "openai/gpt-oss-120b:deepinfra"
input_usd_per_m_tokens = 0.037
output_usd_per_m_tokens = 0.17
```

Provider names are local aliases. A provider declares an OpenAI-compatible endpoint and the name of the environment variable containing its key. A model entry selects that provider, the concrete model id, and optional input and output prices in USD per million tokens. Secrets never enter either configuration file.

The CLI creates `~/.huggr/models.toml` with mappings for all four tiers the first time any `huggr` command runs. Set `HUGGR_HOME` to relocate the Huggr home, or `HUGGR_MODELS_FILE` to point directly at another catalog. The generated file is ordinary user configuration and can be edited at any time.

## Resolution before a build

When a source huglet is run or built, each tier is resolved independently in this order:

1. A concrete `[models.<tier>]` entry in `huggr.toml`.
2. `HUGGR_MODEL_FAST`, `HUGGR_MODEL_BALANCED`, `HUGGR_MODEL_POWERFUL`, or `HUGGR_MODEL_MAX`.
3. The matching entry in `~/.huggr/models.toml`.

Manifest overrides use the same provider and model shape as the global catalog:

```toml
[providers.local]
base_url = "http://localhost:11434/v1"
api_key_env = "LOCAL_MODEL_KEY"

[models.fast]
provider = "local"
model = "qwen3:4b"
input_usd_per_m_tokens = 0.0
output_usd_per_m_tokens = 0.0
```

An environment override replaces only the concrete model id. It inherits the provider and prices from the global mapping selected for that tier. This keeps the one-value override useful without duplicating provider configuration.

`--config` shows the effective provider, model, prices, source, fallback tier, key environment variable, and whether that variable is set. It never shows the key. `--describe` lists the resolved tiers and pricing on the agent card.

## Missing tiers

A catalog may define any non-empty subset of the four tiers. A missing tier uses the closest configured tier, preferring the lower tier when two candidates are equally close. For example, missing `max` resolves to `powerful`, then `balanced`, then `fast`; missing `fast` resolves upward. `--config` reports the tier it resolved from.

This fallback is for operator convenience. The logical selector remains the tier requested by the author, so traces and aggregate statistics continue to say `fast`, `balanced`, `powerful`, or `max`.

## Build-time snapshots and runtime overrides

`huggr build` resolves all four tiers on the builder's machine and embeds that complete catalog in the artifact. The artifact can therefore run on a machine with no Huggr configuration file.

If a `models.toml` exists on the machine running a built CLI or generated Python artifact, that catalog replaces the embedded mappings for the run. `HUGGR_MODELS_FILE` and `HUGGR_HOME` select the file using the same rules as the CLI. The per-tier `HUGGR_MODEL_*` environment variables can still replace model ids. An invalid existing catalog is an error rather than a reason to silently use the embedded snapshot.

Python-defined and TypeScript-defined agents use the same catalog shape. Pass `model_overrides=` to Python's `Agent`, or `modelCatalog` in the TypeScript runtime options, for an explicit in-process override. Explicit runtime catalogs take precedence over host-global and built-in mappings.

Inside `huggr-core`, selectors remain plain strings and provider payloads remain opaque. Catalog loading, environment access, HTTP adapters, and pricing all stay in host layers.

## Retries and the error split

Adapters follow one rule: if retrying the same request unchanged might work, retry inside the adapter; if the model must change something, return the error to the turn loop.

The OpenAI-compatible adapter in `huggr-providers` retries 429 and 5xx responses and transport failures with exponential backoff (250 ms doubling, capped at 10 s, four attempts total). Other 4xx responses are returned immediately. The brain never sees any of this: only the final outcome becomes an event, so a replayed trace does not re-suffer transient failures. Once a streaming response has started, a mid-stream failure is final rather than retried.

Semantic errors, such as malformed tool arguments or a logical tool failure, take the other path: they return to the model as tool results so it can correct itself within the same turn.

There is no per-request HTTP timeout in the adapter; wall-clock bounds belong to `[limits].timeout_s`, which caps the whole ask (see [limits](limits.md)).

## Cost accounting

`AnswerMeta` is mandatory on every answer, and its `cost_micro_usd` comes from arithmetic over the trace: for each ended model op, `input_tokens × input_usd_per_m_tokens + output_tokens × output_usd_per_m_tokens`, using the resolved tier prices embedded in the running configuration, rounded to the nearest micro-USD. Tokens come from the provider's returned usage, not estimates.

Consequences worth knowing:

- **Omitted prices mean zero cost**, not an error. Tokens and call counts are still reported; only the dollars are missing. Set both prices on every tier you care to account for.
- **A resumed ask bills only its new work.** The fold starts at the resume baseline, so re-asking an old trace never re-bills its ancestry.
- **Delegated cost folds up.** A child agent's `AnswerMeta` merges into the parent's cost, tokens, and call counts (not duration, which the parent's wall clock already covers), so an orchestrator's cost line is complete. `huggr stats` separates own from delegated cost when reporting.
- The upstream router may also report its own cost figure; it is kept as host-side metrics in the usage `extra` and never feeds `AnswerMeta`. The resolved catalog prices are the accounting source of truth for that run.

Micro-USD (1 USD = 1,000,000) keeps the arithmetic in integers; user-facing reports print USD and show `<$0.01` for nonzero dust.

## Limitations

- The adapter speaks the OpenAI-compatible streaming protocol only, and streaming is the only mode.
- Prices are configuration, not provider discovery. Update `models.toml` when a price changes. Existing traces retain the cost recorded when they were produced.
- Built-in policies do not escalate tiers on failure or difficulty. They use the author-selected default and summarizer tiers.
