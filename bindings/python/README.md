# huggr-agents — the Python runtime API

Define a huglet entirely in Python — tools as callables, config as data — running on the same Rust runtime (`huggr-agent`) every other surface uses. This is runtime *embedding*: distinct from `huggr build --surface python`, which ships an already-built agent as a wheel.

```python
import huggr_agents as huggr

@huggr.tool
def lookup_policy(query: str) -> dict:
    """Search policy text."""
    return {"matches": search_policy_text(query)}

agent = huggr.Agent(
    name="policy-helper",
    system="Answer from the policy tools. Return JSON.",
    models={
        "default": "medium",
        "base_url": "https://router.huggingface.co/v1",
        "api_key_env": "HUGGR_API_KEY",
        "medium": {"model": "moonshotai/Kimi-K2-Instruct",
                   "input_usd_per_m_tokens": 1.0, "output_usd_per_m_tokens": 1.5},
    },
    tools=[lookup_policy],
)

answer = agent.ask("Can I expense a train ticket?")
print(answer.status, answer.response, answer.trace_id)

async def stream():
    async for event in agent.run("What receipt is needed?"):
        if isinstance(event, huggr.TextDeltaEvent):
            print(event.text, end="")
        elif isinstance(event, huggr.AnswerReadyEvent):
            print(event.answer.trace_id)
```

Config keys mirror `huggr.toml` 1:1: `models`, `limits`, `context` are the same tables; `grants` maps to the manifest's `[tools]` (library tools, `mcp`, `agent` namespaces). Fixed-shape input sections are exported `TypedDict`s (`TierConfig`, `LimitsConfig`, `ContextConfig`, `GrantsConfig`, and individual grant shapes); arbitrary tier selectors and external grant instance names use typed mappings because their keys are intentionally open. Tools defined in Python are sync **or** async callables; the advertised JSON schema is inferred from the function's type annotations (name from the function, description from the docstring, parameters without defaults required), or passed explicitly with `schema=` — the callable then receives the raw arguments dict as its single parameter.

All structured outputs are dataclasses, recursively: `ask()` returns `Answer`; `run()` yields the `AgentEvent` union (`TextDeltaEvent`, `ToolStartedEvent`, `AnswerReadyEvent`, and the other variants); `describe()`, `traces()`, `feedback()`, and `stats()` return `AgentCard`, `TraceHead`, `Feedback`, and `AgentStats`. Domain-owned opaque JSON remains `JsonValue`/`JsonObject`: answer payloads, tool arguments/results, schemas, feedback payloads, and `extra`. Rust performs validation; the Python layer only casts validated JSON into these dataclasses.

Traces persist under `~/.huggr/<name>/` exactly like a manifest-defined agent, and verify with the Rust CLI (`huggr verify`) without importing Python — capability results are recorded events.

Security note: Python callables are **trusted host code**. Huggr jails what the model can invoke (sandbox-by-registration), not what your Python does once invoked.

## Development

```bash
cd bindings/python
python3 -m venv .venv && . .venv/bin/activate
pip install maturin mypy pytest
maturin develop --release
pytest
mypy python/huggr_agents
```

The native crate lives at `crates/huggr-python` (PyO3, abi3). It is excluded from the root cargo workspace so `cargo test --workspace` never needs a Python toolchain.
