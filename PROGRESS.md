# Progress

Running log of what's implemented, phase by phase (see `docs/ROADMAP.md`).

## Branding rename — Hugr ✅

The project branding now uses Hugr across the workspace, keeping only the repository root directory unchanged. Crate directories, package names, Rust crate imports, CLI binary/help text, env var docs, browser extension metadata, generated WASM glue, tests, and design/progress docs now use the `hugr-*` / `hugr_*` / `Hugr` naming scheme from `docs/BRANDING.md`. Verification: `cargo check --workspace`, `cargo test`, `cargo clippy --all-targets`, `cargo tree -p hugr-core`, and `./crates/hugr-wasm/build-extension.sh`.

## Roadmap 2 Phase 0 — Foundations ✅

**Goal:** retrofit the post-foundation product defaults from `docs/ROADMAP_2.md`: three model tiers, host-side token estimates on durable content, and the two-mode permission model (`auto-approve` by default, `yolo` as explicit allow-all).

Done:

- `hugr-providers` now owns a host-side `TierModelConfigSet` for exactly `small`/`medium`/`big`, loaded from the `models` section of `HUGR_CONFIG` and overridden by `HUGR_BASE_URL` / `HUGR_MODEL_SMALL` / `HUGR_MODEL_MEDIUM` / `HUGR_MODEL_BIG` / CLI `--model`. All three tiers default to the same HF router model until better product defaults are chosen. `OpenAiAdapter` accepts per-tier default sampling knobs while request-level params still win.
- `hugr-core` keeps `ModelSelector` open, but `StaticPolicy::default()` and `EngineBuilder` now default fresh sessions to the `medium` selector. `choose_model` remains the only pure brain-side tier decision; real routing stays for Phase B.
- Durable content records now carry `est_tokens`: `UserMessage`, `ModelOutput`, and `ToolResult`. The host attaches estimates to `UserInput`, `ModelDone`, capability/agent/user-answer results, and denied permission decisions before they enter the brain; the reducer copies those values into the log and never tokenizes. Replay re-feeds the recorded estimates, so it does not re-estimate.
- `hugr-host::policy::AutoApprove` is the default headless permission policy for the CLI: gated actions call the configured `small`-tier judge model, parse a JSON `{ safe, reason }` verdict, and return `Allow` or `Deny { reason }`. Read-only capabilities still skip permission entirely. The judge result is recorded as the ordinary `PermissionDecision` event, so replay verifies without the judge model.
- `hugr-cli` registers all three tiers, uses `medium` for normal turns, defaults to auto-approve, and exposes `--yolo` / `-y` for allow-all. The startup banner shows the active mode and tier-to-model mapping.
- The Chrome extension settings now store `small`/`medium`/`big` model ids, the side-panel brain defaults to `medium`, model calls resolve logical selectors through those settings, and permissioned browser actions either run in yolo mode or call the `small` judge. No permission popup appears for the core permission flow.

Tests:

- `hugr-core/tests/scripted_session.rs` pins that host-supplied `est_tokens` land on every durable content record in a scripted turn.
- `hugr-host/tests/end_to_end.rs::auto_approve_denies_risky_shell_and_replay_uses_recorded_verdict` proves a risky shell command is denied with a model-visible reason and the recorded trace `verify()`s without re-running the judge; `::auto_approve_allows_benign_shell` proves a benign gated command proceeds.
- Focused verification run: `cargo test -p hugr-core -q`, `cargo test -p hugr-replay -q`, `cargo test -p hugr-host -q`, `cargo test -p hugr-wasm -q`, `cargo check -p hugr-cli`, and escalated `cargo test -p hugr-providers --test retry -q` for local loopback retry tests.

## Roadmap 2 Phase A — Context kernel & lossless compaction

### A1 — ContextPlan projection ✅

Done:

- `hugr-core` now plans projection through `TurnPolicy::project_context(log, budget) -> ContextPlan` instead of returning a `ModelRequest` directly. `TurnPolicy::context_budget` supplies the budget, and the reducer renders `ModelRequest` from the returned plan before emitting `StartModelCall`.
- `ContextPlan` records the token budget, per-source `ContextPlanEntry`s, dispositions (`Included`, `Referenced`, `Summarized`, `Omitted`), per-entry reasons, budget totals, cache hints, tools, params, and opaque provider extras. Public plan/budget structs and enums are serializable, `#[non_exhaustive]`, and constructor-backed.
- `StaticPolicy` remains behavior-compatible: it produces an all-included pass-through plan for user/model/tool content, explicitly omits `OpEnded` metadata with a reason, and sums the host-recorded `est_tokens` values without tokenizing.
- `docs/ARCHITECTURE.md` now describes the plan-first projection contract.

Tests:

- `crates/hugr-core/tests/scripted_session.rs::context_plan_explains_dispositions_and_renders_request` pins the A1 shape: budget is threaded through, every log entry gets a disposition/reason, totals use recorded token estimates, and rendering the plan produces the model request.
- Verification run: `cargo test`.

### A2 — Durable summary records ✅

Done:

- `hugr-core` now has durable `Record::Summary` entries with an exact inclusive `SeqRange`, explicit `SummaryCoverage`, the tier that produced the summary, and host-recorded `est_tokens_in` / `est_tokens_out`. Summaries are appended like any other log record and the source records remain untouched.
- `StaticPolicy` projection consumes complete summaries: uncovered summaries render as `Summarized` assistant blocks, and covered source records render as explicit `ContentPart::Ref` references back to their original log seqs. Projection remains pure and synchronous; it only reads summaries already present in the log.

Tests:

- `crates/hugr-core/tests/scripted_session.rs::summary_records_round_trip_and_evict_covered_span_to_refs` pins JSON log round-trip for summary records and verifies a later projection evicts the covered span to references.
- Verification run: `cargo test -p hugr-core`.

### A3 — Automatic compaction sub-loop ✅

Done:

- `TurnPolicy` now exposes a pure high-water/selection surface for automatic compaction: `compaction_high_water(state, budget)` and `select_compaction_span(log, plan)`. `StaticPolicy` defaults to a 90% high-water mark, can disable or tune it with `with_compaction_high_water_percent`, and selects the oldest still-included durable content while keeping the newest compactable entry live for the active turn.
- The reducer runs the compaction sub-loop from ARCHITECTURE §3.4: when a planned projection exceeds the high-water mark, it emits a `small`-tier `StartModelCall` with a structured request over the exact selected span, records the returned `ModelDone` as `Record::Summary`, checkpoints, and then re-projects before starting the normal turn model call.
- Compaction model deltas remain transport-only and are not rendered as assistant output. Replay stays deterministic because the summarizer result is just another recorded `ModelDone` event carrying host-recorded token estimates; re-feeding the event stream reproduces the same summary record and command sequence without asking the host to invent new data.

Tests:

- `crates/hugr-core/tests/scripted_session.rs::automatic_compaction_summarizes_then_reprojects_and_replays` pins the command sequence (`small` compaction call, checkpoint, normal `medium` turn call), summary metadata (`summary_of`, tier, token-in/out), compacted projection refs, and replay equality.
- Verification run: `cargo test -p hugr-core`.

### A4 — Manual compaction trigger ✅

Done:

- `hugr-core` now accepts `Event::CompactContext`, a pure host-injected control event that runs one deterministic compaction selection over the current `ContextPlan`.
- Manual compaction reuses the same `small`-tier summary request and durable `Record::Summary` path as automatic compaction, but returns to idle after checkpointing instead of starting a normal model turn.
- Busy brains and sessions with no compactable span produce cosmetic notices only; no durable records are mutated unless a summary model result is folded back through `ModelDone`.

Tests:

- `crates/hugr-core/tests/scripted_session.rs::manual_compaction_event_runs_one_pass_without_starting_turn` pins the single-event command sequence, summary metadata, idle postcondition, and replay equality.
- Verification run: `cargo test -p hugr-core`.

### A5 — CLI context inspection and compact command ✅

Done:

- `Brain::context_plan()` and `Engine::context_plan()` expose the same pure projection the reducer uses for the next normal model turn, without mutating state or starting a call.
- `Engine::compact_context()` injects `Event::CompactContext` and drives the resulting manual compaction pass to idle.
- The CLI REPL now handles `/context` and `/compact`. `/context` prints budget totals plus every plan entry's source, disposition, token estimate, and reason. `/compact` fires the A4 trigger; one-shot CLI invocations can also run these slash commands directly.
- `hugr replay --step` recognizes `CompactContext`, and the README documents the new REPL commands.

Tests:

- `crates/hugr-host/tests/end_to_end.rs::context_plan_inspection_and_manual_compaction_feed_next_request` proves the host-facing plan reflects the real projection, manual compaction reduces the planned request, and the next model request contains the expected summary and log reference.
- Verification run: `cargo test -p hugr-host context_plan_inspection_and_manual_compaction_feed_next_request -q`; `cargo check -p hugr-cli`.

### A6 — Browser context drawer and compact button ✅

Done:

- `hugr-wasm` now exposes `contextPlanJson()` over the JSON binding, backed by the same `Brain::context_plan()` projection API as the native host.
- The browser engine exposes `contextPlan()` and `compactContext()`. Manual browser compaction feeds the recorded `CompactContext` event and drives the resulting model pass to idle.
- The Chrome side panel has a context drawer showing budget usage, retained blocks, summaries, refs, omissions, tools, and every plan entry's source/disposition/token count/reason.
- A compact button fires one A4 compaction pass and refreshes the drawer. The checked-in extension WASM bundle was rebuilt with `./crates/hugr-wasm/build-extension.sh`.

Tests:

- `crates/hugr-wasm/src/lib.rs::tests::context_plan_json_exposes_projection` pins the JSON binding.
- Verification run: `cargo test -p hugr-wasm -q`; `./crates/hugr-wasm/build-extension.sh`.

## Roadmap 2 Phase B — Tier routing

### B1 — Pure routing inputs ✅

Done:

- `TurnPolicy::choose_model` now receives a pure `RoutingInputs` snapshot alongside `BrainState`. The snapshot exposes the routing phase, recent tool-risk signal, context pressure, recent failure count, and a future one-shot override slot; every field is derived from the log/state/current `ContextPlan`, so routing remains sans-IO and replay-deterministic.
- The reducer computes `RoutingInputs` at the normal model-call boundary, classifying fresh user turns vs tool-follow-up turns without consulting the host. `StaticPolicy` keeps its previous behavior and simply ignores the inputs.
- `docs/ARCHITECTURE.md` documents the routing-input contract as derived state, never observed environment.

Tests:

- `crates/hugr-core/tests/scripted_session.rs::routing_inputs_are_purely_derived_for_turn_and_followup` pins the B1 inputs for a normal turn and a failed-tool follow-up.

### B2 — Deterministic tier routing policy ✅

Done:

- `hugr-core` now provides `RoutingPolicy`, a real `TurnPolicy` that delegates projection, permissions, background ops, sub-agent seeding, and compaction to a `StaticPolicy` base while replacing only model selection.
- Routing is deterministic over recorded data: `small` is used for session/title naming, quick classification, and the explicit compaction/judge/title/classification phases; `big` is used for recent failure signals, denied/failed tool results, high context pressure, or hard repo-wide/architecture prompts; the base selector, normally `medium`, is used otherwise.
- Fresh native `EngineBuilder` sessions now use `RoutingPolicy` and serialize it into new traces. `hugr-replay` and `hugr-wasm` decode both new `RoutingPolicy` configs and legacy `StaticPolicy` configs, so existing traces/configs keep working.

Tests:

- `crates/hugr-core/tests/scripted_session.rs::routing_policy_deterministically_uses_small_medium_and_big` proves deterministic routing across all three tiers and replay equality from the same recorded events.
- Verification run: `cargo test -p hugr-core -q`; `cargo test -p hugr-replay -q`; `cargo check -p hugr-host -q`.

### B3 — Trace-visible routing/spend metadata ✅

Done:

- `OpMeta` now carries optional `RoutingDecision` metadata for model ops: chosen selector, routing reasons, and an opaque snapshot of the pure routing inputs. Normal model calls record the policy's explanation; automatic/manual compaction records its small-tier compaction reason.
- Per-op selector, usage, and injected start/end timestamps remain on `OpMeta`, so model tokens and latency are trace-visible. Provider cost is still read host-side from `Usage.extra`, preserving the core's narrow-waist rule.
- `hugr-host::spend_report(log)` scans only trace/log `OpEnded` records and returns per-tier calls, input/output tokens, cost, latency, and recent routing decisions. B4 status output builds on this.

Tests:

- The routing-policy scripted test now asserts that the escalated `big` call's `OpMeta.routing` contains the selector, failure reason, and recorded input snapshot.
- `crates/hugr-host/tests/end_to_end.rs::metrics_flow_through_engine` asserts per-tier spend and recent routing decisions are queryable from the engine log via `spend_report`.
- Verification run: `cargo test -p hugr-core -q`; `cargo test -p hugr-host metrics_flow_through_engine -q`.

### B4 — CLI tier/status controls ✅

Done:

- Added `Event::ModelOverride { selector }`, folded by the reducer as a one-shot selector override for the next normal model turn. The event is recorded like other host input, so tier overrides replay deterministically and clear after one use.
- `Engine::override_next_model` injects that event for native hosts.
- The CLI REPL now supports `/model` (tier mapping + pending override), `/tier [small|medium|big|auto]` (set/clear the next-turn override), and `/status` (tier mapping, pending override, context budget fullness, per-tier spend, and recent routing reasons from `spend_report`).
- `hugr replay --step` summarizes recorded `ModelOverride` events.

Tests:

- `crates/hugr-core/tests/scripted_session.rs::model_override_forces_one_turn_then_clears` pins the one-shot override behavior.
- Verification run: `cargo test -p hugr-core -q`; `cargo check -p hugr-cli -q`.

### B5 — Browser tier chips and override ✅

Done:

- The Chrome side panel now builds a `RoutingPolicy` config for the WASM brain, matching the native host's three-tier routing behavior.
- Each assistant response bubble shows a stable tier chip (`used small`, `used medium`, or `used big`) from the `StartModelCall` selector.
- The composer has a next-turn tier selector (`auto`/`small`/`medium`/`big`). Selecting a tier injects the same recorded `ModelOverride` event as the CLI; the browser engine clears the UI after the next normal model call consumes it.
- The checked-in extension WASM bundle was rebuilt so the browser host understands `RoutingPolicy`, `RoutingDecision`, and `ModelOverride`.

Tests:

- Verification run: `cargo test -p hugr-wasm -q`; `./crates/hugr-wasm/build-extension.sh`.

## Roadmap 2 Phase C — Skills & MCP

### C1 — MCP stdio client ✅

Done:

- `hugr-host::mcp` now connects to stdio MCP servers, performs the MCP `initialize` / `notifications/initialized` handshake, discovers remote tools through `tools/list`, and exposes each tool as an ordinary `Capability` in the existing registry. No `hugr-core` contract changes were needed.
- MCP tool names are namespaced as `mcp__<server>__<tool>` before advertisement so they coexist with built-in and plugin tools in the narrow-waist capability registry; remote arguments and results stay opaque `Value`s.
- `tools/call` results route back as normal capability results. MCP `isError` tool responses and transport/protocol failures become error-shaped tool results the model can react to, while startup/discovery failures remain host-side load errors.

Tests:

- `crates/hugr-host/tests/end_to_end.rs::mcp_stdio_tool_runs_through_real_engine` starts a tiny external stdio MCP server, loads its tool into the real engine, has the model call it, and verifies the result is logged with zero core changes.
- Verification run: `cargo test -p hugr-host`.

### C2 — CLI MCP configuration and status ✅

Done:

- `hugr-cli` now accepts repeatable `--mcp <cmd>` flags and reads MCP servers from the shared `HUGR_CONFIG` JSON root through either `mcp` or `mcp_servers`. Entries may be command strings or objects with `name`, `command`/`cmd`, and optional `args`.
- Loaded MCP stdio servers are registered on the same `EngineBuilder` as built-in tools and plugins, so their namespaced tools appear in the `ModelRequest` tool list with no core change.
- `/status` now reports connected MCP servers and their advertised capability names alongside model tier mapping, context fullness, spend, and routing reasons.
- `README.md` documents the config shape and `--mcp` behavior.

Tests:

- `crates/hugr-cli/src/main.rs` unit tests pin command-spec splitting and JSON config parsing for string and object MCP entries.
- Verification run: `cargo test -p hugr-cli`.

### C3 — Browser MCP limitation and settings fallback ✅

Done:

- The Chrome extension settings now store an `mcpServers` JSON array with the same name/command/args shape used by the CLI config, so user intent is configurable without changing the WASM brain contract.
- The browser host explicitly documents and surfaces the MV3 limitation: extension pages cannot spawn local stdio subprocesses, so stdio MCP is unavailable without a native bridge or future browser-compatible transport. The side panel warns when MCP declarations are configured but inactive.
- The extension README documents the supported fallback: use the native CLI `--mcp <cmd>` or `HUGR_CONFIG` MCP section today; the browser declarations are reserved for a native bridge / browser transport.

Tests:

- Verification run: `cargo test -p hugr-wasm`.

### C4 — Host-side skills loader ✅

Done:

- `hugr-host::skills` discovers skill bundles from explicit roots or well-known locations (`HUGR_SKILLS_DIR`, `.hugr/skills`, and `$HOME/.config/hugr/skills`). Each bundle is a directory with `SKILL.md`; roots may point directly at one bundle or at a directory of bundles.
- A discovered `SkillBundle` exposes stable host metadata: id, title, summary, root path, full instructions, and optional contributed tool schemas loaded from `tools/*.json`. Discovery is host IO only and does not change `hugr-core`.
- `hugr-host` re-exports `SkillBundle` / `SkillError` for embedders and later CLI/browser surfacing.

Tests:

- `hugr-host::skills::tests::discovers_skill_bundle_metadata_and_tool_schemas` creates a bundle on disk, discovers it, and verifies metadata plus optional tool-schema loading.
- Verification run: `cargo test -p hugr-host skills`.

### C5 — Core skill descriptors and activation ✅

Done:

- `hugr-core` now has a pure `SkillDescriptor` that hosts can supply to `StaticPolicy::with_skills`. Each descriptor advertises a lightweight model-invocable tool named `skill__<id>` with no host IO.
- `TurnPolicy::activate_skill(capability)` makes skill selection a policy decision, analogous to background ops and sub-agent seeding. The reducer asks the policy; it does not hardcode skill names.
- When the model invokes a skill descriptor, the brain appends a durable `Record::SkillActivated` containing id, title, summary, instructions, and host-supplied token estimate, then appends the correlated tool result and resumes the model turn. Later projection renders those instructions from the durable record, so replay does not depend on rediscovering the skill bundle from disk.

Tests:

- `crates/hugr-core/tests/scripted_session.rs::skill_invocation_records_activation_and_projects_instructions` proves the descriptor is advertised, invocation records durable activation, the next request includes the skill instructions, and no core IO is involved.
- Verification run: `cargo test -p hugr-core`.

### C6 — Skill surfacing in CLI and browser ✅

Done:

- `EngineBuilder::skills` threads `SkillDescriptor`s into the fresh-session `RoutingPolicy`, so host-discovered/configured skills are advertised as model-invocable descriptors.
- The CLI discovers disk skill bundles through `hugr-host::skills::discover()`, registers them with the engine, shows the count in the startup banner, adds `/skills`, and reports the active skill in `/status` by scanning durable `Record::SkillActivated` entries.
- The Chrome extension settings now store skill descriptors as JSON, passes them into the WASM brain policy, adds a Skills drawer, and updates the drawer when a projected request shows an active skill.
- `README.md` and the extension README document the skill list/active-skill surfaces.

Tests:

- Verification run: `cargo check -p hugr-cli`; `cargo test -p hugr-wasm`; `cargo test -p hugr-host skills`.

## Phase 0 — Pure core skeleton (no IO) ✅

**Goal:** the brain exists as a pure state machine with zero IO.

Done:

- Workspace set up (`crates/hugr-core`), ready to grow into the full layout.
- `hugr-core` — the sans-IO reducer, split into modules:
  - `primitives.rs` — `OpId`, `Seq`, `Timestamp`, `Value`, `ObjectKey`.
  - `model.rs` — canonical `ModelRequest`/`ModelDelta`/`ModelOutput`, `ToolCall`, `ToolSchema`, `Usage`, `ModelSelector` (+ constructors). `Usage` carries `input_tokens`/`output_tokens` plus an **opaque `extra: Value`** (narrow-waist passthrough) for provider extras such as cost — the brain never reads it; only the host does.
  - `command.rs` / `event.rs` — the two-enum brain↔host contract, `#[non_exhaustive]` throughout.
  - `record.rs` — the append-only log (`LogEntry`, `Record`, `OpOutcome`, `OpMeta`).
  - `state.rs` — `BrainState` + in-flight op table (derived; foldable from the log).
  - `policy.rs` — pluggable `TurnPolicy` + `StaticPolicy` (trivial pass-through projection).
  - `brain.rs` — `Brain::poll()` / `submit()` + the turn-loop reducer.
- Tests (`crates/hugr-core/tests`): scripted session, permission round-trip, parallel tool calls, projection contents, deterministic replay, delta-vs-log, JSON round-trip. **9 passing.**

**Exit criteria — met:**

- ✅ Scripted `user → model → tool → model → done` reduces to the expected command sequence (`scripted_session.rs`).
- ✅ Deterministic replay: same event stream twice → identical commands (`determinism.rs`).
- ✅ No `tokio`/`reqwest`/`fs` in `hugr-core` (`cargo tree -p hugr-core` shows only `serde`/`serde_json`).

Decisions:

- Single crate for Phase 0; model types kept in `hugr-core` (move to `hugr-model` later if needed).
- `#[non_exhaustive]` on enums **and** host-facing structs, with constructors on the structs (forward-compatible, narrow-waist).
- Dropped `panic = "abort"` from the release profile (conflicts with the test harness; belongs in a WASM-specific profile in Phase 4).

## Phase 1 — Batteries-included CLI host (the showcase) ✅

**Goal:** a real, usable terminal agent driven by the Phase 0 core.

Done:

- `hugr-host`: the tokio [`Engine`] driver loop (drain `poll()` → perform commands as concurrent tasks → await next event → `submit()`), plus:
  - [`Capability`] + [`ModelAdapter`] traits and their registries.
  - Host-side permission [`Policy`]: `AutoApprove` (small-tier judge), `AllowAll`/yolo, `DenyAll`, and the legacy/test `Interactive` prompt policy.
  - [`Frontend`] trait + streaming `StdoutFrontend`.
  - `EngineBuilder` that assembles the brain's `StaticPolicy` from registered capabilities (their schemas → advertised tools; sensitive ones → gated set).
- Capabilities (`hugr-host::capabilities`): `shell` (streams stdout), `fs_read` (read-only, no permission), `fs_write`, `http`.
- `hugr-providers`: `OpenAiAdapter` — chat completions with streaming SSE, tool-call assembly (every consolidated `ToolCall` is guaranteed a stable, non-empty id — synthesized from the stream index when a compatible server streams `name`/`arguments` before the `id` or omits it entirely — so the brain's `tool_call_id` result correlation never silently breaks; pre-id args are buffered and flushed once), usage accounting (including **real cost from the router**: the adapter reads `usage.cost`/`total_cost`/`cost_details.total_cost` from the response and surfaces it verbatim in `Usage.extra` as `{ "cost", "cost_source": "router" }`; when the response omits cost it falls back to a tiny static per-token price table, tagged `"cost_source": "estimated"`, and emits no cost at all for unknown models), configurable base URL/model. Defaults target the **Hugging Face router** (`https://router.huggingface.co/v1`, `google/gemma-4-31B-it:cerebras`); the API key resolves from `HUGR_API_KEY` → `HF_TOKEN` → the Hugging Face token file read directly (`HF_TOKEN_PATH`, else `$HF_HOME/token`, else `~/.cache/huggingface/token`) → `hf auth token` (last resort, only if no token file is present). Reading the token file directly means a logged-in user needs no `hf` binary on `PATH`. Transport-level **retry with exponential backoff** (the adapter's job, per CLAUDE.md): transient failures — network/connect errors, HTTP 429, and 5xx — are retried with capped exponential backoff up to a configurable `max_attempts` (`with_max_attempts`, default 4); non-429 4xx are semantic errors and are never retried.
- `hugr-cli`: the `hugr` binary. One-shot (`hugr "prompt"`) or interactive REPL; `--yolo` / `-y` for allow-all. Prints a startup banner (tier mapping · endpoint · mode).
- CLI observability: the `Frontend` trait gained lifecycle hooks (model start/end + token usage, tool start with args, tool result, permission decision, session end); `StdoutFrontend` renders them with ANSI colors (auto-disabled off a TTY / under `NO_COLOR`).
- CLI metrics: `StdoutFrontend` renders per-call metric lines and a session-totals footer. Per model call it shows **cost** (read from `Usage.extra` — the narrow-waist passthrough the adapter fills, ARCHITECTURE §2.4), **input/output tokens**, and **elapsed time**; per tool call it shows elapsed time. Elapsed below `0.01s` is treated as zero and omitted. At session end (`Frontend::on_session_end`, driven by `Engine::session_end` after a one-shot run or interactive exit) it prints a `Σ` footer with total elapsed, total in/out tokens, and total cost. All timing is **host-side** (`Instant` in the front-end); `hugr-core` stays clock-free / sans-IO. The accumulation + formatting live in a pure, unit-tested `Metrics` struct (folding model/tool calls into totals; tiny-cost precision; empty-session yields no footer).
- Collapsed tool output: `StdoutFrontend` renders large tool results as a head (first `RESULT_HEAD_LINES` = 8 lines) plus a "… +N lines" summary, so a 1000-line shell result stays compact. Full output is restored by `HUGR_FULL_OUTPUT` (truthy env var; honoured by `StdoutFrontend::default`) or the CLI's `--full-output` flag (`StdoutFrontend::with_full_output`). Object results expand multiline string fields (e.g. a shell `stdout`) so the line count reflects real output.
- Streaming is the **only** model mode (explicit contract on `ModelAdapter`): adapters stream deltas live via the sink, then return the consolidated output. No non-streaming path exists.

Refinement to `hugr-core` made for real providers: the durable `ToolResult` now carries the originating model `tool_call` id, so projection emits provider-correct `tool_call_id` correlation. Added `ModelOutput::new`, `ModelRequest::new` and `SamplingParams` builders (host-facing structs are `#[non_exhaustive]`).

Tests (40 total across the workspace):

- `hugr-host/tests/end_to_end.rs` — a real multi-turn session driven through the tokio loop with a scripted model + the **real shell capability**; a denied-permission round-trip; plus a metrics flow test (a cost-reporting scripted model drives `on_model_end` with tokens + cost from `Usage.extra`, tool ends fire, and `Engine::session_end` triggers `on_session_end` once).
- `hugr-host` `frontend` unit tests — tool-result collapse/full-output, and the `Metrics` accumulation + footer formatting (token/cost folding, tiny-cost precision, elapsed floor, empty-session = no footer).
- `hugr-providers` — unit tests for request building + SSE accumulation + retry classification/backoff, `tests/streaming.rs` driving the adapter against a **local mock SSE server** (real reqwest streaming path), and `tests/retry.rs` driving retries against a **local mock HTTP server** (transient 429/5xx retried to success, persistent 5xx gives up after `max_attempts`, 4xx not retried).

**Exit criteria:**

- ✅ "CLI on a laptop" host setup ≈ 10 lines on top of `hugr-host` (see the marked block in `crates/hugr-cli/src/main.rs`).
- ✅ Genuine multi-turn session end-to-end. Verified **live** against the HF router: `hugr -y "Use the shell tool to run 'echo hugr-live-test', then tell me what it printed."` — the model called the shell tool, the host ran it and streamed the output, and the model produced a final answer. Also covered by the driver-loop + mock-SSE tests for CI (no key needed).

## Phase 2 — Concurrency & streaming (the differentiator) ✅

**Goal:** multiple in-flight operations; the LLM is "just another stream."

### P2-1 — Multiple concurrent ops ✅

The op table already held many in-flight ops keyed by `OpId`, and the host already ran one task per op (one `tokio::spawn` per `StartModelCall`/`StartCapability`, feeding a single inbox channel — the brain reduces interleaved events one at a time, atomically). The missing piece for "a model stream **and** a background `shell` op run simultaneously" was a way for an op to *not* hold the turn open. Added:

- `hugr-core`: `OpKind::Capability` gained a `background: bool` flag; `OpKind::blocks_turn()` returns `false` for background capabilities (and was rewritten as an exhaustive match so a future op kind can't silently default to "blocks"). `TurnPolicy` gained `is_background(capability) -> bool` (default `false`), implemented by `StaticPolicy` via a configurable background set (`with_background`). The reducer (`brain.rs`): after a model turn's tool fan-out, if nothing blocks the turn it resumes the model immediately so it streams alongside the background op(s); a granted-permission background op resumes likewise; and `on_model_done` **defers `Done`** while a background op is still in flight (the turn isn't over while work runs — the background result is folded in and a fresh turn picks it up). No new `Command`/`Event`/`Record` variants — background-ness is a brain-side scheduling decision the host never sees.
- `hugr-host`: `Capability` gained `runs_in_background()` (default `false`); `CapabilityRegistry::background_names()`; `EngineBuilder::build()` threads those into the brain's `StaticPolicy` (`.with_background(...)`), mirroring the existing permissioned-names wiring. The `Engine` driver loop needed **no** change — it already spawns one task per op and reacts event-driven (the shell capability awaits `wait_with_output()`, so `ProcessExited` is instant with no polling/`sleep`).

Tests (44 total across the workspace, +4):

- `hugr-core/tests/concurrent_ops.rs` — the headline scripted interleave (model stream + background shell, pinning the exact command sequence including the deferred `Done`); deterministic replay over the interleaved stream (identical commands **and** identical log); and a mixed background + foreground fan-out asserting only the *foreground* op gates the turn.
- `hugr-host/tests/end_to_end.rs::model_stream_runs_while_background_op_is_in_flight` — through the **real tokio engine**: a background op blocks on a channel while the next model call provably runs (true overlap, not "both ran eventually"), then releases it; the final turn picks up the result and ends with exactly one `EndTurn`.

**Exit criteria:**

- ✅ Kick off a long background op and stream a model response simultaneously; react to its completion instantly (no polling/`sleep`).
- ✅ Cancel an in-flight model stream cleanly; the log records "N tokens then cancelled"; replay reproduces it (P2-2 below).
- ✅ Delta coalescing with exact recording (P2-3 below).

### P2-2 — First-class cancellation ✅

The brain already had the cancellation *shape* (`Command::Cancel`, `Event::OpCancelled`, `Brain::on_op_cancelled` logging a `Cancelled { partial }` outcome that preserves a model op's `text_so_far`); the host already aborted the tokio task on `Cancel` and emitted `OpCancelled`. P2-2 closed the end-to-end path and hardened the reducer:

- `hugr-core` (`brain.rs`): `on_op_cancelled` now (1) **ignores a cancel confirmation for an op that already resolved** — the host aborts the task *and* emits `OpCancelled`, but the task may have queued its real terminal event (`ModelDone`) a hair before the abort; that event is folded first and removes the op, so the late `OpCancelled` must be a no-op or it would append a spurious `Cancelled` `OpEnded` and break replay (cancellation is idempotent, ARCHITECTURE §6.4); and (2) emits the terminal `Done { reason: Cancelled }` once the **last** in-flight op drains on a plain abort (`UserAbort`/ESC) with nothing to resume — previously a bare abort left the brain silently idle and the front-end (which already renders `DoneReason::Cancelled`) never saw it. The existing steer-interrupt path (`pending_resume` → fresh turn) is unchanged, and a single-op cancel while other work is still in flight does **not** force `Done` (the turn only ends when the brain is idle). No new `Command`/`Event`/`Record` variants — the cancellation contract was already in place.
- `hugr-host` (`engine.rs`): added a cloneable `EventSender` handle (`Engine::event_sender()`) for injecting events into the running loop from *outside* a turn — the realistic wiring for a Ctrl-C / signal handler sending `UserAbort` while `user_turn` is awaiting the model stream. `EventSender::abort()` is the `UserAbort` convenience. The driver loop itself was already correct (it aborts the per-op `JoinHandle` on `Command::Cancel` and confirms with `OpCancelled`); nothing else changed there.

Tests (50 total across the workspace, +6):

- `hugr-core/tests/cancellation.rs` — the headline scripted "stream N tokens then `UserAbort`" pinning the command sequence (`StartModelCall` → `Cancel` → `Done { Cancelled }`) and asserting the partial (`"Hello, wor"`) is in the log; deterministic replay (identical commands **and** identical log — partial reproduced *then* the cancel); the stale-`OpCancelled`-after-`ModelDone` race is a no-op (exactly one `Ok` `OpEnded`, no spurious `Cancelled`); and cancelling one background op mid-stream does **not** end the turn (the model op still gates it → `EndTurn`, not `Cancelled`).
- `hugr-host/tests/end_to_end.rs` — through the **real tokio engine**: `cancel_in_flight_model_stream_preserves_partial` (a model that streams two tokens then hangs forever; a `UserAbort` injected via `event_sender()` aborts the task; the turn ends `Cancelled`, the partial `"Hello, wor"` is in the durable log, and **no** consolidated `ModelOutput` was recorded); and `cancel_in_flight_background_op_cleanly` (a never-finishing background op is aborted on `UserAbort`, logged `Cancelled`, with the engine fully drained — `inflight_len() == 0`, no leaked work).

**Exit criteria:**

- ✅ Cancel an in-flight model stream cleanly (host aborts the task; partial text preserved). Background capability ops cancel cleanly too (no leaked work).
- ✅ Replay reproduces the partial output then the cancel, deterministically.
- ✅ Delta coalescing with exact recording (P2-3 below).

### P2-3 — Delta coalescing with exact recording ✅

The host coalesces high-frequency streamed deltas for the **render** while still recording exactly **one** consolidated `Record` per message — deltas are transport, never durable (ARCHITECTURE §4.4/§4.5), so replay stays bit-for-bit identical regardless of how the stream was batched. Implemented entirely host-side; `hugr-core` is untouched (no new `Command`/`Event`/`Record` variants — coalescing is invisible to the brain):

- `hugr-host` (`coalesce.rs`): a small, pure, IO-free [`Coalescer`] that buffers *consecutive same-op streamed text* (`ModelText` / `ModelReasoning`, kept separate since they render differently) and merges it into one larger `OutputEvent`. Any other event — a different op, a tool chunk, a tool start, a notice — first flushes the pending buffer (preserving order), then passes through. It takes `OutputEvent`s in and yields the `OutputEvent`s the front-end should render, so it is fully unit-testable without stdout.
- `hugr-host` (`engine.rs`): the `Engine` routes `Command::Emit` through the coalescer (`push` → render the merged result), and `flush_render`es it at every boundary where order matters — before any lifecycle hook (model/tool start, permission, done, notice; a single guard at the top of `perform` for every command except `Emit`), before a completion event in `observe` (`ModelDone`/`CapabilityDone`/`CapabilityError`, so the metric line follows its text), at the end of each turn (`drive_to_idle`), and in `session_end`. **Critically, the engine still submits *every* `ModelDelta` to the brain** (the `perform`/`observe` submit path is unchanged) — so the brain's `text_so_far` stays complete and a cancelled op's partial loses no tokens; coalescing batches only the front-end render, never the brain's event stream.

Tests (57 total across the workspace, +7):

- `hugr-host` `coalesce` unit tests — consecutive same-op text merges on flush; a non-text event flushes first (order preserved); switching op flushes the previous op; text vs reasoning never merge; empty flush is a no-op; and the headline **chunking-invariant** property (per-char vs few-chunk vs single-chunk streams all render identical text, and per-char churn collapses to one render event).
- `hugr-host/tests/end_to_end.rs::delta_coalescing_keeps_recording_exact` — through the **real tokio engine**: the same answer streamed per-character, in 5-char chunks, and as a single delta yields byte-for-byte identical *logical* records (`UserMessage`/`ModelOutput`/`ToolResult`) and exactly **one** consolidated `ModelOutput` per call (no per-delta log entries), while the per-character stream is coalesced to a single render call.

[`Coalescer`]: crates/hugr-host/src/coalesce.rs

## Phase 3 — Traces: save, replay, inspect ✅ (complete)

**Goal:** sessions are first-class artifacts (record, replay, resume).

**Phase 3 exit criterion — met (P3-3):** a real Phase 1/2 session is recorded through the engine, saved to a trace, reloaded, and replayed through a fresh brain **bit-for-bit** — the reconstructed command sequence and durable log are byte-identical to the recording (`hugr-host/tests/end_to_end.rs::record_then_replay_reconstructs_the_session_bit_for_bit`). **Resume (P3-4) closes the phase:** a saved trace can be reloaded into a fresh engine, continued with a new turn, and re-saved into a trace that still replays bit-for-bit.

### P3-1 — `hugr-replay` crate + trace format ✅

New crate `hugr-replay` owning the versioned, portable on-disk **trace** format (ARCHITECTURE §12). A trace is the saved form of a session: because the brain is a pure fold over an ordered event stream, the trace is just *that stream made durable*. P3-3 (replay) and P3-4 (resume) build on this container.

- `hugr-replay` (`src/lib.rs`): the [`Trace`] container — `{ meta, events, log, blobs }`:
  - `meta: TraceMeta` — `{ codename, format_version, created_at }`. `FORMAT_VERSION` is a single integer (currently `1`) bumped on any breaking layout change; `Trace::from_json`/`load` reject an unknown *future* version with `TraceError::UnsupportedVersion` rather than mis-parsing (forward-compat).
  - `events: Vec<hugr_core::Event>` — the ordered host→brain stream, the **input** to replay (re-feed into a fresh brain → identical commands, §6.3).
  - `log: Vec<hugr_core::LogEntry>` — the consolidated, seq-stamped durable log, the **truth** (one record per logical message/tool-result, §4.5). `BrainState` is **never** stored — always rederivable by folding `log` (§12.1).
  - `blobs: BlobManifest` — `Vec<BlobRef { hash, len, media }>`, references to content-addressed payloads (bytes live elsewhere). Empty for now; the structure is in place so the format is stable for the P3-2 blob store. Blobs are referenced, not inlined.
- **IO boundary kept out of core.** `hugr-replay` depends on `hugr-core` only as pure data (serializing its `serde`-derived types) and is the *only* place in the trace story that uses `std::fs` (`Trace::save`/`load`). `cargo tree -p hugr-core` stays free of any environmental deps — only `serde`/`serde_json`. Errors are a typed `TraceError` (`Io`/`Serde`/`UnsupportedVersion`).
- Constructors throughout (`Trace::new`/`with_blobs`, `TraceMeta::new`, `BlobRef::new`, `BlobManifest::new`/`push`); every public struct/enum is `#[non_exhaustive]` (narrow-waist, forward-compatible).
- Trace files are plain JSON (`to_json`/`from_json` are pure; `save`/`load` add the fs boundary), so a trace recorded on a server replays in a browser or a Python host — portability (§12.3).

Tests (`hugr-replay/tests/roundtrip.rs`, 5 passing; 62 total across the workspace, +5): the headline **write-then-load** round-trip persists a realistic Phase 1/2 session (user → model+tool-call → tool result → model → done, with a tick, permission decision, streamed delta, and `OpEnded`/`OpMeta` cost metadata) to disk and asserts the reconstructed `Trace` is byte-for-byte equal; an in-memory JSON round-trip; an empty-session round-trip; a blob-manifest round-trip; and a rejection of an unsupported future `format_version`.

**Trace format shape (for P3-2/P3-3/P3-4 to consume):**

```text
Trace { meta: TraceMeta, events: Vec<Event>, log: Vec<LogEntry>, blobs: BlobManifest }
TraceMeta { codename: String, format_version: u32, created_at: Option<u64> }
BlobManifest { refs: Vec<BlobRef> }
BlobRef { hash: String, len: u64, media: String }
```

[`Trace`]: crates/hugr-replay/src/lib.rs

### P3-2 — Blob store capability ✅

A content-addressed, disk-backed blob store (ARCHITECTURE §3.3) so large tool outputs / inputs are referenced by digest from the trace instead of inlined — keeping the log small and a trace shippable with or without its bytes. The store produces `BlobRef`s in the exact shape the trace's `BlobManifest` already carries (P3-1), so a large payload offloaded by digest rehydrates on load.

- `hugr-replay` ([`BlobStore`]): a disk-backed, content-addressed store rooted at a configurable directory. The key of a blob is the SHA-256 of its bytes, rendered `"sha256:<hex>"` (matching the manifest's `BlobRef.hash`). `BlobStore::put(bytes, media) -> BlobRef` writes the bytes to a file named by their hash (the `:` swapped for a filesystem-friendly `-`) and returns the ref; `get(hash) -> Vec<u8>` rehydrates them, returning `TraceError::BlobNotFound` (new variant) for an absent hash; `contains`/`root`/`hash` round it out. **Content-addressing gives dedup for free:** identical content lands on the same path, so a repeat `put` is a no-op (the file isn't rewritten). `BlobStore::hash` is pure (no IO); the writes/reads are this host-side crate's `std::fs` (never `hugr-core`). The new `sha2` workspace dep is host-side only. `BlobStore` is `#[non_exhaustive]` with a `new` constructor (narrow-waist).
- `hugr-host` (`capabilities::Blob`): wraps a `BlobStore` as an **ordinary `Capability`** named `blob` — no privileged built-in, registered exactly like `shell`/`fs`/`http`. Args/results are kept **opaque `Value`** (ARCHITECTURE §2.4): `{ "op": "put", "content", "media"? }` → `{ "hash", "len", "media" }`, and `{ "op": "get", "hash" }` → `{ "hash", "content" }`. Like `fs_read` it is read-only/idempotent so it does not gate on a permission round-trip. Constructors `Blob::new(root)` / `Blob::with_store(store)` (share one store between the capability and trace persistence) / `store()` accessor. A bad `op`, a missing arg, an absent hash, or non-UTF-8 bytes are returned as **semantic errors** (`Err(Value)`) the model can react to — never transport failures (ARCHITECTURE §5.4). `hugr-host` gained a `hugr-replay` dependency for the store.

Tests (72 total across the workspace, +10):

- `hugr-replay` `blob` unit tests — put/get round-trip of a 1 MiB payload (rehydrated bytes equal the original); same-content dedup (same hash, exactly one file on disk; different content → different hash); the hash matches the known `SHA-256("abc")` constant and is stable; a missing blob is `BlobNotFound` and `contains` is `false`.
- `hugr-replay/tests/blob_store.rs` — the **manifest integration**: a ~500 KiB payload offloaded to the store, referenced by a single `BlobRef` in a `Trace`'s `BlobManifest`; the trace JSON is an order of magnitude smaller than the payload (referenced, not inlined); round-tripping the trace and rehydrating from the manifest's hash yields the original bytes; plus a large-payload dedup check.
- `hugr-host` `capabilities::blob` unit tests — through the real `Capability::invoke`: a 200 KB put/get round-trip (and the stored ref is reachable from `store().contains`); same content → same hash; a missing-hash `get` and an unknown `op` are semantic `Err`s.

**Trace integration (for P3-3/P3-4 to consume):** the recorder offloads a large tool result with `BlobStore::put`, pushes the returned `BlobRef` into the `Trace`'s `BlobManifest`, and stores the small ref in place of the bytes; replay/resume rehydrate the bytes with `BlobStore::get(ref.hash)`. The capability and the persistence layer share one `BlobStore` (via `Blob::with_store`) so they agree on the store root and hashes.

[`BlobStore`]: crates/hugr-replay/src/blob.rs

### P3-3 — `hugr replay <trace>` + inspector ✅

Replay is the whole point of the sans-IO design: because the brain is a pure fold over an ordered event stream, re-feeding a trace's recorded `Event`s into a *fresh* `Brain` reproduces every `Command` it ever emitted — bit-for-bit, with no IO (ARCHITECTURE §6.3). The recorded `log` is the *truth* a replay is checked against; `BrainState` is never stored, only rederived (§12.1). Implemented host-side; `hugr-core` is untouched.

- `hugr-replay` (`src/replay.rs`): [`replay`]`(trace) -> Replay { commands, log }` re-feeds the events into a fresh brain (mirroring the host driver loop, zero IO) and returns the reconstructed command sequence + folded log; [`verify`] does that and asserts the reconstructed log equals the recorded log (`TraceError::ReplayMismatch` otherwise) — the Phase 3 exit criterion. Because the brain *branches* on some of the policy's pure decisions (`needs_permission`, `is_background`), faithful reconstruction needs the *same* policy: `StaticPolicy` is now `Serialize`/`Deserialize`, the trace gained an opaque `policy: Option<Value>` field (`Trace::with_policy`), and `replay`/`verify`/`Inspector` decode it (`replay_with_policy`/`verify_with_policy` accept a custom one; fall back to the default when absent/undecodable). [`Inspector`] wraps the same reconstruction as a step-through debugger: `step()` feeds the next recorded event and reports the commands it produced + the log tail it appended; `run()` collects every `Step`. All public types are `#[non_exhaustive]` with constructors.
- `hugr-host` (`engine.rs`): an opt-in `Recorder` (`EngineBuilder::record(true)`) captures the exact ordered `Event` stream at the single `submit` chokepoint (including the injected `Tick`s; the first tick seeds the trace's `created_at`), and serializes the brain's `StaticPolicy` once at build time so the trace carries it. `Engine::trace()` builds a `Trace` on demand (captured events + the brain's current durable log + policy); `Engine::save_trace(path)` writes it (clear error if recording was off). A non-recording engine pays nothing. The trace + replay surface is re-exported from `hugr-host` (`Trace`, `Inspector`, `Replay`, `Step`, `TraceError`, and `hugr_replay` itself) so an embedder needs one crate.
- `hugr-cli`: `hugr --record <path>` records a live one-shot/interactive session to a trace (banner shows `· recording`); `hugr replay <trace>` loads a trace, reconstructs the session through a fresh brain, and `verify`s it bit-for-bit against the recorded log; `hugr replay <trace> --step` first walks the session one event at a time via the `Inspector`, printing each event with the command(s) and log entry(ies) it produced.

Tests (81 total across the workspace, +9): `hugr-replay/tests/replay.rs` — replay reconstructs a hand-built Phase 1/2 trace's commands + log; `verify` passes on a faithful trace and returns `ReplayMismatch` on a tampered log; a trace round-trips through disk and still replays bit-for-bit; the `Inspector` yields one step per event (`run()` collects them all) and its appended log tails reassemble the full log; an empty trace replays to nothing. `hugr-host/tests/end_to_end.rs::record_then_replay_reconstructs_the_session_bit_for_bit` — the exit criterion through the **real engine**: record a shell-tool session → save to disk → reload → replay through a fresh brain → reconstructed command sequence + log byte-identical to the live log, a second replay yields identical commands, and the inspector reassembles the same log step by step; `::engine_without_recording_has_no_trace` — a non-recording engine has no trace and `save_trace` errors cleanly.

### P3-4 — CLI resume from a trace ✅

Resume is replay turned into a starting point: because the brain is a pure fold over an ordered event stream, *resuming* a session = rebuild the brain by re-feeding the saved trace's events into a fresh brain (with **zero IO** — the host does **not** re-run the recorded model/shell/http calls; it only re-folds the events to reconstruct `BrainState`), then continue feeding NEW live events (a new user turn) while still recording, so the grown session can be saved again. Reuses the existing `replay`/`Recorder`/`Trace` machinery; `hugr-core` is untouched.

- `hugr-replay` (`src/replay.rs`): `policy_from_trace(&Trace) -> Box<dyn TurnPolicy>` is now public — it decodes the trace's captured `StaticPolicy` (or the default if absent/undecodable). Both replay and resume run the continued brain under it, so the session branches identically.
- `hugr-host` (`engine.rs`): `EngineBuilder::resume(trace)` builds an engine whose brain is **pre-seeded** from the trace. At `build()` time it restores the recorded policy (`policy_from_trace`), re-feeds the trace's recorded events into the fresh brain draining (and discarding) the commands they re-emit (no IO — exactly like `hugr_replay::replay`), and **pre-loads the `Recorder`** with those same events (carrying the original `created_at`), so any new live turns append after them and a later `save_trace` writes the full history (old + new). `resume` implies recording. The trace's opaque `policy` value is carried through verbatim, so re-saving round-trips it bit-for-bit. New events get fresh injected `Tick`s as usual; the seeded events are never double-counted.
- `hugr-cli`: `hugr resume <trace> [prompt...]` — load a trace, rebuild the brain from it (no IO), restore the policy, then continue with a new one-shot turn or an interactive loop. The grown session is written back to `<trace>` by default (so it accumulates), or to `--record <path>` to leave the original untouched. `--yolo` / `-y` and `-m`/`--model` mirror the live-session flags. The banner shows what is being resumed and where it will be saved.

Tests (82 total across the workspace, +1 end-to-end resume test over P3-3, plus a new public `policy_from_trace` export): `hugr-host/tests/end_to_end.rs::resume_from_trace_continues_the_session` — record a shell-tool session through the **real engine** → save → resume into a fresh engine and assert the brain reconstructs the original log *before* any new turn (with nothing in flight, and the new mock model un-invoked, proving the seed performed no IO) → add a NEW user turn → assert the grown log contains the original logical records as a prefix **and** the new turn's records → re-save and assert the grown trace appends new events after the recorded ones, its log equals the live grown log, its policy survived the round-trip, and the whole grown session still `verify()`s bit-for-bit through a fresh brain.

## Phase 4 — Portability: the Chrome extension ✅ (Python host still deferred)

**Goal:** the *same* sans-IO brain running in a second, radically different environment — a browser, with **no backend** (ROADMAP Phase 4). This lands the **Chrome extension** leg of the portability story; the `hugr-py` (PyO3) leg remains deferred, and the WASM *plugin* transport (Phase 5's `WasmPlugin` scaffold) is still a stub.

**Exit criterion — met (browser leg):** the identical `hugr-core` brain — compiled to WebAssembly, byte-for-byte the same reducer as the CLI — drives a real, installable Chrome side-panel agent that reads pages and navigates tabs, with **no server**. Verified by running the shipped WASM artifact + JS glue through a full scripted turn loop in Node (`user → model → read tool → model resume → Done`, plus the permission round-trip for a navigation tool) and by native unit tests over the binding's JSON boundary.

Done:

- `hugr-wasm` — the browser/JS binding (ARCHITECTURE §10). A `cdylib` wrapping `hugr_core::Brain` with `wasm-bindgen`, exposed as `HugrBrain` with `submit(eventJson)` / `poll() -> commandsJson` / `inflightLen()` / `logJson()` and a `version()`. The boundary is **JSON in, JSON out** — every `Event`/`Command` is already `serde`-serializable, so there is *zero* hand-written type marshalling (the narrow waist pays off again, §2.4). The pure logic lives in a native-testable inner `Core` (JSON strings, `String` errors); the `#[wasm_bindgen]` wrapper only adds JS error conversion (its string intrinsics abort off-wasm, so `cargo test` exercises `Core`, and the *shipped* artifact is exercised in Node). `hugr-core` stays sans-IO — `hugr-wasm` depends on it as pure data + adds `wasm-bindgen` host-side only (`cargo tree -p hugr-core` unchanged: only `serde`/`serde_json`). The release build is **236 KB** of `.wasm` (well under the §11 "< 2 MB" target), before optional `wasm-opt`.
- The **Chrome extension** (`crates/hugr-wasm/extension/`) — a Manifest V3, installable-unpacked side-panel agent that is the browser *host* (the analogue of `hugr-host` + `hugr-cli`), with `hugr-core` as the identical brain:
  - **Driver loop** (`host/engine.js`) — mirrors `engine.rs`: drain `poll()`, spawn one async task per op, merge all events into one ordered inbox, `submit()` (stamping a `Tick` first — the brain never reads a clock), repeat until nothing is in flight. Handles `StartModelCall`/`StartCapability`/`RequestPermission`/`AskUser`/`Cancel`/`Emit`/`Done`, first-class cancellation (an `AbortController` per op; a **Stop** button injects `UserAbort`), and the permission round-trip (auto-approve toggle = the CLI's `-y`). The browser host also records the exact submitted `Tick`+event stream so the side panel can export a JSONL trace envelope alongside the folded durable log.
  - **Model adapter** (`host/model.js`) — the exact analogue of `openai.rs` but `fetch` + streamed `ReadableStream` SSE: builds the chat-completions body from the canonical `ModelRequest`, streams text deltas live, assembles tool calls (with the same stable-id synthesis), and returns the consolidated `ModelOutput` + `Usage` (including router cost) in serde shape. An MV3 page with host permissions fetches the endpoint cross-origin, so there is **no backend of our own** — the Phase 4 "no server" property.
  - **Capabilities** (`host/tools.js`, `host/schemas.js`) — ordinary tools over `chrome.tabs` / `chrome.scripting`, **read + navigate only** (no click/type/form-submit by design): `list_tabs`, `get_current_page`, `get_page_text`, `get_page_links`, `get_page_outline`, `get_interactive_elements` (read-only, no permission), plus permission-gated `open_tab`, `navigate_tab`, `activate_tab`, `close_tab`, plus the agent-UX tools `ask_user_confirmation` and `show_plan`. Semantic errors (e.g. a privileged `chrome://` page that can't be injected) route back to the model as tool results (§5.4).
  - **Front-end** (`sidepanel.js` + `styles.css`) — a DOM renderer of the brain's `OutputEvent`s: streamed assistant text with dependency-free Markdown rendering (headings, lists, quotes, code blocks, links, tables, emphasis), reasoning, tool cards with collapsible results, plan cards, and interactive permission/confirmation cards; per-call token/cost metrics; header actions for starting a fresh in-panel chat and downloading the current trace as JSONL. Settings (`options.html`) persist the API key/base URL/model/temperature/auto-approve in `chrome.storage.local` (a browser has no env vars or token files, unlike `OpenAiAdapter::from_env`).
  - **Build** (`build-extension.sh`) — compiles `hugr-wasm` to `wasm32-unknown-unknown` and runs `wasm-bindgen --target web` into `extension/wasm/` (committed, so the extension loads with no build step); MV3's `content_security_policy` grants `'wasm-unsafe-eval'` so the module instantiates.
  - Docs: `extension/README.md` (install + an architecture-mapping table) and `extension/DEMOS.md` (eight lightweight demos — summarize a page, triage tabs, navigate-with-permission, multi-tab research, plan-then-confirm, describe-read-only, interrupt a turn, "same brain, prove it").

Tests (104 total across the workspace, +4): `hugr-wasm` unit tests over the native-testable `Core` — a user turn drives a `StartModelCall`, the log holds the `UserMessage`, the default policy constructs idle, and invalid event/policy JSON are clean errors. Plus out-of-band validation of the *shipped* artifact (WASM + generated glue) in Node: a full turn loop (`user → model → list_tabs → model resume → Done{EndTurn}`, 12 tools advertised) and the navigation permission round-trip (`navigate_tab → RequestPermission → Deny → model resumes`).

Deferred (still open for a future Phase 4 pass): `hugr-py` (PyO3) host, the WASM *plugin* transport backend (wasmtime), size/cold-start benchmarking against §11, and browser-side trace persistence/resume (the side panel can export JSONL with events + log, but re-seeding a brain from a saved browser trace is not yet wired).

## Phase 6 — Sub-agents & forks ✅ (built before Phase 4, by request)

**Goal:** cheap, portable sub-agents built on log forking — a sub-agent is *not* a special subsystem, it is **another `hugr-core` instance** (ARCHITECTURE §13).

**Exit criterion — met:** a parent agent fans out to N child agents (fork-shared context), collects their results, and the whole tree replays deterministically from one recorded trace (`hugr-host/tests/end_to_end.rs::parent_fans_out_to_sub_agents_and_replays`).

Done:

- `hugr-core` — sub-agents as an op, forks as a log-prefix copy, all as *strategy*, not reducer hardcoding:
  - `Command::StartAgent { op, config, seed }` — the brain emits this (instead of `StartCapability`) when the policy designates a tool as a sub-agent spawner. `config` is the opaque tool-call args (the host interprets the child's prompt/model/tools); `seed` is the **forked log prefix** the child starts from.
  - `AgentSeed` (`Fresh` / `ForkAt { seq }` / `ForkFull`) + `TurnPolicy::agent_seed(capability) -> Option<AgentSeed>` (default `None`; mirrors `is_background`). `StaticPolicy` gained `with_agent`/`with_agents` (and a `#[serde(default)]` field so pre-Phase-6 traces still decode). The reducer's `begin_tool_call` checks `agent_seed` first; `resolve_seed` turns the strategy into the actual prefix (pure — the brain owns the log).
  - `OpKind::Agent { name, call_id }` now carries the correlation ids (so the child's result is a provider-correct tool result); it already `blocks_turn()`, so a fan-out of children joins before the model resumes (§6.3). `on_agent_done`/`on_agent_error` (previously stubs) now fold the child's digest back like any tool result.
  - `Brain::from_log` / `BrainState::from_log` — the **fork/seed primitive** (§14): re-derive a brain's state (log, `next_seq`, `next_op`, clock) by folding an inherited log prefix, with zero IO. `Record::op_id()` supports reconstructing the next op id so a child's new ops don't collide with the inherited prefix.
- `hugr-host` — running children in-process (§13.2):
  - `agent.rs` (`run_agent`) — drives a child brain to completion on a spawned task, reusing (a subset of) the parent's model + capability registries. It returns a **boxed** future so a child can itself spawn children (nested agents). The child's ops live in a `JoinSet` that aborts them all on drop, so a parent `Cancel` tears down the whole subtree cleanly. The child's config (`prompt`, optional `model`/`system`/`tools` allowlist) is the opaque args; its digest (last answer text + aggregated token usage) flows back as `AgentDone`, and streamed child text is forwarded to the parent as cosmetic `CapabilityChunk`s.
  - `Engine` gained the `StartAgent` arm (spawns `run_agent`, tracked in `tasks` for cancellation) and observes `AgentDone`/`AgentError` for the front-end (rendered like a tool completing). Registries are now `Clone` (cheap `Arc` clones); `CapabilityRegistry::subset` narrows a child's tools to an allowlist. `TurnPolicy` gained a `Send + Sync` bound so the host can own a child brain on a worker task (still single-threaded per brain).
  - `EngineBuilder::agent(schema, seed)` advertises a sub-agent tool to the model and registers its seed strategy. The **CLI** ships a built-in `task` sub-agent tool (`ForkFull`) so the model can delegate self-contained work live, plus inspector rendering for `StartAgent`/`AgentDone`/`AgentError`.

Tests (+6): `hugr-core/tests/sub_agents.rs` — model delegates to a sub-agent and the result folds back; `ForkFull`/`ForkAt`/`Fresh` seed the child correctly; a two-child fan-out joins once and replays deterministically (identical commands **and** log). `hugr-host/tests/end_to_end.rs::parent_fans_out_to_sub_agents_and_replays` — through the **real engine**: a parent spawns two children (each its own brain, reusing the model registry), both digests fold back as `task` tool results, the turn ends once, and the recorded parent trace `verify()`s bit-for-bit (the recorded `AgentDone`s drive the fold — children are not re-run, §13.3).

## Phase 5 — Extensibility (plugins) ✅ (built before Phase 4, by request)

**Goal:** third parties add tools without recompiling the core (ARCHITECTURE §8).

**Exit criteria — met:** a third-party plugin (a separate crate/binary, no core recompile) adds a working tool the agent can call, and it cannot touch core internals; the contract is versioned and documented (`hugr-example-plugin` + its `tests/e2e.rs`).

Done:

- `hugr-plugin-abi` — the versioned, narrow, transport-agnostic plugin contract:
  - `protocol.rs` — three verbs as tagged JSON: `Request::{Describe, Invoke, OnEvent}` and `Response::{Description, Chunk, Result, Error}`, an integer `PROTOCOL_VERSION` (a plugin reporting a newer one is rejected on load), all payloads opaque `Value` (adding a tool/arg touches zero core types, §2.4). `on_event` is defined but reserved (the host doesn't yet deliver it — narrow now, widen later). Wire shape pinned by unit tests.
  - `transport.rs` — `PluginTransport` (the single trait the host depends on): `describe() -> [ToolSchema]` and `invoke(name, args, sink) -> Result<Value, Value>` (semantic ok/err both route back to the model, §5.4). `PluginSink` bridges streamed chunks without coupling to the host's own sink; `PluginError` is the typed load/transport error.
  - `subprocess.rs` — `SubprocessPlugin`: a plugin is an external program; each request spawns a fresh process, writes one JSON request, reads chunk lines then a terminal result/error. Stateless and naturally concurrent (no shared pipe to multiplex). Language-agnostic, process-sandboxed, needs no Hugr dependency.
  - `wasm.rs` (behind the `wasm` feature) — `WasmPlugin`, a scaffold implementing the *same* `PluginTransport` trait so the roadmap's **primary** WASM component-model transport drops in with no host changes; its wasmtime backend lands with Phase 4. Every call currently reports "not yet implemented". This is the **both** choice: subprocess is the working default, WASM is scaffolded behind the trait+feature.
  - Host-side IO crate: uses `hugr-core` only as pure data, so `cargo tree -p hugr-core` stays free of any environmental deps.
- `hugr-host` — plugins as ordinary capabilities:
  - `plugins.rs` (`PluginCapability`) wraps one plugin tool as a `Capability` (no privileged built-ins, no privileged plugins); `invoke` bridges the host `ChunkSink` to the plugin `PluginSink` so streamed chunks reach the brain. `load(transport)` / `load_subprocess(program, args)` describe a plugin and return its tools as capabilities to register. The plugin ABI is re-exported from `hugr-host` so an embedder needs one crate. `ChunkSink` is now `Clone` (op id + `Arc` sender).
  - The **CLI** gained `--plugin <CMD>` (repeatable): load a subprocess plugin's tools live.
- `hugr-example-plugin` — an example **third-party** plugin: a standalone binary depending on **nothing** from Hugr (only `serde_json`), providing `uppercase`/`reverse` tools over the stdio protocol. Proof that a plugin needs no core recompile and cannot reach core internals.

Tests (+7): `hugr-plugin-abi` protocol round-trip + wire-shape + hand-written-JSON decode unit tests; `hugr-example-plugin/tests/e2e.rs` — the subprocess transport `describe`s + `invoke`s the real plugin process (streamed chunk forwarded, unknown tool is a semantic `Err`), and the agent calls the `uppercase` plugin tool **end-to-end through the real engine** with the result folded into the durable log; a standalone-binary sanity check.

## Phase 7 — Durable resume & scheduling (cron) ✅

**Goal:** survive crashes; fire prompts on a schedule (ARCHITECTURE §15), without pulling any IO into `hugr-core`.

Done:

- Durable checkpoints in the native host: `EngineBuilder::checkpoint(path, cadence)` implies recording and writes the current trace atomically during the run. `CheckpointCadence` is explicit: `OnCommand` saves when the brain emits `Command::Checkpoint`, `EveryEvent` saves after each host event submitted to the brain (the crash-safe mode that captures “op N is in flight” mid-turn), and `EveryNEvents(n)` trades write frequency for durability. `Trace::save_atomic` writes a sibling temp file and renames it into place, creating parent directories when needed.
- Resume-after-crash reconciliation: `EngineBuilder::resume(trace)` still re-folds the recorded event stream with zero IO, and now, if that fold reconstructs stale in-flight ops from a killed process, applies `CrashResumePolicy::CancelInflight` by appending recorded `OpCancelled` events under fresh injected `Tick`s before going live. That records the policy choice in the trace itself, so the grown trace remains replayable bit-for-bit. Idempotent re-issue is left as a future host policy; cancel is the conservative default.
- Compaction policy made explicit: `TraceCompaction::PreserveFull` is the native default and only Phase 7 policy. It deliberately keeps the full event stream plus consolidated log because the log is the source of truth; destructive log compaction would break replay/resume. This lands the checkpoint policy surface without changing the core or losing history.
- Host-side scheduler: new `hugr_host::scheduler` surface (`CronExpr`, `Schedule`, `TriggerTarget`, `fire_once`) parses a minimal cron cadence (`@every 10s`, `@every 5m`, `* * * * *`, `*/N * * * *`) and fires a prompt into one of the three roadmap targets: `ResumeExisting { trace }`, `NamedPersistent { dir, name }`, or `FreshSession { trace }`. A fire is exactly the architecture story: optionally load a trace, build/resume an engine, inject one `UserInput`, run the driver loop, and checkpoint the trace.
- CLI scheduling: `hugr schedule --cron ... --trace|--session|--fresh ... [prompt...]` runs the same host scheduler. `--once` performs one fire and exits; without it, the CLI sleeps for the parsed cadence and fires repeatedly. Live `--record` and `hugr resume` now use durable `EveryEvent` checkpoints, so a killed process leaves a usable trace behind.

Tests (+2 end-to-end Phase 7 tests): `hugr-host/tests/end_to_end.rs::durable_checkpoint_resumes_after_mid_turn_crash` starts a model call that streams a partial delta and then hangs, waits for an `EveryEvent` checkpoint, aborts the engine task to simulate a process kill, loads the checkpoint into a fresh engine, asserts stale in-flight work is recorded as `Cancelled`, continues with a new turn, saves the grown trace, and `verify()`s it bit-for-bit. `::scheduled_trigger_fires_into_named_persistent_session` fires the same `Schedule` twice into a named persistent session; the second fire resumes the existing trace, appends a second user message, and the final trace verifies.

**Exit criteria — met:**

- ✅ Kill the process mid-turn; resume and continue correctly from the trace.
- ✅ A scheduled trigger fires a prompt into a session on a cron cadence.

[`replay`]: crates/hugr-replay/src/replay.rs
[`verify`]: crates/hugr-replay/src/replay.rs
[`Inspector`]: crates/hugr-replay/src/replay.rs

[`Engine`]: crates/hugr-host/src/engine.rs
[`Capability`]: crates/hugr-host/src/capability.rs
[`ModelAdapter`]: crates/hugr-host/src/model.rs
[`Policy`]: crates/hugr-host/src/policy.rs
[`Frontend`]: crates/hugr-host/src/frontend.rs
