# hugr-sqlite-2 — a subagent with a custom Rust tool

Companion example to [`hugr-docs-2`](../hugr-docs-2/): the docs agent needed nothing beyond the predefined tool library, so it was pure data. This one answers the next question — **what if my agent needs a tool the library doesn't have?** Here: read-only access to a SQLite file. (In the real roadmap `sqlite_query` ends up *in* the library (T1.2) precisely because it's this useful — pretend it isn't there; this folder shows the escape hatch you'd use for a genuinely custom domain.)

Illustrative only — nothing here compiles or runs today.

```
hugr-sqlite-2/
  hugr.toml            # the manifest — now with a [tools.rust.sqlite] grant
  SYSTEM.md            # the prompt — tells the agent to cache the schema in its scratchpad
  tools/               # a small cargo crate, compiled INTO the artifact
    Cargo.toml         # depends only on hugr-host (the Capability trait)
    src/lib.rs         # two Capability impls + one exported constructor
```

## How a custom Rust tool plugs in

Three facts carry the whole design:

1. **A custom tool is an ordinary `Capability`** — the exact trait every library tool, `shell`, and MCP-bridged tool implements (ARCHITECTURE §7.1). No privileged tools means no lesser path for yours: same permission handling, same trace records, same replay story, same narrow waist (the brain only ever sees your `ToolSchema` and opaque JSON args/results, so a custom tool *cannot* require a core change).
2. **The manifest owns the scope; the Rust owns the mechanism.** `[tools.rust.sqlite]` names the crate and declares the audit-relevant facts — which file (`db = "${db_path}"`), `read_only = true`, `max_rows = 200`. That config table is handed to the crate's exported constructor at startup; the constructor enforces its own claims (this one refuses to open writable at all). Reviewing the agent is still: read `hugr.toml`, then skim one small tool crate.
3. **Native code means the build path.** `hugr run` interprets pure-data definitions; it cannot dynamically load Rust. A definition with `[tools.rust.*]` grants goes through `hugr dev` (build-and-run for the edit loop) and `hugr build` (artifacts) — the toolkit generates the shim crate with your `tools/` as a dependency and wires the exported constructor into the registry. If you can't or won't compile, the other two escape hatches stay available in the same manifest: `[tools.mcp.*]` (any MCP server, config-only) and `[tools.plugin.*]` (the subprocess plugin ABI, any language). Rule of thumb: MCP/plugin for reuse and language freedom; Rust for tight jails, zero per-call process overhead, and single-binary shipping.

The integration surface is deliberately one function:

```rust
#[hugr_toolkit::export_tool]
pub fn sqlite(config: Value) -> Result<Vec<Box<dyn Capability>>, String> { ... }
```

See [`tools/src/lib.rs`](tools/src/lib.rs) for the annotated sketch: a `sqlite_schema` tool (tables/columns/indexes as one JSON document) and a `sqlite_query` tool (single-SELECT-only on a read-only connection — defense in depth: the connection can't write *and* the statement gate rejects ATTACH/PRAGMA/multi-statement smuggling; truncation at `max_rows` is reported to the model, never silent).

## The scratchpad as schema memory

The prompt (SYSTEM.md) instructs the agent to check its scratchpad for `schema.md` before exploring, and to write its findings there after a first discovery. Combined with trace lineage this is what makes conversations cheap:

```bash
hugr dev . --db ./expenses.db "How many expenses were filed in June?"
# turn 1: sqlite_schema → writes scratchpad/schema.md → queries → answer     (trace tr_A)

hugr dev . --db ./expenses.db "And what's the June total per employee?" --trace tr_A
# turn 2: reads schema.md from the scratchpad — no rediscovery → one query   (tr_B, depends_on tr_A)

hugr dev . --db ./expenses.db "Same but for travel expenses only" --trace tr_A
# a FORK of tr_A: gets a copy-on-fork view of tr_A's scratchpad, so it also
# skips rediscovery — but its own notes never leak into tr_B's branch        (tr_C)
```

Scratchpad state follows the trace DAG (ARCHITECTURE §19.3): resumed asks see ancestor notes, sibling forks are isolated from each other.

## What the shipped artifact looks like

`hugr build . --surface cli,python,mcp` — identical surfaces to any other agent, because the custom tool changed nothing about the contract:

```json
{
  "status": "success",
  "message": "142 expenses were filed in June, totalling $18,304.",
  "trace_id": "tr_9m2kq",
  "metadata": { "duration_ms": 3100, "cost_micro_usd": 900, "tokens_in": 700, "tokens_out": 150, "model_calls": 2, "tool_calls": 2, "per_tier": [ ... ] },
  "extra": { "queries": ["SELECT COUNT(*), SUM(amount_usd) FROM expenses WHERE date BETWEEN '2026-06-01' AND '2026-06-30'"] }
}
```

`--describe` lists `sqlite_schema` and `sqlite_query` alongside their privilege class (read-only) and scope, straight from the manifest + tool schemas. An orchestrator cannot tell a custom tool from a library one — which is the point.
