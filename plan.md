# Hugr Roadmap Plan

This plan turns every item in `new_ideas.md` into either a detailed TODO or an entry in the Won't-do / Deferred sections. It is grounded in the codebase as of commit `875a9a2` — file references were verified against source. Tasks are grouped in phases ordered by dependency; tasks within a phase are mostly parallelizable unless a `Depends on` says otherwise.

Status legend: each task starts `[ ]`; flip to `[x]` when merged (code + tests + docs). A task is not done until `ARCHITECTURE.md` / `README.md` / `AGENTS.md` match reality (see the Doc-sync checklist at the end). Sizes: S (&lt;1 day), M (1–3 days), L (~1 week), XL (multi-week).

## Ground rules (checked against every task below)

- `hugr-core` stays sans-IO, pure, single-threaded, dependency-free beyond serde. None of the tasks below adds anything environmental to it; the only core changes in this plan are pure (new `ContextDisposition` variants, one `Record` variant for model-backed compaction, policy plumbing) and each ships with scripted determinism + replay tests.
- All nondeterminism stays injected as events. Anything that needs a clock (cron), a model call (summarization), or IO (storage backends) lives in a host layer or rides the existing command/event cycle.
- The log stays the source of truth and traces stay immutable. Feedback, memory, and analytics are all sidecars or folds — never mutations of a stored trace. Compaction changes the *projection*, never the log.
- Narrow waist: new capabilities (memory, feedback-as-tool, traces_read) are ordinary `Capability` registrations with opaque args — zero core type changes. New manifest sections are open string sets where possible.
- Sandbox-by-registration: every new tool is granted in the manifest, jailed to a declared scope, and gets a threat-model note in ARCHITECTURE Part IV. The library stays exec-free.
- One way to do each thing: where this plan adds a mechanism (e.g. storage traits), it replaces the old shape (concrete structs threaded everywhere) rather than living beside it.

## Traceability: new_ideas.md → plan

| new_ideas.md line | Idea | Where in this plan |
|---|---|---|
| 1 | Feedback mechanism between subagents | 2.3 |
| 2 | Shared memory between runs | 2.2 |
| 3 | Trace-reading self-improvement agent | 4.1 |
| 4 | new_ideas.md workflow in AGENTS.md | 0.5 |
| 5 | Detailed analytics | 2.4 |
| 6 | Shared blobs, no local copies | 1.5 |
| 7 | Cron jobs for an agent | 2.5 |
| 8 | Swappable storage backends | 1.2 |
| 9 | Built-in configurable compaction/forget | 2.1 |
| 10 | Comment cleanup | 0.4 |
| 11 | hugr-docs + weather template → examples/ | 0.1, 0.2 |
| 12 | Split hugr-wasm generic vs extension | 0.3 |
| 13 | Python runtime API | 3.1 |
| 14 | TypeScript runtime API | 3.2 |
| 15 | Tutorials | 4.2 |
| 16 | Skills for hugr | 4.3 |
| 17 | Traces default to `~/.hugr/<agent>/traces` | 1.1 |
| 18 | (at some point) Android surface | Deferred D1 |
| 19–22 | (at some point) Hugr on the Hub | Deferred D2 |

---

## Phase 0 — Restructure & cleanup

Do these first: they move files around, and every later phase touches the moved files. Order inside the phase: 0.1 → 0.2 → 0.3 → 0.4 (cleanup sweeps the final layout) → 0.5.

### 0.1 `[x]` Move `hugr-docs` to `examples/hugr-docs` (idea 11) — S

- Why: `hugr-docs` is a reference *user* of the framework, not part of it; keeping it under `crates/` blurs the library boundary.
- Today: workspace member `crates/hugr-docs` (root `Cargo.toml:10`); referenced by path in `hugr-toolkit/src/build.rs:473` (test), `hugr-toolkit/tests/build_python.rs:20-35`, `README.md:53,92,95,101,116`, `ARCHITECTURE.md` §9; carries checked-in traces in `crates/hugr-docs/.hugr-docs-traces/` and a `[traces] store = ".hugr-docs-traces"` override in its manifest.
- Steps:
  - Move the folder to `examples/hugr-docs/`; add `"examples/*"` (or explicit members) to workspace `members`; keep the package name `hugr-docs`.
  - Update the two toolkit tests that reach into it by relative path (`build.rs:473`, `build_python.rs`).
  - Delete the checked-in `.hugr-docs-traces/` contents (they are stale artifacts, and 1.1 removes the per-agent store override anyway); if a test needs a real trace fixture, generate it in the test.
  - Update `README.md`, `ARCHITECTURE.md` §9 crate layout, `AGENTS.md` project layout.
- Acceptance: `cargo test --workspace` green; `hugr run examples/hugr-docs ./docs "..."` works; no `crates/hugr-docs` references left (grep).

### 0.2 `[x]` Materialize the weather template as `examples/hugr-weather` (idea 11) — M

- Why: the weather agent exists only as embedded format-string generators in `hugr-toolkit/src/scaffold.rs` (`manifest_for:144`, `lib_rs_for:190`, `system_for:230`, `weather_readme:252`). An on-disk example is browsable, runnable, and testable; generators drift silently.
- Design: one source of truth — check in `examples/hugr-weather/` as a real agent crate (Cargo.toml, hugr.toml, SYSTEM.md, src/lib.rs, README.md), and make `hugr new --template weather` emit those files via `include_str!("../../../examples/hugr-weather/...")` with the crate/agent name substituted. The `blank` template stays a small generator. Delete the per-file generator functions.
- Note: `include_str!` paths embed at toolkit compile time, so a published `hugr` binary still carries the template; a name-substitution pass (replace `hugr-weather`/`hugr_weather` tokens) replaces today's format strings.
- Steps: create the example crate (content = today's generator output); rewrite `scaffold.rs` to embed + substitute; keep `Template::{Weather, Blank}` enum; fix the stale `docs` template mention in the scaffold comment; update `tests/build_cli.rs:46`, `tests/conformance.rs:40` if paths change (they scaffold into temp dirs, so likely no change); add a sync test asserting `scaffold_files("hugr-weather", Weather)` equals the checked-in example files.
- Docs: README quickstart already says `--template weather`; add the example folder to layout sections.
- Acceptance: `hugr new my-agent` output == example content modulo name; conformance suite still passes.

### 0.3 `[x]` Split `hugr-wasm` into a generic browser package + a Chrome-extension example (idea 12) — L

- Why: the Rust side of `crates/hugr-wasm` is already generic (drives `hugr-core` directly, zero `chrome.*`), and half the JS is generic too; the extension we shipped is one *example* of a browser host. Goal: anyone can build a different extension (other layout, tools, policies) from the reusable parts.
- Today: generic — `src/{lib,exports,capabilities,config}.rs`, `extension/agent_driver.js` (command loop), `extension/openai_adapter.js` (fetch adapter + compaction POC), `extension/indexed_db.js`. Chrome-specific — `manifest.json`, `service_worker.js`, `content_script.js`, `chrome_api.js`, `sidepanel.{html,js,css}`, `build.sh`. Coupling seams: `agent_driver.js` imports `chrome_api.js` directly and hardcodes defaults in `toRustConfig` (`agent_driver.js:255`); `sidepanel.js:38` calls `runBrowserAgent` directly. Checked-in build artifacts: `extension/pkg/`, `hugr-wasm-extension.zip`.
- Target layout:
  - `crates/hugr-wasm/` (keep the name) — Rust wasm-bindgen bindings only: `exports.rs`, `config.rs`, and the browser tool *schemas* (`capabilities.rs` — they are the model⇄browser contract, not Chrome-specific). Remove the baked-in `SYSTEM.md`/`hugr.toml` (`include_str!` in `exports.rs:32,54`) — the host passes prompt/config at construction. Drop unused deps `schemars`, `wasm-bindgen-futures`, `js-sys` from `Cargo.toml`.
  - `bindings/typescript/` — the generic JS moves here and becomes the seed of the TS package (3.2): `agent_driver.js` (capability dispatcher becomes an injected interface instead of the `chrome_api.js` import), `openai_adapter.js`, `indexed_db.js` behind a storage interface. Until 3.2 lands this is plain JS with a thin `index.js`; 3.2 converts it to typed TS.
  - `examples/chrome-extension/` — everything Chrome-specific plus the UI, `build.sh`, and a README; it imports the generic driver/adapter/storage and implements the capability dispatcher over `chrome.*`.
- Steps: extract the capability-dispatch interface (`invokeCapability(name, args) -> Promise<result>`) in `agent_driver.js`; move files; un-hardcode defaults into host-passed config; delete `extension/pkg/` and the `.zip` from git (add to `.gitignore`, document `build.sh` regenerates); update `HUGR_WASM_PLAN.md` (fold what's still relevant into ARCHITECTURE and delete the file — one-doc rule).
- Compaction POC note: the prune/compact code in `openai_adapter.js` stays temporarily in the generic package but is marked for replacement by 2.1 (built-in policy compaction); the browser-observation staleness rules become a configurable policy the example wires up.
- Docs: ARCHITECTURE §9 + §23 (browser packaging), AGENTS.md layout, README crate layout.
- Acceptance: extension builds and runs from `examples/chrome-extension`; `cargo check -p hugr-wasm` clean natively and for wasm32; a second minimal browser host (a plain web page in the tutorial, 4.2) can drive the same package.

### 0.4 `[x]` Comment cleanup sweep (idea 10) — M

- Why: ~2,600 comment lines across crates (hugr-replay 39%, hugr-core 24%, hugr-agent 23%, hugr-host 24% of src lines). Most restate what the code shows, cite doc sections, or narrate "how it works".
- Rules for the sweep (add these to AGENTS.md conventions, replacing the current "Reference design sections in comments as `ARCHITECTURE §X`" rule, which idea 10 explicitly reverses):
  - Delete: references to `ARCHITECTURE §N` / `ROADMAP` / `DESIGN §` / T-numbers; "how it works" narration; comments restating the signature or the next line; `--- section ---` banner comments; stock-phrase reminders ("pure, instant, no IO", "narrow-waist") on items whose names/types already say it.
  - Keep: genuinely non-obvious constraints and failure modes (e.g. `brain.rs:310-316` stale-cancel guard, `limits.rs:129-131` cost-check timing, `state.rs:26-30` BTreeMap determinism note — rewritten without the § citation); safety/jail invariants on tool code; public-API doc comments that state contracts (trim, don't gut — `cargo doc` output should still make sense).
  - Fix while there: stale comments describing never-implemented things — `hugr-replay/src/replay.rs:118-121` (ChildTrace/`ChildMismatch` don't exist), `hugr-agent/src/store.rs:141` (`children` field doesn't exist), `manifest.rs:700` test comment about default temperature.
- Steps: one PR per crate (reviewable diffs), order: hugr-replay, hugr-core, hugr-host, hugr-agent, hugr-providers, hugr-toolkit; no behavior changes (assert with `cargo test` + identical `cargo clippy` output).
- Docs: update AGENTS.md conventions (comment policy); ARCHITECTURE untouched (it keeps the rationale that comments used to duplicate — that's the point).
- Acceptance: comment-line count reduced by well over half per crate; every kept comment states something the code cannot.

### 0.5 `[x]` new_ideas.md workflow polish (idea 4) — S

- Today: `new_ideas.md` exists and AGENTS.md already ends with "Drop short raw ideas into `new_ideas.md`" — the idea is mostly done.
- Steps: expand the AGENTS.md convention to what idea 4 actually asks: coding agents (Claude/Codex) should append *short* one-line ideas they get **while implementing** (not designs, not TODO lists), so the owner can review and promote them to `plan.md`; add "when an idea is promoted into `plan.md`, remove it from `new_ideas.md`"; mention `plan.md` as the structured roadmap in AGENTS.md.
- Acceptance: AGENTS.md documents the loop new_ideas.md → plan.md → implementation.

---

## Phase 1 — Foundations

These change defaults and introduce the seams that Phase 2/3 build on. Order: 1.1 and 1.3 and 1.4 are independent; 1.2 before 1.5.

### 1.1 `[x]` Default agent home `~/.hugr/<agent-name>/` (idea 17) — M

- Why: traces should land in one predictable per-agent place (`~/.hugr/hugr-docs/traces`) with zero per-agent configuration; today the default is `<source_dir>/.hugr-traces` in dev (`hugr-toolkit/src/runtime.rs:37-52`) and `$XDG_DATA_HOME/hugr/<name>@<version>/.hugr-traces` for built binaries (`surface.rs:agent_home_dir:580-591`), and `hugr-docs` even sets a custom `[traces] store` — three shapes for one thing.
- Design: one agent home for every surface, dev and built alike: `~/.hugr/<sanitized-name>/` containing `traces/`, `scratch/`, `memory/` (2.2), `feedback/` (2.3). No version segment (idea 17's example has none; traces carry `agent_version` in their meta already). Resolution order: `$HUGR_AGENT_HOME` (full home override, kept) → `$HUGR_HOME/<name>` (new, overrides the `~/.hugr` base) → `$HOME/.hugr/<name>` → temp dir fallback. `[traces] store` and `[scratchpad] root` remain as explicit overrides but disappear from every example, doc, and template.
- Steps:
  - Rewrite `agent_home_dir` (`surface.rs`) to the new scheme; make `trace_store_for` (`runtime.rs:43`) default to `<home>/traces` instead of `<source_dir>/.hugr-traces`; scratch default `<home>/scratch` (drop the `.scratch`-under-traces nesting in `agent.rs:111` — pass an explicit scratch root); blob default handled in 1.5.
  - Remove `run_typed_definition`'s forcing of `HUGR_AGENT_HOME` to the source dir (`bin/hugr.rs:369`) — dev runs now share the same `~/.hugr/<name>` home, which is exactly the point of idea 17.
  - Remove `[traces] store = ".hugr-docs-traces"` from `examples/hugr-docs/hugr.toml`; scrub the override from the reference manifest comments only if wrong, otherwise keep documented-but-commented.
  - `hugr traces`/`replay`/`verify` (`bin/hugr.rs:load_store`) resolve through the same path.
- Docs: README quickstart output paths; ARCHITECTURE §19 (TraceStore location), §6 (manifest), glossary; AGENTS.md layout note.
- Tests: unit tests for home resolution (env precedence); end-to-end test asserting a dev run and a built-binary run of the same agent share `~/.hugr/<name>/traces` (under a temp `$HUGR_HOME`).
- Invariant check: pure path policy in host layers; core untouched.

### 1.2 `[x]` Pluggable storage backends (idea 8) — L

- Why: scratchpad, traces, and blobs are hardwired to `std::fs` via concrete structs — `TraceStore` (`hugr-agent/src/store.rs:135`), `BlobStore` (`hugr-replay/src/blob.rs:36`), `ScratchDir` (`hugr-agent/src/scratch.rs:46`); there is not a single storage trait in the workspace. The goal is *not* to ship Postgres/browser backends but to make an agent implementation able to swap one in from its own crate.
- Design — three narrow async traits, defined in `hugr-agent` (the layer that consumes them), mirroring the existing method sets exactly:
  - `trait TraceBackend: put(trace, header) -> TraceId; get(id) -> Trace; head(id) -> TraceHead; list() -> Vec<TraceHead>` — the current `TraceStore` becomes `FsTraceStore` implementing it; the pure parts of `Trace` (`to_json`/`from_json`, id hashing) stay in `hugr-replay` as data.
  - `trait BlobBackend: put(bytes, media) -> BlobRef; put_file(path, media) -> BlobRef (fast path, see 1.5); get(hash) -> Vec<u8>; contains(hash)` — current `BlobStore` becomes `FsBlobStore` (stays in `hugr-replay` behind an `fs` feature so the pure trace format still compiles to wasm — this also unblocks TS-side `verify`, 3.2).
  - `trait ScratchBackend: prepare(parent: Option<TraceId>) -> ScratchHandle; finalize(handle, id: TraceId); read/write/list(handle, rel_path)` — the scratch *capabilities* (`scratch.rs:171-254`) call through the handle instead of `std::fs` directly; `FsScratch` keeps today's jail discipline (canonicalize + `starts_with` re-check) and copy-on-fork semantics.
  - `Agent` fields become `Arc<dyn TraceBackend>` / `Arc<dyn BlobBackend>` / `Arc<dyn ScratchBackend>`; `Agent::new` keeps a convenience fs constructor so existing call sites barely change.
- Extension point from an agent crate (matching the `answer_hooks()` compile-time pattern): optional `pub fn storage() -> hugr_agent::StorageOverrides` in the agent's `src/lib.rs`, detected by `build.rs`'s existing const/fn scan (`response_dependency:305`, `has_pub_fn:385`) and wired by the generated shim. A Postgres- or S3-backed agent is then: implement the trait in your agent crate, return it from `storage()`, `hugr build` — no framework change.
- Ship in-repo: `FsTraceStore`/`FsBlobStore`/`FsScratch` (the defaults) plus `MemTraceStore`/`MemBlobStore`/`MemScratch` (in-memory, used by tests and as the reference "how to write a backend" example). Nothing else — see Won't-do W2.
- Keep sync fs code inside the async trait impls (they're cheap); the traits are `async_trait` so DB/object-store impls are natural.
- Docs: ARCHITECTURE — resolve open question §23 "Storage backends", new subsection in Part II describing the traits + the `storage()` extension point + a threat note (a backend sees all trace/blob content; it is trusted host code like a custom capability); AGENTS.md "where logic goes".
- Tests: run the existing store/scratch/blob test suites generically over fs and mem backends; end-to-end ask on `MemTraceStore` proving no fs writes.
- Invariant check: traits live in host layers; `hugr-core` never learns storage exists; `hugr-replay`'s pure data stays pure.

### 1.3 `[x]` TurnPolicy injection + policy registry (prerequisite for 2.1, 3.1, 3.2) — M

- Why: `EngineBuilder::build` hardwires `StaticPolicy` (`hugr-host/src/engine.rs:638-657`) — there is no public way to run the engine with a custom policy — and replay can only reconstruct `StaticPolicy` (`hugr-core/src/policy.rs:decode_policy:25`, tried by `hugr-replay/src/replay.rs:policy_from_trace:61`). Built-in compaction (2.1) and Python/TS-configured policies need both.
- Design:
  - `EngineBuilder::policy(config: PolicyConfig)` where `PolicyConfig` is opaque JSON with a `kind` string tag (open string set — narrow waist) plus the derived tool/permission/background lists the builder already computes; the engine records it as `trace.policy` exactly as today.
  - A pure `PolicyRegistry` in `hugr-core`: `register(kind: &str, decode: fn(&Value) -> Option<Box<dyn TurnPolicy>>)`; `decode_policy` becomes a lookup over built-in kinds (`static`, `budget` from 2.1) and hosts can register more; `hugr-replay::policy_from_trace` takes an optional registry so custom-policy traces still verify.
  - Manifest: no new section yet (2.1 adds `[context]` which maps onto this).
- Tests: engine round-trip with a custom test policy; replay/verify of a trace recorded under a non-static policy.
- Invariant check: policies remain pure (`project_context` must not do IO — document loudly on the registry); the registry is data + fn pointers, no environment.

### 1.4 `[x]` Agent event-stream API (prerequisite for 3.1, 3.2; gives CLI streaming for free) — M

- Why: idea 13 wants `async for event in agent.run()`. Today the only observation seams are the `Frontend` trait (`hugr-host/src/frontend.rs:14-43`, callbacks, no stream) and `EventSender` (injection only). A first-class event stream in `hugr-agent` serves Python, TS, and a CLI `--stream` mode with one mechanism.
- Design: `Agent::ask_events(ask) -> (impl Stream<Item = AgentEvent>, JoinHandle<Result<Answer, AskError>>)` implemented with a channel-backed `Frontend`; `AgentEvent` is a host-layer enum (serializable, `#[serde(tag = "type")]`): `AskStarted { trace_parent }`, `ModelStarted { op, tier }`, `TextDelta { op, text }`, `ModelEnded { op, usage }`, `ToolStarted { op, name, args }`, `ToolEnded { op, name, is_error }`, `Notice`, `LimitTripped`, `Done`, `AnswerReady { answer }`. Payload details ride opaque `Value` fields — the enum only types what surfaces branch on (render vs finish), mirroring the narrow-waist rule at the host level.
  - `Agent::ask` becomes a thin wrapper draining the stream.
  - CLI: `<agent> "q" --stream` prints NDJSON `AgentEvent`s on stdout followed by the final `Answer` line (machine-consumable), `--pretty` renders them live; default behavior unchanged.
- Docs: ARCHITECTURE §4 built-binary shape (+`--stream`), §5 contract note (events are observability, never the contract — `Answer` remains the one product).
- Tests: scripted ask with a fake adapter asserting the exact event sequence; conformance test extended with `--stream`.
- Invariant check: events are derived host-side observations; nothing new enters the core or the trace.

### 1.5 `[x]` Shared blob store, zero-copy exchange, parent↔child forwarding (idea 6) — L

- Why: blobs can be huge (datasets). Today every agent has a private store at `store.root()/.blobs` (`agent.rs:112`); inbound blobs are byte-copied into scratch (`blobs.rs:materialize_inbound:70`), outbound files are read+rewritten into the store (`blobs.rs:sweep_outbound:93`), and agent-as-tool forwards no blobs at all (explicit TODO, `runtime.rs:394`). A parent handing a 10 GB dataset to a child currently can't, and would copy it if it could.
- Design (three independent pieces):
  - **Shared store**: default `BlobBackend` root becomes `~/.hugr/blobs/` (global, content-addressed — dedup across *all* agents by construction since keys are `sha256:<hex>`). Per-agent override stays possible via 1.2. Add two-level sharding (`sha256-ab/sha256-abcd...`) while relocating, since a global store will actually accumulate.
  - **Zero-copy paths**: `materialize_inbound` hardlinks from the blob store into scratch when same-filesystem, falling back to copy (`std::fs::hard_link` then fallback); `sweep_outbound` and `BlobBackend::put_file` hash the file streaming, then hardlink/rename into the store instead of read-all+write. Scratch stays writable-safe because blob-store files are set read-only and a tool writing "through" a hardlink is the same trust boundary as today's scratch (document it; if it proves sharp, switch to reflink/copy — decision recorded in the threat note).
  - **Parent↔child forwarding**: extend the `agent_<name>` tool schema (`agent_tool.rs:schema:67`) with optional `blobs: [BlobHandle]`; the subprocess resolver (`runtime.rs:run_subprocess_agent:395`) passes `--blob sha256:<hash>` args (CLI already takes `--blob <path>`; add the `sha256:` ref form) and sets `HUGR_BLOB_STORE=<shared root>` so the child resolves refs from the same store — no bytes cross the process boundary. Child `Answer.blobs` (already `Sha256` refs) flow back into the parent's tool result unchanged and are resolvable by the parent for the same reason.
- Docs: ARCHITECTURE §5 (BlobHandle materialization semantics), §8 (composition — blobs now compose), §16 (blob store location), threat note (shared store is cross-agent readable by hash — hashes are unguessable but not secrets; an agent can only obtain hashes it was handed or created).
- Tests: hardlink + fallback behavior; parent→child→parent blob round-trip with zero byte duplication asserted (same inode); dedup across two different agents.
- Depends on: 1.1 (`~/.hugr`), 1.2 (`BlobBackend`).

---

## Phase 2 — Runtime features

### 2.1 `[x]` Built-in, configurable compaction / forget (idea 9) — XL (split: 2.1a M, 2.1b L, 2.1c M)

- Why: the only compaction in the project is the wasm POC in `bindings` JS (`openai_adapter.js`: stage-1 relevance prune of stale page observations, stage-2 deterministic token-threshold truncate+summarize) — edge-only, invisible to the trace's `ContextPlan`, not reusable. The core already has the right seams: `TurnPolicy::project_context(log, budget) -> ContextPlan` (`policy.rs:51`), `ContextDisposition::{Included, Omitted}` (`model.rs:176`), `TokenBudget` threaded but never enforced (`StaticPolicy` includes everything), `ContextBudgetTotals` built to make truncation visible.
- **2.1a — Deterministic compaction policy (pure, in `hugr-core`)**:
  - New `ContextDisposition` variants: `Truncated { block }` (content clipped to a per-block cap, with a deterministic elision marker) and `Dropped { note: Option<String> }` (replaced by nothing or a one-line structural note rendered into a synthetic block — the generalization of the POC's "forgotten stale observations" system note). `to_model_request` renders `Included`/`Truncated`/notes; totals track used vs truncated vs dropped.
  - New `BudgetPolicy` (wraps the static projection): config `{ budget_tokens, trigger_tokens, keep_recent_tokens, max_block_tokens }`. Algorithm (deterministic, replay-safe): always keep system + the most recent turns within `keep_recent_tokens`; when the projected total exceeds `trigger_tokens`, walk oldest-first truncating heavy tool results to `max_block_tokens`, then dropping whole turn-groups (assistant tool-call + its results stay atomic, reusing the existing grouping logic) until under budget; emit one summary-note block listing what was dropped. Pure string manipulation — no model call, no clock.
  - **Forget rules** (the POC's stage 1, generalized and config-driven): `{ tool_ttl: { <tool_name>: <turns> }, keep_last_per_tool: { <tool_name>: <n> } }` — drop a tool result once N newer turns exist or once a fresher result from the same tool arrived. Tool names are open strings (narrow waist). The browser example configures `page_snapshot: keep_last 1` and gets the POC behavior back.
  - Manifest: new `[context]` section (`budget_tokens`, `compaction = "none" | "truncate"`, `trigger_tokens`, `keep_recent_tokens`, `max_block_tokens`, `[context.forget]` maps) parsed in `manifest.rs`, mapped to a `PolicyConfig { kind: "budget", ... }` via 1.3, serialized into `trace.policy` so `verify()` replays identically.
- **2.1b — Model-backed summarization (the only acceptable shape: through the event loop)**:
  - When deterministic compaction can't reach budget (or config says `compaction = "summarize"`), the brain — told by the policy via a new pure signal on `ContextPlan` (`wants_summary: Option<SummaryRequest { up_to: Seq, selector }>`) — issues a `StartModelCall` with the configured summarizer tier *before* the main call, and appends the result as a new `Record::ContextSummary { op, replaces_up_to: Seq, text, est_tokens }`. Projection then renders the summary block instead of everything ≤ `replaces_up_to`. The log keeps every original record (the log is truth; compaction changes projection only) and replay is bit-for-bit because the summary output is an ordinary recorded model event.
  - Core changes: one `Record` variant + reducer arm + policy signal; each ships with scripted command-sequence tests and a replay test (per AGENTS.md conventions). Summaries cost money → they show up in `AnswerMeta` like any model call, under their own tier selector.
- **2.1c — Adoption**: browser package (0.3) switches from the adapter-side POC to `BudgetPolicy` (the POC's request-shaping is then deleted from the generic package — one way to do each thing); Python/TS expose `[context]` config verbatim (3.1/3.2); `--describe` includes the context config.
- Docs: ARCHITECTURE — §16 currently says "no in-session summarization/compaction machinery; the projection includes the log" → rewrite; new §"Context management" (deterministic first, model-backed second, log-immutability guarantee); manifest §6; risks table row.
- Tests: golden `ContextPlan` fixtures across configs; determinism tests re-feeding event streams; a long-session scripted test proving the projection shrinks while the log grows; replay of a summarizing session.
- Depends on: 1.3.

### 2.2 `[x]` Shared memory between runs (idea 2) — M

- Why: scratch is per-lineage copy-on-fork (`agent.rs:prepare_scratch:414`) — siblings and unrelated runs never share notes. Some agents want a durable, agent-wide memory (e.g. "remember the docs layout I discovered last week").
- Design: an opt-in library tool, not a change to scratch semantics:
  - Manifest grant `[tools.memory]` with optional `readonly = true`; registers `memory_read` / `memory_write` / `memory_list` — same jail discipline as `ScratchDir` (component-wise rejection + post-canonicalize `starts_with` re-check), rooted at `<agent home>/memory/` (1.1), backed by `ScratchBackend`-style storage via 1.2 so it swaps with the rest.
  - Shared mutable state across concurrent asks: last-write-wins with an advisory file lock per write; documented, not "solved" — memory is for notes, not coordination.
  - Never copied per-ask, never entering the trace as content (tool results carry relative paths + read bytes like scratch results do).
- Threat note (ARCHITECTURE Part IV): memory is a *persistence channel for prompt injection* — content written under one ask influences every future ask; that is its purpose, and the mitigation is the grant being opt-in, `readonly` mode for consumer agents, and memory being wipeable (`rm -rf ~/.hugr/<name>/memory`, plus `hugr traces`-adjacent CLI listing in 2.4).
- Docs: §3 (infrastructure list), §6 manifest, §7 tool library, Part IV note; SYSTEM.md template mention in the reference manifest.
- Tests: jail tests (mirror `scratchpad.rs` suite); two sequential asks sharing state; fork isolation of scratch unaffected; readonly mode enforced.
- Depends on: 1.1; nicer after 1.2.

### 2.3 `[x]` Feedback mechanism (idea 1) — M

- Why: an orchestrator that just used a subagent knows whether the answer helped; capturing that beside the trace enables offline improvement (4.1). Explicitly *not* consumed in real time.
- Design:
  - Contract type in `hugr-agent`: `Feedback { trace_id: TraceId, payload: Value, created_at }` — payload fully opaque (score, text, "this was what I asked / this was not", whatever the caller wants; the framework never interprets it).
  - Storage: append-only sidecar under the agent home — `<home>/feedback/<trace_id>.jsonl`, one JSON line per feedback event. Traces stay immutable; feedback is keyed *to* a trace, never *in* it. Behind `TraceBackend`? No — its own tiny `FeedbackStore` (fs + mem impls) so trace verify/replay never sees it.
  - Surfaces (all thin wrappers over `Agent::feedback(Feedback)`): built binary `<agent> --feedback <trace_id> [--json '<payload>' | reads stdin]`; MCP: a second tool `feedback` beside `ask` in `--mcp-serve`; agent-as-tool: the `[tools.agent.<name>]` grant registers a sibling capability `agent_<name>_feedback` (args: `trace_id`, `payload`) so a *parent model* can file feedback right after a delegated call — subprocess resolver maps it to `--feedback`; Rust/Python/TS: `agent.feedback(trace_id, payload)`.
  - Reading it back: `<agent> --traces` and `hugr traces` annotate each head with its feedback count; `hugr stats` (2.4) folds it; raw access is just the JSONL files (4.1 reads them).
- Docs: §5 contract (feedback is the one asynchronous back-channel; never load-bearing for an answer), §8 composition, Part IV note (feedback payloads are untrusted text from the caller's model — anything consuming them (4.1) treats them as attacker-controlled).
- Tests: CLI + MCP + capability round-trips; append-only property; unknown trace_id → error answer.
- Depends on: 1.1 (home layout). Real-time feedback consumption: Won't-do W1.

### 2.4 `[x]` Detailed analytics (idea 5) — M

- Why: every number already exists in traces — `Record::OpEnded` carries `OpMeta { started_at, ended_at, model: Option<selector>, usage }` per op, and child-agent answers (with their own `AnswerMeta`) are recorded as `agent_<name>` tool results — but nothing aggregates them.
- Design: a pure fold over a `TraceBackend`, in `hugr-agent` (`analytics.rs`), exposed two ways: `hugr stats <agent-dir> [--since <trace_id>] [--json]` and built-binary `<agent> --stats`. Computed per agent (and per trace with `--trace <id>`):
  - Per-ask: cost, duration, tokens in/out, model_calls, tool_calls (recompute via the existing `meta_from_trace` fold, `agent.rs:906-931`).
  - Per model tier: calls, tokens in/out, cost (selector is on `OpMeta.model`).
  - Per tool: call count, error count, total/mean latency (`OpEnded` spans; name from `ToolResult`/`InflightOp`).
  - Per child agent — **never nested** (idea 5's constraint): a child's cost is attributed to the direct `agent_<name>` tool call that produced it (read from the recorded child `Answer.metadata`), reported as `cost_delegated` per child name; the agent's own line reports `cost_own` — grandchildren are already folded into the child's number by `merge_child` and are *not* re-walked.
  - Aggregates: totals, mean/median/p95 across traces, ask count, feedback count (2.3).
  - Output: one JSON document (stable shape, documented) + `--pretty` table rendering.
- Docs: §4 built-binary shape (`--stats`), CLI section, a short "Accounting" subsection consolidating what `AnswerMeta` vs `hugr stats` each promise.
- Tests: golden stats over a fixture set of traces (crafted with fake adapter: multi-tier, tools, one child call, one error); `--stats` surfaced through conformance.
- Depends on: 1.1; benefits from 2.3.

### 2.5 `[x]` Cron jobs for an agent (idea 7) — M

- Why: "a prompt + a cron formula" — recurring asks (poll a feed, re-index docs, daily summary) without an external orchestrator.
- Design (host-layer only; the brain never sees a clock):
  - Manifest: `[cron.<name>]` sections — `schedule = "*/30 * * * *"` (5-field cron, parsed with the `croner` crate at load time so typos are manifest errors), `question = "..."`, optional `lineage = "fresh" | "chain"` (`chain` threads each run's `trace_id` as the next run's parent — a slowly-growing conversation; `fresh` default), optional per-job `[cron.<name>.limits]` overriding `[limits]` for these unattended asks.
  - Runtime: built binary `<agent> --cron-serve` (and dev `hugr cron <agent-dir>`) runs a small tokio scheduler: sleep-until-next-fire per job, each firing is an ordinary `ask` with `extra: {"cron": "<name>", "fired_at": ...}`, answer logged to stderr, trace persisted as always; overlapping fires of the same job are skipped with a notice (asks can be slow). No daemonization, no persistence of the schedule itself — the process *is* the scheduler; systemd/launchd own keeping it alive. `--cron-print` emits ready-to-paste crontab lines (`0 8 * * * /path/agent "question" --json >> log`) for people who prefer system cron — S, optional.
- Docs: §4 (new mode), §6 manifest, Part IV note: unattended asks make `[limits]` (especially `max_cost_micro_usd`) *strongly recommended* — the scheduler refuses to start a job with no cost cap unless `--allow-uncapped` is passed.
- Tests: schedule parsing errors; scheduler unit test with injected clock; end-to-end one-shot fire with fake adapter; overlap-skip behavior.
- Depends on: nothing hard; nicer after 1.1 (predictable trace location for consumers of cron output).
- Invariant check: the clock lives in the host scheduler; each fired ask is a normal deterministic session (`Tick`-stamped like any other).

---

## Phase 3 — Language surfaces

### 3.1 `[x]` Python runtime API (idea 13) — XL

- Why: define agents *in Python* — tools as Python callables, config as Python data, events as an async iterator — running on the same Rust runtime. This is the runtime-embedding surface from `PYTHON_RUNTIME_API_PLAN.md` (still architecturally valid; supersede that file into this plan + ARCHITECTURE when done), distinct from the existing per-agent wheel (`hugr build --surface python`, which stays: it *ships* an agent; this *defines* one).
- Design (updates the old plan where idea 13 goes further):
  - Crate `crates/hugr-python`, PyO3 `cdylib`, module published as… **naming task**: `hugr` on PyPI is taken (Quantinuum's HUGR IR); verify and pick (`hugr-agents` / `pyhugr`) — decide at implementation, the module import name can still be short.
  - `hugr.Agent(name=..., system=..., models={...}, tools=[...], limits={...}, context={...}, hooks=...)` — plain data mirroring `hugr.toml` sections 1:1 (same keys, so the manifest docs teach both surfaces); assembled directly onto `hugr_agent::Agent` (Option A of the old plan) with a parity test against `hugr-toolkit::runtime` assembly.
  - Tools: `@hugr.tool(name=..., description=..., schema={...})` on sync **and** async callables — a `PyCapability` implementing `hugr_host::Capability`; sync runs under `spawn_blocking` + GIL, async bridges via `pyo3-async-runtimes`; Python exceptions → semantic tool errors (`Err(Value)`), never panics. Explicit schemas required in v1 (auditability); signature inference later at most.
  - Run: `agent.ask(question, trace_id=None, blobs=None, extra=None) -> Answer` (blocking) and `async for event in agent.run(question, ...):` yielding typed events, ending with `AnswerReady` — a direct wrap of 1.4's `AgentEvent` stream. `agent.feedback(...)` (2.3).
  - Typing: hand-written `@dataclass`/`TypedDict` types in a pure-Python layer (`Answer`, `AnswerMeta`, `BlobHandle`, `Feedback`, every `AgentEvent` variant, config TypedDicts), `py.typed`, mypy/pyright-clean; field names identical to the JSON contract. Response contracts: `response_schema={...}` (JSON Schema dict) in v1; optional dataclass→schema convenience later.
  - Compaction/hooks/config: `context={...}` maps to the `[context]` policy config (2.1); `ask_hooks`/`answer_hooks` as Python callables mutating typed `Ask`/`Answer` (host-side, mirrors `AskHook`/`AnswerHook`); storage defaults to `~/.hugr/<name>` via the same resolution (1.1), overridable, with `StorageOverrides` bridging (1.2) as a v2 item.
  - Replay: recorded runs verify without importing Python (capability results are recorded events); `hugr` CLI `verify` works on Python-produced traces.
  - Packaging: maturin, abi3 wheels, CI job (Linux/macOS to start); workspace membership optional-by-default so `cargo test` doesn't require Python.
- Docs: ARCHITECTURE §4.1 rewritten ("language surfaces" → generated wrapper *and* runtime embedding, and why both exist), security note verbatim from the old plan (Python callables are trusted host code — Hugr jails what the *model* can invoke, not what your Python does); delete `PYTHON_RUNTIME_API_PLAN.md`; README section.
- Tests: fake-adapter suite driving Python tools (sync + async), resume/fork from Python, errors-as-answers, event-stream ordering, trace parity (same session via Python API and via manifest produces equivalent logs), wheel smoke test in CI.
- Depends on: 1.3, 1.4; benefits from 1.1, 2.1.

### 3.2 `[x]` TypeScript runtime API (idea 14) — XL

- Why: same as 3.1 for TS — full agent definition in TypeScript (not just shelling to a binary), for both Node and the browser. The wasm split (0.3) is the foundation: one WASM core artifact + a TS host.
- Design:
  - Package in `bindings/typescript/` (npm name task: check `hugr`, else scope) with subpath exports: the core WASM bindings (built from `crates/hugr-wasm` via the existing wasm-bindgen pipeline), `hugr/node` (fs-backed `TraceStore`/`BlobStore`/scratch + `process.env` config), `hugr/browser` (the IndexedDB store from 0.3, `fetch` everywhere).
  - `new Agent({ name, system, models, tools, limits, context })` — same config keys as the manifest and 3.1; tools are `{ name, description, schema, invoke(args): Promise<Result> }` objects (the capability-dispatcher interface extracted in 0.3); the driver is the typed evolution of `agent_driver.js`.
  - `agent.ask(q, opts): Promise<Answer>` and `for await (const event of agent.run(q, opts))` — the TS driver emits the same `AgentEvent` shapes as 1.4 (one documented event vocabulary across Rust/Python/TS; since the TS host drives the brain itself, it constructs them host-side).
  - Model adapter: the generic fetch-based OpenAI-compatible adapter from 0.3, typed, with the same retry rules as `hugr-providers` (429/5xx only, exponential backoff).
  - Traces: same `Trace` JSON format (`format_version` checked); storage behind a TS `TraceStore` interface with node-fs and IndexedDB impls; **`verify` in TS** by exposing `hugr-replay`'s pure fold through the wasm bindings (enabled by the `fs` feature-gating in 1.2) — TS-recorded traces are replayable by the Rust CLI and vice versa (add a cross-verification fixture test in CI).
  - Types: generated `.d.ts` from wasm-bindgen for the core boundary + hand-written TS types for the contract (`Answer`, `AnswerMeta`, `BlobHandle`, events, config), kept in lockstep with the JSON contract by a fixture test.
  - Compaction: config passthrough to `BudgetPolicy` (2.1) — the policy runs inside the WASM brain, so TS gets it for free.
- Docs: ARCHITECTURE §4.1 + §9 (bindings/ layout), README; the chrome-extension example (0.3) migrates onto this package when it lands.
- Tests: node test suite with a mock model server (ask, resume, fork, tool errors, events); browser smoke via the extension example; cross-language trace verification fixtures.
- Depends on: 0.3, 1.3, 1.4, 2.1a (for config passthrough), 1.2 (`fs` feature gating in hugr-replay).

---

## Phase 4 — Self-improvement, docs & DX

### 4.1 `[x]` Insights agent: analyze traces + feedback to improve a subagent (idea 3) — L

- Why: with traces (always) and feedback (2.3) accumulated under `~/.hugr/<name>/`, a Hugr agent can mine them for patterns — repeated tool sequences that should be one tool, recurring questions that belong in the prompt, common failure feedback — closing the loop offline (never real-time).
- Design (an *example*, dogfooding the framework — not framework code):
  - New library tool `traces_read` first (framework piece, S/M): read-only capability family jailed to a traces root — `trace_list` (heads), `trace_ops(id)` (op sequence with names/durations/costs — *summaries*, not raw logs, to keep context small), `trace_transcript(id, range)` (paged), `feedback_list(id)`. Rationale: raw trace JSON via `fs_read` would blow any context budget; this is the same "domain tools beat generic ones" pitch Hugr makes. Threat note: trace content is attacker-influenced (it contains model/tool output), and feedback doubly so.
  - `examples/hugr-insights/`: agent crate granted `[tools.traces_read] root = "~/.hugr/hugr-docs"` (runtime arg, like `docs_path`) + typed response contract `InsightsResponse { patterns: [...], prompt_suggestions: [...], tool_suggestions: [...], feedback_themes: [...] }`; SYSTEM.md teaches the mining method. Runbook: `hugr run examples/hugr-insights ~/.hugr/hugr-docs "What should hugr-docs improve?"`.
  - Suggestions are a report for a human (or an orchestrator) — auto-applying them is explicitly out (W1 adjacency: no self-mutation loop).
- Docs: §7 tool library (+`traces_read`), example README, Part IV note.
- Tests: `traces_read` jail + pagination tests over fixtures; example smoke test with fake adapter.
- Depends on: 2.3, 2.4 (fixtures/shape), 1.1.

### 4.2 `[ ]` Tutorials (idea 15) — L

- Why: didactic, narrative on-ramps per surface — the README/ARCHITECTURE are reference, not teaching.
- Location: `docs/tutorials/` (the empty `docs/` dir finally earns its keep); each tutorial is standalone, tested-where-possible, single-line-markdown convention.
  - `01-first-agent-cli.md` — `hugr new` (weather), manifest anatomy, run, resume/fork, `--describe`, build → one binary. (Available now.)
  - `02-typed-responses-and-hooks.md` — `RESPONSE_RUST_TYPE`, `MODEL_RESPONSE_RUST_TYPE`, answer hooks, using hugr-docs as the worked example. (Available now.)
  - `03-first-chrome-extension.md` — build a *different* extension than the shipped example from the browser package. (After 0.3.)
  - `04-agent-binary-from-python.md` — `hugr build --surface python`, the typed wheel, subprocess/MCP alternatives. (Available now.)
  - `05-agent-entirely-in-python.md` — the 3.1 runtime API end-to-end. (After 3.1.)
  - `06-agent-entirely-in-typescript.md` — the 3.2 API, node + browser variants. (After 3.2.)
  - `07-composition-and-cost.md` — agents-as-tools, blob passing, feedback, `hugr stats`. (After 1.5/2.3/2.4.)
  - `08-traces-replay-debugging.md` — trace anatomy, `hugr replay --step`, `verify`, cron + insights workflow. (Mostly available; finish after 2.5/4.1.)
- Steps: write 01/02/04 immediately; others gated on their features; add a CI job that extracts and runs the shell blocks from 01 (doctest-style smoke) where secrets aren't needed.
- Docs: README links the tutorial index; AGENTS.md "one doc" rule amended: ARCHITECTURE stays the *spec*; tutorials are teaching material and must not restate spec (link instead).

### 4.3 `[ ]` Skills for building Hugr agents (idea 16) — M

- Why: hugr must be agent-first — a coding agent dropped into any repo should be able to build a Hugr subagent without reading the whole spec. Agent skills are the delivery vehicle.
- Design: in-repo `.agents/skills/` (checked in, so contributors' agents get them; installable elsewhere by copy):
  - `hugr-build-agent/SKILL.md` — the main skill: scaffold, manifest schema cheat-sheet (every section incl. `[context]`, `[cron]`, `[tools.memory]` as they land), tool library + jail semantics, typed contracts, run/build/traces/replay commands, packaging, troubleshooting (missing key env, maturin absent, etc.).
  - `hugr-python/SKILL.md`, `hugr-typescript/SKILL.md`, `hugr-chrome-extension/SKILL.md` — per-surface skills, gated on 3.1/3.2/0.3.
  - `hugr-debug-traces/SKILL.md` — replay/verify/stats/insights workflow.
- Keep each skill short, imperative, example-heavy; skills reference tutorials for narrative and ARCHITECTURE for rationale — never duplicate spec.
- Steps: write `hugr-build-agent` + `hugr-debug-traces` now; others land with their features; add a docs-sync checklist item (a manifest change is not done until the skill's cheat-sheet is updated — add to AGENTS.md "done" definition).

### 4.4 `[ ]` Additional ideas (mine — proposed, each independently droppable)

- `[ ]` **Trace GC** (resolves open question §23) — S/M: `hugr traces gc <agent-dir> [--keep-days N | --keep-last N] [--dry-run]`; deletes only *leaf* traces (never a `depends_on` target — lineage stays intact), sweeps orphaned scratch dirs and unreferenced blobs (refcount by scanning trace blob manifests; shared store makes this a mark-and-sweep across all agents' traces). Explicit command only — no automatic deletion.
- `[ ]` **Eval harness** (`hugr eval`) — M: `evals.toml` beside the manifest (`[[case]] question / expect.path / expect.contains / expect.status / max_cost_micro_usd`); runs each case as a normal ask (live or against a recorded-response fake adapter for CI), reports pass/fail + cost table, exit code for CI. The natural companion to 4.1: insights propose, evals verify. Regression story: pin a case's trace and assert replay equivalence.
- `[ ]` **Anthropic-native provider adapter** — M: `hugr-providers::AnthropicAdapter` (Messages API streaming, tool use, same retry rules); proves the `ModelAdapter` seam with a second real implementation and unlocks non-OpenAI-compatible endpoints. Registered per tier via a `provider = "anthropic"` key on `[models.<tier>]` (open string, default `openai`).
- `[ ]` **Release pipeline** — M: tag-driven GitHub workflow — crates.io publish order (core → replay → host → providers → agent → toolkit), `hugr` CLI binaries (linux/macos artifacts), Python wheels (3.1), npm package (3.2). (Distinct from Deferred D2, which is HF-Hub-specific distribution.)
- `[ ]` **CI additions** — S: run the `#[ignore]`d conformance/build_cli suites in a nightly/weekly workflow (they're the real gates and currently never run in CI); add `cargo deny`/`audit`; extend the sans-IO canary with a `cargo tree -p hugr-core` allowlist check (catches non-wasm-visible deps too).
- `[ ]` **`code_exec` sandboxed capability** — L (already designed in ARCHITECTURE §7 as the one future exec exception): pinned interpreter, cwd = scratchpad, no network, output caps; keep last in line — it's the highest-risk tool and nothing above depends on it.

---

## Deferred (the "at some point" items — listed, not planned)

- **D1 — Android surface** (idea 18): a JNI/UniFFI host around the same core; blocked on nothing architecturally (the wasm host proves the pattern) but no current need. Revisit after 3.2 (the mobile story likely reuses the TS/WASM work via React Native or a Kotlin host).
- **D2 — Hugr on the Hub** (ideas 19–22): store traces in HF buckets (a `TraceBackend` impl — 1.2 makes this a clean plugin, likely living outside this repo); run agents in HF Jobs/sandboxes; GitHub Action producing a binary per commit → bucket with xet dedup, commit-hash + tag/branch aliases. Prereqs all land in this plan (1.2 backends, release pipeline, `--stats`); the Hub pieces themselves stay out until wanted.

## Won't do (kept apart — would break the key rules, or explicitly excluded)

- **W1 — Real-time feedback consumption**: feedback (2.3) is never read during an ask and never alters a live session; analysis is offline (4.1). (Explicitly excluded in idea 1; also protects determinism and the one-way Ask/Answer door.)
- **W2 — Concrete Postgres / browser-localStorage / cloud storage backends in this repo**: 1.2 ships the traits + fs + in-memory reference impls (and 3.2 the IndexedDB TS impl); anything heavier is written *in an agent implementation* via `storage()` — that extensibility is the requirement in idea 8, not a Postgres driver dependency in the framework.
- **W3 — Compaction that rewrites the durable log**: "forget" only ever changes the projection (2.1); the log/trace stays append-only and immutable. Any design that summarizes-then-deletes records is rejected — it breaks replay, fork, and audit.
- **W4 — Model-backed summarization outside the event loop**: an adapter or host silently calling a model to compact (as the wasm POC's *shape* would suggest if generalized) hides an unrecorded model call from the trace; the only acceptable shape is 2.1b (a `StartModelCall` command + recorded `Record::ContextSummary`). The deterministic parts of the POC are absorbed by 2.1a instead.
- **W5 — Environmental anything in `hugr-core`**: no storage traits, cron clocks, Python/TS types, or async in core. All surfaces in this plan are hosts.
- **W6 — A `shell` tool / bespoke plugin protocol / second external-tool escape hatch**: unchanged; MCP remains the only external-process escape hatch, the library stays exec-free (`code_exec` in 4.4 is the designed, sandboxed exception and is not a shell).
- **W7 — Per-agent generated Python packages as the "Python API"**: the runtime API (3.1) does not replace or restore per-agent codegen beyond the existing `--surface python`; one generic runtime package, per the old plan's recommendation.

## Doc-sync master checklist (rolled up from the tasks)

- `ARCHITECTURE.md`: crate layout §9 (examples/, bindings/, hugr-wasm slimmed); §4 surface shape (`--stream`, `--stats`, `--feedback`, `--cron-serve`); §4.1 language surfaces rewritten (generated wrappers + runtime embeddings: Python, TS); §5 contract (feedback back-channel, blob zero-copy semantics); §6 manifest (`[context]`, `[cron.*]`, `[tools.memory]`, home-dir defaults); §7 tool library (`memory`, `traces_read`); §8 composition (blob forwarding, `agent_<name>_feedback`); new "Context management" section (2.1) replacing the §16 "no compaction" sentence; §16/§19 storage (backend traits, `~/.hugr` layout, shared blob store); Part IV new threat notes (memory, feedback, traces_read, storage backends, cron caps, hardlink note); §23 open questions — remove "Storage backends" and "Browser packaging" (resolved), add trace-GC resolution, keep schema-migration.
- `README.md`: quickstart paths (`~/.hugr`), crate/bindings/examples layout, hugr-docs path → `examples/hugr-docs`, new features one-liner each, tutorial links.
- `AGENTS.md`: project layout (examples/, bindings/); comment conventions rewritten (0.4); new_ideas.md ↔ plan.md loop (0.5); "done" definition includes skills cheat-sheets (4.3); command list (`hugr stats`, `hugr cron`, `hugr eval`, `hugr traces gc`).
- Delete when superseded: `PYTHON_RUNTIME_API_PLAN.md` (into 3.1), `HUGR_WASM_PLAN.md` (into 0.3 + ARCHITECTURE).
