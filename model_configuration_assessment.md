# Model configuration assessment

This document reviews how Huggr currently configures models and pricing, identifies the sources of friction, and proposes a simpler configuration model that keeps built huglets self-contained while allowing one operator change to reconfigure many huglets.

## Recommendation

Add an optional operator-owned model profile file, but keep model roles in each huglet.

The mental model should be:

1. A huglet declares the model roles it needs, such as `brain` and `summary`.
2. Each role names a deployment profile, such as `quality` or `cheap`.
3. An operator defines those profiles once, including provider, model id, credentials, and fallback prices.
4. `huggr build` resolves the profiles and embeds a concrete snapshot in the artifact.
5. At runtime, an operator profile file may replace that snapshot. Without one, the artifact uses the embedded snapshot.

This is a small extension of the abstraction Huggr already has. `huggr-core` uses open-string `ModelSelector` values and knows nothing about endpoints, credentials, model ids, or pricing. The host already resolves selectors through a `ModelRegistry`. The main change is to stop making every huglet repeat the host-side resolution data.

Pricing should also have one accounting path. Prefer provider-reported cost when it exists, otherwise use the resolved profile's static price, then persist the cost used for that call in the trace. A later price edit must not reprice old traces.

I do not recommend a framework-maintained global catalog of current commercial models and prices. It would create release churn, cannot represent negotiated rates reliably, and would still be incomplete. The global file should be operator-owned data. Importing provider catalogs can be added later as a convenience without making them part of the runtime contract.

## How it works today

### The manifest combines four decisions

Every manifest contains a required `[models]` block. Its block-level fields select one OpenAI-compatible endpoint and API-key environment variable for the whole huglet. Every nested table defines a logical tier, concrete model id, and optional static prices:

```toml
[models]
base_url = "https://router.huggingface.co/v1"
api_key_env = "HUGGR_API_KEY"
default = "brain"

[models.brain]
model = "provider/large-model"
input_usd_per_m_tokens = 1.0
output_usd_per_m_tokens = 1.5

[models.summary]
model = "provider/small-model"
input_usd_per_m_tokens = 0.2
output_usd_per_m_tokens = 0.3

[context]
compaction = "summarize"
summary_model = "summary"
```

This table combines four concerns that change at different rates:

| Concern | Owner | Typical lifetime |
|---|---|---|
| Logical role (`brain`, `summary`) | Huglet author or policy | Stable with huglet behavior |
| Provider endpoint and credential name | Operator | Deployment-specific |
| Concrete model id | Operator | Changed during model evaluation or rollout |
| Token prices | Operator or provider | Changed independently of huglet behavior |

The tier name is already a logical selector. The core records it on every model operation and the policy chooses it. `default` selects the main role, while `[context].summary_model` can select a cheaper role for compaction. A custom `TurnPolicy` may choose any other declared selector.

If `default` is omitted, Huggr chooses `medium` when it exists, otherwise the alphabetically first tier. This fallback saves one line but gives `medium` accidental significance and makes effective configuration less obvious.

### Assembly repeats one adapter per tier

`huggr-toolkit` builds one `OpenAiAdapter` per tier. All adapters receive the same API key and base URL, with only the model id changing. This has two consequences:

- A huglet cannot directly use different providers for `brain` and `summary`. It must put both behind one compatible router.
- Changing endpoint, credentials, models, or prices across several huglets requires editing every manifest or arranging an external search-and-replace workflow.

The lower layers do not impose this restriction. `huggr-host::ModelRegistry` maps each `ModelSelector` to an arbitrary adapter. The one-provider limitation belongs to the toolkit manifest and assembly code.

### Runtime arguments are per-huglet and per-invocation

A manifest can expose runtime arguments that patch `models.base_url`, `models.api_key_env`, or fields of an already-declared tier. This is useful when the huglet author intentionally exposes a caller-facing knob, but it is not a global deployment mechanism:

- Every huglet must declare each argument.
- Every invocation must supply the values, directly or through the declared environment fallback.
- A runtime argument cannot add a tier or replace the whole model set.
- A two-role huglet needs several coordinated values.
- MCP callers receive the same arguments, so using them for operator configuration can unintentionally expose endpoint control to callers.

Runtime arguments should remain an explicit part of a huglet's public call surface. Operator-wide model configuration should be process configuration and should not appear in the `ask` schema.

### Bundles are self-contained

`huggr build` packs the huglet definition, including `huggr.toml`, into the generated artifact. At runtime the bundle is unpacked into a content-addressed definition cache and assembled from that embedded manifest. A built artifact therefore carries its endpoint name, credential environment-variable name, concrete model ids, and prices. It needs the secret itself from the environment, but it does not need a separate configuration file.

This property is worth preserving. A global registry must be an optional override, not a new mandatory runtime dependency.

### Python, TypeScript, and browser configuration repeat the same shape

The Python and TypeScript APIs mirror the manifest: provider fields and arbitrary nested tier tables live in one `models` mapping. The TypeScript/browser form additionally accepts an inline API key because browsers do not have environment variables. Neither surface has a shared profile or override concept.

The same conceptual change should reach all surfaces, but filesystem discovery should not. Native CLI and Python hosts may load a conventional operator file. TypeScript and browser hosts should receive the same resolved profile data explicitly from their host application.

### Pricing currently has two disconnected sources

There are two price mechanisms in the current implementation:

1. Manifest tier prices are used by `AnswerMeta`, cost limits, and `huggr stats`.
2. The OpenAI-compatible adapter reads a router-reported cost when available and stores it in `Usage.extra`. If no cost is reported, the adapter has a small built-in estimate table for `gpt-4o` and `gpt-4o-mini`.

The first mechanism ignores the second. The adapter can record a real router cost while the answer reports the manifest estimate. It can also calculate an estimate from its built-in table while the answer reports zero because the manifest omitted prices. The documentation accurately states that manifest pricing is authoritative, but the adapter comments describe its `Usage.extra` cost as something host metrics can use, and the agent accounting path does not use it.

There is a second historical issue. Tokens and the selector are durable trace data, but the configured static prices are not stored per operation. `huggr stats` receives the currently assembled agent's pricing table, so changing a manifest can reprice old traces. `AnswerMeta` created at the time of the ask remains unchanged wherever that answer was retained, but later trace analytics can disagree with it.

This should be fixed even if model profiles are not implemented.

## Proposed configuration model

### Huglets declare roles

A huglet should say which roles it uses and which operator profile normally serves each role:

```toml
[models]
default = "brain"

[models.brain]
profile = "quality"

[models.summary]
profile = "cheap"

[context]
compaction = "summarize"
summary_model = "summary"
```

`brain` and `summary` remain open strings. They are local to the huglet and are the selectors recorded in traces. `quality` and `cheap` are deployment profile names. They are stable aliases chosen by the operator, not claims about a model's intrinsic size or quality.

The distinction is useful:

```text
policy chooses role -> role names profile -> profile resolves provider + model + price
       "brain"              "quality"          HF router + model X + rates
```

No core type needs to change. The policy continues to choose `brain`; toolkit assembly resolves `quality` and registers the resulting adapter under `brain`.

The default role should become explicit in newly generated manifests. Existing fallback behavior can remain for compatibility, but scaffolds and documentation should stop relying on it.

### Operators define profiles once

The native default file can be `~/.huggr/models.toml`, with `HUGGR_MODELS_FILE` overriding its location:

```toml
[providers.hf]
base_url = "https://router.huggingface.co/v1"
api_key_env = "HF_TOKEN"

[providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[profiles.quality]
provider = "hf"
model = "provider/large-model"
input_usd_per_m_tokens = 1.0
output_usd_per_m_tokens = 1.5
price_as_of = "2026-07-01"

[profiles.cheap]
provider = "openai"
model = "small-model"
input_usd_per_m_tokens = 0.2
output_usd_per_m_tokens = 0.3
price_as_of = "2026-07-01"
```

This structure separates credentials and endpoints from concrete model choices, allows roles in one huglet to use different providers, and updates all huglets referring to a profile with one edit.

The file contains no secrets. It only names environment variables. A browser host can provide an equivalent object with inline credentials through its trusted runtime configuration, as it does today.

`price_as_of` is informational and supports diagnostics. Huggr should not reject an old date because prices can be contractual and intentionally stable. A `huggr models doctor` command could later warn about old or incomplete entries.

### Builds embed a resolved snapshot

Profile indirection must not make artifacts depend on the build machine's home directory at runtime. During `huggr build`, Huggr should resolve every referenced profile and embed a concrete snapshot alongside the original role-to-profile mapping:

```toml
[roles.brain]
profile = "quality"
provider = "hf"
base_url = "https://router.huggingface.co/v1"
api_key_env = "HF_TOKEN"
model = "provider/large-model"
input_usd_per_m_tokens = 1.0
output_usd_per_m_tokens = 1.5

[roles.summary]
profile = "cheap"
provider = "openai"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
model = "small-model"
input_usd_per_m_tokens = 0.2
output_usd_per_m_tokens = 0.3
```

This can be an internal bundle entry rather than a new author-edited file. It should be visible through `--config` so users can inspect what the artifact will actually use.

Resolution should use one predictable precedence order:

1. An explicitly supplied host configuration object, used by embedded Rust, Python, TypeScript, and browser hosts.
2. The native operator file selected by `HUGGR_MODELS_FILE`, or `~/.huggr/models.toml` when it exists.
3. The concrete snapshot embedded in the artifact.
4. A legacy inline tier from `huggr.toml`.

Only complete profiles replace a snapshot. Huggr should not merge arbitrary individual fields from several layers because a model id from one layer combined with prices or credentials from another is hard to reason about. Resolve a role to one complete profile, validate it, then register it.

An operator file should not be exposed as a runtime argument or MCP `ask` property. It is trusted host configuration loaded before the public surface starts.

### Source and artifact behavior

There are three useful modes:

| Definition | Development/build requirement | Built artifact |
|---|---|---|
| Existing inline tier | No profile file | Uses embedded inline configuration; optional operator override only if a profile is named later |
| Profile reference | Matching profile available when running or building | Carries the resolved snapshot and runs without the profile file |
| Host-defined agent | Host supplies profiles directly | Host owns fallback and persistence policy |

This keeps all existing manifests valid. New scaffolds may use an inline tier initially, because a first-time user should not need to understand global configuration. The profile form becomes the recommended next step when the user has multiple huglets or multiple model roles.

A profile-only source checkout is not fully runnable on a machine that has neither its operator file nor a previously resolved snapshot. That is acceptable if the error is direct: `model role 'brain' references missing profile 'quality'; define it in ~/.huggr/models.toml or use an inline tier`. The built artifact remains standalone, which is the stronger portability boundary in Huggr.

An optional checked-in lock file can be considered later if reproducible source builds across machines become important. It is not necessary for the first implementation because the bundle snapshot already solves artifact portability. Adding both a lock file and a bundle snapshot initially would create two mechanisms to maintain.

### Introspection must show origin and effective values

`--config` should show each role, its profile name, the selected source (`host`, `operator`, `bundled`, or `inline`), and the effective non-secret fields. `--describe` should continue to expose public model roles and pricing, but it should not expose credential values.

For example:

```json
{
  "models": {
    "default": "brain",
    "roles": {
      "brain": {
        "profile": "quality",
        "source": "operator",
        "provider": "hf",
        "model": "provider/large-model",
        "api_key_env": "HF_TOKEN",
        "api_key_resolved": true,
        "pricing": {
          "input_usd_per_m_tokens": 1.0,
          "output_usd_per_m_tokens": 1.5,
          "as_of": "2026-07-01"
        }
      }
    }
  }
}
```

This makes configuration failures diagnosable without printing secrets. The trace should additionally record the concrete model id and normalized cost used for each call, as described below.

## Pricing and accounting

### Use one precedence rule

For every completed model call, determine cost in this order:

1. Provider- or router-reported cost from `Usage.extra`, when explicitly marked as reported.
2. Static input/output prices from the resolved profile.
3. Unknown.

The adapter should only parse and preserve provider-reported cost. Its two-entry static estimate table should be removed. Static estimates belong in profiles, where they are visible, overridable, shared by all surfaces, and subject to the same provenance rules.

The host should normalize the selected result into `Usage.extra`, for example:

```json
{
  "cost_micro_usd": 123,
  "cost_source": "provider",
  "model": "provider/large-model",
  "profile": "quality"
}
```

This remains an opaque payload to `huggr-core`, consistent with the narrow-waist rule. The trace already persists `Usage`, so the effective cost becomes immutable trace data without adding a core pricing type.

`AnswerMeta`, cost limits, and `huggr stats` should all fold the normalized per-operation cost. Older traces without it can fall back to the current static pricing behavior, clearly marked as recomputed when displayed.

### Do not silently equate unknown with free

The current numeric contract uses zero when prices are omitted. That preserves simple arithmetic but loses the difference between free and unknown. The least disruptive improvement is to retain `cost_micro_usd` and add an accounting completeness indicator to answer and stats metadata, such as `cost_status = "complete" | "partial" | "unknown"`.

If `max_cost_micro_usd` is configured, every reachable role should have static fallback prices at assembly time. Provider-reported cost alone is not a safe basis for a limit because it may be absent on a later response. Failing assembly is better than silently disabling a declared spend guard.

Free models can declare both prices as `0.0`, which is complete accounting rather than missing accounting.

### Do not ship a live global price catalog in the framework

A framework-owned catalog looks convenient but has poor ownership properties:

- Prices vary by provider, routing suffix, region, batch mode, cache treatment, and negotiated contract.
- Model ids and aliases change independently of Huggr releases.
- Automatic network updates make builds and accounting less reproducible.
- A central catalog would still need local overrides, returning to a layered merge problem.

Operator profiles are the source of static fallback prices. Later conveniences can write those profiles:

- `huggr models import <provider>` can fetch a provider catalog into a chosen file.
- A provider adapter can preserve authoritative cost returned with usage.
- A deployment can generate `models.toml` from its existing secrets or infrastructure configuration.

These are input mechanisms, not new accounting sources. Once a call completes, its normalized cost in the trace is authoritative.

## Alternatives considered

### One global replacement `[models]` block

Replacing every huglet's entire `[models]` block from one global file is simple until huglets use different selector names or need different roles. It also makes the global file depend on each huglet's policy details. Profiles provide shared concrete configuration without moving role ownership away from the huglet.

### Standardize only `small`, `medium`, and `large`

Fixed size tiers are easy to explain but do not describe intent. A summarizer, vision model, embedding model, and tool-using model can have different requirements unrelated to size. Huggr's open selectors are a useful property and should remain open. Examples may use `brain` and `summary` without making them enums.

### Environment variables for every field

Environment variables work for one endpoint and one model but become awkward for several roles and providers. They also have poor introspection and no natural structure for pricing. Keep environment variables for secrets and for selecting the profile-file path, not for encoding the whole registry.

### Runtime arguments for profiles

Runtime arguments are caller-controlled and become part of CLI, MCP, and generated language signatures. Global model selection is operator-controlled. Combining the two would weaken the distinction between a huglet's public parameters and its deployment configuration.

### Always require the global file

This removes duplication but breaks standalone bundles and makes examples harder to run. The embedded snapshot gives global configuration during development and deployment while retaining a portable artifact.

### Automatically maintain prices in Huggr

This moves an external data-maintenance problem into framework releases and still cannot know private rates. Provider-reported cost plus operator-owned fallback prices is smaller and more accurate.

## Suggested implementation sequence

### 1. Unify accounting before changing syntax

- Remove adapter-side static price guesses.
- Add a host-side accounting wrapper that prefers reported cost and otherwise applies resolved static prices.
- Persist normalized cost, source, concrete model id, and optional profile name in `Usage.extra` before the completion event enters the trace.
- Make `AnswerMeta`, limits, and analytics fold the persisted cost.
- Preserve fallback support for older traces and expose when analytics were recomputed.
- Add an accounting completeness field across Rust, Python, and TypeScript contract mirrors.

This fixes a current inconsistency independently of the profile design.

### 2. Add pure profile resolution in the toolkit

- Introduce provider and profile configuration types outside `huggr-core`.
- Allow a tier to contain either the existing inline `model` fields or a `profile` reference, but not both initially.
- Resolve roles from a supplied profile object and return one validated effective configuration per role.
- Keep the existing `ModelSelector` and host `ModelRegistry` unchanged.
- Reject partial profile overlays and missing providers with located errors.

The resolution function should be pure. File discovery belongs in native surface code, and browser storage belongs in the browser host.

### 3. Add native operator-file loading and bundle snapshots

- Load `HUGGR_MODELS_FILE` when set, otherwise load `~/.huggr/models.toml` when present.
- Make missing default files harmless and malformed present files fatal.
- Resolve profiles during `huggr run` and `huggr build`.
- Embed the effective snapshot during build.
- Resolve runtime configuration with the documented precedence order.
- Extend `--config` and `--describe` with role, profile, source, provider, model, and pricing provenance.

The global file should be read by the host before agent assembly. No IO belongs in `huggr-core`.

### 4. Extend programmatic surfaces

- Add equivalent typed provider/profile objects to Rust `RuntimeOptions`, Python, and TypeScript.
- Let native Python use the conventional file by default, matching the CLI.
- Require browser hosts to pass profiles explicitly or use the bundle snapshot.
- Keep inline API keys limited to trusted programmatic/browser host configuration and omit them from introspection.
- Update all Rust/Python serialized mirrors and tests together.

### 5. Migrate examples without forcing the advanced path

- Keep the beginner weather example inline and self-contained.
- Add a guide that converts two inline tiers into `brain` and `summary` roles backed by shared profiles.
- Move multi-role examples to profile references where that makes the benefit visible.
- Update the `huggr-build-agent`, Python, TypeScript, and Chrome-extension skills with the final syntax.
- Update tutorials only after the implementation is real and their command output has been rerun.

### Expected code surface

The implementation should stay concentrated in host layers:

| Area | Current location | Change |
|---|---|---|
| Manifest shape and validation | `crates/huggr-toolkit/src/manifest.rs` | Add role profile references while retaining inline tiers |
| Pure profile resolution | New toolkit module | Resolve a complete effective role from host, operator, bundle, or inline data |
| Native file discovery | `crates/huggr-toolkit/src/surface.rs` and CLI entry points | Load the optional operator file before assembly |
| Bundle snapshot | `crates/huggr-toolkit/src/build.rs` and bundle preparation | Add and read the resolved non-secret snapshot |
| Adapter assembly | `crates/huggr-toolkit/src/runtime.rs` | Construct each role from its resolved provider and profile |
| Reported cost parsing | `crates/huggr-providers/src/openai.rs` | Keep provider-reported cost and delete the built-in static table |
| Cost normalization and limits | `crates/huggr-agent/src/agent.rs` and `limits.rs` | Persist and fold one normalized per-call cost |
| Historical analytics | `crates/huggr-agent/src/analytics.rs` | Prefer persisted cost, with an explicit old-trace fallback |
| Python mirror | `crates/huggr-python/src/config.rs` and `bindings/python/python/huggr_agents/_types.py` | Add typed profiles, providers, and accounting status |
| TypeScript/browser mirror | `bindings/typescript/src/contract.ts` and `agent.ts` | Add the same data model without native file discovery |
| User documentation | Model concepts, agent reference, runtime arguments, tutorials, and agent skills | Explain roles, profiles, provenance, and migration after behavior lands |

`crates/huggr-core` should not appear in the implementation diff unless a test fixture needs updated opaque `Usage.extra` data. No new provider, profile, pricing, or provenance type belongs there.

## Compatibility and invariants

The proposal preserves the important boundaries:

- `huggr-core` remains pure and unchanged. It continues to branch only on open-string selectors and typed model output structure.
- Provider resolution, file loading, credentials, pricing, and snapshots remain host-side concerns.
- Existing inline manifests remain valid.
- Existing built artifacts remain valid and keep their current embedded configuration.
- New built artifacts run from their embedded snapshot when no operator configuration is present.
- Secrets are never written to manifests, snapshots, traces, or introspection output.
- Traces remain immutable. Normalized call cost and model identity enter the trace as part of the existing opaque usage payload.
- Delegated agents inherit native process environment and can discover the same operator file, while each child still resolves only its own declared roles and grants.
- A model/profile addition touches no core type. New provider adapters remain host implementations.

The main compatibility cost is the optional accounting completeness field and the changed interpretation of `Usage.extra.cost`. Huggr is still a prototype with synchronized Rust, Python, and TypeScript surfaces, so this is preferable to maintaining two conflicting accounting paths.

## Decisions to make before implementation

The following choices are narrow enough to settle during implementation:

- Whether the external file uses `profiles` or `models` as its table name. `profiles` is clearer because it includes provider and pricing, while `models` matches the filename.
- Whether `price_as_of` is a date string or an unrestricted provenance object. A date string is enough for the first version.
- Whether `cost_status` belongs directly on `AnswerMeta` or inside a small accounting object. A direct field changes less of the existing contract.
- Whether an explicit host profile object fully disables native file discovery. It should, to keep embedded hosts deterministic.

The larger design should remain fixed: roles are local, profiles are operator-owned, artifacts carry a resolved fallback, and each trace stores the cost actually used.
