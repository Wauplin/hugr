# Your first agent from the CLI

In this tutorial you'll scaffold a weather-answering subagent with `hugr new`, look at every file it generates, ask it a question with `hugr run`, resume and fork past conversations by trace id, inspect the agent card with `--describe`, and finally compile it into one self-contained binary with `hugr build`. No prior Hugr knowledge is assumed; for the design rationale behind any step, see [ARCHITECTURE.md](../../ARCHITECTURE.md#3-what-a-subagent-is).

## 1. Scaffold the agent

From the directory where you want the new folder to appear, run:

```bash
hugr new my-agent
```

This creates `./my-agent` from the default `weather` template (the checked-in `examples/hugr-weather` crate, embedded at compile time with the name substituted — pass `--template blank` for a tool-free starting point instead). The command refuses to overwrite an existing folder and tells you the next step on stderr.

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

The manifest is the whole privilege story — the agent can use exactly what is granted here and nothing else (see [the manifest](../../ARCHITECTURE.md#6-the-manifest)). The weather template's manifest has four sections:

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

[limits]
max_model_calls = 20
max_cost_micro_usd = 50000
timeout_s = 120
```

- `[agent]` is the identity: the name also names the agent's state home (`~/.hugr/<name>/` by default, where traces and the scratchpad live).
- `[models]` points at any OpenAI-compatible endpoint; `api_key_env` names the environment variable that holds the key (the value itself never appears in any output). Tiers like `[models.medium]` carry the model id and its per-million-token prices, which is how every answer gets a cost.
- `[tools.web_fetch]` is a *grant*: it registers the library's GET-only HTTP tool, jailed to those two Open-Meteo hosts. Delete the section and the agent has no network at all.
- `[limits]` caps model calls, spend (in micro-USD), and wall time per ask.

### SYSTEM.md

The system prompt, in plain Markdown. Template variables like `{{agent_name}}` are substituted at assembly time. The weather prompt tells the model exactly which two Open-Meteo endpoints to hit with `web_fetch` and to answer in one short sentence — edit this file first when you want different behavior.

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

`hugr run` and `hugr build` read `RESPONSE_RUST_TYPE`, derive a JSON Schema from the type with `schemars`, ask the provider for that structured output, and cast the final JSON with `serde` before it lands in `Answer.response`. Right now it is a single string; tutorial 2 shows how to grow it.

## 3. Ask a question

Set the provider key named by `api_key_env`, then run one ask from inside (or pointing at) the folder:

```bash
export HUGR_API_KEY=...   # e.g. an hf_... token for router.huggingface.co
hugr run my-agent "what's the weather in Paris?"
```

You get one pretty-printed JSON `Answer` on stdout (diagnostics go to stderr). Add `--json` for compact single-line output. The `Answer` carries `status`, your typed `response` object, a `trace_id`, and mandatory `metadata` (duration, cost in micro-USD, tokens, model/tool call counts). One important contract: **the ask path always exits 0** — a missing key, a bad manifest, or a blown limit comes back as a `status: "error"` answer, not a crash (see [Ask/Answer](../../ARCHITECTURE.md#part-i--what-hugr-is)).

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

A resumed ask never mutates the old trace — it writes a **new** trace with `depends_on` pointing at the parent. That means resuming the *same* id twice forks the conversation into two branches, and `hugr traces` shows the tree. Two more inspection commands close the loop: `hugr verify my-agent <TRACE_ID>` proves a trace replays bit-for-bit, and `hugr replay my-agent <TRACE_ID> --step` walks it event by event (why this works: [determinism and replay](../../ARCHITECTURE.md#19-determinism-replay-and-the-trace-format)). `hugr stats my-agent` aggregates cost/tokens/tool usage across stored traces.

## 5. Inspect the agent card

Every agent surface answers `--describe` with its agent card — name, tools with their scopes, model tiers with pricing, and limits — and `--config` with the parsed manifest as JSON (the API key env *name* and whether it resolves, never the secret):

```bash
hugr run my-agent -- --describe
```

(The `--` keeps `hugr` from eating the flag; the flags after it go to the agent's own generated surface.)

## 6. Build one standalone binary

```bash
hugr build my-agent --release
```

This generates a shim crate under `my-agent/dist/` (override with `--out <dir>`) that embeds the whole agent bundle — manifest, prompt, response contract — and compiles it with cargo. The result is one self-contained binary at `my-agent/dist/my_agent-cli/target/release/my_agent` that needs no repo checkout: on startup it unpacks its bundle into `~/.hugr/<name>/`, so traces persist across runs and `--trace` resume works anywhere you copy it. (`--surface python` additionally generates a pip-installable Python module.)

The built binary speaks the same universal surface as `hugr run`:

```
my_agent "question" [--trace <ID>] [--json|--pretty] [--blob <PATH>...] [--stream]
my_agent --describe | --config | --traces | --stats [--trace <ID>]
my_agent --feedback <TRACE_ID> [--feedback-payload <JSON>]
my_agent --mcp-serve
my_agent --cron-serve [--allow-uncapped]
```

- `--trace <ID>` resumes/forks exactly as with `hugr run`; `--json` switches from the default pretty printing to compact.
- `--blob <PATH>` (repeatable) hands local files in as inbound blobs.
- `--stream` emits one JSON event per line as the run progresses, then the final `Answer` line.
- `--describe`, `--config`, `--traces`, and `--stats` are the audit views (JSON out, non-zero exit on failure — unlike the ask path).
- `--feedback <TRACE_ID>` appends a JSON feedback payload to a stored trace (from `--feedback-payload` or stdin).
- `--mcp-serve` turns the binary into a stdio MCP server exposing an ask tool — register the command in any MCP client and your agent becomes a tool.
- `--cron-serve` runs any `[cron.<name>]` jobs from the manifest until stopped.

That's the whole loop: scaffold, edit two text files, run, inspect, ship one binary.

## Next

[Tutorial 2 — Typed responses and answer hooks](02-typed-responses-and-hooks.md): grow the response contract beyond a single string, give the model a different schema than your users see, and post-process answers deterministically with `answer_hooks()`.
