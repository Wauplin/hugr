# Your first agent from the CLI

This guide scaffolds a weather-answering subagent with `hugr new` and explains every generated file. It then asks a question with `hugr run`, resumes and forks conversations by trace id, inspects the agent card with `--describe`, and compiles one self-contained binary with `hugr build`.

No prior Hugr knowledge is assumed. For the design rationale behind any step, see [the subagent overview](../overview.md#what-a-subagent-is).

## 1. Scaffold the agent

From the directory where you want the new folder to appear, run:

```bash
hugr new my-agent
```

This creates `./my-agent` from the default `weather` template (the checked-in `examples/hugr-weather` crate, embedded at compile time with the name substituted; pass `--template blank` for a tool-free starting point instead). The command refuses to overwrite an existing folder and tells you the next step on stderr.

## 2. Anatomy of the generated files

The folder is both an agent definition and a small Rust crate:

```
my-agent/
  hugr.toml    # the manifest: identity, models, tool grants, limits
  SYSTEM.md    # the system prompt
  src/lib.rs   # the typed Rust response contract
  Cargo.toml   # a normal crate manifest (serde + schemars)
  README.md    # next steps
```

### hugr.toml

The manifest defines the agent's privileges. The agent can use only what is granted here (see [the manifest reference](../agents.md#the-manifest)). The weather template's manifest has four sections:

```toml
[agent]
name = "my-agent"
version = "0.1.0"
description = "Answers current-weather questions via the Open-Meteo API."

[models]
base_url = "https://router.huggingface.co/v1"
api_key_env = "HUGR_API_KEY"
default = "medium"

[models.medium]
model = "google/gemma-4-31B-it:cerebras"
input_usd_per_m_tokens = 1.0
output_usd_per_m_tokens = 1.5

# GET-only HTTP, jailed to an allowlist of hosts (the sandbox boundary).
[tools.web_fetch]
allow_hosts = ["api.open-meteo.com", "geocoding-api.open-meteo.com"]
```

- `[agent]` is the identity: the name also names the agent's state home (`~/.hugr/<name>/` by default, where traces and the scratchpad live).
- `[models]` points at any OpenAI-compatible endpoint; `api_key_env` names the environment variable that holds the key (the value itself never appears in any output). Tiers like `[models.medium]` carry the model id and its per-million-token prices, which is how every answer gets a cost.
- `[tools.web_fetch]` is a *grant*: it registers the library's GET-only HTTP tool, jailed to those two Open-Meteo hosts. Delete the section and the agent has no network at all.
- There is no `[limits]` block and none is required: an agent has no caps by default. Add `[limits]` (`max_model_calls`, `max_cost_micro_usd`, `timeout_s`) when you want to bound an ask; every unset key is unbounded.

### SYSTEM.md

The system prompt, in plain Markdown. Template variables like `{{agent_name}}` are substituted at assembly time. The weather prompt tells the model exactly which two Open-Meteo endpoints to hit with `web_fetch` and to answer in one short sentence; edit this file first when you want different behavior.

### src/lib.rs — the response contract

The crate exports a typed response contract:

```rust
pub const RESPONSE_RUST_TYPE: &str = "my_agent::Response";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub response: String,
}
```

`hugr run` and `hugr build` read `RESPONSE_RUST_TYPE`, derive a JSON Schema from the type with `schemars`, ask the provider for that structured output, and cast the final JSON with `serde` before it lands in `Answer.response`. Right now it is a single string; guide 2 shows how to grow it.

## 3. Ask a question

Set the provider key named by `api_key_env`, then run one ask from inside (or pointing at) the folder:

```bash
export HUGR_API_KEY=...   # e.g. an hf_... token for router.huggingface.co
hugr run my-agent "what's the weather in Paris?"
```

You get one pretty-printed JSON `Answer` on stdout, while diagnostics go to stderr. Add `--json` for compact single-line output.

The `Answer` carries `status`, your typed `response` object, a `trace_id`, and mandatory `metadata`: duration, cost in micro-USD, tokens, and model/tool call counts.

**The ask path always exits 0.** A missing key, a bad manifest, or a blown limit returns a `status: "error"` answer instead of crashing. See [the Ask and Answer contract](../agents.md#the-ask-and-answer-contract).

Because this agent has a typed Rust contract, the first `hugr run` compiles a small cached shim crate that links your `src/lib.rs`; later runs reuse it, so only the first ask pays the compile.

## 4. Resume and fork with trace ids

Every ask is recorded as an immutable trace. List them as a lineage tree:

```bash
hugr traces my-agent
```

To continue a conversation, pass the parent's trace id back in:

```bash
hugr run my-agent --trace <TRACE_ID> "and in London?"
```

A resumed ask never mutates the old trace. It writes a **new** trace with `depends_on` pointing at the parent. Resuming the same id twice forks the conversation into two branches, and `hugr traces` shows the tree.

`hugr verify my-agent <TRACE_ID>` confirms that a trace replays bit-for-bit. `hugr replay my-agent <TRACE_ID> --step` walks through it event by event. See [determinism and replay](../runtime.md#determinism-replay-and-traces) for the underlying design.

`hugr stats my-agent` aggregates cost, tokens, and tool usage across stored traces.

## 5. Inspect the agent card

Every agent surface answers `--describe` with its agent card, including its name, scoped tools, priced model tiers, and limits. `--config` returns the parsed manifest as JSON, including the API key environment variable name and whether it resolves, but never the secret:

```bash
hugr run my-agent -- --describe
```

(The `--` keeps `hugr` from eating the flag; the flags after it go to the agent's own generated surface.)

## 6. Build one standalone binary

```bash
hugr build my-agent --release
```

This generates a shim crate under `my-agent/dist/` (override with `--out <dir>`). The shim embeds the agent bundle, including the manifest, prompt, and response contract, then compiles it with cargo.

The result is one self-contained binary at `my-agent/dist/my_agent-cli/target/release/my_agent` that needs no repository checkout. On startup, it installs its bundle into a content-addressed `.definitions/<name>/<hash>/` cache beside `~/.hugr/<name>/`; traces and other mutable state remain in the agent home, so `--trace` resume works anywhere you copy the binary.

`--surface python` also generates a pip-installable Python module.

The built binary speaks the same universal surface as `hugr run`:

```
my_agent "question" [--trace <ID>] [--json|--pretty] [--blob <PATH>...] [--skill <PATH>...] [--stream]
my_agent --describe | --config | --traces | --stats [--trace <ID>]
my_agent --feedback <TRACE_ID> [--feedback-payload <JSON>]
my_agent --mcp-serve
my_agent --cron-serve [--allow-uncapped]
```

- `--trace <ID>` resumes/forks exactly as with `hugr run`; `--json` switches from the default pretty printing to compact.
- `--blob <PATH>` (repeatable) hands local files in as inbound blobs.
- `--skill <PATH>` (repeatable) adds a standard `SKILL.md` folder, or a folder containing skills, for this ask. The model receives a compact catalog and loads matching instructions on demand.
- `--stream` emits one JSON event per line as the run progresses, then the final `Answer` line.
- `--describe`, `--config`, `--traces`, and `--stats` are the audit views. They return JSON and exit non-zero on failure, unlike the ask path.
- `--feedback <TRACE_ID>` appends a JSON feedback payload to a stored trace (from `--feedback-payload` or stdin).
- `--mcp-serve` turns the binary into a stdio MCP server exposing an ask tool; register the command in any MCP client and your agent becomes a tool.
- `--cron-serve` runs any `[cron.<name>]` jobs from the manifest until stopped.

The workflow is: scaffold the agent, edit two text files, run it, inspect it, and ship one binary.

## Next

[Guide 2: Typed responses and answer hooks](02-typed-responses-and-hooks.md): grow the response contract beyond a single string, give the model a different schema than your users see, and post-process answers deterministically with `answer_hooks()`.
