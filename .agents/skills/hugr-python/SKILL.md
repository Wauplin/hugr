---
name: hugr-python
description: Define and run Hugr subagents in Python with typed contract objects, sync or async callable tools, streaming events, feedback, stats, and standard traces. Use for the hugr-agents Python runtime API, or when deciding between a Python-defined agent and a generated Python wheel from a Rust-defined agent.
---

# Build Hugr agents with Python

Choose the surface first:

- To define the system prompt, model config, and tools in Python, use the `hugr-agents` runtime API below.
- To consume an existing Rust-defined agent as a typed Python package, keep its definition in Rust and run `hugr build <agent-dir> --surface python --release`; follow [tutorial 04](../../../docs/tutorials/04-agent-binary-from-python.md).

## Prepare the runtime package

From a Hugr checkout:

```bash
cd bindings/python
python3 -m venv .venv
. .venv/bin/activate
pip install maturin pytest
maturin develop --release
```

Import the package as `hugr_agents`. It runs the native Rust runtime; the Python layer is a typed JSON boundary, not a second agent implementation.

## Define tools and the agent

Give every callable an explicit JSON Schema. Sync and async callables are both supported; exceptions become semantic tool errors returned to the model.

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

agent = hugr.Agent(
    name="policy-helper",
    system="Use lookup_policy, then return a JSON object with an answer field.",
    models={
        "base_url": "https://router.huggingface.co/v1",
        "api_key_env": "POLICY_API_KEY",
        "default": "medium",
        "medium": {"model": "google/gemma-4-31B-it:cerebras"},
    },
    tools=[lookup_policy],
    limits={"max_model_calls": 10, "max_cost_micro_usd": 50000, "timeout_s": 60},
    response_schema={
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"],
    },
)
```

Use `grants={"fs_read": {"root": "./docs"}}` for vetted library grants. Keep Python callables privilege-minimal: they are trusted host code, so Hugr controls whether the model can invoke them but does not sandbox what their Python bodies do.

## Ask, stream, resume, and inspect

```python
answer = agent.ask("Can I expense a train ticket?")
follow_up = agent.ask("What receipt is needed?", trace_id=answer.trace_id)
agent.feedback(answer.trace_id, {"score": 5})
print(agent.traces())
print(agent.stats())
```

```python
async for event in agent.run("Can I expense a train ticket?"):
    if event["type"] == "text_delta":
        print(event["text"], end="")
    elif event["type"] == "answer_ready":
        answer = event["answer"]
```

Branch on `answer.ok` or `answer.status`; errors are answers with mandatory metadata. Use `BlobHandle.from_path(...)` and the `blobs=` ask argument for files. State defaults to `~/.hugr/<name>/`, shared with Rust and TypeScript surfaces.

## Validate

```bash
cd bindings/python
. .venv/bin/activate
pytest
```

Python-written traces use the standard format. When a manifest-defined agent directory with the same agent name resolves to that home, verify it with the Rust CLI:

```bash
hugr verify <matching-agent-dir> <trace-id>
hugr replay <matching-agent-dir> <trace-id> --step
```

Read [tutorial 05](../../../docs/tutorials/05-agent-entirely-in-python.md) for the full API. If native import fails, rerun `maturin develop --release` inside the active venv. If model auth fails, set the variable named by `api_key_env`. If a callable throws, fix the tool's input validation or implementation; do not turn a semantic error into a process crash.
