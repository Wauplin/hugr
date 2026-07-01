# Roadmap 2

> Companion to `ROADMAP.md`, `DESIGN.md`, and `ARCHITECTURE.md`. The post-foundation roadmap: stop spreading into Python/Node bindings and provider breadth; double down on the **Rust CLI** (a serious coding agent) and the **Chrome extension** (a general-purpose web agent) over one smarter pure brain. This document is meant to be *actionable*: every task is small, tagged by domain, and carries its own exit criterion, so work can be picked up and parallelised.

## How to read this roadmap

- Phases are ordered by dependency, not by importance. Within a phase, tasks are independent unless a dependency is noted.
- Every task has an id (e.g. `A3`), one or more **domain tags**, a one-line deliverable, and an **Exit** criterion you can check.
- **Parallelism rule:** two tasks whose tag sets are *disjoint* touch different code and can be built by two agents concurrently. Tasks that share a tag touch a common crate/module and must be serialised or coordinated. Example: `D9 [CLI]` (terminal appearance) and `E7 [Browser]` (side-panel UI) share no tags → parallel.

### Domain tags

- `[Brain]` — `hugr-core` only. Pure, sans-IO, single-threaded. Never grows an environmental dependency. The hottest shared crate — expect many tasks to touch it, so it is a natural serialisation point.
- `[Engine]` — `hugr-host` native runtime: the tokio driver, capability/model registries, host `Policy`, scheduler, checkpointing. Also shared/hot.
- `[Providers]` — `hugr-providers`: the Hugging Face router adapter.
- `[Replay]` — `hugr-replay`: trace format, blob store, save/load/replay/resume.
- `[CLI]` — `hugr-cli` binary **and** its terminal front-end (`hugr-host`'s `StdoutFrontend` / a future TUI). CLI-side UI lives here.
- `[Browser]` — `hugr-wasm` + the Chrome extension (`extension/`): the browser host JS **and** the side-panel UI. Browser-side UI lives here.
- `[Docs]` — `docs/`, `PROGRESS.md`, `README`s.

There is intentionally **no standalone `UI` tag**: presentation work belongs to the host that owns it (`[CLI]` or `[Browser]`), because that is where the merge-conflict domain actually is. That is also what makes "polish the CLI" and "polish the extension" trivially parallel.

## Locked-in product decisions

These narrow the design deliberately. The architecture doc keeps the general mechanisms; here we commit to the subset we actually build now.

- **Three model tiers, nothing else.** Logical selectors are exactly `small`, `medium`, `big`. No `summarizer`/`vision`/`browser`/`coder`/`router` aliases. Compaction and the permission judge simply reuse `small`. (The `ModelSelector` type stays open per the narrow waist, but the product ships three.)
- **Text-to-text only.** No audio, no vision, no image parts in the near term. Drop multimodal considerations from every phase.
- **Two permission modes only, never "ask the user".**
  - **yolo** — allow everything (today's `AllowAll` / `-y`).
  - **auto-approve** (the default) — the host consults the `small` model on each gated action ("is this safe given these args? yes/no + reason"); *yes* ⇒ Allow, *no* ⇒ Deny with the reason routed back to the model so it can adapt. There is **no interactive prompt** — it is annoying and blocks headless/scheduled runs.
  - The host `Policy` trait stays flexible so richer policies (sandboxes, profiles, approval memory) can return later — but none of that is in scope now.
- **No Python/Node bindings; no multi-provider push.** Keep the HF router path. Bindings and extra providers stay in the backlog.
- **Core principles are non-negotiable** even though the prototype architecture is otherwise free to change: `hugr-core` stays sans-IO/pure/single-threaded; all nondeterminism is injected as events; the log is the source of truth and `BrainState` is a fold over it; deltas are transport-only and never durable; the narrow waist holds (type only what the brain branches on); streaming is the only model mode.

## Deviations from `ARCHITECTURE.md` (intentional)

`ARCHITECTURE.md` remains the rationale doc and still describes the general mechanisms. Where this roadmap narrows them, the narrowing wins for now:

- §5.3 lists example selectors (`fast`/`vision`/`summarizer`/`router`). We ship only `small`/`medium`/`big` and route compaction + the permission judge through `small`.
- §7.2 shows an interactive `Ask` policy. We do **not** build the interactive path; `Ask` is replaced by the auto-approve judge.
- Multimodal `ContentPart::Image` (§5.1) is out of scope; projection stays text-only.

---

## Phase 0 — Foundations (unblocks everything)

**Goal.** The three primitives the rest of the roadmap leans on: tiers, host-side token accounting, and the two-mode permission model. Small, mechanical, high-leverage.

**Status.** Implemented in the native CLI/host and Chrome extension; see `PROGRESS.md` → "Roadmap 2 Phase 0 — Foundations" for the exact shipped behavior and verification.

- **0.1** `[Engine][Providers]` — Model registry maps `small`/`medium`/`big` → HF router model ids + per-tier knobs (temperature, max tokens), from one config file/section. All three may point at the same model initially. **Exit:** one config file wires all three tiers; no core change needed to remap a tier.
- **0.2** `[Brain]` — `StaticPolicy` default tier becomes `medium` (was `big`); `choose_model` still returns a single configured tier for now (routing is Phase B). **Exit:** a fresh session defaults to `medium`; `choose_model` is the only place a tier is decided.
- **0.3** `[Engine][Brain]` — Host-side **token accounting at ingestion** (ARCHITECTURE §3.5): when a `ModelOutput`/`ToolResult` enters the log, the host annotates its record with an approximate token count; the brain stores it on the record and never tokenises. **Exit:** every durable content record carries an `est_tokens`; replaying a trace reuses the recorded counts (the host never re-tokenises on replay).
- **0.4** `[Engine][Providers]` — **Auto-approve judge** `Policy`: on a gated `RequestPermission`, the host makes a `small`-tier model call classifying the action as safe/unsafe and returns `Allow` / `Deny{reason}`. Read-only capabilities skip the judge. **Exit:** a risky shell command is denied with a reason the model sees; a benign one is allowed; no user prompt appears.
- **0.5** `[Brain][Engine]` — Confirm the permission flow stays replay-deterministic: the judge's verdict is captured as the `PermissionDecision` event in the log, so replay re-feeds the recorded decision and never re-runs the judge. **Exit:** a recorded auto-approve session `verify()`s bit-for-bit with the judge model absent.
- **0.6** `[CLI]` — `--yolo` (allow-all) and default auto-approve; banner shows the active mode + the tier→model mapping. Remove the interactive-prompt code path from the CLI wiring (leave the trait). **Exit:** `hugr` runs headless with no prompt; `--yolo` skips the judge.
- **0.7** `[Browser]` — Settings map the three tiers to model ids; the existing auto-approve toggle selects yolo vs judge. **Exit:** the side panel runs either mode with no permission popup.

---

## Phase A — Context kernel & lossless compaction

**Goal.** Make context management a first-class brain capability while keeping projection pure and the log authoritative. Depends on `0.3` (token counts).

- **A1** `[Brain]` — ✅ Implemented. `TurnPolicy::project_context` takes a token budget and returns a `ContextPlan` (included / referenced / summarised / omitted blocks, per-block reasons, budget totals, cache hints) rather than a bare `ModelRequest`; the reducer derives the `ModelRequest` from the plan. **Exit:** `project_context(log, budget) -> ContextPlan`; the plan explains every block's disposition; projection stays pure and synchronous.
- **A2** `[Brain]` — ✅ Implemented. Add durable **summary records** referencing exact log spans (`summary_of: SeqRange`, `coverage`, `tier`, `est_tokens_in/out`). Summaries are appended; original entries are never deleted, only evicted-to-reference by later projections. **Exit:** a summary record round-trips through the log and a later projection evicts the covered span to references.
- **A3** `[Brain]` — ✅ Implemented. Implement the **compaction sub-loop** (ARCHITECTURE §3.4): when a projection crosses a high-water mark, emit a `small`-tier `StartModelCall` over the span to compact, append its consolidated summary, then re-project. **Exit:** a long scripted trace compacts automatically; replay reproduces it from log + recorded token metadata *without* re-running the summariser.
- **A4** `[Brain]` — ✅ Implemented. Manual compaction trigger reduced to an injected event/command the hosts can fire. **Exit:** a single event triggers one compaction pass deterministically.
- **A5** `[CLI]` — ✅ Implemented. `/context` (inspect the current `ContextPlan`: budget used, retained turns, summaries, large refs, omission reasons) and `/compact` (fire A4). **Exit:** `/context` output matches the `ModelRequest` actually sent; `/compact` shrinks the next request without mutating prior records.
- **A6** `[Browser]` — ✅ Implemented. Context drawer (same info as `/context`) and a compact button (fires A4). **Exit:** the drawer reflects the real projection; the button compacts.

---

## Phase B — Tier routing

**Goal.** Spend model quality deliberately: `small` for cheap/fast steps, `medium` for normal interaction, `big` for hard reasoning and final coding decisions. Depends on `0.1`/`0.2`; benefits from A (context-pressure signal).

- **B1** `[Brain]` — ✅ Implemented. Decide the routing inputs and, if needed, widen `choose_model`'s signature so it can see phase, tool risk, context pressure, and recent failures (all derivable from `BrainState`/log — no IO). **Exit:** `choose_model` is pure and has the inputs it needs; documented.
- **B2** `[Brain]` — ✅ Implemented. Implement a routing policy (a real `TurnPolicy` beyond `StaticPolicy`): escalate to `big` on failed edits, repeated test failures, or ambiguous repo-wide changes; downgrade to `small` for session/title naming, summarisation, quick classification, and the permission judge; `medium` otherwise. **Exit:** a scripted session proves deterministic routing across all three tiers and replays identically (routing keys off *recorded* token estimates, never re-tokenises).
- **B3** `[Brain][Engine]` — ✅ Implemented. Record the chosen selector + per-op tokens/cost/latency in `OpMeta` so the trace answers "why was this expensive?". **Exit:** per-tier spend and escalation reasons are queryable from the trace alone.
- **B4** `[CLI]` — ✅ Implemented. `/model`, `/tier`, `/status` show active tier defaults, per-tier spend, context budget, and recent escalation reasons; a manual per-turn tier override. **Exit:** status output reflects live routing; an override forces a tier for the next turn.
- **B5** `[Browser]` — ✅ Implemented. Tier chips per response ("used small/medium/big") and a manual tier override. **Exit:** each response shows the tier it used; override works.

---

## Phase C — Skills & MCP (high priority)

**Goal.** Two of the most-requested harness features, added early. Both reuse the existing uniform capability model, so they touch the host, not the brain's contract. Largely independent of A/B → parallelisable.

- **C1** `[Engine]` — ✅ Implemented. **MCP client**: connect to MCP servers (stdio first, reusing the Phase-5 subprocess pattern) and expose each server's tools as ordinary `Capability`s in the registry. Semantic errors route back as tool results; transport errors stay host-side. **Exit:** an external MCP server's tool is called end-to-end through the real engine with zero core changes.
- **C2** `[CLI]` — ✅ Implemented. Configure MCP servers (a config file + `--mcp <cmd>` flag) and list connected servers/tools in `/status`. **Exit:** `hugr` loads an MCP server from config; its tools appear to the model.
- **C3** `[Browser]` — ✅ Implemented. MCP over a browser-compatible transport where feasible (or document the limitation); configure servers in settings. **Exit:** at least one MCP server's tools are usable from the side panel, or the constraint is documented with a fallback.
- **C4** `[Engine]` — ✅ Implemented. **Skills loader**: discover skill bundles (a `SKILL.md` + optional scripts/tools) from well-known locations; a skill contributes on-demand instructions and optionally registers capabilities. **Exit:** a skill bundle on disk is discovered and its metadata is available to the host.
- **C5** `[Brain]` — ✅ Implemented. Skills are exposed to the model as lightweight, model-invocable descriptors; when invoked, the skill's instructions are projected into context (durably referenced, not silently inlined). Skill selection is a `TurnPolicy` decision, not reducer-hardcoded. **Exit:** the model can "invoke" a skill and the skill's instructions appear in the next projection; the choice is replay-deterministic.
- **C6** `[CLI][Browser]` — ✅ Implemented. Surface available skills (a `/skills` command; a browser skill list) and show which skill is active. **Exit:** users can list skills and see the active one in both hosts.

---

## Phase D — Rust CLI as a serious coding agent

**Goal.** Turn the CLI into a tool a Rust developer keeps open all day. Depends on Phase 0 (permissions), A (context), and some of B. UI tasks (`D9`) are independent of the rest.

- **D1** `[Engine]` — ✅ Implemented. Repo-orientation capabilities: fast file listing, `rg` symbol/text search, targeted file read, `git status/diff/log`, package metadata. Ordinary host capabilities; the brain sees only schemas + opaque results. **Exit:** the model can orient in an unfamiliar Rust repo using these tools.
- **D2** `[Brain][Engine]` — ✅ Implemented. **Finish the stale-edit CAS** (ARCHITECTURE §7.3): the types (`VersionRef`, `Conflict` routing) already exist; wire the brain to stamp `expected_version` from its version table (the TODO at `brain.rs`), and add the schema-declared object-key metadata so the host performs an atomic compare-and-swap. **Exit:** an edit against a stale file returns `Conflict`, the model re-reads and retries, and no file is corrupted.
- **D3** `[Engine]` — Robust edit path: patch application with preview, applied/reverted distinction, conflict results routed back to the model. **Exit:** a patch previews, applies, and can be reverted; conflicts surface as tool results.
- **D4** `[Brain][CLI]` — **Plan mode**: the model proposes a short plan; the user accepts/edits/rejects; an accepted plan becomes a durable record projected into future context. **Exit:** an accepted plan persists across turns without the model restating it.
- **D5** `[Brain][CLI]` — **Task/todo state** as durable records (or host session metadata) projected into context, with progress shown in the UI. **Exit:** todo progress is visible turn-to-turn from durable state, not model re-statement.
- **D6** `[Engine]` — **Verification loops**: `cargo fmt`/`test`/`clippy`, targeted test detection, failure summarisation, bounded auto-retry; long builds/tests run as background ops (Phase 2 primitive) so the model keeps reasoning. **Exit:** the CLI repairs at least one failing test within a bounded retry budget while a background build streams.
- **D7** `[CLI]` — Git ergonomics: `/diff`, `/review`, `/commit-message`, `/branch`, `/rewind`, `/resume`, named sessions. Commits stay user-approved actions. **Exit:** the CLI shows a diff, drafts a commit message, and can rewind/resume a session.
- **D8** `[Brain][Engine][CLI]` — **Coding subagents** (`explorer`, `implementer`, `reviewer`, `test-fixer`) — configuration over the existing Phase-6 subagent primitive: constrained tools, tier defaults, depth limits, trace-visible usage. **Exit:** a `reviewer` subagent inspects the final diff and returns findings with file references; the whole run replays from one trace.
- **D9** `[CLI]` — **Terminal UX**: decide and land the front-end strategy (stay stdout-streaming vs adopt a TUI framework — call this out as a one-way door), then a stable status line, background-op list, compact/collapsible tool cards, token/cost/context counters, and calm idle states. **Exit:** a documented front-end decision plus a status line + tool cards that stay readable on a noisy session.
- **D10** `[Engine][Brain]` — **Hooks** (genuinely new; `on_event` is reserved but undelivered per §8.1): pre-tool / post-tool / session-start / compaction / stop hooks as host-side capabilities/events that can add context, warn, or deny — but cannot mutate core internals. **Exit:** a stop hook and a pre-tool hook fire deterministically and appear in the trace.

---

## Phase E — Chrome extension as a general-purpose web agent

**Goal.** Make the browser host excellent in its own domain. Parallel to Phase D (different crate). Text-only.

- **E1** `[Browser]` — Expand observation tools: page text, DOM outline, selected text, visible-viewport text, links, forms, buttons, tables, metadata, console logs, network errors, tab groups. Read-only, no permission. **Exit:** the agent can describe a page's structure and read its console errors.
- **E2** `[Browser]` — Permissioned **action tools**: click, type, select, submit, scroll, wait, copy, download, upload-to-input, multi-step form fill — each gated through the auto-approve judge. **Exit:** the agent fills and submits a form under auto-approve; unsafe actions are denied with a reason.
- **E3** `[Browser][Brain]` — Browser task plans (reuse D4's plan record): show a short plan before action-heavy work, update progress as durable state. **Exit:** a navigate-and-act task shows a plan and live progress.
- **E4** `[Browser]` — Extraction workflows: page/table/list → CSV/JSON/Markdown, with preview, copy/download, and source-URL provenance. **Exit:** a table is extracted to CSV with its source URL, previewed, and downloaded.
- **E5** `[Browser]` — Comparison workflows: compare tabs, summarise many open pages, cluster/dedupe tabs, build reading queues. **Exit:** the agent summarises N open tabs into one comparison.
- **E6** `[Browser][Replay]` — Local persistence (IndexedDB) for traces/blobs mirroring native trace semantics, plus **resume** a saved browser trace through the same WASM brain. **Exit:** a browser session is saved, reloaded, resumed, and replays through the WASM brain.
- **E7** `[Browser]` — Side-panel UX polish: session list, pinned context, trace import/export, context drawer, tool timeline, searchable history, and clear "what can Hugr see/do on this site?" copy. **Exit:** sessions are first-class in the panel and the site-capability copy is understandable.

---

## Phase F — Memory, instructions, sessions & branching

**Goal.** Native-feeling continuity across both products. Depends on traces (built) + sessions.

- **F1** `[Engine][Brain]` — Instruction discovery: global + project `AGENTS.md`, nested overrides, browser profile/session instructions; the loaded instruction chain is inspectable and projected with token accounting. **Exit:** instruction sources display in precedence order and appear in the projection with token costs.
- **F2** `[Engine][Brain]` — Path-scoped rules for the CLI: rules load only when projection includes matching files or a tool result references matching paths. **Exit:** a path-scoped rule loads only when a matching file is in context.
- **F3** `[Brain][Engine]` — Memory records: durable notes the agent proposes and a policy/flag persists (auto-memory *proposes*, never silently persists); per-repo/per-host scopes. **Exit:** a memory suggestion is visible, editable, scoped, and only persists on explicit acceptance.
- **F4** `[Engine]` — Session registry: names, summaries, last activity, branch, active tier, active repo/site, token/cost totals, trace path/blob root. **Exit:** sessions are listable objects, not raw trace files.
- **F5** `[Brain][Engine][CLI][Browser]` — Branch/rewind/edit-resume UX over the existing fork primitive: branch at a `seq`, edit a prior user turn into a new branch, keep originals immutable. **Exit:** a user branches before a bad turn and continues on the branch without losing the original, in both hosts.
- **F6** `[CLI][Browser]` — Continuity commands: `/status`, `/sessions`, `/resume`, `/rename`, `/branch`, `/rewind`, `/memory`, `/instructions` (CLI) + equivalent side-panel controls. **Exit:** both hosts expose the full set.

---

## Phase G — Scratchpad (lower priority)

**Goal.** A per-session temp workspace the agent can freely read/write, isolated from the user's project. Niche but nice.

- **G1** `[Engine]` — A session-scoped scratch directory exposed as a capability with read/write allowed without a permission round-trip (it is sandboxed to the scratch root). **Exit:** the agent writes and re-reads a scratch file with no permission gate; nothing escapes the scratch root.
- **G2** `[CLI][Browser]` — Surface the scratchpad (its path/contents) and clean it up on session end. **Exit:** users can see and clear the scratchpad in both hosts.

---

## Cross-cutting tracks (run throughout, from Phase A)

Pulled out of a terminal phase deliberately: quality must be measurable *as* the brain gets smarter, not after.

- **X1** `[Replay][Docs]` — **Golden-trace corpus** started with Phase A: short coding fix, long fix with compaction, test-failure repair, stale-edit conflict, branch/rewind, browser extraction, browser form workflow, browser resume. New behaviour lands with a golden trace. **Exit:** every merged phase adds at least one regression trace.
- **X2** `[Brain][Engine][Replay]` — **Conformance tests**: deterministic scripted scenarios for projection, compaction, routing, permissions (auto-approve), skills/MCP, subagents, hooks, and branching. **Exit:** the suite gates releases; any new control-flow path has a replay test.
- **X3** `[CLI][Browser]` — **UX/DX bars** as testable acceptance criteria, since UX is treated as correctness: no-noise-by-default output, visible active tier + context fullness on every turn, keyboard-first interaction, and a measured time-to-first-token. **Exit:** each product meets its stated bars before a phase is called done.
- **X4** `[Docs]` — Keep `PROGRESS.md` + `docs/` in sync per phase (per `CLAUDE.md`). **Exit:** docs match reality at each phase boundary.

---

## Dependency & parallelism map

- **Phase 0** unblocks most work (tiers → B; tokens → A; permissions → D/E). Do it first.
- **Phase A** (Brain-heavy) and **Phase C** (Engine-heavy: MCP/Skills) are largely independent → run in parallel.
- **Phase B** needs 0 + benefits from A.
- **Phase D** `[CLI]` and **Phase E** `[Browser]` share no tags → run in parallel by two agents.
- Within D/E, the pure-UI tasks (`D9`, `E7`) are the easiest parallel wins; the `[Brain]`/`[Engine]` tasks are the serialisation points.
- **Phase F** needs sessions; **Phase G** is independent and low-priority.

## Backlog (explicitly after the two-product focus)

- Python and Node bindings.
- More provider adapters; optional extra tiers/roles (`summarizer`, `vision`, `router`) and multimodal parts.
- Richer permission policies: sandboxes, named profiles, approval memory (built on the flexible `Policy` trait kept from Phase 0).
- WASM plugin runtime completion.
- Hosted sync service for traces/sessions; team sharing.

## Immediate next slice

Lowest-risk path that makes the brain observably smarter before making it autonomous:

1. **Phase 0 in full** — tiers, token accounting, auto-approve + yolo (drop the interactive prompt). This is mostly mechanical and unblocks everything.
2. **A1 + A5** — `ContextPlan` + `/context` inspection with *no* behaviour change (expose what the brain does before changing it).
3. **A2 + A3 + A4 + A6** — summary records and replay-safe compaction through the `small` tier, with manual triggers in both hosts.
4. **C1 + C2** — MCP client + CLI config (high-value, independent, parallelisable with the A work).
5. **B2** — real tier routing, once A's context-pressure signal and the golden-trace corpus (X1) exist to keep it honest.
