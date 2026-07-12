# Composition and cost

## What you'll build

You'll grant one Huggr agent to another as a tool, hand a blob from parent to child without copying a byte, file feedback on the child's trace, and read the bill with `huggr stats`. This shows how agents compose and how their cost folds upward. Guide 08 builds on this workflow.

This assumes you've done [01](01-first-agent-cli.md) (you have a built binary) and [02](02-typed-responses-and-hooks.md) (you know the runtime-arg pattern). The composition model is specified in [the agents-as-tools documentation](../agents.md#agents-as-tools); this guide is the worked example.

## Build two agents

Start with the weather agent and a tiny "summarizer" agent. Build both into standalone binaries; an agent grant always points at a **built artifact**:

```bash
huggr build examples/huglet-weather --release
huggr build ./my-summarizer --release     # any agent crate you have
```

The built binary lands under each crate's `dist/target/`. The grant you're about to write points at one of these binaries, so `huggr build` must run first for each agent you want to compose (see `--help` for `--out` to choose the destination).

## Grant one agent to another

An agent grant is a manifest line under `[tools.agent.<name>]`. Add this to the orchestrator's `huggr.toml` (the parent), pointing at the child's built binary directory:

```toml
[tools.agent.weather]
artifact = "../examples/huglet-weather/dist"
```

`<name>` is `weather`, so the parent gets a capability named `agent_weather`. The parent's model sees it as an ordinary tool whose args are an [`Ask`](../agents.md#the-ask-and-answer-contract): a `question`, an optional `trace_id` to resume the child, and optional `blobs`:

```json
{
  "question": "what's the weather in Paris?",
  "trace_id": "optional-child-trace-to-resume",
  "blobs": []
}
```

…whose result is the child's full `Answer`, including the child's own `trace_id`. That round-trip is why the parent can keep the child's conversation alive across its own turns: pass the child's `trace_id` back on the next `agent_weather` call. The schema and resolver live in `crates/huggr-agent/src/agent_tool.rs`; the manifest keys are parsed in `crates/huggr-toolkit/src/manifest.rs`.

### What the grant does *not* widen

- **Privileges compose downward only.** The child runs under its **own** manifest, tool jail, tiers, and limits. Granting it never exposes the parent's capabilities; an agent with `[tools.fs_read]` cannot reach into the child's scratch.
- **Process access is explicit.** Child agents are built artifacts spawned over the CLI JSON contract. MCP and full shell grants are separate trusted subprocess boundaries; restricted shell mode invokes only exact allowlisted programs without shell parsing.

## Hand the child a blob, zero bytes crossing

Large payloads (datasets, images) flow between agents through the shared, content-addressed blob store at `~/.huggr/blobs`; every agent points at the same store by default, so a parent can hand a child a blob by reference alone.

From the parent binary, attach a local file as an inbound blob with `--blob`:

```bash
my-orchestrator "summarize this dataset" --blob ./data.csv
```

The file is hashed and copied into an atomically installed shared-store object; the caller's mutable inode is never used as the content-addressed object. The implementation is in `crates/huggr-agent/src/blobs.rs`. The parent's model receives a `sha256:<hash>` handle.

When the parent calls `agent_weather` with `blobs: [{"type":"sha256","hash":"…"}]`, the resolver passes the reference to the child as `--blob sha256:<hash>`. It also sets `HUGGR_BLOB_STORE` to the same shared root.

The child resolves the reference from that store, so **zero bytes cross the process boundary**. The child's answer blobs are also `sha256` references and return unchanged in the parent's tool result.

Hashes are capabilities, not secrets: anyone handed a hash can read that object from the shared store. If that's not what you want, keep the blob in scratch and read it directly instead.

## File feedback on the child's trace

Feedback is the one asynchronous back-channel for recording, beside an immutable trace, whether an answer helped. It is never read during a live ask (see [the security documentation](../security.md)); it's for offline analysis (guide 08).

A parent model can file feedback on the child right after a delegation through a sibling capability `agent_<name>_feedback`, registered automatically beside each `<name>` grant. Its args are `{ trace_id, payload }`:

```json
{ "trace_id": "the-child-trace-id", "payload": { "score": 1, "note": "wrong city" } }
```

The `payload` is fully opaque; Huggr never interprets it. From the built binary directly:

```bash
my-orchestrator --feedback <trace_id> --feedback-payload '{"score": 1}'
# or pipe it:
echo '{"score": 1}' | my-orchestrator --feedback <trace_id>
```

Feedback appends to `<agent-home>/feedback/<trace_id>.jsonl`, one JSON line per event; the trace itself stays immutable.

## Read the bill with `huggr stats`

Every number in `huggr stats` comes from the traces. `OpEnded` carries per-op cost, tokens, and timing, and the command folds those records. From the agent crate:

```bash
huggr stats ./my-orchestrator                # pretty table
huggr stats ./my-orchestrator --json         # one stable JSON document
huggr stats ./my-orchestrator --trace <id>   # one trace only
huggr stats ./my-orchestrator --since <id>   # from a trace onward
```

The built binary has the same fold behind `--stats`. The thing to know is the **never-nested** attribution rule (idea 5's constraint, in `crates/huggr-agent/src/analytics.rs`):

- A child's cost is attributed to the direct `agent_<name>` tool call that produced it, read from the recorded child `Answer.metadata`, and reported as `cost_delegated` per child name.
- The orchestrator's **own** line reports `cost_own`; grandchildren are already folded into the child's number and are **not** re-walked.
- So `cost_micro_usd == cost_own + cost_delegated`: one level of delegation, accounted where it was spent. This keeps the orchestrator's bill complete without recursive walks through arbitrary subtrees.

Accounting is kept in micro-USD (`cost_micro_usd` in `AnswerMeta` and the `--json` output) for precision; the pretty table displays USD, rendering a nonzero amount under a penny as `<$0.01`.

The same fold provides per-tier, per-tool, and duration percentiles; see `crates/huggr-agent/src/analytics.rs` and `crates/huggr-toolkit/src/stats.rs` for the exact shape.

## Next

You've composed agents and read their cost. The resulting traces are replayable bit-for-bit, providing the debugging surface and input for offline improvement analysis. Continue with [Traces, replay, and debugging](08-traces-replay-debugging.md).
