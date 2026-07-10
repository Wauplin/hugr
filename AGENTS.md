# AGENTS.md

Guidance for working in the Hugr repository.

## What this is

Hugr is a **toolkit for building tiny, self-contained, domain-specific subagents** — "build your subagent, ship it anywhere" — on a runtime-free, sans-IO Rust core. A subagent is a small Rust crate plus a system prompt and a set of tools with declared privileges; Hugr turns that agent crate folder into one standalone binary exposing the ask/answer contract (and an MCP server via `--mcp-serve`), with traces, forking, a scratchpad, blob exchange, and cost accounting built in.

There is exactly one doc, keep it in sync with reality: `ARCHITECTURE.md` (design + architecture + threat model — **the spec**; read it before non-trivial changes).

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

- **Agent strategy** (which model selector, how to project context, whether a capability is gated) lives in the pluggable `TurnPolicy` — never hardcoded in the reducer. `StaticPolicy` is the trivial pass-through projection; custom policy decoders registered in `PolicyRegistry` must be pure so replay/resume stay deterministic.
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
                        #   + fs content-addressed BlobStore + replay/verify/inspect
crates/hugr-agent/      # the subagent runtime: Ask/Answer, trace/blob/scratch backends
                        #   (trace_id/depends_on, fork), scratchpad, blobs,
                        #   limits, cost accounting, agent-as-tool (subprocess)
crates/hugr-toolkit/    # agent crate manifests (hugr.toml + SYSTEM.md), the tool library
                        #   (fs_read, http_fetch, sqlite_query), and the `hugr`
                        #   CLI: new / run / build / traces / replay / verify
crates/hugr-wasm/       # generic WASM bindings around hugr-core for browser/JS
                        #   hosts (submit/poll over JSON + browser tool schemas)
bindings/typescript/    # generic JS host layer: agent driver, fetch model adapter,
                        #   IndexedDB stores (grows into the typed TS runtime API)
examples/hugr-docs/     # the reference subagent crate (docs Q&A): hugr.toml +
                        #   SYSTEM.md plus typed response contract using hugr-toolkit
examples/hugr-weather/  # the self-contained beginner example; also the source of
                        #   the `hugr new --template weather` scaffold (embedded
                        #   at compile time, name substituted)
examples/chrome-extension/ # a concrete browser host: chrome.* capabilities,
                        #   side-panel UI, MV3 manifest (vendors the generic JS)
```

`hugr-replay` is a host-side **persistence** crate — it may use `std::fs`, but it depends on `hugr-core` as *pure data only*. The layers stack strictly: `hugr-agent` on `hugr-host` + `hugr-replay`; `hugr-toolkit` on `hugr-agent`. Nothing reaches into `hugr-core` internals — they are hosts like any other. **Never add environmental dependencies to `hugr-core`** to make a host easier; put them in the host crate.

Subagent-layer conventions (ARCHITECTURE Part I): the `Ask`/`Answer` contract is the one-way door — `AnswerMeta` (cost/duration/tokens) is mandatory, errors are answers (`status: "error"`, exit 0), the user-facing payload rides `Answer.response` as a JSON object, and typed Rust response contracts derive provider JSON Schema with `schemars` and cast final JSON with `serde`. Traces are immutable; a resumed ask writes a **new** trace with `depends_on` set. Default agent state is `~/.hugr/<agent>/` (`traces/`, `scratch/`, `memory/`, `feedback/`) plus the shared blob store `~/.hugr/blobs` (override with `HUGR_AGENT_HOME`, `HUGR_HOME`, or `HUGR_BLOB_STORE`), and custom `StorageOverrides` are trusted host code that must stay outside `hugr-core`. Tools are granted in the manifest and jailed to their declared scope — sandbox-by-registration, so never register a capability the manifest doesn't grant. A built Hugr agent is itself grantable as a tool (`[tools.agent.<name>]`, subprocess over the CLI JSON contract) — delegation never widens privileges and the child's cost folds into the caller's `AnswerMeta`. The tool library is exec-free (the planned sandboxed `code_exec` is the only future exception) — never add a `shell` to the library. MCP (`[tools.mcp.<name>]`) is the **only** external-process tool escape hatch.

When extending the host: capabilities are uniform (no privileged built-ins); a model call is "an effect the host provides" registered like a capability; transport errors (retries, 429s) are the adapter's job, semantic errors route back to the model as tool results.

## Commands

```bash
cargo test                  # all tests
cargo test -p hugr-core    # core only
cargo clippy --all-targets  # lint (keep it clean)
cargo fmt --all             # format before committing
cargo tree -p hugr-core    # audit: must stay free of tokio/reqwest/fs
hugr stats <agent-dir>      # aggregate trace costs/tokens/tools/feedback
hugr cron <agent-dir>       # run configured [cron.<name>] recurring asks
```

## Conventions

- **Keep the docs in sync — it's part of "done".** After completing a task update `ARCHITECTURE.md` if behavior changed. A task isn't finished until the docs match reality.
- **Prefer deletion over abstraction.** One way to do each thing; if two mechanisms do the same job, keep the one the live stack uses and delete the other.
- **Markdown is single-line.** One physical line per paragraph or bullet — never hard-wrap prose; rely on soft-wrap. (Fenced code blocks and table rows are exempt.)
- **Comments state what the code cannot.** No references to other docs (`ARCHITECTURE §X` etc.), no "how it works" narration, no comments restating the signature or the next line, no section banners. A comment is justified only for a non-obvious constraint, failure mode, or safety/jail invariant; public items keep one concise doc line stating the contract.
- Keep event handlers O(1)-ish (append to a buffer); no heavy work in the reducer.
- When you add a `Command`/`Event`/`Record` variant: update the reducer's match and add a scripted test that pins the resulting command sequence.
- Determinism is testable: any new control-flow path should have a replay test asserting identical commands on a re-fed event stream; `verify()` is the release gate.
- **Ideas flow: `new_ideas.md` → `plan.md` → implementation.** While implementing, drop short one-line ideas (not designs or TODO lists) into `new_ideas.md` so the owner can review them; when an idea is promoted into the structured roadmap `plan.md`, remove it from `new_ideas.md`.
