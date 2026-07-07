# Roadmap & Progress

> Companion to `ARCHITECTURE.md` (which describes the **target** state — treat it as the spec). This file is both the progress log and the work plan. The current work is **the trim**: the project grew "platform" features ahead of need; we are cutting it back to the product — *a definition folder becomes one self-contained binary that answers, persists forkable traces, and serves `--mcp-serve`*.

## 1. Where the project stands

Built and working (pre-trim):

- **`hugr-core`** — the sans-IO brain: turn loop, log + projection, op table, cancellation, deterministic replay. Also carries pre-pivot subsystems slated for deletion (tier routing, skills, plan/todo, hooks, compaction, model-override, stale-edit CAS, in-core sub-agent ops).
- **`hugr-host`** — tokio engine (`Engine`/`EngineBuilder`, resume, checkpointing), uniform `Capability`/`ModelAdapter` registries, MCP stdio client. Also carries a pre-pivot capability set, frontends, policies, scheduler, and skills loader slated for deletion.
- **`hugr-providers`** — OpenAI-compatible streaming adapter with retries (the live model path).
- **`hugr-replay`** — the trace format (`Trace { meta, events, log, commands, blobs, children }`), content-addressed `BlobStore`, replay/verify/inspect. Kept nearly as-is.
- **`hugr-agent`** — `Agent::ask`: Ask/Answer contract, `TraceStore` with `trace_id`/`depends_on` + fork, scratchpad with copy-on-fork, blob exchange, `[limits]` enforcement, cost accounting, agent-as-tool.
- **`hugr-toolkit`** — `hugr.toml` + `SYSTEM.md` parsing, the tool library (`fs_read`, `http_fetch`, `sqlite_query`, scratchpad wiring), `hugr new/run/build/traces/replay/verify`, definition bundling into a CLI binary, `--mcp-serve`.
- **`hugr-docs`** — the reference subagent, ported to run on a checked-in definition folder over the shared runtime; ships a PyO3 wheel (the only CI workflow).
- Pre-pivot crates still present: `hugr-cli` (coding-agent REPL), `hugr-wasm` (+ Chrome extension), `hugr-plugin-abi` + `hugr-example-plugin` (bespoke subprocess plugin protocol).

Everything below in §2 is the plan to get from this state to the `ARCHITECTURE.md` state.

## 2. The trim

### Ground rules (read first)

- **Breaking changes are fine.** No deprecation shims, no serde back-compat fields, no `#[non_exhaustive]` ceremony added for stability's sake. Delete, don't wrap.
- **One way to do each thing.** When two mechanisms do the same job, keep the one the live stack (`hugr-agent`/`hugr-toolkit`/`hugr-docs`) uses and delete the other.
- **No enum nobody branches on.** If code only stores/serializes/displays an enum, replace it with a `String` (or delete it). Error enums matched with `?`/`thiserror` are fine.
- **One commit per phase.** After each phase: `cargo fmt --all`, `cargo clippy --all-targets` clean, `cargo test` green, `cargo tree -p hugr-core` free of tokio/reqwest/fs. Delete the tests of deleted features; keep and adapt the tests of surviving behavior. Line numbers below are indicative (from the audit); trust symbol names.
- `ARCHITECTURE.md` already describes the target. If an instruction here conflicts with obvious reality in the code, prefer the smallest change that reaches the ARCHITECTURE.md behavior, and note the deviation in §3's log.

### Phase 1 — Delete the pre-pivot crates and dead weight ✅ DONE

Goal: remove whole crates with no live consumers. ~2.9k LOC.

1. Delete `crates/hugr-cli` entirely. Its 6 tests only guard its own private helpers (MCP spec string parsing, `/diff` arg splitting, commit-message helpers) — no engine behavior. Remove it from the workspace `Cargo.toml` members.
2. Delete `crates/hugr-wasm` entirely, including the `extension/` JS host, icons, and the checked-in `hugr_wasm_bg.wasm`. Its tests only re-check serde round-trips already covered in `hugr-core`.
3. Delete `crates/hugr-example-plugin` entirely.
4. Dismantle `crates/hugr-plugin-abi`: **first move `framing.rs`** (JSON-line framing, ~58 LOC) into `hugr-host` (e.g. `crates/hugr-host/src/framing.rs`) — it is load-bearing for MCP: `hugr-host/src/mcp.rs` and `hugr-toolkit/src/mcp_serve.rs` import `write_json_line`/`read_json_line` from it. Update those imports. Then delete the crate (`protocol.rs`, `subprocess.rs`, `transport.rs`, `wasm.rs`).
5. Delete the plugin tool path everywhere: `hugr-host/src/plugins.rs` (`load_subprocess`, `PluginCapability`), the `[tools.plugin.*]` manifest key (`ToolKind::Plugin` in `hugr-toolkit/src/manifest.rs`, its dispatch arm in `runtime.rs`, its commented block in `reference/hugr.toml`, its scaffold-template mentions). MCP is the only external-tool escape hatch now.
6. Delete the untracked local junk: `archive-light-2026-07-01/` (regenerable demo corpus), empty `.agents/` and `.codex/` dirs. Update the two README example commands that referenced `archive-light-2026-07-01` to point at any docs folder.
7. Exit: workspace builds and tests green with 7 crates (`core`, `host`, `providers`, `replay`, `agent`, `toolkit`, `docs`); `grep -ri "plugin" crates/` finds no tool-path references.

### Phase 2 — Gut `hugr-host` and `hugr-providers` to the live slice ✅ DONE

Goal: the live stack imports only `Engine`/`EngineBuilder` (+ `resume`), the `Capability` trait + `ChunkSink`, `ModelAdapter`/`ModelSink`, the `Frontend` trait, `AllowAll`, and the MCP loader (`mcp::load_stdio`). Delete the rest. ~4k LOC.

1. Delete `crates/hugr-host/src/capabilities/` entirely (`shell.rs`, `fs.rs`, `http.rs`, `repo.rs`, `patch.rs`, `verify.rs`, `blob.rs`, `mod.rs`) — all consumers were `hugr-cli` or host tests. The toolkit's `tools/` library is the one tool set. Keep `capability.rs` (the trait + registry).
2. In `frontend.rs`, delete `StdoutFrontend`, `Metrics`, and all ANSI rendering; keep the ~40-line `Frontend` trait (the agent runtime implements a silent frontend).
3. Delete `coalesce.rs` and the unconditional `Coalescer` wiring in `engine.rs` — it existed only to batch deltas for a rendering frontend.
4. Delete `scheduler.rs` (cron), `skills.rs` (skills loader), `spend.rs` (spend report) — all `hugr-cli`-only.
5. In `policy.rs`, delete `AutoApprove` (judge-model call) and `Interactive` (terminal prompt). Collapse what remains: the host answers `RequestPermission` with `Allow` always (library tools are ungated; the sandbox is registration). If that makes the `Policy` trait single-impl, delete the trait and inline the allow.
6. Delete the host's in-process sub-agent runner: `src/agent.rs` (`AgentHost`, `run_agent`, `ChildTraceSink`) and the `EngineBuilder` surface for it (`AgentDefaults`, `agent()`, `agent_with_defaults()`, `max_agent_depth`, child-trace recording hooks). The live delegation path is `hugr-agent`'s `agent_tool.rs` (Phase 4).
7. In `engine.rs`, delete the parked/dead knobs: `CrashResumePolicy` and `TraceCompaction` (zero external consumers), `CheckpointCadence` if only the deleted CLI drove it (keep the simplest always-checkpoint behavior the agent runtime relies on). Also remove the unconditional `RoutingPolicy::new(base_policy)` wrap (near `engine.rs:1316`) — pass the policy through unchanged; `RoutingPolicy` itself dies in Phase 3. **This fixes a real bug:** the routing wrapper hardcodes `"small"`/`"big"` selectors and kills the turn of any single-custom-tier agent when a keyword heuristic fires.
8. In `crates/hugr-providers/src/openai.rs`, delete the dead second tier-config surface: `TierModelConfig`, `TierModelConfigSet`, `adapters_from_env`, `adapter_for_tier`, `hf_router_default`, `mapping_summary`, `with_all_models`. The toolkit builds adapters directly from the manifest (`OpenAiAdapter::new` + `with_base_url` + `with_default_params`). Keep the streaming adapter core, retries, and `with_max_attempts` (test-used).
9. Exit: `cargo test` green; `hugr run` on the docs definition still answers; grep confirms `hugr_host::capabilities` no longer exists.

### Phase 3 — Trim `hugr-core` to the subagent shape (in progress)

Goal: delete the pre-pivot subsystems and non-branching enum variants. The pivot crates consume core purely as serde data and never name any of these types. ~1.5k LOC plus tests. `hugr-replay` is touched only where noted.

1. ✅ **Tier routing — delete.** `RoutingPolicy`, `RoutingPhase`, `RoutingInputs`, `ToolRisk`, `is_small_text_task`/`is_big_text_task` and the routing-input assembly in `brain.rs`/`policy.rs`. `TurnPolicy::choose_model` shrinks to returning the policy's default selector.
2. ✅ **Skills — delete.** `SkillDescriptor`, `StaticPolicy::with_skills`, `activate_skill`, `Record::SkillActivated`, the projection arm rendering it, and `brain.rs` handling.
3. ✅ **Plan/todo — delete.** `Event::PlanAccepted`, `Event::TodoUpdated`, `Record::Plan`, `Record::TodoList`, `TodoItem`/`TodoList` types, their projection arms and reducer handling.
4. **Hooks — delete.** `HookPhase`, `Event::HookFired`, `Record::Hook`, projection arm, reducer handling. (Host-side: `engine.rs` self-fired hook constants go with it if Phase 2 didn't already remove them.)
5. **Model-override — delete.** `Event::ModelOverride`, `Record::ModelOverride`, the `next_model_override` state field and its `from_log` re-derivation in `state.rs`.
6. **Compaction — delete fully** (decided). `Event::CompactContext`, `Record::Summary`, `SummaryCoverage`, `CompactionTarget`, `OpKind::Compaction` and the compaction branches in `brain.rs` (`start_selected_compaction`, the auto high-water trigger, the `compaction_op` short-circuit), the `TurnPolicy` compaction hooks (`compaction_request`, `render_summary_record`, `select_compaction_span`, `extend_past_tool_group`), and `ContentPart::Ref` if compaction projection was its only producer (verify with grep first). `ContextDisposition` reduces to `Included`/`Omitted`.
7. **Stale-edit CAS — delete.** `VersionRef`, `Version`, `ObjectKey`, `ToolVersioning`, the `versions` read-set in `state.rs`, `capability_versioning`/`stamp_expected_version` in `brain.rs`, and the `Conflict` capability-error variant. Its only consumers were the host fs/patch capabilities deleted in Phase 2 (verify with grep before deleting).
8. **In-core sub-agent ops — verify, then delete.** With the host runner gone (Phase 2) and agent-as-tool running as an ordinary capability (Phase 4), grep for consumers of `Command::StartAgent`, `Event::AgentDone`/`AgentError`, `OpKind::Agent`/`OpState::Agent`, `AgentSeed`, `TurnPolicy::agent_seed`/`with_agents`, `resolve_seed`. Expected: only core tests (`tests/sub_agents.rs`). Delete them all, plus `Trace::children`/`ChildTrace` and the recursive child-verify in `hugr-replay` (nothing produces children anymore). **Keep `Brain::from_log`** — it is the resume/fork mechanism `hugr-agent` depends on.
9. **Enum cleanup.** `ModelSelector`: single-variant enum → newtype `pub struct ModelSelector(pub String)`. `SteerMode`: delete (`Queue` and `AppendAndContinue` were behaviorally identical; `UserInput` just queues; `UserAbort` stays). `StopReason`: brain never branches on it → plain `String` field (providers set it, trace records it). `ModelDelta`: drop the no-op `ToolCallArgsDelta`/`ToolCallEnd` variants (the adapter consolidates tool-call args internally; update `hugr-providers` accordingly). `OutputEvent`: reduce to `ModelText` and `Notice`. Drop `ContextSource::Synthetic` (never constructed) and delete `ContextCacheHint` (never attached).
10. Simplify the `ContextPlan` layer to what the reducer needs: entries with include/omit + token totals + `to_model_request()`; delete the per-entry reason/observability fields if nothing consumes them.
11. Update `StaticPolicy::project_context`'s big match (it loses the Skill/Plan/Todo/Hook/Summary arms) and every scripted test in `crates/hugr-core/tests/` that exercised deleted subsystems. Keep and re-pin the surviving replay/determinism tests — determinism remains the release gate.
12. Exit: `cargo test` green across the workspace; a fresh `hugr run` on the docs definition answers; a saved trace `verify()`s; `cargo tree -p hugr-core` clean.

### Phase 4 — One way per thing in `hugr-agent` / `hugr-toolkit`

Goal: one artifact, one composition mechanism, no speculative orchestration features, no non-branching enums. ~2.5k LOC.

1. **Surfaces collapse to the CLI binary.** In `hugr-toolkit/src/build.rs`: delete the `Surface` enum and the `crate` + `python` + `mcp` build paths (`build_crate`, `crate_cargo_toml`, `agent_crate_dir`, `LIB_RS`, `build_python`, `python_cargo_toml`, `pyproject_toml`, `python_lib_rs`, `PY_LIB_RS`). `Mcp` was already an alias of `Cli`; every built binary serves `--mcp-serve`. Keep `build_cli`, `bundle.rs` (the definition-embedding mechanism), `MAIN_RS`, `cli_cargo_toml`, `sanitize_crate_name`, `run_cargo`. Update `bin/hugr.rs` to drop `--surface`.
2. **One run path.** Keep `hugr run` (dev loop) and the built binary; both must call the same `run_ask`/`print_answer` helpers in `surface.rs` — delete the duplicated `print_answer`/`print_error`/`error_answer`/blob-handling helpers in `bin/hugr.rs`. Demote the programmatic `Agent` API to `pub(crate)`-ish internal status (stop documenting it as a user path in `lib.rs`), and fold `AgentBuilder` away: `runtime::build_agent` constructs the `Agent` directly instead of mirroring `EngineBuilder` field-by-field.
3. **Resource groups/grants — delete** (all layers): `ResourceRef`, `ResourceGroup`, `ResourceGrant`, `Access` in `contract.rs`; `Ask.groups`/`Ask.grants`; `GroupBinding`, `GroupCapabilityFactory`, `resolve_group_bindings`, `effective_grants`, `RecordedGrants` in `agent.rs`; `TraceHeader.grants`/`with_grants` in `store.rs`; `group_scope`, `library_group_binding`, `build_group_tool`, `resolve_resource` in `tools/mod.rs`; the `group:<name>` scope syntax in the manifest; `tests/resource_groups.rs`.
4. **Agent-as-tool — keep, subprocess-only.** In `agent_tool.rs`/`runtime.rs`: delete the interpreter path (running a child *definition folder* in-process) and the `build_agent_depth_with_provider_key` wrapper trio — collapse to one `build_agent(def)` entry. Keep the subprocess-artifact path (spawn the built binary, speak the CLI JSON contract, parse the `Answer`), the child-cost folding into `AnswerMeta` (`merge_child`), and the static depth stub that cuts cycles (`max_agent_depth`, default small). The manifest grant becomes `[tools.agent.<name>] artifact = "<path>"`.
5. **Answer-schema machinery — delete** (`extra` stays an opaque `Value`): `answer_schema.rs`, the `[answer]` manifest section + `AnswerConfig`, `resolve_answer_schema`, the ask-path lift/validate block, `Answer.warnings`.
6. **Config provenance — delete.** `ConfigProvenance`, `ConfigEntry`/`AgentConfig`, `effective_config` in `runtime.rs`, and the parallel fallback renderer in `agent.rs`. `--config` prints the parsed manifest as JSON with the API key env **name** shown and any resolved secret value redacted (~15 lines).
7. **Manifest simplification.** Delete the unknown-key warning machinery (`KnownKeys`, `warn_unknown_keys`, `warn_unknown_top_level`, `locate_key`, `Span`, `Warning`, and the warning plumbing threaded through runtime/surface/bin) — use `#[serde(deny_unknown_fields)]` for a hard error instead. Delete the parsed-but-never-applied `TierConfig.top_p` (or apply it; deleting is fine). Keep template vars (`{{agent_name}}`, `{{tools}}`, `{{date}}`).
8. **Contract de-ceremony.** Drop `#[non_exhaustive]` and the constructor/builder boilerplate on `Ask`, `Answer`, `AnswerMeta`, `BlobHandle` — plain `pub` fields, `Default` where useful. Keep `TraceId` as a newtype and `AnswerMeta::merge_child` (real logic). `AnswerStatus` enum → `status: String` (`"success"`/`"error"`; `OffTopic` was never produced — delete `status_wire`). Delete `BlobPerms`, the `perms` field, and `apply_perms` in `blobs.rs` (advisory-only mode bits inside a jail the agent owns).
9. **More enum→string / dead code.** `ToolPrivilege` and `PrivilegeClass` → plain `String` labels on tool descriptions. `LimitKind` → a string reason on the limit trip; merge `max_turns` into `max_model_calls` (they counted the same thing). Simplify cost accounting: keep total `cost_micro_usd`/`tokens_in`/`tokens_out`/`model_calls`/`tool_calls`; delete the `per_tier: Vec<TierSpend>` breakdown and `TierAccumulator` if nothing consumes it. Delete `TraceStore::prune`/`PrunePolicy`/`size`/`StoreSize`/`PruneReport` and the `hugr traces --prune/--size` CLI arms (manual deletion is fine for now).
10. Exit: `hugr new` → `hugr run` → `hugr build` produces a binary that answers, resumes via `--trace`, self-describes, and serves `--mcp-serve`; the toolkit conformance test passes against the built binary; `grep -rn "non_exhaustive" crates/hugr-agent crates/hugr-toolkit` is empty.

### Phase 5 — Finish the `hugr-docs` port

Goal: the docs crate is a definition folder + answer shaping + thin CLI/PyO3 packaging. ~750 LOC.

1. Delete the seven dead `Docs*` capabilities in `crates/hugr-docs/src/lib.rs` (`DocsList`, `DocsRead`, `DocsReadRange`, `DocsReadMany`, `DocsReadRangeMany`, `DocsOutline`, `DocsSearch`, roughly lib.rs:442–1178) plus `DocsRoot::capabilities()` — the agent runs on the toolkit's `fs_read` grant; `capabilities()` has no call site. Delete the ~5 unit tests that only exercised them (`read_range_returns_line_window`, `read_many_returns_partial_successes_and_errors`, `read_range_many_returns_line_windows`, `outline_extracts_markdown_headings`, `read_tool_cannot_escape_root`) — the equivalent jail behavior is tested on the toolkit's `fs_read`.
2. Simplify `DocsConfig`/`DocsConfigOptions`: the manifest `[models]` covers model/base_url/pricing; keep only the env-var overrides the Python binding actually documents. Remove the dual `docs_*`/`fs_*` tool-name handling in `read_document_sets` (only `fs_*` exists now).
3. Keep: the `definition/` folder, `main.rs`, `python.rs` (the PyPI wheel ships from here), and the answer-shaping/`related_documents` logic (now just data in `Answer.extra`).
4. Exit: `hugr-docs` tests green; the Python binding answers and resumes; net crate LOC roughly halves.

### Phase 6 — Docs, README, CI sync

1. Rewrite `README.md` to the trimmed reality: pitch, quickstart (`hugr new`/`run`/`build`), the two docs (`docs/ARCHITECTURE.md`, `docs/ROADMAP.md`), crate list (7 crates), no parked crates, no "seven docs tools" claim (it's the six `fs_*` tools).
2. Re-check `AGENTS.md` (aka `CLAUDE.md`) against the trimmed code: crate layout, invariants, conventions. It was pre-rewritten for the target state — fix anything the implementation invalidated.
3. Add a CI workflow running `cargo fmt --check`, `cargo clippy --all-targets`, `cargo test` on push — there is currently **no** test CI (only the manual PyPI wheel workflow, which stays). Optionally add `cargo build --target wasm32-unknown-unknown -p hugr-core` as the sans-IO canary.
4. Record the outcome in §3 below (date, phases done, final LOC delta, deviations from this plan).

## 3. Trim log

*(Append one short entry per completed phase: date, what was done, deviations from the plan and why.)*

- 2026-07-06 — Plan authored from a full-repo audit; docs rewritten to the target state (this file + `ARCHITECTURE.md`; `DESIGN.md`/`THREAT_MODEL.md`/`BRANDING.md`/`PROGRESS.md` merged or deleted). Implementation not started.
- 2026-07-06 — **Phase 1 done.** Deleted `hugr-cli`, `hugr-wasm`, `hugr-example-plugin`, `hugr-plugin-abi` (framing.rs moved to `hugr-host/src/framing.rs`), the plugin tool path (`plugins.rs`, `ToolKind::Plugin`, `[tools.plugin.*]`), and the untracked junk dirs. One deviation: the framing wildcard match arm in `mcp.rs` became unreachable once `FramingError` moved in-crate, so the arm and its `#[non_exhaustive]` were dropped now rather than later. 7 crates; 275 tests green.
- 2026-07-07 — **Phase 3.3 done (plan/todo deleted).** `Event::PlanAccepted`/`TodoUpdated`, `Record::Plan`/`TodoList`, `TodoItem`, their projection/summary-render arms and reducer handling, and their two scripted tests are gone. No deviations.
- 2026-07-07 — **Phase 3.2 done (skills deleted).** `SkillDescriptor`, `with_skills`, `activate_skill`, `Record::SkillActivated`, the projection/summary-render arms, and the reducer's skill branch in `begin_tool_call` are gone; `capability_versioning` now looks up configured tools only. No deviations.
- 2026-07-07 — **Phase 3.1 done (tier routing deleted).** `RoutingPolicy`, `RoutingPhase`, `RoutingInputs`, `ToolRisk`, the text-task heuristics, and the routing-input assembly in `brain.rs` are gone; `TurnPolicy::choose_model(state)` returns the policy's selector directly (the one-shot override is applied by the reducer). Deviations: `RoutingDecision` + `OpMeta.routing` and `TurnPolicy::explain_model_choice` were deleted too (pure routing observability, nothing branched on them); the two compaction-routing tests died here rather than in 3.6 since they existed only to pin `RoutingPhase::Compaction` behavior; `end_to_end.rs`'s crash-resume test now registers `medium` — the old `RoutingPolicy` fallback had escalated a resumed crashed session to `big` via the tool-risk heuristic (the §2 Phase 2.7 bug).
- 2026-07-06 — **Phase 2 done.** Deleted host capabilities/, coalescer, scheduler, skills loader, spend report, `StdoutFrontend`/`Metrics`, the in-process sub-agent runner (`agent.rs`, `AgentDefaults`, `agent()`/`max_agent_depth`), and the whole `Policy` trait (the engine now answers `RequestPermission` with `Allow` inline — the sandbox is registration); `hugr-agent`/`hugr-toolkit` dropped their policy plumbing. Deviations beyond the plan: the *entire* checkpoint machinery (`CheckpointCadence`, `CheckpointShared`, background writers) went too — the agent runtime only uses `.record(true)`/`.resume(trace)` and persists via `TraceStore`, so nothing drove it; the engine's self-fired hooks (`fire_hook`, session-start/pre-tool/post-tool/stop) were removed now rather than in Phase 3; `OpenAiAdapter::from_env` + the HF-token resolution helpers were deleted alongside the listed tier-config surface (the manifest is the one config path). `end_to_end.rs` was rewritten to the surviving behaviors (tool round-trip, MCP, background overlap, cancellation, delta consolidation, record/replay/resume, crash-quiescence via a hand-crafted in-flight trace). 233 tests green.

## 4. After the trim

In rough priority order, deliberately not scheduled:

- **The demo.** One story proving the pitch: 3–4 differently-privileged subagents (docs Q&A on a policies folder, read-only SQLite over an expenses db, a scratchpad-only report writer) + a ~100-line orchestrator script that delegates, resumes one thread via `trace_id`, forks a what-if, and prints a per-agent cost table. Runs from checked-in sample data; later pinned in CI via recorded traces (replay mode, no live model).
- **Golden traces.** Recorded fork trees and definition-run sessions as regression fixtures; `verify()` stays the release gate.
- **Publish.** Crates + the `hugr` binary on crates.io so `cargo install` → `hugr new` → `hugr run` works outside the repo; a short "define → run → build → orchestrate" guide.
- **Tool library growth on demand.** `pdf_read` (text/table extraction, no network) for a receipts-style agent; `code_exec` as the one sandboxed exec-class tool (pinned interpreter, cwd = scratchpad, no network, caps from `[limits]`); a general `shell` never enters the library.
- **Provider breadth.** An Anthropic-native adapter behind the same streaming contract.
- **Trace migration.** Versioned migration hooks so old traces stay resumable across schema bumps.
- **Ideas backlog.** Unstructured candidates live in `new_ideas.md` (feedback channel between agents, shared memory, pluggable storage backends, Hub integration, cron, a local agent registry + gateway MCP server, a builder agent that scaffolds new definitions). Promote to this section only when the toolkit needs them.
