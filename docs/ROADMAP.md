# Roadmap — the subagent toolkit

> Companion to `DESIGN.md` and `ARCHITECTURE.md`. This roadmap replaces the previous two (the "prove the core" plan and the "CLI + Chrome extension product" plan — both retired; their built output is summarized below). The new direction: **Hugr is a toolkit for building tiny, self-contained, domain-specific subagents** — "build your subagent, ship it anywhere." Every task is small, tagged by crate, and carries an exit criterion.

## Where we start from (built and kept)

The foundation phases are complete and are exactly what the pivot needs. Kept, load-bearing:

- `hugr-core` — the sans-IO brain: turn loop, projection + `ContextPlan`, lossless compaction, tier routing (`small`/`medium`/`big`), skills, plan/todo records, hooks, stale-edit CAS, sub-agents + forking (`AgentSeed`), deterministic replay. **Untouched by this roadmap except where explicitly noted.**
- `hugr-host` — tokio engine, uniform capabilities, model/capability registries, auto-approve/yolo policies, MCP stdio client, skills loader, checkpointing, crash resume, scheduler.
- `hugr-providers` — OpenAI-compatible streaming adapter (HF router default), per-tier config.
- `hugr-replay` — versioned trace format (`Trace { meta, events, log, commands, blobs, children }`), content-addressed blob store, replay/verify/inspect, resume.
- `hugr-plugin-abi` (+ `hugr-example-plugin`) — the versioned subprocess plugin contract; the "custom tool without recompiling" escape hatch.
- `hugr-docs` — the prototype subagent (docs Q&A, read-only tools, JSON answer + cost metadata, Python binding). **It is the template the toolkit generalizes**, and it gets rebuilt on the toolkit in T1 as proof.

**Parked** (kept compiling as core regression hosts; no product work): `hugr-cli` (the coding agent) and `hugr-wasm` (+ Chrome extension). Their tests keep guarding engine behavior; their feature backlogs are dropped. Revisit only as "packaged agents" once the toolkit exists.

## Guiding principles for sequencing

1. **Contract first.** The ask/answer contract and trace-id semantics are the one-way door of this roadmap — every surface, binding, and orchestrator will depend on them. Design and pin them (T0) before any packaging exists.
2. **Prove by porting.** `hugr-docs` is rebuilt on each new layer as it lands (T0: the agent API; T1: the declarative definition). If the port isn't a deletion of `hugr-docs` code, the layer is wrong.
3. **Config before codegen.** The declarative toolkit (T1) ships before the multi-surface bundler (T2); an agent must be runnable from its config folder (`hugr run`) before we invest in packaging.
4. **Surfaces are thin or they are wrong.** Every surface (CLI/Python/MCP) wraps the same `hugr-agent` Rust API. A surface needing agent-specific logic signals a hole in the common API — fix the API.
5. **Docs stay in sync per phase** (`AGENTS.md` convention); each phase adds golden traces to the regression corpus.

## Crate tags

- `[Core]` — `hugr-core`. Pure, sans-IO. Expected to change **rarely**; any change here needs strong justification.
- `[Agent]` — **new** `hugr-agent`: the common subagent runtime API (ask/answer, trace store, scratchpad, blobs, accounting) on top of `hugr-host` + `hugr-replay`.
- `[Toolkit]` — **new** `hugr-toolkit` + the `hugr` builder CLI: manifest parsing, tool library wiring, `new`/`run`/`build`/`traces`.
- `[Host]` — `hugr-host`. `[Replay]` — `hugr-replay`. `[Providers]` — `hugr-providers`.
- `[Docs]` — documentation + `PROGRESS.md`. `[Demo]` — the demo agents + orchestrator example.

Two tasks with disjoint tag sets can proceed in parallel.

---

## Phase T0 — `hugr-agent`: the common subagent API

**Goal.** A new crate that turns "an engine + a trace dir + a config" into a callable subagent with the uniform contract. This is the layer every surface wraps and the phase where the one-way-door decisions get made.

- **T0.1** `[Agent]` — **The `Ask`/`Answer` contract.** `Agent::ask(Ask) -> Answer`. `Ask { question: String, trace_id: Option<TraceId>, blobs: Vec<BlobHandle>, extra: Value }`. `Answer { status: success|off_topic|error, message: String, trace_id: TraceId, blobs: Vec<BlobHandle>, metadata: AnswerMeta, extra: Value }`. `AnswerMeta` is mandatory: `duration_ms`, `cost_micro_usd`, `tokens_in/out`, `model_calls`, `tool_calls`, per-tier breakdown. Both types `#[non_exhaustive]` with constructors, serde-stable, documented as **the** contract (JSON schema committed). Generalizes the `hugr-docs` output shape. **Exit:** the contract types round-trip serde; a JSON schema file is committed and pinned by a test.
- **T0.2** `[Agent][Replay]` — **Trace store with `trace_id` / `depends_on`.** A `TraceStore` (default: a directory under the agent's data dir) storing immutable traces keyed by generated `trace_id`, each with header metadata `{ trace_id, depends_on: Option<TraceId>, agent_name, agent_version, created_at, question, status }`. `list()`, `get()`, `head()` (metadata without loading events). `TraceMeta` gains these fields serde-defaulted so existing traces load. **Exit:** a run persists a trace; `list()` shows lineage; loading by id re-folds with zero IO beyond the file read.
- **T0.3** `[Agent]` — **Resume & fork semantics.** `ask` with `trace_id` loads the parent, re-folds it into a fresh brain, appends the new turn, and persists a **new** trace with `depends_on` set — the parent is never mutated. Two asks from the same parent yield sibling traces. **Exit:** a scripted three-way fork (root → t1 → {t2a, t2b}) works end-to-end; each answer returns the right id; parents verify unchanged byte-for-byte; a replay test pins determinism of the resumed fold.
- **T0.4** `[Agent][Host]` — **Scratchpad capability.** A per-agent (per-session-tree) scratch directory exposed as `scratch_read`/`scratch_write`/`scratch_list` capabilities: writable with no permission gate, canonicalized, jailed to the scratch root (same path-escape discipline as `hugr-docs` tools). Scratch lifetime is tied to the trace lineage so a forked ask sees the ancestor's notes (copy-on-fork for divergence-safety). **Exit:** the agent writes and re-reads a note across a resumed ask; nothing escapes the root; a fork's writes don't leak into the sibling.
- **T0.5** `[Agent]` — **Blob exchange with permissions.** `BlobHandle { ref: bytes|path|sha256, media_type, perms: {read, write, execute} }`. Inbound blobs are materialized into the scratchpad with the declared perms; outbound blobs are returned by content-addressed ref (reusing `hugr-replay::BlobStore`). Enforcement v1: materialize-with-mode-bits + jail (advisory beyond that, documented). **Exit:** an orchestrator hands a file in and receives a produced file back; perms are applied; blobs dedupe by hash.
- **T0.6** `[Agent][Providers]` — **Pricing & cost accounting.** Per-tier pricing (`input/output USD per M tokens`) in the agent config; `AnswerMeta.cost_micro_usd` computed by folding the trace's `OpMeta` usage — including sub-agent children — so cost is derivable from the trace alone. **Exit:** a run's reported cost equals the hand-computed sum from its trace; children included.
- **T0.7** `[Agent]` — **Introspection API.** `Agent::describe() -> AgentCard { name, version, description, tools + privileges, model tiers, pricing, limits }`, `Agent::config()` (effective config incl. env-var resolution, secrets redacted), `Agent::traces()`. Same data every surface will expose (`--describe`, Python `.describe()`, MCP server info). **Exit:** describe/config/traces return complete, redacted, serde-stable data pinned by tests.
- **T0.8** `[Agent][Docs]` — **Port `hugr-docs` onto `hugr-agent`.** Rebuild the docs agent's runner on `Agent::ask`; its CLI/Python output do not have to stay backward-compatible (do your best, currently it's `status`/`message`/`related_documents`/`metadata` but I think errors should be better handled) + it's gaining `trace_id` resume. Delete the bespoke runner code the port obsoletes. **Exit:** existing `hugr-docs` tests pass on the new layer; a follow-up question via `trace_id` works from CLI and Python; net LOC in `hugr-docs` goes down.

**Exit criteria (phase).** A Rust program can `Agent::ask` a docs question, get an answer with cost/duration/trace_id, ask a follow-up on that trace, and fork a sibling — with every trace verifying deterministically. The contract JSON schema is committed and stable.

---

## Phase T1 — `hugr-toolkit`: declarative agent definitions

**Goal.** A subagent is a config folder, not a Rust project.

- **T1.1** `[Toolkit]` — **Manifest format.** `hugr.toml`: `[agent]` (name, version, description), `[models]` (tiers → model id + knobs + pricing, base_url, api-key env), `[tools.<name>]` (grant + scope params), `[limits]` (max turns, max cost, timeout), `[scratchpad]`, `[traces]` (store location). `SYSTEM.md` beside it is the system prompt (supports a small set of template vars: agent name, tool list, date). Parsed into a typed `AgentDefinition` with precise error spans. **Exit:** a definition folder parses; unknown keys warn; a documented reference manifest is committed.
- **T1.2** `[Toolkit][Host]` — **Predefined tool library.** Vetted, parameterized capabilities selectable from the manifest, each with declared privileges and scope config: `fs_read` (root-jailed read/list/search/outline/read_range/read_many — generalized from `hugr-docs`), `scratchpad` (from T0.4), `http_fetch` (allowlisted hosts, GET-only default), `sqlite_query` (read-only default, file-scoped). Each tool documents its privilege class (read-only / mutating / network) — the manifest is the audit surface. **Exit:** each library tool is manifest-configurable, jailed to its declared scope, and covered by a capability test; `hugr-docs`' tool code is subsumed.
- **T1.3** `[Toolkit]` — **`hugr run`: interpret a definition.** `hugr run <agent-dir> "question" [--trace <id>] [--json]` loads the definition, assembles the `hugr-agent` runtime (tools, tiers, prompt, policies), and executes one ask. This is the "interpreter mode" every definition gets before any bundling. **Exit:** a docs-agent definition folder answers a question with the standard JSON answer; no Rust written.
- **T1.4** `[Toolkit]` — **`hugr new`: scaffolding.** `hugr new <name> [--template docs|sqlite|blank]` emits a working definition folder with commented manifest and prompt. **Exit:** `hugr new` + edit one path + `hugr run` answers within minutes.
- **T1.5** `[Toolkit]` — **External tools in the manifest.** `[tools.mcp.<name>]` (command + args → namespaced MCP tools, reusing the C1 client) and `[tools.plugin.<name>]` (subprocess plugin via `hugr-plugin-abi`). **Exit:** a manifest-declared MCP server's tool is callable by a definition-run agent; same for a subprocess plugin.
- **T1.6** `[Toolkit][Docs]` — **Redefine `hugr-docs` as a definition.** The docs agent becomes a checked-in definition folder (manifest + prompt) run by the toolkit; the crate shrinks to its packaging surfaces (CLI arg-compat + Python) over the shared runtime. **Exit:** the definition-run docs agent matches the crate's behavior on the existing test corpus.
- **T1.7** `[Toolkit]` — **Trace tooling.** `hugr traces <agent-dir>` (list with lineage tree), `hugr replay <agent-dir> <trace-id> [--step]`, `hugr verify <trace-id>` — the existing replay/inspect machinery pointed at the agent's trace store. **Exit:** a fork tree renders as a tree; any listed trace replays and verifies.

**Exit criteria (phase).** `hugr new` → edit config → `hugr run` gives a working, sandboxed, trace-persisting subagent with zero Rust. The docs agent is a definition folder.

---

## Phase T2 — Surfaces: ship it anywhere

**Goal.** `hugr build` turns a definition into self-contained artifacts. Surface choice is compile-time, never part of the agent definition.

- **T2.1** `[Toolkit]` — **`hugr build --surface cli`: standalone binary.** Embed the definition (manifest + prompt + tool config) into a single binary wrapping `hugr-agent`. Standard CLI shape for **every** agent: `<agent> "question" [--trace <id>] [--json|--pretty] [--describe] [--traces] [--config]`; JSON answer on stdout, logs on stderr, exit 0 (the `hugr-docs` contract, now universal). Implementation: a small generated shim crate + `cargo build` (document the Rust-toolchain-at-build-time requirement; prebuilt-runtime embedding is a later optimization). **Exit:** `hugr build` on the docs definition yields a binary that answers, resumes by trace id, and self-describes — on a machine with no repo checkout.
- **T2.2** `[Toolkit][Agent]` — **Rust-crate surface.** `hugr build --surface crate` emits a library crate exposing the typed `Agent` for direct embedding (orchestrators in Rust skip serialization entirely). **Exit:** a downstream Rust example depends on a generated crate and calls `ask` natively.
- **T2.3** `[Toolkit]` — **Python surface.** `--surface python` emits a maturin/PyO3 package: `answer(question, trace_id=None, **config_overrides) -> dict` (the `hugr-docs` binding generalized: never raises for run failures, env-var fallbacks per config key) plus `describe()`/`traces()`. **Exit:** `pip install` of a built wheel answers and resumes from Python; the docs agent's Python API is reproduced by the generator.
- **T2.4** `[Toolkit]` — **MCP server surface.** `--surface mcp` emits/enables a stdio MCP server mode (`<agent> --mcp-serve`): one `ask` tool (question + optional trace_id + blob refs), answer + full metadata in the result; server info from `describe()`. This is how Claude Code/other orchestrators consume Hugr agents natively. **Exit:** a built agent registered in an MCP client answers with trace_id round-tripping across calls.
- **T2.5** `[Toolkit][Docs]` — **Surface conformance suite.** One scripted scenario (ask → follow-up → fork → describe) run against every surface of the same definition, asserting identical answers/metadata modulo transport. **Exit:** the suite passes for cli/crate/python/mcp and gates `hugr build` changes.

**Exit criteria (phase).** One definition folder ships as a binary, a crate, a wheel, and an MCP server — same contract, proven by the conformance suite.

---

## Phase T3 — Orchestration hardening

**Goal.** The features an orchestrator hits in week two.

- **T3.1** `[Agent]` — **Limits enforcement.** Manifest `[limits]` (max model calls/turns, max cost, wall-clock timeout) enforced host-side; exceeding one yields `status: error` with a typed reason and a persisted (still-verifying) trace. **Exit:** each limit triggers cleanly in a test; the partial trace replays.
- **T3.2** `[Agent]` — **Concurrent asks.** Document + test the model: each ask is an independent session; trace-store writes are atomic and id-collision-free under parallel `ask`s (immutability makes forks race-free by design). **Exit:** N parallel asks (mixed fresh/forked) produce N valid traces with correct lineage.
- **T3.3** `[Agent][Replay]` — **Trace lifecycle.** `hugr traces prune` with a policy (age/LRU, pinned roots, keep-lineage-closed so no orphaned `depends_on`); trace-store size reporting. **Exit:** pruning never breaks a surviving trace's lineage chain.
- **T3.4** `[Agent]` — **Structured answer extras.** A manifest-declared optional JSON schema for `Answer.extra` (e.g. `related_documents` for the docs agent), validated post-hoc, never load-bearing for the core contract. **Exit:** the docs agent's `related_documents` moves to a declared extra; schema violations surface as warnings not failures.
- **T3.5** `[Toolkit]` — **Config/env audit surface.** `<agent> --config` prints effective config with provenance (default / manifest / env / flag) and redacted secrets — the auditability story, machine-readable. **Exit:** provenance is correct for every key; secrets never print.
- **T3.6** `[Host][Agent]` — **Sandbox tightening pass.** Review every library tool's jail (symlink escapes, canonicalization races, http allowlist bypass, sqlite ATTACH), document the threat model per tool. **Exit:** a written threat-model doc + regression tests for each reviewed escape vector.
- **T3.7** `[Agent][Toolkit]` — **Resource groups & grants (ARCHITECTURE §18.5).** Typed `ResourceGroup`/`ResourceGrant` slots on `Ask`; manifest tool scopes may bind `group:<name>`; a bound tool is registered only when a matching grant arrives, with `Read` grants satisfying read-class tools only. Grants are recorded in the trace header and re-derived on resume/fork/replay. **Exit:** one definition answers with different effective tool sets under read vs read-write vs no grant; a resumed ask re-derives identical registration from the trace alone; an ungranted bound tool is provably absent from the session's schemas.
- **T3.8** `[Agent][Toolkit]` — **Agent-as-tool grants (ARCHITECTURE §20.5).** `[tools.agent.<name>]` registers a child Hugr agent (definition folder, artifact, or registry ref) as one ordinary capability generated from its `AgentCard`; the child's `Answer` is the tool result, its cost folds into the caller's `AnswerMeta`, forwarded resource-group grants attenuate (never widen), and depth/cycles are cut by `max_agent_depth`. **Exit:** an agent definition delegates a sub-question to a granted child agent (interpreter + subprocess-artifact paths), the parent's reported cost includes the child, a follow-up via the child's `trace_id` works, and the recorded parent trace `verify()`s with the nested child.

---

## Phase T4 — The demo: four subagents, one story

**Goal.** A self-explanatory demo proving the pitch end-to-end. **Scenario: the expense-audit assistant.** A team lead asks one question — *"Which of last month's expenses violate our travel policy, and by how much?"* — and an orchestrator answers it by delegating to four specialized Hugr subagents. Every piece is explainable in one sentence, each agent's privilege set is visibly different, and the flow exercises blobs, forks, and cost lines naturally.

- **T4.1** `[Demo]` — **`policy-docs` agent.** Tools: `fs_read` jailed to a folder of company travel-policy markdown. Answers policy questions with citations (the docs template, unchanged — proving reuse). *Privileges: read one folder.*
- **T4.2** `[Demo]` — **`receipts` agent.** Tools: `pdf_read` (new library tool: text + table extraction from PDFs, no network) + scratchpad. Receives receipt PDFs **as blobs** from the orchestrator and extracts structured expense lines. *Privileges: read handed-in blobs only.* (Adds the `pdf_read` tool to the T1.2 library — first post-v1 library addition, validating the extension path.)
- **T4.3** `[Demo]` — **`ledger` agent.** Tools: `sqlite_query` (read-only) on `expenses.db`. Answers questions about recorded expenses, totals, and per-employee breakdowns. *Privileges: read one database file.*
- **T4.4** `[Demo]` — **`report-writer` agent.** Tools: scratchpad only — no external reads at all. Turns the orchestrator's gathered findings into a polished markdown report returned **as an outbound blob**. *Privileges: none but its own scratchpad.*
- **T4.5** `[Demo]` — **The orchestrator example.** A ~200-line script (one Python variant via the wheels, one Rust variant via the crates) that: asks `ledger` for last month's expenses → asks `policy-docs` for the relevant rules (follow-up via `trace_id` for a clarification, demonstrating resume) → fans receipts to `receipts` → **forks** the `policy-docs` trace to test a what-if ("what if the hotel cap were $250?") without polluting the main thread → hands findings to `report-writer` → prints the report plus a **per-agent cost/duration table** summed from answer metadata. **Exit:** one command runs the whole scenario from checked-in sample data (policies, PDFs, DB) with no network but the model endpoint; the README walkthrough matches reality.
- **T4.6** `[Demo][Docs]` — **Demo as conformance.** The scenario runs in CI against recorded model traces (replay mode — no live model), pinning the multi-agent flow deterministically. **Exit:** CI replays the full demo bit-for-bit.

**Exit criteria (phase).** A newcomer clones the repo, runs one command, and watches four differently-privileged subagents answer a cross-domain question with costs, a resumed thread, and a forked what-if — then reads four small config folders to understand *exactly* what each agent could touch.

---

## Phase T5 — Publish & harden

**Goal.** Other people can build on this.

- **T5.1** `[Docs]` — **The book.** A "define → run → build → orchestrate" guide: manifest reference, tool-library reference with privilege classes, answer-schema reference, trace/fork semantics, per-surface how-tos.
- **T5.2** `[Replay][Agent]` — **Trace migration.** Versioned migration hooks so an old `trace_id` remains resumable across a `Record`/`Event` schema bump; a golden old-format trace in CI. **Exit:** a deliberately old trace resumes after a simulated schema change.
- **T5.3** `[Toolkit]` — **Distribution.** Publish crates (`hugr-core`, `hugr-agent`, `hugr-toolkit`, …) and the `hugr` builder binary; template registry for `hugr new`. **Exit:** `cargo install hugr-toolkit` → `hugr new` → `hugr run` works outside the repo.
- **T5.4** `[Agent]` — **Protocol adapters (as demanded).** A2A surface adapter (Agent Card from `describe()`, task lifecycle from the turn loop, Artifacts from blob exchange, cost via an A2A usage extension) and/or a Zed Agent Client Protocol adapter for editor integration, if/when demand materializes. The IBM/BeeAI "ACP" merged into A2A in 2025 and is not a target. Explicitly last: adapters, not foundations.
- **T5.5** `[Providers]` — **Provider breadth.** Additional adapters (Anthropic-native first) behind the same streaming contract; per-provider pricing presets for the cost config.
- **T5.6** `[Host][Toolkit]` — **`code_exec`: the sandboxed exec-class library tool (ARCHITECTURE §20.2).** V1: subprocess jail — manifest-pinned interpreter, cwd = scratchpad, no network, env scrubbed, wall-clock/memory/output caps from `[limits]`. Target backend: WASM/WASI (wasmtime, shared with the plugin ABI) with preopened dirs only (scratchpad + read-only mounts of granted resource-group roots). Privilege class `exec`; threat-model note required per T3.6; a general `shell` never enters the library. **Exit:** a definition-run agent executes a granted script that reads its scratchpad; escapes (fs outside jail, network, runaway time/memory) are blocked by regression tests; an agent without the grant contains no exec path.

---

## Phase T6 — Discovery & self-extension (deliberately lower priority)

**Goal.** Two capstones that only make sense once the toolkit is real and stable: a machine-level registry so orchestrators can *find* agents, and the Pi-style endgame — a subagent that builds subagents. Design sketches in `ARCHITECTURE.md` §22; sequenced last on purpose: both consume the T0–T2 contracts and would ossify them if built earlier.

- **T6.1** `[Toolkit][Agent]` — **Machine-level agent registry.** A well-known registry (`~/.local/share/hugr/registry/`) of installed agents: one entry per agent = its `AgentCard` (from `describe()`) + artifact location + definition provenance. `hugr build`/`hugr install` register; `hugr agents list|show|remove` manage; entries are verified live (a stale card for a deleted binary is flagged, and `--describe` on the artifact is always the ground truth — the registry is a cache, never an authority). **Exit:** an orchestrator lists all agents on the machine with their tools/privileges/pricing via one call, and a stale entry is detected.
- **T6.2** `[Toolkit]` — **Gateway MCP server.** `hugr serve --mcp` exposes *every* registered agent as one tool each from a single stdio server (cards → tool descriptions, asks proxied to the artifacts). This is how an orchestrator like Claude Code gets the whole local agent fleet from one config line. **Exit:** two registered agents are both callable through one gateway registration; `trace_id` round-trips per agent.
- **T6.3** `[Demo][Toolkit]` — **`hugr-builder`: the subagent that builds subagents.** An agent whose tools are the toolkit itself: `agent_scaffold` (from templates), `agent_edit` (manifest/prompt, schema-validated on write), `agent_validate`, `agent_test_run` (a sandboxed `hugr run` of the candidate against a probe question, returning the standard `Answer`), `agent_register`. **v1 constraint: it emits pure-data definitions only** (library tools + MCP grants — no `[tools.rust.*]`), so its output is auditable config, interpretable by `hugr run` with no compiler, and its own privilege set stays small (fs write jailed to one agents workspace + the toolkit tools; no shell). Building native-tool agents stays a human step (`hugr dev`/`hugr build`). **Exit:** asked "build me an agent that answers questions about this folder of CSVs", `hugr-builder` scaffolds a definition, iterates until a probe question passes, registers it, and the new agent immediately appears in `hugr agents list` and answers through the gateway — with the whole build conversation itself a replayable trace.
- **T6.4** `[Docs]` — **Self-extension guardrails.** Written policy for builder-produced agents: registry entries carry `built_by` provenance; a builder-made agent can be granted at most the tool classes the builder itself was allowed to grant (no privilege escalation by generation); human review points documented. **Exit:** the guardrails doc exists and the T6.3 demo abides by it.

---

## Cross-cutting tracks

- **X1 — Golden traces.** Every phase adds recorded traces as regression fixtures: fork trees (T0), definition-run sessions (T1), per-surface conformance runs (T2), the full demo (T4).
- **X2 — Determinism gate.** Any new control-flow path ships with a replay test; `verify()` stays the release gate. The core invariants (sans-IO, single-threaded brain, injected nondeterminism, log-as-truth, transport-only deltas) are non-negotiable throughout.
- **X3 — Token-efficiency budget.** Track per-demo-agent context size and cost; a library tool that bloats every agent's prompt gets redesigned. The pitch is *tiny* agents — measure it.
- **X4 — Docs sync.** `PROGRESS.md` + `docs/` updated at every phase boundary, per `AGENTS.md`.

## Dependency & parallelism map

- **T0 is the serialization point** — the contract everything wraps. Within T0: T0.1→T0.3 are sequential; T0.4/T0.5/T0.6/T0.7 parallel after T0.1; T0.8 last.
- **T1** needs T0. T1.2 (tool library) and T1.1 (manifest) are parallel; T1.3 joins them.
- **T2** needs T1.3; the four surfaces are parallel after T2.1 settles the embedding approach.
- **T3** is parallel to late T2. **T4** needs T2.1 + T2.3 (binary + Python) and T1.2's tools plus `pdf_read`. **T5** closes the main arc.
- **T6** is explicitly after everything: the registry (T6.1) consumes stable AgentCards; the gateway (T6.2) consumes the registry; the builder (T6.3) consumes the whole toolkit as its tool set.

## Immediate next slice

1. **T1.1 + T1.2 in parallel** — manifest + tool library; then **T1.3** (`hugr run`) makes the pivot demoable without any packaging.
2. **T1.6** — redefine `hugr-docs` as a definition folder once the manifest runner exists; this is the second proof that the shared layers delete crate-specific runtime wiring.
3. **T1.7** — trace tooling over the agent trace store so fork trees are inspectable from the toolkit CLI.
