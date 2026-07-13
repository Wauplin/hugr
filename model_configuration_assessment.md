# Fixed-tier model configuration

This document records the model configuration implemented by Huggr. It replaces the earlier free-form tier and per-huglet provider design.

## Mental model

Huglet authors choose capability. Operators choose deployments.

A huglet refers only to four stable tiers: `fast`, `balanced`, `powerful`, and `max`. Its main turn selects `[models].default`; components such as context summarization may select another tier. The operator maps those tiers to providers, concrete model ids, credential environment variables, and prices in one global catalog.

```text
huglet component -> fixed tier -> host catalog -> provider + model + price
main turn            powerful     ~/.huggr/models.toml
summarizer           fast
```

This keeps model policy visible in the huglet without copying deployment details into every manifest. The mapping remains host-side data and does not add IO or provider types to `huggr-core`.

## Author configuration

A typical huglet needs only:

```toml
[models]
default = "powerful"

[context]
compaction = "summarize"
summary_model = "fast"
```

The four tier names are closed across manifest, Python, and TypeScript configuration. This deliberately trades arbitrary labels for a smaller shared vocabulary.

An author may pin a concrete mapping when a huglet truly depends on one deployment:

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

Pins are exceptions. Ordinary huglets should leave concrete mappings to the operator catalog.

## Operator catalog

The recommended configuration is `~/.huggr/models.toml`:

```toml
[providers.hf]
base_url = "https://router.huggingface.co/v1"
api_key_env = "HF_TOKEN"

[models.fast]
provider = "hf"
model = "deepseek-ai/DeepSeek-V4-Flash:fireworks-ai"
input_usd_per_m_tokens = 0.14
output_usd_per_m_tokens = 0.28

[models.balanced]
provider = "hf"
model = "google/gemma-4-31B-it:cerebras"
input_usd_per_m_tokens = 1.0
output_usd_per_m_tokens = 1.5

[models.powerful]
provider = "hf"
model = "zai-org/GLM-5.2:together"
input_usd_per_m_tokens = 1.4
output_usd_per_m_tokens = 4.4
```

The CLI writes the built-in catalog on first run when the file is absent. It omits `max`, which falls back to `powerful` unless the operator configures it. `HUGGR_HOME` relocates the Huggr home, and `HUGGR_MODELS_FILE` selects a catalog directly. The file holds no secrets.

[Hugging Face Inference Providers](https://huggingface.co/inference/models) lists provider and model combinations for the built-in `hf` provider. A different OpenAI-compatible service should use a separate provider alias whose `base_url` is the chosen API URL and whose `api_key_env` names its own credential.

A non-empty partial catalog is valid. A missing tier resolves to the nearest configured tier, with the lower tier winning equal-distance ties. For example, `max` falls back through `powerful`, `balanced`, and `fast`. The requested selector does not change, so trace accounting stays in the author's four-tier vocabulary.

## Resolution order

For a source huglet, each tier resolves in this order:

1. A complete manifest `[models.<tier>]` pin.
2. `HUGGR_MODEL_<TIER>`.
3. The global catalog.

An environment override changes the model id only. It inherits provider and pricing from the catalog tier, including nearest-tier fallback. This rule makes the one-value override concise and keeps its inherited fields visible through introspection.

`--config` reports the effective mapping, source, fallback tier, provider, credential environment variable, and whether that variable is set. It never reports credential values. `--describe` reports the resolved tiers and prices on the agent card.

## Build and runtime behavior

`huggr build` resolves all four tiers using the builder's environment and writes the complete result into the bundle. The built CLI and generated Python wheel can run with no host catalog.

At runtime, an existing host `models.toml` replaces the embedded snapshot. This lets an operator reconfigure all deployed huglets without rebuilding them. If the host file is absent, the artifact uses its snapshot. If the file exists but is invalid, startup fails instead of silently using stale embedded values. Per-tier environment overrides remain available.

Python-defined agents accept an explicit `model_overrides` catalog. TypeScript hosts accept `AgentRuntime.modelCatalog`. These explicit host objects use the same provider and model shape and take precedence over built-in mappings. Browser hosts may place an inline key in trusted runtime provider configuration because they have no process environment.

## Pricing

Input and output prices live beside each concrete model mapping. Moving a tier to another model therefore moves its accounting rates in the same edit. `AnswerMeta`, cost limits, and `huggr stats` use the prices resolved for the run. Omitted prices mean zero recorded cost while token counts remain available.

There is no `price_as_of` field. The catalog is operator-owned configuration, so Huggr does not attempt to judge whether a price is current. Operators update mappings when their provider or negotiated rate changes. Existing traces retain the cost recorded when they were produced.

## Maintenance boundary

The built-in catalog is a bootstrap default, not a continuously synchronized provider registry. It changes when maintainers intentionally update Huggr defaults. Operators can update their global catalog independently of framework releases, and built artifacts preserve the resolved values available when they were built.

Model selectors remain strings in `huggr-core`. File discovery, environment precedence, provider assembly, credentials, and price resolution belong to native, Python, TypeScript, or browser hosts. Adding a provider or changing a concrete model must not require a core type change.
