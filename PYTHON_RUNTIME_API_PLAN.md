# Python Runtime API Plan

## Short Answer

Yes, this is feasible without breaking Hugr's core architecture, as long as the Python API is treated as another host-side embedding surface and not as a new mode inside `hugr-core`.

The key rule is: Python-defined functions become host capabilities. The brain still only sees `StartCapability { name, args: Value }`, `CapabilityDone { result: Value }`, model events, ticks, and the durable log. No Python object, interpreter state, filesystem access, clock, network call, or callback logic enters `hugr-core`.

This should not revive the old "build a Python package per agent" path. The Python path would be runtime-only: define an agent in Python, run it in the current Python process, persist traces, and return the standard `Answer`.

## Target User Shape

```python
import hugr

def lookup_policy(args):
    query = args["query"]
    return {"matches": search_policy_text(query)}

agent = hugr.Agent(
    name="policy-helper",
    system="Answer from the policy tools. Return JSON.",
    models={
        "default": "medium",
        "base_url": "https://router.huggingface.co/v1",
        "api_key_env": "POLICY_API_KEY",
        "tiers": {
            "medium": {
                "model": "google/gemma-4-31B-it:cerebras",
                "temperature": 0.2,
                "input_usd_per_m_tokens": 1.0,
                "output_usd_per_m_tokens": 1.5,
            }
        },
    },
    tools=[
        hugr.tool(
            lookup_policy,
            name="lookup_policy",
            description="Search policy text.",
            schema={
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
            },
        )
    ],
    limits={"max_model_calls": 10, "timeout_s": 60},
    traces="./.hugr-traces",
)

answer = agent.run("Can I expense a train ticket?")
```

The exact API can change, but the important properties are that the agent is ordinary Python data, tools are ordinary Python callables, and `agent.run()` returns the same Ask/Answer contract Hugr already uses.

## Architecture Fit

- `hugr-core` remains unchanged, sans-IO, single-threaded, and deterministic.
- `hugr-host` remains the place where model calls, capability calls, clocks, cancellation, and concurrency happen.
- `hugr-agent` remains the runtime that turns an `Ask` into an `Answer`, handles trace persistence, resume/fork, scratchpad, blobs, limits, and accounting.
- The Python crate sits above those layers, likely as `crates/hugr-python`, using PyO3 to expose a Python module.
- Python functions implement the existing `hugr_host::Capability` trait through a Rust wrapper.
- Python-provided agent config is converted into the same runtime ingredients as `hugr.toml`: identity, system prompt, model tiers, tool list, limits, scratch/traces, and optional response schema.
- Replay remains valid because live Python tool outputs are recorded as capability result events; replay verifies the recorded stream and does not re-run Python callbacks.

## Minimal Scope

The first version should support:

- Define an agent entirely from Python data.
- Register Python sync callables as foreground capabilities.
- Provide JSON-schema tool metadata to the model.
- Configure one or more OpenAI-compatible model tiers using the existing provider adapter.
- Run `agent.run(question, trace_id=None, blobs=None, extra=None)` and return a Python representation of `Answer`.
- Persist traces, blobs, and scratchpad on the local filesystem using the existing `TraceStore` and `BlobStore`.
- Resume/fork by passing a prior `trace_id`.
- Preserve `AnswerMeta` accounting, including child/tool/model counts already available from the trace.

The first version can omit:

- Building a standalone binary from a Python-defined agent.
- Exposing Python-defined agents as `[tools.agent.*]` child artifacts.
- Python async callbacks.
- Background Python tools.
- MCP grants from the Python API.
- Runtime-argument patching, because the agent is already being defined at runtime.
- A full Python mirror of every `hugr.toml` field if there is no immediate user need.

## Implementation Plan

1. Add a new optional crate, probably `crates/hugr-python`, built as a PyO3 `cdylib` and published as the `hugr` Python module.
2. Keep the crate outside `hugr-core`; dependencies should be `hugr-agent`, `hugr-host`, `hugr-providers`, `hugr-replay`, `serde_json`, `tokio`, and PyO3-related crates.
3. Define a small Python-facing `Agent` class that owns an internal Rust `hugr_agent::Agent` plus a Tokio runtime or runtime handle.
4. Add a Rust `PyCapability` wrapper implementing `hugr_host::Capability`; it stores name, description, schema, permission/background flags, and a `Py<PyAny>` callable.
5. Convert capability args from `serde_json::Value` to Python dict/list/scalar values, call the Python function under the GIL, then convert its return value back to `serde_json::Value`.
6. Treat Python exceptions as semantic tool errors by returning `Err(Value)` from `Capability::invoke`, so the model can see and react to the failure as a tool result.
7. Start with sync Python callables only; call them through `tokio::task::spawn_blocking` or an equivalent blocking boundary so the host runtime is not stalled by long Python work.
8. Assemble model tiers directly with `OpenAiAdapter`, matching the existing manifest runtime behavior: `api_key_env`, `base_url`, model id, pricing, temperature, and max tokens.
9. Assemble limits, scratch root, trace store, system prompt, response schema, and default model directly on `hugr_agent::Agent`.
10. Expose `run()` as a blocking Python method that internally awaits `Agent::ask`; optionally add `run_async()` later once the asyncio integration story is clear.
11. Return plain Python objects or lightweight dataclasses for `Answer`, `AnswerMeta`, and blob handles; keep field names identical to the JSON contract.
12. Add tests with a fake model adapter or recorded model output so Python capability invocation, trace persistence, resume/fork, and error-as-answer behavior can be tested without a live provider.

## Shared Assembly Question

There are two reasonable ways to assemble the Rust `Agent`.

Option A: `hugr-python` constructs `hugr_agent::Agent` directly. This is simplest and avoids making `hugr-toolkit`'s definition-folder logic more public, but it duplicates some model-tier and limits wiring from `hugr-toolkit::runtime`.

Option B: extract a small shared assembly helper from `hugr-toolkit::runtime` that can accept an in-memory definition plus extra host capabilities. This reduces duplication and keeps the Python path closer to `hugr run`, but it is a small refactor of the host/toolkit boundary.

For a first implementation, prefer Option A unless duplication becomes painful. The Python path is explicitly not the definition-folder CLI path, so direct assembly is acceptable if it stays small and covered by tests.

## Security Model

The model can only call Python tools that the Python user registers, so sandbox-by-registration still holds at Hugr's model/tool boundary.

Python callbacks themselves are trusted host code. If a Python function reads files, opens sockets, mutates globals, or shells out, that is outside Hugr's jail model in the same way an operator-written Rust capability would be. The API should document this clearly: Hugr controls what the model can invoke, not what trusted Python code is allowed to do once invoked.

For tool schemas, the Python API should require explicit names, descriptions, and JSON schemas instead of inferring too much from function signatures. Signature inference can be a later convenience, but the auditable surface should remain visible.

## Determinism And Replay

Live runs involving Python tools are only as deterministic as the Python functions during that live execution, which is already true for any host capability.

Once a run is recorded, replay remains deterministic because the recorded capability result is fed back as an event. Replay should not import Python modules or call Python functions.

The Python API must not add unrecorded state to `BrainState`. Any Python-specific metadata should live in trace metadata, `Answer.extra`, capability result payloads, or host-side files, never in `hugr-core`.

## Packaging

The Python package should be one generic Hugr runtime package, not one generated package per agent.

Development packaging can use `maturin` or `setuptools-rust`. The Rust workspace can keep the crate optional so the core `cargo test` path does not require Python packaging tools unless the Python feature or crate is explicitly tested.

The module name can be `hugr`, but the Rust crate should be named `hugr-python` or `hugr-py` to avoid confusing it with the CLI/toolkit crates.

## Risks

- PyO3 plus Tokio plus the Python GIL can get subtle; keeping v1 blocking and sync-only reduces the surface.
- Returning arbitrary Python objects must be constrained to JSON-compatible values, otherwise the narrow waist is lost.
- Direct assembly may drift from `hugr-toolkit::runtime`; tests should pin parity for model config, limits, and answer shape.
- The old docs explicitly say Python callers use subprocess or MCP; adding this API requires updating `ARCHITECTURE.md`, `ROADMAP.md`, and `README.md` if the plan is accepted.
- Users may assume Python callbacks are sandboxed; the API and docs must say they are trusted host code.

## Recommendation

Proceed only as an optional host-side embedding crate. Do not touch `hugr-core`, do not add Python-specific core types, do not restore per-agent Python build output, and do not make Python callbacks durable state. With those constraints, this is architecturally clean and preserves the core Hugr properties: one reducer, opaque tool payloads, durable traces, replay, resume/fork, and mandatory `AnswerMeta`.
