# Project structure

## Crate layout

```
crates/hugr-core/       # the sans-IO brain. NO tokio, NO reqwest, NO fs.
crates/hugr-host/       # native tokio host: driver loop, capability/model registries, MCP client.
crates/hugr-providers/  # OpenAI-compatible streaming model adapter.
crates/hugr-replay/     # the trace format + fs content-addressed blob store + replay/verify/inspect.
crates/hugr-agent/      # the subagent runtime: Ask/Answer/Feedback, storage backends (trace/blob/scratch),
                        #   resume/fork, blob exchange, limits, cost accounting, agent-as-tool.
crates/hugr-toolkit/    # agent crate manifests (hugr.toml + SYSTEM.md), the tool library,
                        #   the `hugr` CLI (new / run / build / traces / replay / verify), and
                        #   the language-surface generators (CLI shim, PyO3/maturin).
crates/hugr-python/     # PyO3 runtime embedding: define agents/tools in Python on the same
                        #   runtime. Outside the cargo workspace; built by maturin from bindings/python.
examples/hugr-docs/     # the reference subagent crate (docs Q&A): hugr.toml + SYSTEM.md plus
                        #   typed response contract, run/buildable by hugr-toolkit
examples/hugr-weather/  # the self-contained beginner agent; single source of truth for the
                        #   `hugr new --template weather` scaffold (embedded at compile time).
examples/hugr-insights/ # offline self-improvement agent: mines another agent's traces +
                        #   feedback via `traces_read` and reports improvement suggestions.
examples/hugr-datasmith/ # docs-QA dataset synthesizer: fs_read-jailed, typed QaDataset
                        #   contract, buildable as a typed Python wheel.
examples/hf-librarian/  # Python-surface pipeline: the datasmith wheel in-process, a jailed
                        #   Hub publisher, and a judge-graded eval of hugr-docs.
crates/hugr-wasm/       # generic WASM bindings around hugr-core for browser/JS hosts: submit/poll
                        #   over JSON, the portable-trace AgentSession + verify_trace_json (the
                        #   hugr-replay fold compiled to wasm), and the browser tool schemas.
bindings/python/        # the `hugr-agents` Python package: pyproject (maturin), typed pure-Python
                        #   layer over crates/hugr-python, pytest suite with a mock provider.
bindings/typescript/    # the `hugr-agents` TS package: typed Agent over the WASM brain with
                        #   node/browser storage + the OpenAI-compatible fetch adapter; also hosts the
                        #   plain-JS extension driver modules the chrome-extension example vendors.
examples/chrome-extension/ # a concrete browser host: chrome.* capability dispatcher, content
                        #   script, side-panel UI, MV3 manifest; vendors the generic JS at build time.
```

**`hugr-core` depends on nothing environmental.** Verify this with `cargo tree -p hugr-core`.

`hugr-replay` may use `std::fs`, but it consumes `hugr-core` as pure data. The native layers stack strictly: `hugr-agent` depends on `hugr-host` + `hugr-replay`, then `hugr-toolkit` depends on `hugr-agent`.

Browser-specific behavior lives in JavaScript hosts under `bindings/typescript` and `examples/chrome-extension`. Chrome APIs, IndexedDB, extension UI, and browser tool execution never enter the core or native host crates. `crates/hugr-wasm` is only a JSON-in/JSON-out binding around the brain.

Browser context management uses the same core `BudgetPolicy`. The OpenAI-compatible JavaScript adapter only translates `ModelRequest` blocks to provider messages.

Nothing reaches into `hugr-core` internals. These layers are all hosts.

## Standards

- **MCP** exposes a Hugr agent as a tool to orchestrators. Claude Code and most frameworks speak it.

  Every built binary serves `--mcp-serve` with an `ask` tool whose structured result carries the full `Answer`. It also exposes a `feedback` tool keyed to a returned `trace_id`.

  Session continuity uses the `trace_id` in tool arguments rather than MCP session state. Hugr does not use deprecated MCP sampling; the agent owns its provider.
- **A2A** is the surviving agent↔agent standard for *remote* orchestration; an adapter is possible later (our `describe()` output is card-shaped) but is deliberately not a foundation.
- **The gap Hugr fills**, verified unowned: (a) a cross-process **forkable session contract** (`trace_id`/`depends_on` with bit-for-bit deterministic replay), (b) **mandatory cost/duration metadata on every response**, and (c) **single-binary agent packaging**. Hugr provides this combination.
