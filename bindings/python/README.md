# huggr-agents, the Python runtime API

Define a huglet entirely in Python, tools as callables, config as data, running on the same Rust runtime (`huggr-agent`) every other surface uses. This is runtime *embedding*: distinct from `huggr build --surface python`, which ships an already-built agent as a wheel.

```python
import huggr_agents as huggr

@huggr.tool
def lookup_policy(query: str) -> dict:
    """Search policy text."""
    return {"matches": search_policy_text(query)}

agent = huggr.Agent(
    name="policy-helper",
    system="Answer from the policy tools. Return JSON.",
    models={"default": "balanced"},
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

Config keys correspond to `huggr.toml`: `models`, `limits`, and `context` use the same tables, while `grants` maps to the manifest's `[tools]` (library tools, `mcp`, `agent` namespaces). Models use the fixed `fast`, `balanced`, `powerful`, and `max` tiers; `model_overrides` accepts an explicit provider and model catalog from the embedding host. Fixed-shape input sections are exported `TypedDict`s. Tools defined in Python are sync **or** async callables. The advertised JSON schema is inferred from the function's type annotations (name from the function, description from the docstring, parameters without defaults required), or passed explicitly with `schema=`; the callable then receives the raw arguments dict as its single parameter.

All structured outputs are dataclasses, recursively: `ask()` returns `Answer`; `run()` yields the `AgentEvent` union (`TextDeltaEvent`, `ToolStartedEvent`, `AnswerReadyEvent`, and the other variants); `describe()`, `traces()`, `feedback()`, and `stats()` return `AgentCard`, `TraceHead`, `Feedback`, and `AgentStats`. Domain-owned opaque JSON remains `JsonValue`/`JsonObject`: answer payloads, tool arguments/results, schemas, feedback payloads, and `extra`. Rust performs validation; the Python layer only casts validated JSON into these dataclasses.

`ask()` and `run()` both use the shared signal-aware native event bridge. `Ctrl+C` aborts a blocking ask promptly. Cancelling the task reading `run()` or closing the iterator aborts the in-flight Rust ask and its cancellable native effects. Python callables already executing on a blocking worker cannot be preempted safely and may finish after the ask is cancelled; dropping the agent does not wait for those callables. Keep their side effects idempotent and add application-level cancellation where needed.

Traces persist under `~/.huggr/<name>/` in the same `huggr-replay` format as a manifest-defined agent. Capability results are recorded events, so replay does not import Python. The `huggr` CLI currently needs an agent crate folder that resolves to the trace store; it does not accept a Python agent home or trace file directly.

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

The native crate lives at `crates/huggr-python` (PyO3, abi3), with the reusable event stream behind `huggr-toolkit`'s optional `python-bridge` feature. The embedding crate is excluded from the root cargo workspace, and ordinary workspace builds leave the PyO3 feature disabled.
