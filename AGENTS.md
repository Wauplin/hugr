# CLAUDE.md

Guidance for working in the Hugr repository.

## What this is

Hugr is a **toolkit for building tiny, self-contained, domain-specific subagents** — "build your subagent, ship it anywhere" — on a runtime-free, sans-IO Rust core. A subagent is a system prompt + a set of tools with declared privileges; Hugr turns that definition folder into one standalone binary exposing the ask/answer contract (and an MCP server via `--mcp-serve`), with traces, forking, a scratchpad, blob exchange, and cost accounting built in.

There are exactly two docs, keep both in sync with reality: `docs/ARCHITECTURE.md` (design + architecture + threat model — **the spec**; read it before non-trivial changes) and `docs/ROADMAP.md` (progress log + work plan). **The repo is mid-trim:** `docs/ROADMAP.md` §2 is the authoritative phase-by-phase plan cutting the code down to the ARCHITECTURE.md state — when code and ARCHITECTURE.md disagree, the roadmap phases are the bridge.

## The one rule that matters most

**`hugr-core` is sans-IO and pure.** It is a reducer: `submit(event)` folds an event into state and queues commands; `poll()` drains them. It must never do IO.

Hard invariants — do not break these:

- **No environmental dependencies in `hugr-core`.** No `tokio`, no `reqwest`, no `std::fs`, no sockets, no clock, no RNG, no threads. Only pure-data crates (`serde`, `serde_json`). Verify with `cargo tree -p hugr-core`.
- **The brain is single-threaded.** All concurrency lives in the host. The moment the brain is multithreaded, we lose sans-IO, replay, and easy bindings.
- **All nondeterminism is injected** as events (`Tick` for time; model output, tool results, user input as events). The brain never reads a clock or RNG. This is what makes replay bit-for-bit deterministic — protect it.
- **The log is the source of truth.** `BrainState` is a *fold* over the log and must stay rebuildable from it. Don't add un-derived state.
- **Deltas are transport, never durable.** `ModelDelta`/`CapabilityChunk` feed live op buffers but are *never* written to the log. One consolidated `Record` is appended per logical message/tool-result.
- **Streaming is the only model mode.** Adapters stream deltas live via the sink and return the consolidated output; there is no non-streaming path.

## The narrow-waist rule (ARCHITECTURE §14)

> **Type only what the brain branches on. Everything else is an opaque payload.**

- Typed & stable: op lifecycle (start/delta/done/error/cancel), model *output structure* (text vs tool calls), turn control, permission outcomes.
- Opaque (`Value`): capability args/results, provider knobs, prompts, answers. The brain stores and forwards these; it never interprets.
- Corollary: **an enum nobody branches on should be a string.** Status labels, privilege classes, tier/selector names are open string sets, not variant lists. Error enums matched for handling are fine.
- Adding a new tool, provider knob, or agent grant must touch **zero** core types. If a change forces a core type edit for a new *capability*, reconsider it.
- Breaking changes are acceptable (hobby prototype, no external users): no `#[non_exhaustive]`/constructor ceremony for stability's sake, no serde back-compat fields, no deprecation shims.

## Where logic goes

- **Agent strategy** (which model selector, how to project context, whether a capability is gated) lives in the pluggable `TurnPolicy` — never hardcoded in the reducer. `StaticPolicy` is the trivial pass-through projection.
- **The reducer** (`brain.rs`) only: maintains the log + op table; drives the turn loop; asks the policy; routes opaque payloads; decides done/checkpoint. If you're adding "smarts", it probably belongs in a policy, not the reducer.
- **Everything hard** (IO, HTTP, model resolution, storage) is the *host's* job — not in `hugr-core`.

## Project layout

```
crates/hugr-core/       # the sans-IO brain (NO tokio/reqwest/fs)
  src/primitives.rs  # OpId, Seq, Timestamp, Value
  src/model.rs       # ModelRequest/Delta/Output, ToolCall, Usage, ModelSelector
  src/command.rs     # Command (brain → host) + OutputEvent
  src/event.rs       # Event (host → brain) + Decision
  src/record.rs      # LogEntry, Record, OpOutcome, OpMeta (the durable log)
  src/state.rs       # BrainState + in-flight op table (derived; foldable)
  src/policy.rs      # TurnPolicy trait + StaticPolicy
  src/brain.rs       # Brain: poll() + submit() + the reducer

crates/hugr-host/       # native tokio host: Engine/EngineBuilder driver loop,
                        #   Capability trait + registry, ModelAdapter + registry,
                        #   Frontend trait, MCP stdio client, JSON-line framing
crates/hugr-providers/  # OpenAI-compatible streaming adapter (retries inside)
crates/hugr-replay/     # trace format (Trace { meta, events, log, commands, blobs })
                        #   + content-addressed BlobStore + replay/verify/inspect
crates/hugr-agent/      # the subagent runtime: Ask/Answer, TraceStore
                        #   (trace_id/depends_on, fork), scratchpad, blobs,
                        #   limits, cost accounting, agent-as-tool (subprocess)
crates/hugr-toolkit/    # definitions (hugr.toml + SYSTEM.md), the tool library
                        #   (fs_read, http_fetch, sqlite_query), and the `hugr`
                        #   CLI: new / run / build / traces / replay / verify
crates/hugr-docs/       # the reference subagent (docs Q&A): definition folder +
                        #   typed response contract using hugr-toolkit
```

`hugr-replay` is a host-side **persistence** crate — it may use `std::fs`, but it depends on `hugr-core` as *pure data only*. The layers stack strictly: `hugr-agent` on `hugr-host` + `hugr-replay`; `hugr-toolkit` on `hugr-agent`. Nothing reaches into `hugr-core` internals — they are hosts like any other. **Never add environmental dependencies to `hugr-core`** to make a host easier; put them in the host crate.

Subagent-layer conventions (ARCHITECTURE Part I): the `Ask`/`Answer` contract is the one-way door — `AnswerMeta` (cost/duration/tokens) is mandatory, errors are answers (`status: "error"`, exit 0), the user-facing payload rides `Answer.response` as a JSON object, and typed Rust response contracts derive provider JSON Schema with `schemars` and cast final JSON with `serde`. Traces are immutable; a resumed ask writes a **new** trace with `depends_on` set. Tools are granted in the manifest and jailed to their declared scope — sandbox-by-registration, so never register a capability the manifest doesn't grant. A built Hugr agent is itself grantable as a tool (`[tools.agent.<name>]`, subprocess over the CLI JSON contract) — delegation never widens privileges and the child's cost folds into the caller's `AnswerMeta`. The tool library is exec-free (the planned sandboxed `code_exec` is the only future exception) — never add a `shell` to the library. MCP (`[tools.mcp.<name>]`) is the **only** external-process tool escape hatch.

When extending the host: capabilities are uniform (no privileged built-ins); a model call is "an effect the host provides" registered like a capability; transport errors (retries, 429s) are the adapter's job, semantic errors route back to the model as tool results.

## Commands

```bash
cargo test                  # all tests
cargo test -p hugr-core    # core only
cargo clippy --all-targets  # lint (keep it clean)
cargo fmt --all             # format before committing
cargo tree -p hugr-core    # audit: must stay free of tokio/reqwest/fs
```

## Conventions

- **Keep the docs in sync — it's part of "done".** After completing a task, update `docs/ROADMAP.md` (progress/trim log) and, if behavior changed, `docs/ARCHITECTURE.md`. A task isn't finished until the docs match reality.
- **Prefer deletion over abstraction.** One way to do each thing; if two mechanisms do the same job, keep the one the live stack uses and delete the other.
- **Markdown is single-line.** One physical line per paragraph or bullet — never hard-wrap prose; rely on soft-wrap. (Fenced code blocks and table rows are exempt.)
- Reference design sections in comments as `ARCHITECTURE §X` so code stays traceable to the rationale.
- Keep event handlers O(1)-ish (append to a buffer); no heavy work in the reducer (ARCHITECTURE §17).
- When you add a `Command`/`Event`/`Record` variant: update the reducer's match and add a scripted test that pins the resulting command sequence.
- Determinism is testable: any new control-flow path should have a replay test asserting identical commands on a re-fed event stream; `verify()` is the release gate.
- Drop short raw ideas into `new_ideas.md`; promote them to `docs/ROADMAP.md` §4 only when they become real candidates.
