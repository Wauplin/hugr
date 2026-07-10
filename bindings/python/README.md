# hugr-agents — the Python runtime API

Define a Hugr subagent entirely in Python — tools as callables, config as data — running on the same Rust runtime (`hugr-agent`) every other surface uses. This is runtime *embedding*: distinct from `hugr build --surface python`, which ships an already-built agent as a wheel.

```python
import hugr_agents as hugr

@hugr.tool(name="lookup_policy", description="Search policy text.",
           schema={"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]})
def lookup_policy(args):
    return {"matches": search_policy_text(args["query"])}

agent = hugr.Agent(
    name="policy-helper",
    system="Answer from the policy tools. Return JSON.",
    models={
        "default": "medium",
        "base_url": "https://router.huggingface.co/v1",
        "api_key_env": "POLICY_API_KEY",
        "medium": {"model": "moonshotai/Kimi-K2-Instruct", "temperature": 0.2,
                   "input_usd_per_m_tokens": 1.0, "output_usd_per_m_tokens": 1.5},
    },
    tools=[lookup_policy],
    limits={"max_model_calls": 10, "timeout_s": 60},
)

answer = agent.ask("Can I expense a train ticket?")
print(answer.status, answer.response, answer.trace_id)
```

Config keys mirror `hugr.toml` 1:1: `models`, `limits`, `context` are the same tables; `grants` maps to the manifest's `[tools]` (library tools, `mcp`, `agent` namespaces). Tools defined in Python are sync **or** async callables with explicit JSON schemas. `agent.ask()` blocks; `async for event in agent.run(...)` streams the shared `AgentEvent` vocabulary and ends with `answer_ready`. `agent.feedback(trace_id, payload)` files feedback; `agent.traces()` / `agent.stats()` read the store.

Traces persist under `~/.hugr/<name>/` exactly like a manifest-defined agent, and verify with the Rust CLI (`hugr verify`) without importing Python — capability results are recorded events.

Security note: Python callables are **trusted host code**. Hugr jails what the model can invoke (sandbox-by-registration), not what your Python does once invoked.

## Development

```bash
cd bindings/python
python3 -m venv .venv && . .venv/bin/activate
pip install maturin pytest
maturin develop --release
pytest
```

The native crate lives at `crates/hugr-python` (PyO3, abi3). It is excluded from the root cargo workspace so `cargo test --workspace` never needs a Python toolchain.
