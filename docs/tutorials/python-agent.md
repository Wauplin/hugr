# Define an agent in Python

In this tutorial, you will define a huglet from scratch in pure Python. The system prompt is a string, model config is a dict, and tools are ordinary callables. The agent runs on the same Rust runtime as every other surface. It is a natural fit when the useful capabilities already live in a Python SDK, data pipeline, notebook, or internal library.

The guide covers the `huggr-agents` package end to end. Topics include the `@huggr.tool` decorator for sync and async tools, standard-library dataclasses alongside explicit JSON Schemas, the `Agent` constructor and its manifest-shaped config, `agent.ask()` for blocking runs, and `async for event in agent.run(...)` for streaming. It closes with an optional Pydantic example for applications that already use it, as well as `agent.feedback()`, `agent.stats()`, and Rust CLI verification of traces stored under `~/.huggr/<name>/`.

Prerequisite: [Build your first agent](first-agent.md) for the ask/answer/trace vocabulary. For the design rationale behind runtime embedding and its distinction from `huggr build --surface python`, see [the language surfaces documentation](../reference/agents.md#language-surfaces).

## Install the package

The `huggr-agents` Python package wraps a PyO3 native module built from `crates/huggr-python`. From the repo root:

```bash
cd bindings/python
python3 -m venv .venv && . .venv/bin/activate
pip install maturin
maturin develop --release
```

`maturin develop` compiles the native extension in place and installs it into your venv. The import name is `huggr_agents`:

```python
import huggr_agents as huggr
```

Pydantic is not required to define or run a Python agent; the base guide uses only the standard library. Install it only for the optional schema-generation example later in this page.

The native crate (`huggr_agents._native`) embeds a tokio runtime and drives the real `huggr-agent` assembly path. A Python-defined agent therefore behaves like a manifest-defined one.

The boundary between the two layers is JSON strings. The native module owns the runtime and all validation. The pure-Python layer declares inputs with `TypedDict`s and recursively casts structured outputs into dataclasses.

## Define a tool

A tool is an ordinary annotated function. `@huggr.tool` reads the advertised surface straight off it: the name from the function, the description from the docstring, and the JSON Schema from the type annotations, FastAPI-style:

```python
import huggr_agents as huggr


@huggr.tool
def lookup_policy(query: str, limit: int = 5) -> dict:
    """Search policy text by keyword."""
    return {"matches": search_policy_text(query)[:limit]}
```

This advertises `{"type": "object", "properties": {"query": {"type": "string"}, "limit": {"type": "integer", "default": 5}}, "required": ["query"], "additionalProperties": false}`, and the model's arguments arrive as keyword arguments. Supported annotations: `str`, `int`, `float`, `bool`, `list[...]`, `dict`, and `Optional[...]` (or the equivalent `X | None`); parameters without defaults are required. An unannotated parameter is an error; the advertised surface must stay auditable, so Huggr refuses to guess.

The decorator signature is `tool(fn=None, *, name=None, description="", schema=None, requires_permission=False, background=False)`. It works bare (`@huggr.tool`), called (`huggr.tool(fn, ...)`), or as a decorator factory (`@huggr.tool(...)`), and `name`/`description` override the inferred values.

When a tool's input shape outgrows what annotations can express (nested objects, enums, cross-field constraints), pass `schema=` explicitly. The callable then receives the raw decoded arguments as a single `dict` and validates however it likes:

```python
@huggr.tool(schema={
    "type": "object",
    "properties": {"query": {"type": "string"}},
    "required": ["query"],
})
def lookup_policy(args):
    """Search policy text by keyword."""
    return {"matches": search_policy_text(args["query"])}
```

### Async tools

The callable may be `async`. The runtime drives the returned coroutine with `asyncio.run` on a blocking worker thread:

```python
@huggr.tool
async def lookup(word: str) -> dict:
    """Look a word up."""
    await some_async_work()
    return {"definition": "async ok"}
```

Sync and async tools are interchangeable from the agent's perspective, so choose the form that fits your I/O. An async tool runs on a fresh event loop on a worker thread (via `asyncio.run`), not on your program's running loop, so it must be self-contained: create its own clients and await its own I/O. A coroutine that depends on objects bound to another loop (a loop-scoped client, task, lock, or `contextvars` state created elsewhere) will fail. A tool that raises an exception does not crash the run. The exception message is sent back to the model as a tool error result, allowing the model to recover and try again or finish the answer.

### The `requires_permission` and `background` flags

`requires_permission=True` marks a tool as gated in the brain's recorded control flow; the current native host approves registered tools automatically. `background=True` marks a tool as fire-and-forget, so the result is not fed back to the model. Both are advanced flags for specific trust models; leave them at their defaults (`False`) for this tutorial.

## Assemble the agent

`huggr.Agent` is keyword-only, so every argument is named. The constructor is:

```python
agent = huggr.Agent(
    name="policy-helper",
    system="Answer from the policy tools. Return JSON.",
    models={"default": "balanced"},
    tools=[lookup_policy],
)
```

The full signature is `Agent(*, name, system=None, models=None, providers=None, model_overrides=None, tools=(), grants=None, limits=None, context=None, response_schema=None, version="0.0.0", description="", traces=None, scratchpad=None)`.

Each config key mirrors the corresponding `huggr.toml` section with the same names and shapes. The manifest details from [Build your first agent](first-agent.md) therefore transfer directly.

The package exports `ModelTier`, `TierConfig`, `ProviderConfig`, `ModelCatalogConfig`, `LimitsConfig`, `ContextConfig`, `GrantsConfig`, and the individual grant shapes for static checking. External grant instance names remain open strings.

### `models`

The `models` dict selects a default from `fast`, `balanced`, `powerful`, and `max`, and may pin a concrete mapping for any tier. Python-defined agents otherwise use the built-in catalog. Pass `providers` with author pins, or pass a complete `model_overrides={"providers": ..., "models": ...}` catalog when the embedding host should choose concrete mappings at runtime. The explicit runtime catalog has precedence over author and built-in mappings. See [Models, providers, and pricing](../concepts/models-and-pricing.md).

### `limits`

The `limits` dict accepts `max_model_calls`, `max_cost_micro_usd`, and `timeout_s`, matching the manifest's `[limits]` keys. Limits are opt-in: an agent has none by default, and each unset key is unbounded.

### `grants`

`grants` is the Python name for the manifest's `[tools]` block, including library tools and the `mcp` and `agent` namespaces. Library tools are keyed by tool id (`{"fs_read": {...}, "web_fetch": {"allow_hosts": [...]}}`). MCP and agent grants nest one level deeper:

```python
agent = huggr.Agent(
    name="orchestrator",
    system="...",
    models={...},
    grants={
        "fs_read": {"root": "./docs"},
        "mcp": {"editor": {"command": "npx", "args": ["some-mcp-server"]}},
    },
)
```

Tools you define in Python (the `tools=[...]` list) are registered as capabilities alongside the granted library and external tools; they all show up in the agent card together.

### `context` and `response_schema`

`context` mirrors the manifest's `[context]` block for context projection and deterministic compaction.

`response_schema` is an optional JSON Schema dict. When set, the schema rides the provider request as `response_format`, so a compatible provider can constrain the model's final JSON.

This is the pure-Python counterpart to the Rust `RESPONSE_RUST_TYPE` contract from [Define typed responses and answer hooks](../guides/typed-responses.md). The generic Python path preserves the final value as a JSON object; it does not return a domain dataclass or locally validate it against this schema. A small application can keep using a raw schema dict and construct its own standard-library dataclass from `answer.response`. The optional Pydantic example below shows how an existing Pydantic application can generate the schema and validate the final object from one set of types.

```python
from dataclasses import dataclass


@dataclass
class PolicyAnswer:
    answer: str


policy_response_schema = {
    "type": "object",
    "properties": {"answer": {"type": "string"}},
    "required": ["answer"],
    "additionalProperties": False,
}

# Pass response_schema=policy_response_schema to Agent(...).
answer = agent.ask("Can I expense a train ticket?")
policy_answer = PolicyAnswer(**answer.response)
```

This keeps small contracts easy to inspect and has no dependency beyond Python itself. A standard-library dataclass rejects missing or unexpected constructor fields, but it does not perform runtime type validation; add explicit checks when needed, or choose the optional Pydantic pattern later in this tutorial.

### `traces` and `scratchpad`

`traces` and `scratchpad` are optional string paths that override where traces and the scratchpad live, equivalent to the manifest's `[traces] store` and `[scratchpad] root`. When omitted, defaults apply (see "Where traces land" below).

## Ask a question

`agent.ask(question)` blocks until the turn finishes and returns an `Answer`:

```python
answer = agent.ask("Can I expense a train ticket?")
print(answer.status, answer.response, answer.trace_id)
```

The full signature is `ask(question, *, trace_id=None, blobs=(), skills=(), extra=None)`.

`trace_id` resumes a prior conversation. The parent is re-folded into a fresh brain, and a new trace is written with `depends_on` set. Resuming the same parent twice creates a fork.

`blobs` is a sequence of `BlobHandle` objects (see below). `skills` is a sequence of caller-local standard Agent Skills paths. `extra` is an opaque JSON-serializable value stamped into the trace header.

### The `Answer` type

`Answer` is a dataclass with `status`, `response`, `trace_id`, `metadata`, `blobs`, and `extra`. `status` is `huggr.STATUS_SUCCESS` or `huggr.STATUS_ERROR`. `response` is the user-facing payload dict, `metadata` is an `AnswerMeta`, and `blobs` is a list of `BlobHandle` values.

The `.ok` property is shorthand for `status == STATUS_SUCCESS`. Traced turn failures, such as a blown limit or model error, return `status == "error"` with `response == {"error": ...}`. Configuration and infrastructure failures, including assembly with a missing provider key, raise exceptions because no trace-backed answer exists.

### `AnswerMeta`

`AnswerMeta` carries the mandatory cost accounting: `duration_ms`, `cost_micro_usd`, `tokens_in`, `tokens_out`, `model_calls`, `tool_calls`. Every field is an int defaulting to zero. These numbers come from the runtime's per-op fold; the same ones `huggr stats` aggregates.

### Passing blobs

`BlobHandle` is a dataclass with `ref` (a `BytesBlobRef`, `PathBlobRef`, or `Sha256BlobRef` dataclass), `media_type` (a string), and optional `name`. `from_bytes`, `from_path`, and `from_sha256` cover the three wire variants:

```python
blob = huggr.BlobHandle.from_path("./report.pdf", media_type="application/pdf")
answer = agent.ask("Summarize this report.", blobs=[blob])
```

`from_path(path, media_type="application/octet-stream", name=None)` builds a `PathBlobRef` for a local file the host reads. `from_sha256(sha256, media_type=..., name=None)` builds a content-addressed `Sha256BlobRef` into the shared blob store, while `from_bytes(base64, ...)` builds an inline `BytesBlobRef`. The file is materialized into the agent's scratchpad before the turn starts.

## Stream events

For live UIs or progress reporting, use `agent.run(...)` as an async iterator. It takes the same arguments as `ask` and yields the `AgentEvent` union; every variant and every structured nested value is a dataclass:

```python
import asyncio

async def stream():
    async for event in agent.run("Can I expense a train ticket?"):
        if isinstance(event, huggr.TextDeltaEvent):
            print(event.text, end="", flush=True)
        elif isinstance(event, huggr.ToolStartedEvent):
            print(f"\n[tool: {event.name}]")
        elif isinstance(event, huggr.AnswerReadyEvent):
            print(f"\n→ {event.answer.status}, trace {event.answer.trace_id}")

asyncio.run(stream())
```

Every event dataclass retains its literal `type` attribute for discriminated-union narrowing, while `isinstance` gives the most direct Python branch. The vocabulary is:

- `ask_started`: the turn began; carries `trace_parent` (the resumed parent's id, or `None`).
- `model_started`: a model call started; carries `op` and `tier` (the selector string).
- `text_delta`: a chunk of streamed assistant text; carries `op` and `text`.
- `model_ended`: a `ModelEndedEvent`; carries `op` and a `Usage` dataclass.
- `tool_started`: a tool call fired; carries `op`, `name`, and `args` (the decoded JSON).
- `tool_ended`: a tool call returned; carries `op`, `name`, `is_error` (bool), and `result`.
- `notice`: a free-form status message; carries `message`.
- `done`: a `DoneEvent`; carries a normalized `DoneReason` dataclass (`kind` is `end_turn`, `cancelled`, or `error`, with an optional error `message`).
- `answer_ready`: an `AnswerReadyEvent`; carries the full `Answer` dataclass.

A successful stream starts with `AskStartedEvent` and ends with `AnswerReadyEvent`; the final answer is already available as `event.answer`. Infrastructure failures can terminate iteration without an answer and raise from the native stream.

## File feedback

Feedback is the asynchronous back-channel for recording, beside an immutable trace, whether an answer helped. It is never read during a live ask and is intended for offline analysis (see [Inspect, replay, and verify traces](../guides/inspect-traces.md)).

```python
answer = agent.ask("Can I expense a train ticket?")
fb = agent.feedback(answer.trace_id, {"score": 5, "note": "correct policy cited"})
assert fb.trace_id == answer.trace_id
```

`feedback(trace_id, payload)` returns a `Feedback` dataclass (`trace_id`, `payload`, `created_at_ms`). The payload is opaque JSON; Huggr never interprets it. Read it back with `feedback_for(trace_id)` which returns a `List[Feedback]`. Filing feedback on a nonexistent trace raises `RuntimeError`; a malformed trace id raises `ValueError` before any store access.

## Inspect and aggregate

Two methods give you the same audit views as the CLI flags from [Build your first agent](first-agent.md):

```python
card = agent.describe()
print([tool.name for tool in card.tools])
print(card.model_tiers[0].selector, card.limits)

heads = agent.traces()
for h in heads:
    print(h.trace_id, h.status, h.question)

stats = agent.stats()
print(stats.totals.cost_micro_usd)
```

`describe()` returns an `AgentCard` dataclass with nested `ToolCard`, `ToolSchema`, `ModelTierCard`, `TierPrice`, and `AgentLimits` values. `traces()` returns a list of `TraceHead` dataclasses.

`stats(*, since=None, trace=None)` returns an `AgentStats` graph with typed totals, duration, per-trace, model, tool, and child-agent rows. Pass `since` to aggregate from a trace onward, or `trace` for one trace only.

If the assembly produced any warnings (e.g., a grant referencing an unknown library tool), they're available on the `agent.warnings` property as a list of strings.

## Where traces land

Traces persist under `~/.huggr/<agent-name>/traces/`, the same per-agent home used by every other surface. The agent name in the constructor names the directory.

Override the shared root with `HUGGR_HOME`, or set one agent's home directly with `HUGGR_AGENT_HOME`. The scratchpad lives at `~/.huggr/<name>/scratch/`. The shared blob store lives at `~/.huggr/blobs` and can be overridden with `HUGGR_BLOB_STORE`.

The pytest suite in `bindings/python/tests/` sets `HUGGR_HOME` to a temporary directory for each test.

## Trace compatibility

A trace written by a Python agent is a plain JSON file in the standard Huggr format. It contains no Python metadata and does not need Python to be replayed because capability results are recorded as events. The `huggr` CLI currently requires an agent crate folder that resolves to the trace store, so it cannot take a Python-defined agent home or a trace file directly. The `huggr-replay` library can verify the trace bytes without calling Python. See [Inspect, replay, and verify traces](../guides/inspect-traces.md) for the replay/verify workflow.

## Optional: derive schemas with Pydantic in a data-analysis agent

Python runtime embedding is especially useful when the capabilities the agent needs already live in a Python SDK or data stack. This optional example gives a subscription-retention agent controlled access to a pandas `DataFrame`: pandas does the deterministic filtering and arithmetic, while the model explains the result and recommends follow-up actions. The same pattern works with a warehouse client, analytics SDK, notebook library, or an internal Python package without putting those implementation details into Huggr core.

Pydantic is not required by Huggr, and it is not the default choice for a small agent: the raw schema and standard-library dataclass pattern above has no extra dependency and stays very transparent. Use Pydantic here when the surrounding application already depends on it, or when generated JSON Schema and stricter runtime validation outweigh another dependency. `TypeAdapter.json_schema()` produces the explicit schema that Huggr advertises to the model, and `TypeAdapter.validate_python()` validates decoded tool arguments before the callable uses them. Huggr does not depend on Pydantic or infer schemas from Python annotations; this is application code choosing Pydantic as its schema generator.

If you choose this variant, install its application dependencies next to `huggr-agents`:

```bash
pip install pandas "pydantic>=2"
```

Save the following as `run.py`. Set `HF_TOKEN` for the built-in Hugging Face catalog before running it:

```python
from typing import Literal

import huggr_agents as huggr
import pandas as pd
from pydantic import ConfigDict, Field, TypeAdapter
from pydantic.dataclasses import dataclass

ACCOUNTS = pd.DataFrame.from_records(
    [
        {"account_id": "acme", "segment": "growth", "monthly_revenue_usd": 2400.0, "failed_payments": 2, "weekly_logins": 1},
        {"account_id": "beacon", "segment": "startup", "monthly_revenue_usd": 450.0, "failed_payments": 0, "weekly_logins": 7},
        {"account_id": "cygnus", "segment": "enterprise", "monthly_revenue_usd": 9100.0, "failed_payments": 1, "weekly_logins": 2},
        {"account_id": "delta", "segment": "growth", "monthly_revenue_usd": 1800.0, "failed_payments": 3, "weekly_logins": 0},
    ]
)


@dataclass(config=ConfigDict(extra="forbid"))
class RiskQuery:
    segment: Literal["all", "startup", "growth", "enterprise"] = "all"
    min_failed_payments: int = Field(default=1, ge=0)
    max_weekly_logins: int = Field(default=2, ge=0)


@dataclass(config=ConfigDict(extra="forbid"))
class RiskAccount:
    account_id: str
    reason: str
    monthly_revenue_usd: float


@dataclass(config=ConfigDict(extra="forbid"))
class RetentionReport:
    summary: str
    accounts: list[RiskAccount]
    monthly_revenue_at_risk_usd: float
    recommended_actions: list[str]


risk_query = TypeAdapter(RiskQuery)
retention_report = TypeAdapter(RetentionReport)


@huggr.tool(
    name="find_at_risk_accounts",
    description="Find subscription accounts with both payment failures and low product usage.",
    schema=risk_query.json_schema(),
)
def find_at_risk_accounts(args):
    query = risk_query.validate_python(args)
    matches = ACCOUNTS[
        (ACCOUNTS["failed_payments"] >= query.min_failed_payments)
        & (ACCOUNTS["weekly_logins"] <= query.max_weekly_logins)
    ]
    if query.segment != "all":
        matches = matches[matches["segment"] == query.segment]

    accounts = [
        {
            "account_id": str(row.account_id),
            "segment": str(row.segment),
            "monthly_revenue_usd": float(row.monthly_revenue_usd),
            "failed_payments": int(row.failed_payments),
            "weekly_logins": int(row.weekly_logins),
        }
        for row in matches.itertuples(index=False)
    ]
    return {
        "accounts": accounts,
        "monthly_revenue_at_risk_usd": sum(
            account["monthly_revenue_usd"] for account in accounts
        ),
    }

agent = huggr.Agent(
    name="retention-analyst",
    system="""You investigate subscription churn risk.
Always call find_at_risk_accounts before answering.
Base every account and revenue figure on the tool result.
Return a RetentionReport JSON object and no additional fields.
""",
    models={
        "default": "balanced",
    },
    tools=[find_at_risk_accounts],
    response_schema=retention_report.json_schema(),
)

answer = agent.ask(
    "Find growth accounts at risk using the default thresholds. "
    "Explain why each account qualifies and recommend the next action."
)
if not answer.ok:
    raise RuntimeError(answer.response["error"])

report = retention_report.validate_python(answer.response)
print(report.summary)
for account in report.accounts:
    print(account.account_id, account.monthly_revenue_usd, account.reason)
print("MRR at risk:", report.monthly_revenue_at_risk_usd)
print("trace:", answer.trace_id)

# Inspect what landed on disk.
for head in agent.traces():
    print(head.trace_id, head.depends_on, head.status)
```

Run it with `python run.py`. The first run writes a trace to `~/.huggr/retention-analyst/traces/` in the portable `huggr-replay` format.

There are two distinct validation points. Pydantic validates each tool call inside `find_at_risk_accounts`; an invalid threshold raises an exception, which Huggr returns to the model as a semantic tool error it can correct. `response_schema` asks the provider for the same shape, and after a successful answer `retention_report.validate_python(answer.response)` validates the opaque JSON payload and turns it into the application's typed dataclass graph. `extra="forbid"` both rejects unexpected fields in the application and emits `additionalProperties: false` in the generated JSON Schema.

### Resume and fork

Pass a prior answer's `trace_id` to continue the conversation. A new trace is written with `depends_on` pointing at the parent:

```python
follow_up = agent.ask("Now assess every segment with the same thresholds.", trace_id=answer.trace_id)
assert follow_up.trace_id != answer.trace_id
heads = agent.traces()
by_id = {head.trace_id: head for head in heads}
assert by_id[follow_up.trace_id].depends_on == answer.trace_id
```

## A security note

Python callables are **trusted host code**. Huggr jails what the *model* can invoke (sandbox-by-registration; a tool the agent doesn't grant is a tool the model cannot call), not what your Python does once invoked. A tool that reaches outside its declared scope is a hole you drill, not one Huggr can close. See the threat model in [the security documentation](../concepts/security.md).

## Next

You have defined an agent entirely in Python. To use the same runtime from TypeScript through the `huggr-agents` package over the WASM brain in Node and the browser, continue with [Define an agent in TypeScript](typescript-agent.md).
