# AGENTS.md

Guidance for working in the Hugr repository.

## What this is

Hugr is a **toolkit for building small, self-contained, domain-specific subagents** on a runtime-free, sans-IO Rust core.

A subagent is a small Rust crate plus a system prompt and a set of tools with declared privileges. Hugr turns that folder into one standalone binary that exposes the ask/answer contract and serves MCP through `--mcp-serve`. Traces, forking, a scratchpad, blob exchange, and cost accounting are built in.

The documentation under `docs/` contains the design, architecture, and threat model. Read it before non-trivial changes and keep it in sync with reality.

`docs/guides/` contains per-surface teaching material that links to the reference documentation instead of restating it; `docs/tutorials/` contains self-contained end-to-end walkthroughs whose command outputs come from real runs. A behavior change is not complete until the reference documentation matches reality and every tutorial or guide that shows the changed behavior still works.

## The one rule that matters most

**`hugr-core` is sans-IO and pure.** It is a reducer: `submit(event)` folds an event into state and queues commands; `poll()` drains them. It must never do IO.

Hard invariants:

- **No environmental dependencies in `hugr-core`.** No `tokio`, no `reqwest`, no `std::fs`, no sockets, no clock, no RNG, no threads. Only pure-data crates (`serde`, `serde_json`). Verify with `cargo tree -p hugr-core`.
- **The brain is single-threaded.** All concurrency lives in the host. The moment the brain is multithreaded, we lose sans-IO, replay, and easy bindings.
- **All nondeterminism is injected** as events (`Tick` for time; model output, tool results, user input as events). The brain never reads a clock or RNG. This makes replay bit-for-bit deterministic.
- **The log is the source of truth.** `BrainState` is a *fold* over the log and must stay rebuildable from it. Don't add un-derived state.
- **Deltas are transport, never durable.** `ModelDelta`/`CapabilityChunk` feed live op buffers but are *never* written to the log. One consolidated `Record` is appended per logical message/tool-result.
- **Streaming is the only model mode.** Adapters stream deltas live via the sink and return the consolidated output; there is no non-streaming path.

## The narrow-waist rule

> **Type only what the brain branches on. Everything else is an opaque payload.**

- Typed & stable: op lifecycle (start/delta/done/error/cancel), model *output structure* (text vs tool calls), turn control, permission outcomes.
- Opaque (`Value`): capability args/results, provider knobs, prompts, answers. The brain stores and forwards these; it never interprets.
- Corollary: **an enum nobody branches on should be a string.** Status labels, privilege classes, tier/selector names are open string sets, not variant lists. Error enums matched for handling are fine.
- Adding a new tool, provider knob, or agent grant must touch **zero** core types. If a change forces a core type edit for a new *capability*, reconsider it.
- Breaking changes are acceptable (hobby prototype, no external users): no `#[non_exhaustive]`/constructor ceremony for stability's sake, no serde back-compat fields, no deprecation shims.

## Where logic goes

- **Agent strategy** (which model selector, how to project context, whether a capability is gated) lives in the pluggable `TurnPolicy`, never hardcoded in the reducer. `StaticPolicy` is the trivial pass-through projection. Custom policy decoders registered in `PolicyRegistry` must be pure so replay/resume stay deterministic.
- **The reducer** (`brain.rs`) maintains the log and op table, drives the turn loop, asks the policy, routes opaque payloads, and decides when to finish or checkpoint. Strategy belongs in a policy, not the reducer.
- **IO, HTTP, model resolution, and storage** belong in the host, not in `hugr-core`.

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
                        #   (filesystem, shell, web, state, delegation), and the `hugr`
                        #   CLI: new / run / build / traces / replay / verify
crates/hugr-wasm/       # generic WASM bindings around hugr-core for browser/JS
                        #   hosts (submit/poll over JSON + browser tool schemas)
crates/hugr-python/     # PyO3 runtime embedding (outside the cargo workspace;
                        #   built by maturin from bindings/python)
bindings/python/        # the `hugr-agents` Python package: typed pure-Python
                        #   layer + pytest suite over crates/hugr-python
bindings/typescript/    # the `hugr-agents` TS package: typed Agent over the WASM
                        #   brain (node + browser) + the vendored extension JS modules
examples/hugr-docs/     # the reference subagent crate (docs Q&A): hugr.toml +
                        #   SYSTEM.md plus typed response contract using hugr-toolkit
examples/hugr-weather/  # the self-contained beginner example; also the source of
                        #   the `hugr new --template weather` scaffold (embedded
                        #   at compile time, name substituted)
examples/hugr-insights/ # offline self-improvement agent: mines another agent's
                        #   traces + feedback via traces_read and reports suggestions
examples/hugr-datasmith/ # docs-QA dataset synthesizer: fs_read-jailed, typed QaDataset
                        #   contract, buildable as a typed Python wheel
examples/hf-librarian/  # Python-surface pipeline: datasmith wheel → jailed Hub
                        #   publisher → judge-graded eval of hugr-docs
examples/chrome-extension/ # a concrete browser host: chrome.* capabilities,
                        #   side-panel UI, MV3 manifest (vendors the generic JS)
.agents/skills/          # concise coding-agent workflows for building Hugr agents,
                        #   language/browser surfaces, and trace debugging
```

`hugr-replay` is a host-side **persistence** crate. It may use `std::fs`, but it depends on `hugr-core` as pure data only.

The layers stack strictly: `hugr-agent` on `hugr-host` + `hugr-replay`, then `hugr-toolkit` on `hugr-agent`. Nothing reaches into `hugr-core` internals; these layers are hosts like any other.

**Never add environmental dependencies to `hugr-core`** to make a host easier. Put them in the host crate.

Subagent-layer conventions are documented in `docs/agents.md`. The `Ask`/`Answer` contract is the stable boundary. `AnswerMeta` (cost/duration/tokens) is mandatory, errors are answers (`status: "error"`, exit 0), and the user-facing payload uses `Answer.response` as a JSON object. Typed Rust response contracts derive provider JSON Schema with `schemars` and cast final JSON with `serde`.

Traces are immutable. A resumed ask writes a **new** trace with `depends_on` set. Default agent state is `~/.hugr/<agent>/` (`traces/`, `scratch/`, `memory/`, `feedback/`) plus the shared blob store `~/.hugr/blobs`. Override these paths with `HUGR_AGENT_HOME`, `HUGR_HOME`, or `HUGR_BLOB_STORE`. Custom `StorageOverrides` are trusted host code and must stay outside `hugr-core`.

Tools are granted in the manifest and jailed to their declared scope through sandbox-by-registration. Never register a capability that the manifest does not grant.

A built Hugr agent can be granted as a tool with `[tools.agent.<name>]` and a subprocess over the CLI JSON contract. Delegation never widens privileges, and the child's cost folds into the caller's `AnswerMeta`.

Process access is an explicit operator grant. Restricted `[tools.shell]` executes allowlisted programs directly without shell syntax; `full_access = true`, `[tools.mcp.<name>]`, and agent delegation are external-process escape hatches whose operating-system sandbox belongs to the host. Never register them without the matching manifest grant.

When extending the host, keep capabilities uniform with no privileged built-ins. A model call is an effect provided by the host and registered like a capability. The adapter handles transport errors such as retries and 429s, while semantic errors return to the model as tool results.

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

- **Keep the docs and agent skills in sync.** After a behavior change, update the relevant reference documentation under `docs/` and every tutorial that demonstrates it. A manifest, tool, surface, packaging, or trace-workflow change is not complete until the relevant `.agents/skills/*/SKILL.md` cheat sheet matches reality.
- **Keep Rust and Python API types in sync in both directions.** Any change to a Rust-serialized runtime input or output type must update the corresponding `bindings/python/python/hugr_agents/_types.py` `TypedDict`/dataclass, caster, exports, and tests. This includes contracts, events, cards, trace listings, feedback, stats, and nested values.

  Any change to those public Python mirrors must update the corresponding Rust type, serde wire shape, and tests.
- **Prefer deletion over abstraction.** One way to do each thing; if two mechanisms do the same job, keep the one the live stack uses and delete the other.
- **Markdown is single-line.** Use one physical line per paragraph or bullet. Do not hard-wrap prose; rely on soft wrapping. Fenced code blocks and table rows are exempt.
- **Comments state what the code cannot.** No references to numbered documentation sections, no "how it works" narration, no comments restating the signature or the next line, no section banners. A comment is justified only for a non-obvious constraint, failure mode, or safety/jail invariant; public items keep one concise doc line stating the contract.
- Keep event handlers O(1)-ish (append to a buffer); no heavy work in the reducer.
- When you add a `Command`/`Event`/`Record` variant: update the reducer's match and add a scripted test that pins the resulting command sequence.
- Determinism is testable: any new control-flow path should have a replay test asserting identical commands on a re-fed event stream; `verify()` is the release gate.
- **Ideas flow: `new_ideas.md` → `plan.md` → implementation.** While implementing, drop short one-line ideas (not designs or TODO lists) into `new_ideas.md` so the owner can review them; when an idea is promoted into the structured roadmap `plan.md`, remove it from `new_ideas.md`.

## Documentation writing guidelines

These apply whenever you create or update any Markdown file in this repository: the README, `docs/`, example READMEs, and skills.

- **Write like an experienced engineer.** Clear, natural, precise, understated. A page exists to help a reader complete a task or understand the system, not to impress them.
- **Preserve accuracy.** Do not invent behavior, guarantees, examples, or constraints. Keep commands, code samples, links, API names, and identifiers exactly as they are unless they are incorrect. Prefer concrete explanations over broad claims, and do not overstate the importance of a result or design decision.
- **No AI-style or promotional writing.** Avoid dramatic, suspenseful, or marketing phrasing: "this changes everything", "let's dive in", "here's the catch", "this unlocks...", "at its core", "it is worth noting that", "seamless", "robust", "powerful", "game-changing". Do not describe ordinary behavior as "critical", "major", or "remarkable" unless objectively justified. State the fact and let the reader judge its importance.
- **Avoid recurring rhetorical patterns.** No "not only X, but Y", "it's not about X, it's about Y", "this isn't just X; it's Y", "the result? ...", "the key takeaway is...", forced contrast, punchlines, rhetorical questions, or one-sentence paragraphs written only for emphasis.
- **Punctuation and emphasis.** Avoid em dashes; use commas, periods, colons, or parentheses instead. Keep hyphens where grammar requires them (compound adjectives, flags, identifiers). Use italics sparingly and bold only where it improves scanning. Do not overuse colons, semicolons, parentheses, or exclamation marks.
- **Plain word choice.** "uses" over "leverages", "shows" over "demonstrates", "before" over "prior to", "about" over "with regard to". Remove unnecessary adjectives and adverbs, and do not repeat the same point in slightly different words.
- **Structure.** Start each page with a short statement of what it covers, put the information readers need first, use headings that describe the content directly, prefer short sections, use lists only for genuinely list-like information, include limitations and failure cases where relevant, and end when the topic is covered without a generic conclusion.
- **Markdown formatting.** One physical line per paragraph or bullet, as stated in the conventions above; never hard-wrap prose.
- **Before finishing a page,** reread it and remove dramatic claims, canned transitions, em dashes, exaggerated adjectives, repeated contrast formulas, duplicated explanations, and anything that reads like marketing copy.
