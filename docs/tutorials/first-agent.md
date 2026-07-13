# Build your first agent

In this tutorial, you will scaffold a weather-answering huglet with `huggr new`, inspect every generated file, ask a question with `huggr run`, resume and fork conversations by trace id, inspect the agent card with `--describe`, and compile one self-contained binary with `huggr build`.

No prior Huggr knowledge is assumed. For the design rationale behind any step, see [the huglet overview](../concepts/overview.md#what-a-huglet-is).

## Scaffold the agent

From the directory where you want the new folder to appear, run:

```bash
huggr new my-agent
```

This creates `./my-agent` from the default `weather` template (the checked-in `examples/huglet-weather` crate, embedded at compile time with the name substituted; pass `--template blank` for a tool-free starting point instead). The command refuses to overwrite an existing folder and tells you the next step on stderr.

## Anatomy of the generated files

The folder is both an agent definition and a small Rust crate:

```
my-agent/
  huggr.toml    # the manifest: identity, models, tool grants, limits
  SYSTEM.md    # the system prompt
  src/lib.rs   # the typed Rust response contract
  Cargo.toml   # a normal crate manifest (serde + schemars)
  README.md    # next steps
```

### huggr.toml

The manifest defines the agent's privileges. The agent can use only what is granted here (see [the manifest reference](../reference/agents.md#the-manifest)). The weather template's manifest has four sections:

```toml
[agent]
name = "my-agent"
version = "0.1.0"
description = "Answers current-weather questions via the Open-Meteo API."

[models]
default = "balanced"

# GET-only HTTP, jailed to an allowlist of hosts (the sandbox boundary).
[tools.web_fetch]
allow_hosts = ["api.open-meteo.com", "geocoding-api.open-meteo.com"]
```

- `[agent]` is the identity: the name also names the agent's state home (`~/.huggr/<name>/` by default, where traces and the scratchpad live).
- `[models]` chooses one of the fixed `fast`, `balanced`, `powerful`, or `max` tiers. The CLI creates `~/.huggr/models.toml` with the concrete provider, model ids, and prices on first run, so all local huglets share one operator-owned mapping.
- `[tools.web_fetch]` is a *grant*: it registers the library's GET-only HTTP tool, jailed to those two Open-Meteo hosts. Delete the section and the agent has no network at all.
- There is no `[limits]` block and none is required: an agent has no caps by default. Add `[limits]` (`max_model_calls`, `max_cost_micro_usd`, `timeout_s`) when you want to bound an ask; every unset key is unbounded.

### SYSTEM.md

The system prompt, in plain Markdown. Template variables like `{{agent_name}}` are substituted at assembly time. The weather prompt tells the model exactly which two Open-Meteo endpoints to hit with `web_fetch` and to answer in one short sentence; edit this file first when you want different behavior.

### src/lib.rs, the response contract

The crate exports a typed response contract:

```rust
pub const RESPONSE_RUST_TYPE: &str = "my_agent::Response";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub response: String,
}
```

`huggr run` and `huggr build` read `RESPONSE_RUST_TYPE`, derive a JSON Schema from the type with `schemars`, ask the provider for that structured output, and cast the final JSON with `serde` before it lands in `Answer.response`. Right now it is a single string; [Define typed responses and answer hooks](../guides/typed-responses.md) shows how to grow it.

## Ask a question

The generated global catalog uses the Hugging Face router and reads `HF_TOKEN`. Set that key, then run one ask from inside (or pointing at) the folder:

```bash
export HF_TOKEN=hf_...
huggr run my-agent "what's the weather in Paris?"
```

You get one pretty-printed JSON `Answer` on stdout, while diagnostics go to stderr. Add `--json` for compact single-line output.

The `Answer` carries `status`, your typed `response` object, a `trace_id`, and mandatory `metadata`: duration, cost in micro-USD, tokens, and model/tool call counts.

**The ask path always exits 0.** A missing key, a bad manifest, or a blown limit returns a `status: "error"` answer instead of crashing. See [the Ask and Answer contract](../reference/agents.md#the-ask-and-answer-contract).

Because this agent has a typed Rust contract, the first `huggr run` compiles a small cached shim crate that links your `src/lib.rs`; later runs reuse it, so only the first ask pays the compile.

## Resume and fork with trace ids

Every completed turn is recorded as an immutable trace. List them as a lineage tree:

```bash
huggr traces my-agent
```

To continue a conversation, pass the parent's trace id back in:

```bash
huggr run my-agent --trace <TRACE_ID> "and in London?"
```

A resumed ask never mutates the old trace. It writes a **new** trace with `depends_on` pointing at the parent. Resuming the same id twice forks the conversation into two branches, and `huggr traces` shows the tree.

`huggr verify my-agent <TRACE_ID>` confirms that a trace replays bit-for-bit. `huggr replay my-agent <TRACE_ID> --step` walks through it event by event. See [determinism and replay](../concepts/runtime.md#determinism-replay-and-traces) for the underlying design.

`huggr stats my-agent` aggregates cost, tokens, and tool usage across stored traces.

## Inspect the agent card

Every agent surface answers `--describe` with its agent card, including its name, tools, context policy, resolved model tiers, pricing, and limits. `--config` returns the effective identity, model providers and provenance, grants, skills, runtime arguments, limits, state paths, and response schema as JSON, including the API key environment variable name and whether it resolves, but never the secret:

```bash
huggr run my-agent -- --describe
```

(The `--` keeps `huggr` from eating the flag; the flags after it go to the agent's own generated surface.)

## Build one standalone binary

```bash
huggr build my-agent --release
```

This generates a shim crate under `my-agent/dist/` (override with `--out <dir>`). The shim embeds the agent bundle, including the manifest, prompt, and response contract, then compiles it with cargo.

The result is one self-contained binary at `my-agent/dist/my-agent-cli/target/release/my-agent` that needs no repository checkout. On startup, it installs its bundle into a content-addressed `.definitions/<name>/<hash>/` cache beside `~/.huggr/<name>/`; traces and other mutable state remain in the agent home, so `--trace` resume works anywhere you copy the binary.

`--surface python` also generates a pip-installable Python module.

The built binary speaks the same universal surface as `huggr run`:

```
my-agent "question" [--trace <ID>] [--json|--pretty] [--blob <PATH>...] [--skill <PATH>...] [--stream]
my-agent --describe | --config | --traces | --stats [--trace <ID>]
my-agent --feedback <TRACE_ID> [--feedback-payload <JSON>]
my-agent --mcp-serve
```

- `--trace <ID>` resumes/forks exactly as with `huggr run`; `--json` switches from the default pretty printing to compact.
- `--blob <PATH>` (repeatable) hands local files in as inbound blobs.
- `--skill <PATH>` (repeatable) adds a standard `SKILL.md` folder, or a folder containing skills, for this ask. The model receives a compact catalog and loads matching instructions on demand.
- `--stream` emits one JSON event per line as the run progresses, then the final `Answer` line.
- `--describe`, `--config`, `--traces`, and `--stats` are the audit views. They return JSON and exit non-zero on failure, unlike the ask path.
- `--feedback <TRACE_ID>` appends a JSON feedback payload to a stored trace (from `--feedback-payload` or stdin).
- `--mcp-serve` turns the binary into a stdio MCP server exposing an ask tool; register the command in any MCP client and your agent becomes a tool.

The workflow is: scaffold the agent, edit two text files, run it, inspect it, and ship one binary.

## Next

To grow the response contract beyond a single string, give the model a different schema than your users see, and post-process answers deterministically with `answer_hooks()`, continue with [Define typed responses and answer hooks](../guides/typed-responses.md).
