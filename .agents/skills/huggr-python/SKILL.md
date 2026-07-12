---
name: huggr-python
description: Define and run huglets in Python with typed contract objects, sync or async callable tools, streaming events, feedback, stats, and standard traces. Use for the huggr-agents Python runtime API, or when deciding between a Python-defined agent and a generated Python wheel from a Rust-defined agent.
---

# Build Huggr agents with Python

Select the surface based on where the agent is defined:

- To define the system prompt, model config, and tools in Python, use the `huggr-agents` runtime API below.
- To consume an existing Rust-defined agent as a typed Python package, keep its definition in Rust and run `huggr build <agent-dir> --surface python --release`; follow [Package an agent for Python](../../../docs/guides/package-agent-for-python.md).

## Prepare the runtime package

From a Huggr checkout:

```bash
cd bindings/python
python3 -m venv .venv
. .venv/bin/activate
pip install maturin mypy pytest
maturin develop --release
```

Import the package as `huggr_agents`. It runs the native Rust runtime; the Python layer is a typed JSON boundary, not a second agent implementation.

## Define tools and the agent

Annotate every parameter: the advertised JSON Schema is inferred from the type annotations (`str`/`int`/`float`/`bool`/`list[...]`/`dict`/`Optional[...]`; defaults become optional), the name from the function, the description from the docstring, and the model's arguments arrive as keyword arguments. Pass `schema=` to advertise a hand-written schema instead; the callable then receives the raw arguments dict as its single parameter. Both sync and async callables are supported; async callables run through `asyncio.run` on a blocking worker and cannot reuse objects bound to the caller's event loop. Exceptions become semantic tool errors returned to the model.

```python
import huggr_agents as huggr

@huggr.tool
def lookup_policy(query: str) -> dict:
    """Search policy text by keyword."""
    return {"matches": search_policy_text(query)}

agent = huggr.Agent(
    name="policy-helper",
    system="Use lookup_policy, then return a JSON object with an answer field.",
    models={
        "base_url": "https://router.huggingface.co/v1",
        "api_key_env": "HUGGR_API_KEY",
        "default": "medium",
        "medium": {"model": "google/gemma-4-31B-it:cerebras"},
    },
    tools=[lookup_policy],
    response_schema={
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"],
    },
)
```

Use `grants={"fs_read": {"root": "./docs"}}` for vetted library grants. Keep Python callables privilege-minimal: they are trusted host code, so Huggr controls whether the model can invoke them but does not sandbox what their Python bodies do.

## Ask, stream, resume, and inspect

```python
answer = agent.ask("Can I expense a train ticket?")
follow_up = agent.ask("What receipt is needed?", trace_id=answer.trace_id)
agent.feedback(answer.trace_id, {"score": 5})
print(agent.traces())
print(agent.stats())
```

```python
async def stream():
    async for event in agent.run("Can I expense a train ticket?"):
        if isinstance(event, huggr.TextDeltaEvent):
            print(event.text, end="")
        elif isinstance(event, huggr.AnswerReadyEvent):
            answer = event.answer
```

Fixed-shape inputs use the exported `TypedDict`s: `TierConfig`, `LimitsConfig`, `ContextConfig`, `GrantsConfig`, and the individual grant configs. Tier selectors and external grant instance names remain typed mappings because they are open strings.

Structured outputs are recursive dataclasses: `Answer`, every `AgentEvent` variant, `AgentCard`, `TraceHead`, `Feedback`, and `AgentStats`. Branch on `answer.ok` or `answer.status`. Turn failures are answers with mandatory metadata; configuration and infrastructure failures raise exceptions.

Use `BlobHandle.from_path(...)` and the `blobs=` ask argument for files. Opaque domain payloads remain `JsonValue`/`JsonObject`; validation stays in Rust and Python only casts.

State defaults to `~/.huggr/<name>/`, shared with Rust and TypeScript surfaces.

## Validate

```bash
cd bindings/python
. .venv/bin/activate
pytest
mypy python/huggr_agents
```

Python-written traces use the standard format. When a manifest-defined agent directory with the same agent name resolves to that home, verify it with the Rust CLI:

```bash
huggr verify <matching-agent-dir> <trace-id>
huggr replay <matching-agent-dir> <trace-id> --step
```

Read [Define an agent in Python](../../../docs/tutorials/python-agent.md) for the full API. If native import fails, rerun `maturin develop --release` inside the active venv. If model auth fails, set the variable named by `api_key_env`. If a callable throws, fix the tool's input validation or implementation; do not turn a semantic error into a process crash.
