# An agent entirely in Python

In this tutorial you'll define a Hugr subagent from scratch in pure Python, with the system prompt as a string, model config as a dict, and tools as ordinary callables. It runs on the same Rust runtime as every other surface. You'll learn the `hugr-agents` package end to end: the `@hugr.tool` decorator for sync and async tools, the `Agent` constructor and its config keys (mirroring `hugr.toml` 1:1), `agent.ask()` for blocking runs, `async for event in agent.run(...)` for streaming, `agent.feedback()` and `agent.stats()`, and how the traces it writes under `~/.hugr/<name>/` verify with the Rust CLI without Python in the loop. Prerequisite: [tutorial 01](01-first-agent-cli.md) for the ask/answer/trace vocabulary. For the design rationale behind runtime embedding and why it's distinct from `hugr build --surface python`, see [the language surfaces documentation](../agents.md#language-surfaces).

## Install the package

The `hugr-agents` Python package wraps a PyO3 native module built from `crates/hugr-python`. From the repo root:

```bash
cd bindings/python
python3 -m venv .venv && . .venv/bin/activate
pip install maturin
maturin develop --release
```

`maturin develop` compiles the native extension in place and installs it into your venv. The import name is `hugr_agents`:

```python
import hugr_agents as hugr
```

The native crate (`hugr_agents._native`) embeds a tokio runtime and drives the real `hugr-agent` assembly path, so a Python-defined agent behaves like a manifest-defined one. The boundary between the two layers is JSON strings: the native module owns the runtime and all validation, while the pure-Python layer declares inputs with `TypedDict`s and recursively casts structured outputs into dataclasses.

## Define a tool

A tool is a callable plus an explicit JSON Schema; the advertised surface stays auditable, and Hugr never infers a schema from your signature. Wrap any callable with `@hugr.tool`:

```python
import hugr_agents as hugr

@hugr.tool(
    name="lookup_policy",
    description="Search policy text by keyword.",
    schema={
        "type": "object",
        "properties": {"query": {"type": "string"}},
        "required": ["query"],
    },
)
def lookup_policy(args):
    return {"matches": search_policy_text(args["query"])}
```

The decorator signature is `tool(fn=None, *, name=None, description="", schema=None, requires_permission=False, background=False)`. It works bare (`@hugr.tool`), called (`hugr.tool(fn, ...)`), or as a decorator factory (`@hugr.tool(...)`). When `name` is omitted it defaults to `fn.__name__`; when `description` is omitted it falls back to the function's docstring. When `schema` is omitted, the tool gets `{"type": "object"}` with no required fields and accepts any args dict. The callable takes one argument, a `dict` of decoded JSON args matching the schema, and returns a JSON-serializable result.

### Async tools

The callable may be `async`. The runtime awaits it inside the tokio worker pool:

```python
@hugr.tool(name="lookup", description="d", schema={"type": "object"})
async def lookup(args):
    await some_async_work()
    return {"definition": "async ok"}
```

Sync and async tools are interchangeable from the agent's perspective, so choose the form that fits your I/O. A tool that raises an exception does not crash the run. The exception message is sent back to the model as a tool error result, allowing the model to recover and try again or finish the answer.

### The `requires_permission` and `background` flags

`requires_permission=True` marks a tool as gated. The model can call it, but the host's permission policy must approve it before execution. `background=True` marks a tool as fire-and-forget, so the result is not fed back to the model. Both are advanced flags for specific trust models; leave them at their defaults (`False`) for this tutorial.

## Assemble the agent

`hugr.Agent` is keyword-only, so every argument is named. The constructor is:

```python
agent = hugr.Agent(
    name="policy-helper",
    system="Answer from the policy tools. Return JSON.",
    models={
        "default": "medium",
        "base_url": "https://router.huggingface.co/v1",
        "api_key_env": "POLICY_API_KEY",
        "medium": {
            "model": "moonshotai/Kimi-K2-Instruct",
            "temperature": 0.2,
            "input_usd_per_m_tokens": 1.0,
            "output_usd_per_m_tokens": 1.5,
        },
    },
    tools=[lookup_policy],
    limits={"max_model_calls": 10, "timeout_s": 60},
)
```

The full signature is `Agent(*, name, system=None, models=None, tools=(), grants=None, limits=None, context=None, response_schema=None, version="0.0.0", description="", traces=None, scratchpad=None)`. Each config key mirrors the corresponding `hugr.toml` section with the same names and shapes, so the manifest details from [tutorial 01](01-first-agent-cli.md) transfer directly. The package exports `TierConfig`, `LimitsConfig`, `ContextConfig`, `GrantsConfig`, and the individual grant shapes as `TypedDict`s for static checking. `ModelsConfig` and the nested `mcp`/`agent` instance tables are typed mappings because tier selectors and external grant instance names are deliberately open strings.

### `models`

The `models` dict has three reserved keys (`base_url`, `api_key_env`, and `default`) plus one nested table per tier. A tier table requires a `model` id and optionally carries `temperature`, `max_tokens`, and per-million-token pricing (`input_usd_per_m_tokens`, `output_usd_per_m_tokens`). The `default` knob names which tier the agent uses. This is the exact shape of the `[models]` manifest block.

### `limits`

The `limits` dict accepts `max_model_calls`, `max_cost_micro_usd`, and `timeout_s`, matching the manifest's `[limits]` keys. Each key is optional; an unset key is unbounded.

### `grants`

`grants` is the Python name for the manifest's `[tools]` block, including library tools and the `mcp` and `agent` namespaces. Library tools are keyed by tool id (`{"fs_read": {...}, "web_fetch": {"allow_hosts": [...]}}`). MCP and agent grants nest one level deeper:

```python
agent = hugr.Agent(
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

`context` mirrors the manifest's `[context]` block (context projection and deterministic compaction). `response_schema` is an optional JSON Schema dict: when set, the schema rides the provider request as `response_format`, and the final JSON is validated against it. This is the pure-Python equivalent of the Rust `RESPONSE_RUST_TYPE` contract from [tutorial 02](02-typed-responses-and-hooks.md). Without a Rust type, validation occurs at the schema level rather than through a `serde` cast.

### `traces` and `scratchpad`

`traces` and `scratchpad` are optional string paths that override where traces and the scratchpad live, equivalent to the manifest's `[traces] store` and `[scratchpad] root`. When omitted, defaults apply (see "Where traces land" below).

## Ask a question

`agent.ask(question)` blocks until the turn finishes and returns an `Answer`:

```python
answer = agent.ask("Can I expense a train ticket?")
print(answer.status, answer.response, answer.trace_id)
```

The full signature is `ask(question, *, trace_id=None, blobs=(), extra=None)`. `trace_id` resumes a prior conversation (the parent is re-folded into a fresh brain; a new trace is written with `depends_on` set; forking is just resuming the same parent twice). `blobs` is a sequence of `BlobHandle` objects (see below). `extra` is an opaque JSON-serializable value stamped into the trace header.

### The `Answer` type

`Answer` is a dataclass: `status` (a string; `hugr.STATUS_SUCCESS` or `hugr.STATUS_ERROR`), `response` (a dict; your user-facing payload), `trace_id` (a string), `metadata` (an `AnswerMeta`), `blobs` (a list of `BlobHandle`), and `extra`. The `.ok` property is shorthand for `status == STATUS_SUCCESS`. Errors are answers, not exceptions; a blown limit, a missing key, or a model error comes back as `status == "error"` with `response == {"error": ...}` and `trace_id` still set so you can inspect what happened.

### `AnswerMeta`

`AnswerMeta` carries the mandatory cost accounting: `duration_ms`, `cost_micro_usd`, `tokens_in`, `tokens_out`, `model_calls`, `tool_calls`. Every field is an int defaulting to zero. These numbers come from the runtime's per-op fold; the same ones `hugr stats` aggregates.

### Passing blobs

`BlobHandle` is a dataclass with `ref` (a `BytesBlobRef`, `PathBlobRef`, or `Sha256BlobRef` dataclass), `media_type` (a string), and optional `name`. `from_bytes`, `from_path`, and `from_sha256` cover the three wire variants:

```python
blob = hugr.BlobHandle.from_path("./report.pdf", media_type="application/pdf")
answer = agent.ask("Summarize this report.", blobs=[blob])
```

`from_path(path, media_type="application/octet-stream", name=None)` builds a `PathBlobRef` for a local file the host reads. `from_sha256(sha256, media_type=..., name=None)` builds a content-addressed `Sha256BlobRef` into the shared blob store, while `from_bytes(base64, ...)` builds an inline `BytesBlobRef`. The file is materialized into the agent's scratchpad before the turn starts.

## Stream events

For live UIs or progress reporting, use `agent.run(...)` as an async iterator. It takes the same arguments as `ask` and yields the `AgentEvent` union; every variant and every structured nested value is a dataclass:

```python
import asyncio

async def stream():
    async for event in agent.run("Can I expense a train ticket?"):
        if isinstance(event, hugr.TextDeltaEvent):
            print(event.text, end="", flush=True)
        elif isinstance(event, hugr.ToolStartedEvent):
            print(f"\n[tool: {event.name}]")
        elif isinstance(event, hugr.AnswerReadyEvent):
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

The stream is guaranteed to start with `AskStartedEvent` and end with `AnswerReadyEvent`; the final answer is already available as `event.answer`.

## File feedback

Feedback is the asynchronous back-channel for recording, beside an immutable trace, whether an answer helped. It is never read during a live ask and is intended for offline analysis (see [tutorial 08](08-traces-replay-debugging.md)).

```python
answer = agent.ask("Can I expense a train ticket?")
fb = agent.feedback(answer.trace_id, {"score": 5, "note": "correct policy cited"})
assert fb.trace_id == answer.trace_id
```

`feedback(trace_id, payload)` returns a `Feedback` dataclass (`trace_id`, `payload`, `created_at_ms`). The payload is opaque JSON; Hugr never interprets it. Read it back with `feedback_for(trace_id)` which returns a `List[Feedback]`. Filing feedback on a nonexistent trace raises `RuntimeError`.

## Inspect and aggregate

Two methods give you the same audit views as the CLI flags from [tutorial 01](01-first-agent-cli.md):

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

`describe()` returns an `AgentCard` dataclass with nested `ToolCard`, `ToolSchema`, `ModelTierCard`, `TierPrice`, and `AgentLimits` values. `traces()` returns a list of `TraceHead` dataclasses. `stats(*, since=None, trace=None)` returns an `AgentStats` graph with typed totals, duration, per-trace, model, tool, and child-agent rows; pass `since` to aggregate from a trace onward, or `trace` for one trace only.

If the assembly produced any warnings (e.g., a grant referencing an unknown library tool), they're available on the `agent.warnings` property as a list of strings.

## Where traces land

Traces persist under `~/.hugr/<agent-name>/traces/`, the same per-agent home used by every other surface. The agent name in the constructor is what names the directory. Override the root with the `HUGR_HOME` environment variable (sets the shared root) or `HUGR_AGENT_HOME` (sets one agent's home directly). The scratchpad lives alongside at `~/.hugr/<name>/scratch/`, and the shared blob store at `~/.hugr/blobs`; override the latter with `HUGR_BLOB_STORE`. The pytest suite in `bindings/python/tests/` pins this by setting `HUGR_HOME` to a temp dir per test.

## Verify with the Rust CLI

A trace written by a Python agent is a plain JSON file in the standard Hugr format. It contains no Python metadata and does not need Python to be read. The Rust CLI verifies it bit-for-bit:

```bash
hugr verify ~/.hugr/policy-helper <trace_id>
hugr replay ~/.hugr/policy-helper <trace_id> --step
```

This works because capability results (your Python tools' return values) are recorded as events in the trace; the replayed brain re-folds them without calling Python. The brain is sans-IO and pure, so its output is a pure function of the recorded input log. (See [tutorial 08](08-traces-replay-debugging.md) for the full replay/verify workflow.)

## A complete runnable example

Here is the full agent from this tutorial, runnable end to end. Set `POLICY_API_KEY` to an OpenAI-compatible key (e.g., an `hf_...` token for `router.huggingface.co`) before running:

```python
import hugr_agents as hugr

POLICY_TEXT = "Train tickets within the EU are expensable up to 200 EUR with a receipt."

def search_policy_text(query):
    return [POLICY_TEXT] if "train" in query.lower() else []

@hugr.tool(
    name="lookup_policy",
    description="Search policy text by keyword.",
    schema={
        "type": "object",
        "properties": {"query": {"type": "string"}},
        "required": ["query"],
    },
)
def lookup_policy(args):
    return {"matches": search_policy_text(args["query"])}

agent = hugr.Agent(
    name="policy-helper",
    system="Answer from the policy tools. Return JSON with an 'answer' field.",
    models={
        "default": "medium",
        "base_url": "https://router.huggingface.co/v1",
        "api_key_env": "POLICY_API_KEY",
        "medium": {
            "model": "moonshotai/Kimi-K2-Instruct",
            "input_usd_per_m_tokens": 1.0,
            "output_usd_per_m_tokens": 1.5,
        },
    },
    tools=[lookup_policy],
    limits={"max_model_calls": 10, "timeout_s": 60},
)

answer = agent.ask("Can I expense a train ticket?")
print(answer.status, answer.response, answer.trace_id)
print(answer.metadata.model_calls, answer.metadata.tool_calls, answer.metadata.cost_micro_usd)

# Inspect what landed on disk.
for head in agent.traces():
    print(head.trace_id, head.depends_on, head.status)
```

Save it as `run.py` and execute it. The first run writes a trace to `~/.hugr/policy-helper/traces/`. Verify it without Python:

```bash
hugr verify ~/.hugr/policy-helper <trace_id_from_stdout>
```

### Resume and fork

Pass a prior answer's `trace_id` to continue the conversation. A new trace is written with `depends_on` pointing at the parent:

```python
follow_up = agent.ask("And what about flights?", trace_id=answer.trace_id)
assert follow_up.trace_id != answer.trace_id
heads = agent.traces()
by_id = {head.trace_id: head for head in heads}
assert by_id[follow_up.trace_id].depends_on == answer.trace_id
```

## A security note

Python callables are **trusted host code**. Hugr jails what the *model* can invoke (sandbox-by-registration; a tool the agent doesn't grant is a tool the model cannot call), not what your Python does once invoked. A tool that reaches outside its declared scope is a hole you drill, not one Hugr can close. (See the threat model in [the security documentation](../security.md).)

## Next

You've defined an agent entirely in Python. Next, see the same runtime from TypeScript through the `hugr-agents` package over the WASM brain in Node and the browser: [An agent entirely in TypeScript](06-agent-entirely-in-typescript.md).
