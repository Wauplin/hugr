# Progress

Running log of what's implemented, phase by phase (see `docs/ROADMAP.md`).

## Pivot: the subagent toolkit (docs rewrite) ✅

The project pivoted from "general agent harness with showcase apps" to **"build your subagent, ship it anywhere"** — a toolkit for tiny, self-contained, domain-specific subagents, generalizing the `hugr-docs` shape. No code changed in this step; the docs now define the new direction:

- `docs/DESIGN.md` rewritten: vision, the subagent essence (system prompt + privileged tools + shared infrastructure), the trace_id/depends_on resume-and-fork orchestration contract, a researched standards positioning (MCP first; A2A later; IBM ACP is dead — merged into A2A Aug 2025; MCP sampling deprecated), and the verified gap Hugr fills (forkable deterministic traces + mandatory cost metadata + single-binary agent packaging).
- `docs/ARCHITECTURE.md`: §§1–17 (the built core) kept; §10 crate layout updated (new `hugr-agent` + `hugr-toolkit`, parked `hugr-cli`/`hugr-wasm`); new §18 (Ask/Answer contract), §19 (trace store, fork semantics, scratchpad), §20 (declarative agent definitions + tool library), §21 (build-time surfaces: CLI/crate/Python/MCP).
- `docs/ROADMAP.md` replaced (and `docs/ROADMAP_2.md` deleted): phases T0 (`hugr-agent` common API) → T1 (declarative toolkit) → T2 (surfaces) → T3 (orchestration hardening) → T4 (the expense-audit demo: four differently-privileged subagents + an orchestrator) → T5 (publish/harden), with per-task exit criteria.
- `README.md` and `AGENTS.md` updated to the new framing; `hugr-cli` and `hugr-wasm` are parked as core regression hosts (kept compiling, no product work).
- Follow-up additions: a lower-priority phase T6 (ROADMAP + ARCHITECTURE §22 + DESIGN §7/§12) — the machine-level agent registry (`AgentCard` cache + `hugr agents list`), the gateway MCP server, and `hugr-builder`, the self-extension subagent that builds pure-data subagents on demand under no-privilege-escalation guardrails.
- Pre-build cleanup: the illustrative definition-folder examples (`crates/hugr-docs-2` pure-data, `crates/hugr-sqlite-2` custom Rust tool — see git history at `74f5ed6`/`30cc091` for the authoring-format walkthroughs) and the obsolete `draft/` reducer sketch were removed so implementation starts from a clean tree; the format they illustrated is specified normatively in ARCHITECTURE §20–21.

## Roadmap T0 — `hugr-agent`: the common subagent API

- **T0.1 — the `Ask`/`Answer` contract ✅.** New crate `crates/hugr-agent` (pure-data deps only for now: `serde`/`serde_json`; hosts stack on later tasks). `src/contract.rs` pins the uniform invocation surface (ARCHITECTURE §18.1): `Ask { question, trace_id?, blobs, extra }`, `Answer { status, message, trace_id, blobs, metadata, extra }`, `AnswerStatus::{Success, OffTopic, Error}` (snake_case wire strings), mandatory `AnswerMeta { duration_ms, cost_micro_usd, tokens_in/out, model_calls, tool_calls, per_tier: Vec<TierSpend> }`, `TraceId` (transparent string newtype), and the §18.3 blob slots `BlobHandle { ref: bytes|path|sha256, media_type, perms, name? }` + `BlobPerms` (read-only default). Every type is `#[non_exhaustive]` with constructors/builder setters (`Ask::new().with_trace_id(…)`, `AnswerMeta::with_tier` folds tier lines into the totals); every optional field is `#[serde(default)]` (+ `skip_serializing_if`, so the minimal wire ask is exactly `{"question": …}`). JSON schemas for both wire forms are committed at `crates/hugr-agent/schemas/{ask,answer}.schema.json`. Tests (`tests/contract.rs`): serde round-trips, exact wire-JSON snapshots (field names + enum strings are the contract), errors-are-answers with zeroed-but-present meta, old/sparse JSON keeps loading, and schema↔type lock-step (schema property sets, required lists, status enum, and blob-ref kinds must match what serde actually emits).

- **T0.2 — trace store ✅.** `hugr-agent` gains `src/store.rs` (ARCHITECTURE §19.1): `TraceStore`, a directory-backed store of **immutable** traces keyed by generated `TraceId`, with `put(trace, TraceHeader)` (stamps the header + id, refuses overwrites — a byte-identical re-put lands as a new suffixed sibling), `get(id)` (full trace; one file read, then a pure parse — re-folding needs zero further IO), `head(id)` (header only: a `HeadOnly { meta }` deserialization, so no event vector is ever materialized), and `list()` (all heads, id-sorted for deterministic order; an unused store lists empty). Writes are atomic via `Trace::save_atomic` (temp file + rename). `trace_id` generation is content-derived — no RNG, no clock: SHA-256 of the headed trace JSON before the id is stamped in, truncated to 16 hex chars, with a monotonic collision-checked `-N` suffix. `hugr-replay`'s `TraceMeta` gained the §19.1 header fields `trace_id` / `depends_on` / `agent_name` / `agent_version` / `question` / `status`, all `Option<String>` with `#[serde(default, skip_serializing_if = Option::is_none)]`, so pre-existing traces load unchanged **and** traces recorded outside a store serialize byte-identically to the pre-T0.2 format (`status` is the opaque `Answer.status` wire string — narrow-waist, never interpreted by replay). `hugr-agent` now depends on `hugr-replay`/`thiserror`/`sha2` — it is a host-layer crate; `hugr-core` stays sans-IO. Tests (`tests/store.rs`): persist + reload with immutability, cross-store id determinism, a root → t1 → {t2a, t2b} fork lineage fully visible from `list()` headers, `head()` succeeding on a trace whose *body* was corrupted (proving events are never parsed) while `get()` fails, `NotFound` on a missing id, and back-compat both ways (old JSON loads; store-untouched traces emit none of the new keys).

- **T0.3 — resume & fork ✅.** `hugr-agent` gains `src/agent.rs` (ARCHITECTURE §19.2): `Agent` + `AgentBuilder`, the first real `Agent::ask(Ask) -> Result<Answer, AskError>` path over the native `hugr-host` engine. The builder mirrors the engine's pieces an agent definition declares — `model(selector, adapter)` / `default_model` / `capability` (sandbox-by-registration) / `system_prompt` / `policy` (default `AllowAll`) / `sampling` / `clock` — and stamps `name`/`version` into every trace header. Each ask assembles a **fresh** recording engine: with no `trace_id` a fresh brain runs the turn and persists a new **root** trace; with a `trace_id` the parent is loaded (one file read) and **re-folded** via `EngineBuilder::resume` — no model/tool re-calls (§15.1) — then the new question runs as a live turn and the whole session persists as a **new** trace with `depends_on = parent`. The parent file is never mutated, so forking is just asking the same parent twice → sibling traces. Accounting (`AnswerMeta`) is folded from only the **new** slice of the log (a resumed ask never re-bills its ancestry): tokens/model-calls/tool-calls now, cost stays 0 until pricing lands (T0.6). Error discipline (§18.1): run failures — model erroring, no final answer — are **answers** (`status: Error`) with a persisted trace and the terminal error surfaced in `message`; only infrastructure failures (unknown parent id, store write error) return `AskError` (`missing_trace()` names the absent parent for surfaces). Tests (`tests/resume_fork.rs`, real tokio engine + scripted mock model, no network): a fresh ask persists a root and `verify()`s; a follow-up writes a child with `depends_on` set, bills only the new turn, and leaves the parent byte-for-byte unchanged; the full three-way fork root → t1 → {t2a, t2b} yields four distinct traces with correct header lineage, sibling forks never contend on the shared parent, `list()` shows all four, and **every** persisted trace replays bit-for-bit (`hugr_replay::verify`) — pinning determinism of the resumed fold; an unknown parent id is an `AskError`.

- **T0.4 — scratchpad ✅.** `hugr-agent` gains `src/scratch.rs` (ARCHITECTURE §19.3): a per-lineage scratch directory exposed as three **ungated** capabilities `scratch_write` / `scratch_read` / `scratch_list` (`requires_permission = false` — the jail is the boundary, so no permission round-trip). Every path is canonicalized and jailed to the scratch root with the exact `hugr-docs` discipline: absolute paths and any `..`/root/prefix component are rejected, and the canonical target must still live under the root; writes canonicalize the (created) parent directory too, so a symlinked parent can't escape. Tool results carry only relative paths, never absolute ones — scratch content never enters the log, so traces stay portable and replay stays deterministic. Scratch state follows the **trace lineage** (copy-on-fork, §19.3): each finalized trace owns a `…/<scratch_root>/<trace_id>` subtree; `Agent::ask` prepares a fresh `.pending/<pid>-<n>` working subtree (a monotonic per-agent counter — the one bit of host-side nondeterminism, kept off the trace), **seeds** it by recursively copying the parent's finalized subtree on resume/fork so an ask sees its ancestor's notes, runs the turn against it, and — only after the trace is persisted (the id is content-derived, unknown until then) — finalizes it with a same-filesystem rename to `<trace_id>`. Because the copy is per-ask, two asks that fork the same parent get independent working copies, so sibling branches can never observe each other's writes. The scratch root defaults to a hidden `.scratch` subtree inside the store root (non-`.json`, so `TraceStore::list` skips it) and is overridable via `AgentBuilder::scratch_root`; scratchpad IO failures are infrastructure errors (`AskError::Scratch`). Tests (`tests/scratchpad.rs`, real tokio engine + a scripted mock model that emits `scratch_write`/`scratch_read` tool calls, no network): a note written in one ask is re-read across a **resumed** ask; both `../escape.txt` and an absolute `/tmp/...` write are rejected as tool-level errors and nothing lands outside the jail; and a three-way fork (parent → {A, B}, then resume each) proves A reads back only its own `shared.txt` write while still seeing the ancestor's note, and B reads back only its own — copy-on-fork isolation.

- **T0.5 — blob exchange ✅.** `hugr-agent` gains `src/blobs.rs` (ARCHITECTURE §18.3): the orchestrator↔agent file flow over the contract's typed blob slots. **Inbound** — before the turn starts, each `Ask.blobs` `BlobHandle` is materialized into the ask's scratch working directory (so `scratch_read`/`scratch_list` see plain files in the jail) with the declared `BlobPerms` applied as unix owner mode bits (`read → 0o400`, `write → 0o200`, `execute → 0o100`; advisory no-op on non-unix — enforcement v1 is materialize-with-mode-bits + jail). All three `BlobRef` kinds resolve: `Bytes` is base64-decoded (new `base64` workspace dep), `Path` is read from the orchestrator-local file, `Sha256` is loaded from the store. Files land at the scratch root under the handle's `name` hint (sanitized to a single jail-safe path segment — no traversal/absolute/prefix components) or a stable derived name (`Path`'s source file name, else `blob-<index>-<hash12>`); an existing seeded file is replaced so an inbound blob rides the Ask, never the scratch lineage. **Outbound** — by convention the agent writes files it wants to return into an `out/` subdir of its scratchpad (via `scratch_write`, which creates the dir on demand); after the turn that subtree is swept (deterministic sorted order) into `hugr-replay`'s content-addressed `BlobStore` and each file returns as an `Answer.blobs` entry with a `BlobRef::Sha256` whose `sha256` is the store's full `"sha256:<hex>"` address — so inbound and outbound speak the same address form and the ref resolves directly via `Agent::blob_store().get()`. Dedup is the store's (identical bytes → one object / one hash). The blob store defaults to a hidden `.blobs` subtree inside the store root (overridable via `AgentBuilder::blob_store`; re-exported `hugr_agent::BlobStore`); a best-effort extension→media-type guess tags outbound handles. Blob failures (bad base64, missing store object, unusable name, IO) are infrastructure errors (`AskError::Blob` / `BlobError`), converted to error answers at surfaces. Tests (`tests/blobs.rs`, real tokio engine + scripted mock model, no network): a `Bytes` file handed in is read via `scratch_read` and a produced `out/report.md` comes back as a resolvable sha256 blob; a `Path` hand-in materializes and reads; inbound perms are asserted as `0o400`/`0o700` mode bits on the finalized subtree (unix); and two identical outbound files dedupe to one hash / one on-disk object while a third distinct file differs.

- **T0.6 — pricing & cost accounting ✅.** `hugr-agent` now owns host-side per-tier `Pricing` / `TierPrice` config (USD per million input/output tokens) and `AgentBuilder::pricing`, keeping pricing out of `hugr-core` while deriving `AnswerMeta.cost_micro_usd` entirely from trace-recorded facts: each model `OpEnded` supplies selector + `Usage`, and missing price lines deliberately price at zero while still reporting tokens/calls. Accounting is folded from only the new log slice of an ask, so resume/fork never re-bills ancestry; per-tier lines are deterministic and sorted by selector; and recorded child traces tied to new agent ops are folded recursively with their seed prefix excluded, so sub-agent spend rolls into the parent metadata once agent-as-tool grants are wired. Tests (`tests/resume_fork.rs::pricing_cost_is_folded_from_only_the_new_trace_slice`): a priced mock run reports the hand-computed `7*2 + 3*5 = 29` microUSD total and per-tier line, and a resumed child ask reports the same single-turn cost rather than root+child cost. Verification run: `cargo test -p hugr-agent -q`.

- **T0.7 — introspection API ✅.** `hugr-agent` now exposes the surface-facing audit API from ARCHITECTURE §18.2: `Agent::describe() -> AgentCard`, `Agent::config() -> AgentConfig`, and `Agent::traces() -> Result<Vec<TraceHead>, StoreError>`. The card carries name/version/description, a deterministic tool list with privilege classes (`read_only`, `scratchpad`, `gated`), scratchpad scopes, model tiers, pricing, and declared `AgentLimits` (enforcement remains T3.1); `AgentBuilder::description` and `AgentBuilder::limits` set the new metadata. The effective config is a stable list of `ConfigEntry { key, value, provenance, redacted }` with `ConfigProvenance::{default,builder,manifest,env,flag}` reserved for T1/T2 sources; today builder/default values are reported without inventing manifest/env behavior, and the redaction slot is already part of the contract for future secrets. `describe()` includes the always-registered `scratch_read` / `scratch_write` / `scratch_list` tools by reusing their real schemas, so the card matches ask-time capabilities. Tests (`tests/introspection.rs`): exact serde JSON for the card (tools, scratch scopes, pricing, limits), config provenance/redaction readiness, and `traces()` returning header-only root→child lineage after a real ask/resume. Verification run: `cargo test -p hugr-agent -q`.

- **T0.8 — port `hugr-docs` onto `hugr-agent` ✅.** `crates/hugr-docs` now depends on `hugr-agent` and its runtime path is `Agent::builder(...).ask(Ask)` instead of a crate-local `Engine::builder` session. The docs crate still owns its domain-specific read-only tools and final JSON post-processing (`status`/`message`/`related_documents`), but trace storage, resume/fork semantics, scratchpad injection, and cost accounting now come from the common agent layer. CLI and Python gained trace resume knobs: `hugr-docs --trace <id> --trace-dir <dir>` and `hugr_docs.answer(..., trace_id=None, trace_dir=None)`, with `HUGR_DOCS_TRACE_ID` / `HUGR_DOCS_TRACE_DIR` fallbacks and `.hugr-docs-traces` as the default store. Successful runtime answers now include top-level `trace_id`; metadata tokens/cost/model/tool counts are copied from `AnswerMeta`, so resumed docs asks do not re-bill ancestry. The old `JsonFrontend` shim and bespoke engine assembly were removed; `crates/hugr-docs/README.md` documents the new trace contract. Tests: existing `hugr-docs` tests pass on the new layer, plus `docs_answer_serializes_trace_id_when_present` pins the added output field. Verification run: `cargo test -p hugr-docs -q`; `cargo check -p hugr-docs --features python -q`.

## Roadmap T1 — `hugr-toolkit`: declarative agent definitions

- **T1.1 — manifest format ✅.** New crate `crates/hugr-toolkit` (`src/manifest.rs`) parses a definition folder (`hugr.toml` + optional `SYSTEM.md`) into a typed `AgentDefinition` (ARCHITECTURE §20.1). Sections: `[agent]` (name required, version/description), `[models]` (`base_url`/`api_key_env`/`default` + one nested `[models.<tier>]` per logical tier with `model` + optional pricing and sampling knobs), `[tools.<name>]` (predefined-library grants, arbitrary tool-specific scope kept as `serde_json::Value`) plus the reserved namespaces `[tools.mcp.<name>]` / `[tools.plugin.<name>]` / `[tools.agent.<name>]` classified by `ToolKind::{Library,Mcp,Plugin,Agent}` (grants sorted deterministically by kind then name), `[limits]` (`max_turns`/`max_model_calls`/`max_cost_micro_usd`/`timeout_s`), `[scratchpad]` (`root`), `[traces]` (`store`). `AgentDefinition::load(dir)` reads the folder and stamps `source_dir` + `system_prompt`; `AgentDefinition::parse(src, path)` parses a string; `default_tier()` resolves the explicit `[models].default` → `medium` → first tier. Diagnostics: TOML syntax errors carry a `Span` (byte range resolved to 1-based line/column) via `ManifestError::Parse`; semantic problems (missing/empty `[agent].name`, missing `[models]` or empty tier set, a tier with no `model`, a `default` naming an undeclared tier) are `ManifestError::Validate` pointing at the offending key. Unknown keys **warn, never fail** (`AgentDefinition::warnings`): fixed-schema sections (`[agent]`/`[limits]`/`[scratchpad]`/`[traces]`) and unrecognized top-level tables are flagged, while caller-defined tier names and tool-scope keys are intentionally never flagged. A documented reference manifest is committed at `crates/hugr-toolkit/reference/hugr.toml`. Tests: `src/manifest.rs` unit tests (minimal parse, missing name/models, syntax-error line/column, unknown-key warnings, tiers+pricing, bad `default`, library + namespaced tools) and `tests/reference_manifest.rs` (the reference manifest parses with zero warnings). Verification run: `cargo test -p hugr-toolkit`.

- **T1.2 — predefined tool library ✅.** `hugr-toolkit` gains `src/tools/` (ARCHITECTURE §20.2): vetted, parameterized `hugr_host::Capability` families selectable by a manifest `[tools.<name>]` grant, each documenting a `PrivilegeClass` (`read_only`/`scratchpad`/`network`/`mutating`/`exec`) so the manifest is the audit surface. A `CATALOG` of `LibraryToolSpec { id, privilege, tools, summary }` enumerates the library, and `build_library_grant(grant, base_dir) -> Result<Vec<Arc<dyn Capability>>, ToolError>` turns one `ToolKind::Library` grant into the concrete capabilities it registers (relative scope paths resolved against the definition folder). Tools: **`fs_read`** — generalized from the `hugr-docs` retrieval tools (docs-specific `AI_INDEX`/`is_index` dropped, root now a manifest scope), one grant registers six read capabilities `fs_list`/`fs_search`/`fs_read`/`fs_read_range`/`fs_read_many`/`fs_outline` sharing an `FsRoot` jail (canonicalized, absolute/`..` rejected, symlink-escape re-checked against the root); **`http_fetch`** — network egress jailed to a manifest host allowlist (exact host or subdomain) + method allowlist (GET-only default), fail-closed on an empty allowlist, non-gated (the allowlist is the boundary); **`scratchpad`** — recognized audit marker whose tools are provided by the agent runtime itself (T0.4), so the grant registers nothing here; **`sqlite_query`** — read-only, file-scoped SQLite (opened `SQLITE_OPEN_READ_ONLY`, `ATTACH` rejected, per-call connection on a blocking thread). Privilege classes: `fs_read`/`sqlite_query` read-only, `http_fetch` network, `scratchpad` scratchpad. Tests (`cargo test -p hugr-toolkit`, 18 passed): `fs_read` unit tests exercise list/read/search/outline/range on a temp tree and prove the jail rejects `../` traversal and absolute paths while allowing in-jail paths; `http_fetch` unit + `#[tokio::test]` prove allowlist matching (exact/subdomain/reject), disallowed-host and non-GET rejection *without network*, and fail-closed empty allowlist; the dispatcher tests pin the catalog, unknown-grant error, the scratchpad no-op grant, and `http_fetch` reporting the network class. **Environment note:** the crate mirror in this sandbox cannot vendor `rusqlite` (index + `.crate` downloads time out), so `sqlite_query` is behind an opt-in `sqlite` cargo feature (off by default, `rusqlite` dep commented in `Cargo.toml` with the two-line enable instructions); the default build/test/clippy are green without it and the `sqlite_query` grant reports as unavailable when the feature is off. `cargo clippy -p hugr-toolkit --all-targets` is clean. Verification run: `cargo test -p hugr-toolkit --offline`; `cargo clippy -p hugr-toolkit --all-targets --offline`.

- **T1.3 — `hugr run`: interpret a definition ✅.** `hugr-toolkit` gains `src/runtime.rs` and the `hugr` binary (`src/bin/hugr.rs`), the interpreter path every definition gets before any bundling (ARCHITECTURE §20.4). `runtime::build_agent(&AgentDefinition) -> Result<(Agent, Vec<String>), RuntimeError>` assembles a `hugr_agent::Agent`: one `OpenAiAdapter` per `[models.<tier>]` (model id + shared `base_url` + per-tier `temperature`/`max_tokens` sampling; `top_p` is parsed but not yet applied since core `SamplingParams` is temperature/max_tokens only), the default tier from `default_tier()`, a `Pricing` table folded from per-tier `input/output_usd_per_m_tokens` (a tier declaring either side prices, missing side = 0), the `SYSTEM.md` prompt rendered through a small template-var set (`{{agent_name}}`, `{{tools}}` = the definition's concrete tool names incl. the always-present scratch tools, `{{date}}` = UTC `YYYY-MM-DD` via a `chrono`-free civil-from-days calc), the granted **library** tools via `tools::build_library_grant` (sandbox-by-registration — only manifest grants are registered), `AgentLimits` from `[limits]` (`timeout_s`→`timeout_ms`), and the trace-store (`[traces].store`, default `.hugr-traces`) + scratch-root (`[scratchpad].root`) locations, all relative paths resolved against the definition folder. The provider API key rides `[models].api_key_env` (never the manifest); an unset var warns and the run fails as an error answer. External-tool grants (MCP/plugin/agent) warn and are skipped (ROADMAP T1.5/T3.8). The `hugr run <agent-dir> "question" [--trace <id>] [--json]` CLI follows the universal contract (§21.1): the JSON `Answer` on stdout, diagnostics on stderr, always exit 0 — a bad manifest, a build failure, or an infra `AskError` all come back as `status: "error"` answers. Tests (`cargo test -p hugr-toolkit`, 22 passed): template-var rendering, civil-date epochs, `build_agent` producing an agent whose `describe()` card lists the fs_read family + scratch tools, and an external grant warning+skip. End-to-end smoke: `hugr run` on a scratch definition folder (fs_read jailed to `./docs`) parsed, assembled, ran the ask against an unreachable endpoint, and printed the standard error `Answer` JSON with a persisted `trace_id` at exit 0 — the whole no-Rust interpreter path; the live-model path is the same tested `OpenAiAdapter` `hugr-docs` uses. Verification run: `cargo test -p hugr-toolkit --offline`; `cargo clippy -p hugr-toolkit --all-targets --offline` (clean).

- **T1.4 — `hugr new`: scaffolding ✅.** `hugr-toolkit` gains `src/scaffold.rs` and a `hugr new <name> [--template docs|sqlite|blank]` subcommand (ROADMAP T1.4). `scaffold_files(name, template) -> Vec<ScaffoldFile>` is the pure core (previewable, no IO): it emits a commented `hugr.toml` (identity + HF-router `[models]` with `api_key_env = "HUGR_API_KEY"` + a `medium` tier with pricing + a template-specific tool block + `[limits]`) and a `SYSTEM.md` carrying the `{{agent_name}}`/`{{tools}}`/`{{date}}` template vars `hugr run` substitutes. `docs` grants `fs_read root = "./docs"` and also scaffolds `docs/README.md` so the jail root exists and the agent runs immediately; `sqlite` grants `sqlite_query file = "./data.db"` (with a note to build `--features sqlite`); `blank` grants nothing but the scratchpad. `write_scaffold(parent, name, template)` commits the files, refusing to overwrite an existing folder (`ScaffoldError::Exists`). The `new` CLI command writes progress/next-steps to stderr and exits non-zero on failure (it is a dev scaffolding command, not the ask/answer contract surface). Tests (`cargo test -p hugr-toolkit`, 25 passed): template parsing, every template's scaffolded manifest parses with zero warnings and `default_tier() == "medium"`, `SYSTEM.md` carries the vars, and the docs template creates its root folder. End-to-end exit criterion verified: `hugr new my-docs --template docs` then `hugr run my-docs "…"` assembled and reached the live provider (HTTP 401 with a fake key — the whole pipeline works; a real `HUGR_API_KEY` answers), and the blank/sqlite scaffolds also build via `build_agent`. Verification run: `cargo test -p hugr-toolkit --offline`; `cargo clippy -p hugr-toolkit --all-targets --offline` (clean).

- **T1.5 — external tools in the manifest ✅.** `runtime::build_agent` is now `async` and wires the two external tool grants from ARCHITECTURE §20.3: a `[tools.mcp.<name>]` grant (`command` + optional `args`) spawns the stdio server through the existing C1 client (`hugr_host::mcp::load_stdio`) and registers each discovered tool as an ordinary namespaced (`mcp__<name>__<tool>`) capability; a `[tools.plugin.<name>]` grant loads a subprocess plugin over `hugr-plugin-abi` (`hugr_host::plugins::load_subprocess`) and registers its `describe`d tools. A `command_and_args` helper extracts the spec from the grant config; a missing `command` is `RuntimeError::MissingCommand`, and spawn/handshake/discovery failures are `RuntimeError::Mcp` / `RuntimeError::Plugin` (a granted-but-unloadable tool is a build error, surfaced by `hugr run` as an error answer). Agent-as-tool grants (`[tools.agent.<name>]`, §20.5) still warn and are skipped until T3.8. The `hugr run` CLI awaits the async builder. Tests: `src/runtime.rs` unit tests (agent-as-tool warn+skip, MCP grant missing `command` → build error) plus `tests/external_tools.rs` — a fully manifest-declared python3 stdio MCP server registers `mcp__fake__echo` on the assembled agent's `describe()` card, and a `[tools.plugin.example]` grant pointing at the built `hugr_example_plugin` binary registers its `reverse` tool (both proving the manifest→spawn→discovery→registration path end-to-end; each skips gracefully when python3 / the plugin binary is absent). Verification run: `cargo build -p hugr-example-plugin`; `cargo test -p hugr-toolkit --offline` (28 passed); `cargo clippy -p hugr-toolkit --all-targets --offline` (clean).

- **T1.7 — trace tooling ✅.** `hugr-toolkit` gains `src/traces.rs` and three CLI subcommands over the definition's trace store (ROADMAP T1.7), reusing the existing `hugr-replay` machinery: `hugr traces <agent-dir>` lists the lineage forest as an indented tree, `hugr verify <agent-dir> <trace-id>` checks a trace replays bit-for-bit (`hugr_replay::verify`), and `hugr replay <agent-dir> <trace-id> [--step]` reconstructs a trace (per-event steps with `hugr_replay::Inspector`, or a summary). `runtime::trace_store_for(def)` resolves the same store `build_agent` writes to (`[traces].store` else `.hugr-traces`), and `build_agent` now shares it. The lineage renderer `traces::render_lineage(&[TraceHead]) -> String` is pure (roots = traces whose `depends_on` is absent or points outside the store; children nest, deterministic by id), so a fork tree renders as a tree and is unit-testable without disk. `hugr-agent` gained a `TraceHead::new` constructor (the struct is `#[non_exhaustive]`; tooling/surfaces need to build heads). These are developer inspection commands (like `hugr new`) — they print to stdout and exit non-zero on failure, not the ask/answer contract. Tests (`cargo test -p hugr-toolkit`, 31 passed): empty store placeholder, the T0.3 root→t1→{t2a,t2b} fork shape rendering as an indented tree, and an orphan child (parent absent) listing as a root. End-to-end verified: on a real store, `hugr traces` rendered two roots, `hugr verify` reported bit-for-bit ✓ (exit 0), and `hugr replay --step` printed each recorded event (Tick/HookFired/UserInput/ModelError) with its command/log counts. Verification run: `cargo test -p hugr-toolkit --offline` (31 passed) + `cargo test -p hugr-agent --offline` (26 passed); `cargo clippy -p hugr-toolkit --all-targets --offline` (clean).

**Phase T1 complete.** `hugr new` → edit config → `hugr run` gives a working, sandboxed, trace-persisting subagent with zero Rust, and the traces are inspectable/verifiable via `hugr traces`/`replay`/`verify`. **T1.6 (redefine `hugr-docs` as a definition folder) is deferred** — it is a substantial rewrite of a second crate with a genuine design fork (the docs agent's `fs_read` root is a *runtime* parameter, but a definition folder fixes it, so it needs a run-time scope-override mechanism that is closer to T2.3/T3.5 config-override territory). The `hugr-docs` bespoke tool code is already *subsumed* by the T1.2 `fs_read` library and its runtime by `build_agent`; the remaining port work (a checked-in docs definition + shrinking the crate to its surfaces) is best done alongside the config-override surface.

## Roadmap T2 — Surfaces: ship it anywhere

- **T2.1 — `hugr build --surface cli`: standalone binary ✅.** `hugr-toolkit` gains `src/bundle.rs`, `src/surface.rs`, `src/build.rs`, and a `hugr build <agent-dir> [--surface cli] [--out <dir>] [--release]` subcommand (ARCHITECTURE §21.1). A built binary embeds its whole definition and needs **no repo checkout to run**. **`bundle`** is a tiny, dependency-free, deterministic archive of a definition folder: `pack(dir, exclude_top)` (entries sorted by path → byte-identical rebuilds; symlinks skipped; runtime dirs like the trace store / scratchpad / `target` / `.git` excluded by top-level name), `unpack(bytes, dest)` (validates every path is relative and `..`-free before writing), and `get(bytes, path)` (pull one file — e.g. `hugr.toml` — in memory). **`surface::run_cli(bundle)`** is the universal CLI every built binary wraps: it resolves a stable per-agent home (`$HUGR_AGENT_HOME`, else `<XDG_DATA_HOME|$HOME/.local/share|tmp>/hugr/<name>@<version>`), unpacks the embedded definition there (config is rewritten every run — immutable by design — while traces/scratch live outside the bundle so **`--trace` resume works across invocations**), builds the agent, and dispatches the shared shape `<agent> "question" [--trace <id>] [--json|--pretty] [--blob <path>...]` / `[--describe|--config|--traces]` — one JSON `Answer` on stdout, diagnostics on stderr, **exit 0** for the ask path (a bad manifest, build failure, or infra `AskError` all come back as `status: "error"` answers; the audit views exit non-zero on failure). Inbound `--blob <path>` handles carry a guessed media type + name hint. **`build::build_cli(def, opts)`** generates a small shim crate (`Cargo.toml` with an empty `[workspace]` table to detach from this repo's workspace + a path dep back to the installed `hugr-toolkit`; `src/main.rs` = `include_bytes!("../bundle.bin")` → `run_cli`; `bundle.bin`), runs `cargo build` (documented Rust-toolchain-at-build-time requirement; prebuilt-runtime embedding is a later optimization), and returns the built binary path. `Surface` is `#[non_exhaustive]` (crate/python/mcp are T2.2–T2.4). `hugr-agent`'s `TraceHead` now derives `Serialize` so the `--traces` view (and later surfaces) can emit it. Tests: `bundle` round-trip + determinism + top-dir exclusion + bad-magic rejection; `surface` manifest-identity parse + a scaffolded blank bundle unpacked into a temp home describing correctly; `build` crate-name sanitization + workspace-detach + exclude computation; `tests/build_cli.rs` drives the built binary's *runtime* in-process (docs template unpacks its `docs/` tool data, answers → error-answer with a persisted trace, resumes by that trace id → child with `depends_on`, and self-describes) plus an `#[ignore]`d end-to-end test that actually compiles a shim and runs `--describe`/an ask on the produced binary. End-to-end verified: the ignored build test passed in ~26s (compiled a self-contained binary, `--describe` emitted the agent card, an ask returned `status: "error"` JSON at exit 0). Verification run: `cargo test -p hugr-toolkit --offline` (35 lib + 2 integration passed) + `cargo test -p hugr-agent --offline` (26 passed); `cargo test -p hugr-toolkit --offline --test build_cli -- --ignored real_build` (1 passed, ~26s); `cargo clippy -p hugr-toolkit --all-targets --offline` (clean but a pre-existing T1.4 scaffold lint).

Sections below this line predate the pivot and describe the foundation the toolkit builds on; historical `ROADMAP_2` task ids in them (and in code comments) refer to the deleted roadmap, retrievable from git history.

## Recorded sub-agent child sessions ✅

Sub-agent child sessions are no longer headless: previously a child brain's events/commands/log were discarded except its final digest, making children invisible to the parent trace, replay, and verification. A recording host now captures each completed child session and nests it into the parent trace (ARCHITECTURE §12.1/§13.3). `hugr-core` is untouched — this is entirely a host + replay concern; the narrow waist knows nothing about child traces.

Done:

- `hugr-replay` — `Trace` gained `children: Vec<ChildTrace>` (`#[serde(default, skip_serializing_if = Vec::is_empty)]`, so pre-children traces load unchanged and childless traces serialize byte-identically to the old format) plus `Trace::with_children`. `ChildTrace { op, agent, seed, trace }` (`#[non_exhaustive]`, `ChildTrace::new`) ties each child session to the parent `StartAgent` op that spawned it, carries the agent-kind name, the fork prefix (§14) the child brain was seeded with (serde-default, skipped when empty), and the child's full nested `Trace` — recursive, so grandchildren nest inside their parent child's trace (the recursion is through `Vec`, so serde handles arbitrary depth).
- `hugr-replay::verify` — after the parent's command/log checks, every recorded child is verified recursively: a fresh brain is re-seeded from the child's recorded seed (`Brain::from_log`) under the child's recorded policy (same `policy_from_trace` fallback rules as the parent), its events are re-fed, and the reconstructed commands + log must equal the recorded ones bit-for-bit. A failing child fails the whole verify with the new `TraceError::ChildMismatch { op, agent, source }` naming the op (nested for grandchildren).
- `hugr-host` — the engine's private `Recorder` is now shared with the sub-agent runner (`agent.rs`), which records the child's events in submission order (injected `Tick`s included) and its commands in drained order, exactly like the engine loop. On completion the runner serializes the child's `StaticPolicy`, drains its own grandchildren sink, and pushes the assembled `ChildTrace` into the parent's sink **before** sending `AgentDone` (a side channel keyed by op — `Option<Arc<Mutex<Vec<ChildTrace>>>>` — so no event can be reordered). `Engine::trace()` attaches the collected children, checkpoints carry them automatically, `EngineBuilder::resume` carries a resumed trace's children forward, and a non-recording engine passes `None` so children stay zero-overhead. Semantic pre-spawn failures (depth cap, bad config, empty tool intersection) produce no child session and record nothing.
- The in-process host still cannot spawn grandchildren live (child policies advertise no agent tools), so depth-2 recursion is pinned at the serde + verify level rather than end-to-end.

Tests: `hugr-host/tests/end_to_end.rs::sub_agent_child_sessions_are_recorded_and_verified` — a real fan-out records two `ChildTrace`s tied to the right ops (events/commands/log/policy/seed all captured), the trace save/load round-trips byte-for-byte, `verify()` passes including the recursive child checks, and a corrupted child command sequence fails verification with `ChildMismatch` naming the op. `hugr-replay/tests/roundtrip.rs::traces_without_children_stay_byte_stable_and_old_json_loads` (back-compat both ways) and `::nested_child_traces_round_trip_and_verify_recursively` (depth-2 parent → child → grandchild round-trips, verifies recursively, and a corrupted grandchild fails with nested `ChildMismatch`s naming both levels).

## Docs retrieval showcase — `hugr-docs` ✅

Done:

- Added `crates/hugr-docs`, a specialized Rust CLI host that answers a single question from a docs folder and prints one JSON object with `status`, `message`, `related_documents`, and run metadata.
- Added a product-level Python extension for `hugr-docs`: `hugr_docs.answer(question, docs_path=None, api_key=None, base_url=None, model=None, input_usd_per_m_tokens=None, output_usd_per_m_tokens=None)` returns the same answer dictionary as the CLI JSON output, with each optional field falling back to the matching `HUGR_DOCS_*` environment variable or built-in default.
- `hugr-docs` output now carries a `status` enum (serialized as `"success"` / `"off_topic"` / `"error"`) and renames `answer` to `message`. `status` is `"success"` only when the model produced a real answer; it is `"off_topic"` when the docs lacked evidence (the model emitted the `It is not possible to find an answer in the docs.` phrase) and `"error"` when an error stopped the run, in which case `message` holds the error text. Every run — including config errors, a missing docs root, and model/transport failures — returns a single JSON object (CLI exits `0`, the Python binding never raises), so callers branch on `result["status"]` instead of catching failures.
- The crate reuses `hugr-core`, `hugr-host`, and the OpenAI-compatible streaming adapter, but does not build on `hugr-cli`; it wires its own system prompt, single logical model selector, no-op JSON frontend, and crate-specific `HUGR_DOCS_*` environment variables.
- Tooling is deliberately read-only and scoped to the provided folder: `docs_list`, `docs_search`, `docs_read`, `docs_read_range`, `docs_read_many`, `docs_read_range_many`, and `docs_outline`. Paths are canonicalized and rejected if they escape the docs root; there is no shell, no write/edit tool, and no permission-mode option because registered docs tools are non-mutating.
- Added range, batched, and outline docs retrieval helpers so the model can inspect specific line windows, fetch several known source files in one call, and navigate markdown-style headings without semantic indexes or metadata search.
- The docs prompt now tells the model to decompose compound questions into answer facets, read every required non-index source, and avoid stopping after the first plausible document.
- Final output post-processing parses the model's JSON answer, filters `AI_INDEX.md` out of related documents, falls back to non-index files actually read when needed, and computes metadata including elapsed time, model, endpoint, model/tool calls, token totals, read document counts, and estimated cost in microUSD using configurable per-million-token prices.
- Fixed the model follow-up transcript after docs tool calls: `hugr-core` projection now renders matching `ToolResult` blocks immediately after the assistant `tool_calls` block even when durable host hooks were logged between the call and result, preserving the append-only log while satisfying strict OpenAI-compatible providers such as the Hugging Face router.
- Added `crates/hugr-docs/README.md` and updated workspace/architecture docs to include the new showcase host.

Tests:

- `hugr-docs` unit tests cover fenced JSON answer parsing, related-document filtering/fallback, docs-root path-escape rejection, line-range reads, batched partial successes, markdown-style outline extraction, and read-document accounting for the expanded tool set.
- Regression coverage pins tool-call/result adjacency in `crates/hugr-core/tests/scripted_session.rs::tool_results_are_projected_adjacent_to_tool_calls_even_with_hooks_between` and the real host hook path in `crates/hugr-host/tests/end_to_end.rs::builtin_pre_tool_and_stop_hooks_are_recorded_in_trace`.
- Verification run: `cargo test -p hugr-core -q`; `cargo test -p hugr-host -q`; `cargo test -p hugr-providers -q`; `cargo test -p hugr-docs -q`.

## Branding rename — Hugr ✅

The project branding now uses Hugr across the workspace, keeping only the repository root directory unchanged. Crate directories, package names, Rust crate imports, CLI binary/help text, env var docs, browser extension metadata, generated WASM glue, tests, and design/progress docs now use the `hugr-*` / `hugr_*` / `Hugr` naming scheme from `docs/BRANDING.md`. Verification: `cargo check --workspace`, `cargo test`, `cargo clippy --all-targets`, `cargo tree -p hugr-core`, and `./crates/hugr-wasm/build-extension.sh`.

## Roadmap 2 Phase 0 — Foundations ✅

**Goal:** retrofit the post-foundation product defaults from `docs/ROADMAP_2.md`: three model tiers, host-side token estimates on durable content, and the two-mode permission model (`auto-approve` by default, `yolo` as explicit allow-all).

Done:

- `hugr-providers` now owns a host-side `TierModelConfigSet` for exactly `small`/`medium`/`big`, loaded from the `models` section of `HUGR_CONFIG` and overridden by `HUGR_BASE_URL` / `HUGR_MODEL_SMALL` / `HUGR_MODEL_MEDIUM` / `HUGR_MODEL_BIG` / CLI `--model`. All three tiers default to the same HF router model until better product defaults are chosen. `OpenAiAdapter` accepts per-tier default sampling knobs while request-level params still win.
- `hugr-core` keeps `ModelSelector` open, but `StaticPolicy::default()` and `EngineBuilder` now default fresh sessions to the `medium` selector (`EngineBuilder` tracks default-model explicitness and resolves at `build()` as explicit → first-registered → `named("medium")`, so an explicit `.default_model(...)` is never stolen by the first registered tier). `choose_model` remains the only pure brain-side tier decision; real routing stays for Phase B.
- Durable content records now carry `est_tokens`: `UserMessage`, `ModelOutput`, and `ToolResult`. The host attaches estimates to `UserInput`, `ModelDone`, capability/agent results, and denied permission decisions before they enter the brain; the reducer copies those values into the log and never tokenizes. Replay re-feeds the recorded estimates, so it does not re-estimate.
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
- The reducer runs the compaction sub-loop from ARCHITECTURE §3.4: when a planned projection exceeds the high-water mark, it emits a compaction `StartModelCall` — the selector routed through `TurnPolicy::choose_model` with `RoutingPhase::Compaction` (`RoutingPolicy` picks `small`; `StaticPolicy` falls back to its default model) — over the selected span, records the returned `ModelDone` as `Record::Summary`, checkpoints, and then re-projects before starting the normal turn model call. The default span selection never splits a tool_use/tool_result group (the boundary extends so an assistant tool-call output and its answering tool results are summarized together), and the summarization prompt/per-record rendering are provided `TurnPolicy` methods (`compaction_request` / `render_summary_record`) hosts can override without a reducer edit.
- Compaction model deltas remain transport-only and are not rendered as assistant output. Replay stays deterministic because the summarizer result is just another recorded `ModelDone` event carrying host-recorded token estimates; re-feeding the event stream reproduces the same summary record and command sequence without asking the host to invent new data.

Tests:

- `crates/hugr-core/tests/scripted_session.rs::automatic_compaction_summarizes_then_reprojects_and_replays` pins the command sequence (`small` compaction call, checkpoint, normal `medium` turn call), summary metadata (`summary_of`, tier, token-in/out), compacted projection refs, and replay equality.
- Verification run: `cargo test -p hugr-core`.

### A4 — Manual compaction trigger ✅

Done:

- `hugr-core` now accepts `Event::CompactContext`, a pure host-injected control event that runs one deterministic compaction selection over the current `ContextPlan`.
- Manual compaction reuses the same policy-routed summary request (`choose_model` with `RoutingPhase::Compaction`) and durable `Record::Summary` path as automatic compaction, but returns to idle after checkpointing instead of starting a normal model turn.
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

- Added `Event::ModelOverride { selector }`, folded by the reducer as a one-shot selector override for the next normal model turn. The event now also appends a durable `Record::ModelOverride`, and `BrainState::from_log` rebuilds a pending override from the log (consumed by the next normal `ModelOutput`; compaction summaries don't consume it), so tier overrides replay and resume deterministically and clear after one use.
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

## Roadmap 2 Phase D — Rust CLI as a serious coding agent

### D1 — Repo-orientation capabilities ✅

Done:

- `hugr-host` now ships ordinary read-only repo-orientation capabilities: `repo_files`, `repo_search`, `repo_read`, `git_status`, `git_diff`, `git_log`, and `package_metadata`. They perform host-side filesystem/process IO while the brain sees only tool schemas and opaque JSON results.
- `hugr-cli` registers those capabilities in the default laptop host alongside shell/fs/http, so a model can orient in an unfamiliar Rust repo without needing generic shell commands for common discovery.

Tests:

- `hugr-host::capabilities::repo::tests::repo_orientation_tools_are_read_only_and_list_files` pins read-only registration and fast file listing behavior.
- Verification run: `cargo test -p hugr-host repo_orientation_tools_are_read_only_and_list_files -q`.

### D2 — Stale-edit CAS ✅

Done:

- `ToolSchema` now carries optional declarative optimistic-concurrency metadata (`ToolVersioning`) naming the object-key argument and the brain-stamped expected-version argument. `StaticPolicy` exposes that metadata to the reducer without hardcoding capability names.
- The reducer stamps `expected_version` onto mutating capability args from its version table at `StartCapability` time, and `Record::ToolResult` now durably stores optional `VersionRef` metadata so `BrainState::from_log` rebuilds the read-set from the log.
- `hugr-host::Capability` has default `result_version` / `conflict_version` hooks; `fs_read` returns deterministic content versions and `fs_write` rejects stale expected versions with a conflict-shaped error before writing.

Tests:

- `crates/hugr-core/tests/scripted_session.rs::versioned_tool_calls_stamp_expected_version_and_route_conflict_retry` pins schema-driven stamping, conflict routing back to the model, and read-set reconstruction from a log prefix.
- `hugr-host::capabilities::fs::tests::fs_write_conflicts_on_stale_expected_version_without_overwriting` proves a stale write returns conflict metadata and leaves the changed file intact.
- Verification run: `cargo test -p hugr-core versioned_tool_calls_stamp_expected_version_and_route_conflict_retry -q`; `cargo test -p hugr-host fs_write_conflicts_on_stale_expected_version_without_overwriting -q`.

### D3 — Robust edit path ✅

Done:

- `hugr-host` now ships `patch_apply`, an ordinary capability that previews, applies, or reverts unified diffs through `git apply`. Successful results distinguish `preview_ok`, `applied`, and `reverted`.
- Patch failures return conflict-shaped semantic tool errors, so the existing brain path routes them back to the model as tool results without a special core type.
- `hugr-cli` registers `patch_apply` in the default coding-agent toolset.

Tests:

- `hugr-host::capabilities::patch::tests::patch_previews_applies_reverts_and_conflicts` proves preview is non-mutating, apply mutates, duplicate apply conflicts, and revert restores the file.
- Verification run: `cargo test -p hugr-host patch_previews_applies_reverts_and_conflicts -q`.

### D4 — Plan mode ✅

Done:

- `hugr-core` now accepts `Event::PlanAccepted` and appends durable `Record::Plan` entries with host-supplied token estimates. The default projection renders accepted plans as system context for future turns, and compaction summaries include them.
- `hugr-host::Engine::accept_plan` injects accepted/edited plans through the normal event path so traces replay the same context.
- `hugr-cli` exposes `/plan accept <text>`, `/plan edit <text>`, `/plan reject`, and `/plan`/`/status` display of the active durable plan.

Tests:

- `crates/hugr-core/tests/scripted_session.rs::accepted_plan_persists_and_projects_into_future_context` proves an accepted plan is recorded durably and projected into the next model request.
- Verification run: `cargo test -p hugr-core accepted_plan_persists_and_projects_into_future_context -q`; `cargo check -p hugr-cli -q`.

### D5 — Durable todo state ✅

Done:

- `hugr-core` now has durable `Record::TodoList` snapshots and `Event::TodoUpdated`. The default projection includes the latest todo snapshot as system context and omits superseded snapshots with an explicit `ContextPlan` reason.
- `hugr-host::Engine::update_todos` injects todo snapshots through the recorded event path with host-side token estimates.
- `hugr-cli` exposes `/todo list`, `/todo add <text>`, `/todo done <n>`, `/todo open <n>`, and `/todo clear`; `/status` shows todo progress from durable state.

Tests:

- `crates/hugr-core/tests/scripted_session.rs::todo_state_persists_and_projects_latest_progress` proves todo snapshots are durable and the latest progress is projected into the next model request.
- Verification run: `cargo test -p hugr-core todo_state_persists_and_projects_latest_progress -q`; `cargo check -p hugr-cli -q`.

### D6 — Verification loops ✅

Done:

- `hugr-host` now ships `cargo_verify`, a background capability for `cargo fmt --all`, `cargo test`, and `cargo clippy --all-targets`. It streams stdout/stderr chunks while running and returns exit status, captured output, truncation state, targeted test hints, retry budget, and concise failure summaries.
- Targeted test hints are derived from changed Rust paths, and failure summaries keep high-signal compiler/test/panic lines for model repair loops.
- `hugr-cli` registers `cargo_verify` in the default coding-agent toolset; because the capability declares `runs_in_background`, long builds/tests do not block model reasoning.

Tests:

- `hugr-host::capabilities::verify::tests::summarizes_failures_and_detects_targeted_tests` pins failure summarisation, targeted test detection, and background-op declaration.
- Verification run: `cargo test -p hugr-host summarizes_failures_and_detects_targeted_tests -q`; `cargo check -p hugr-cli -q`.

### D7 — Git/session CLI ergonomics ✅

Done:

- The REPL now supports `/diff`, `/review`, `/commit-message`, `/branch`, `/rewind <seq> <trace-path>`, and `/resume <trace> [prompt...]`. These commands inspect/draft only; commits remain explicit user actions outside the CLI helper.
- `/rewind` writes a branched trace prefix from the current recorded session and prints the exact `hugr resume <trace>` command to continue from it. Existing top-level `hugr resume` remains the concrete resume path.
- `/review` prints diff scope plus a compact review checklist, and `/commit-message` drafts a message from staged files or the unstaged diff.

Tests:

- `hugr-cli` unit tests pin commit-message scope selection and trace-prefix branching while preserving policy metadata.
- Verification run: `cargo test -p hugr-cli _ -q`; `cargo check -p hugr-cli -q`.

### D8 — Coding subagents ✅

Done:

- `Command::StartAgent` carries a typed `agent` name field (serde-default for old traces), so subagent usage is trace-visible without the brain mutating the model's opaque tool-call args — `config` passes through untouched (ARCHITECTURE §2.4).
- Sub-agent nesting depth is enforced host-side via `EngineBuilder::max_agent_depth` (default `DEFAULT_MAX_AGENT_DEPTH = 1`); exceeding it returns an `agent_depth_exceeded` semantic error routed back to the model as the tool result.
- Per-agent-kind defaults live in host registration data (`AgentDefaults` via `EngineBuilder::agent_with_defaults`), not the generic runner: `explorer` defaults to `small` and read-only repo tools; `implementer`, `reviewer`, and `test_fixer` default to `big` with constrained tool sets; unregistered kinds keep the neutral fallback (host default model + all tools). A `tools` allowlist that intersects the registry to zero tools is an `agent_tools_empty` semantic error instead of a silently tool-less child. Child capability results use the same version/conflict hooks as top-level capabilities, child dispatch reuses the engine's shared op runners, and the child loop fires the builtin PreTool/PostTool hooks.
- `hugr-cli` registers the named coding subagent tools (`explorer`, `implementer`, `reviewer`, `test_fixer`) over the `StartAgent` primitive, supplying their `AgentDefaults` at registration.

Tests:

- Core subagent tests pin the typed `agent` field on `StartAgent` and that the opaque config passes through untouched; host e2e tests cover the depth cap and the empty tool intersection.
- `hugr-host::agent::tests::coding_agent_defaults_constrain_tools_and_tiers` pins per-agent default tiers and tool allowlists.
- `hugr-cli::tests::coding_agent_schema_declares_defaults` pins named subagent schema defaults.
- Verification run: `cargo test -p hugr-core sub_agent -q`; `cargo test -p hugr-host coding_agent_defaults_constrain_tools_and_tiers -q`; `cargo test -p hugr-cli coding_agent_schema_declares_defaults -q`; `cargo check -p hugr-cli -q`.

### D9 — Terminal UX ✅

Done:

- The CLI front-end decision is documented in `docs/ARCHITECTURE.md`: stay with stdout streaming for now instead of adopting a TUI framework. This keeps logs copyable and avoids the TUI one-way-door while the agent loop is still evolving.
- `StdoutFrontend` now tracks active model/tool ops and renders a stable status line showing active models and tools (including background ops such as `cargo_verify`), plus an idle status when work drains.
- Existing compact/collapsible tool cards, token/cost footer metrics, `/status` context counters, and full-output toggle remain the stdout strategy for noisy sessions.

Tests:

- `hugr-host::frontend::tests::status_text_tracks_active_model_and_tools` pins active/idle status-line rendering.
- Verification run: `cargo test -p hugr-host status_text_tracks_active_model_and_tools -q`; `cargo check -p hugr-host -q`.

### D10 — Deterministic hooks ✅

Done:

- `hugr-core` now has `HookPhase`, `Event::HookFired`, and durable `Record::Hook`. Hook records project into future context and are compactable content, but they do not mutate core internals.
- `hugr-host` fires built-in deterministic hook events for session start, pre-tool, post-tool, manual compaction, and stop. Pre/PostTool hooks cover sub-agents too: PreTool fires for `Command::StartAgent` and PostTool for `AgentDone`/`AgentError` (a single shared `tool_shaped_completion()` classifier backs both `observe()` and the PostTool hook), and the same hooks fire inside a child agent's loop; skill activations remain hookless (documented limitation). These events are recorded through the same event stream as model/tool/user events, so traces replay the hook context.
- CLI replay step output summarizes hook events as `HookFired(<phase>/<name>)`.

Tests:

- `crates/hugr-core/tests/scripted_session.rs::hook_records_are_durable_and_projected` proves hook events become durable projected context.
- `crates/hugr-host/tests/end_to_end.rs::builtin_pre_tool_and_stop_hooks_are_recorded_in_trace` proves session/pre-tool/post-tool/stop hooks fire through the real engine and appear in the trace.
- Verification run: `cargo test -p hugr-core hook_records_are_durable_and_projected -q`; `cargo test -p hugr-host builtin_pre_tool_and_stop_hooks_are_recorded_in_trace -q`; `cargo check -p hugr-host -q`.

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

- `hugr-core`: `OpKind::Capability` gained a `background: bool` flag; `OpKind::blocks_turn()` returns `false` for background capabilities (and was rewritten as an exhaustive match so a future op kind can't silently default to "blocks"). `TurnPolicy` gained `is_background(capability) -> bool` (default `false`), implemented by `StaticPolicy` via a configurable background set (`with_background`). The reducer (`brain.rs`): after a model turn's tool fan-out, if nothing blocks the turn it resumes the model immediately so it streams alongside the background op(s); a granted-permission background op resumes likewise; and `on_model_done` **defers `Done`** while a background op is still in flight (the turn isn't over while work runs — the background result is folded in and a fresh turn picks it up); `on_model_error` defers its `Done { Error }` symmetrically while background ops run. No new `Command`/`Event`/`Record` variants — background-ness is a brain-side scheduling decision the host never sees.
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

- `hugr-core` (`brain.rs`): `on_op_cancelled` now (1) **ignores a cancel confirmation for an op that already resolved** — the host aborts the task *and* emits `OpCancelled`, but the task may have queued its real terminal event (`ModelDone`) a hair before the abort; that event is folded first and removes the op, so the late `OpCancelled` must be a no-op or it would append a spurious `Cancelled` `OpEnded` and break replay (cancellation is idempotent, ARCHITECTURE §6.4); and (2) emits the terminal `Done { reason: Cancelled }` once the **last** in-flight op drains on a plain abort (`UserAbort`/ESC) with nothing to resume — previously a bare abort left the brain silently idle and the front-end (which already renders `DoneReason::Cancelled`) never saw it. Later hardening (the turn-loop fix wave): `UserAbort` sets an **abort latch** so it always wins races with in-flight terminal events (a terminal event that folds first records its outcome but starts no new work while latched, and `Done { Cancelled }` is emitted exactly once when the last op drains); a cancelled *tool-shaped* op (capability/agent/awaiting-permission) also appends a paired cancelled `Record::ToolResult` before its `OpEnded`, so the next projection never carries a dangling tool_use; and `pending_resume` is consumed by `on_model_done`/`on_model_error` and cleared by `UserAbort`, so interrupting input is neither dropped nor replayed as a surprise turn. A single-op cancel while other work is still in flight does **not** force `Done` (the turn only ends when the brain is idle). No new `Command`/`Event`/`Record` variants — the cancellation contract was already in place.
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

- `hugr-replay` (`src/lib.rs`): the [`Trace`] container — `{ meta, events, log, blobs }` (later grown, always with serde defaults so older traces keep loading: an opaque `policy` in P3-3 and an ordered `commands: Vec<Command>` sequence for bit-for-bit command verification):
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
Trace { meta: TraceMeta, events: Vec<Event>, log: Vec<LogEntry>, commands: Vec<Command>, policy: Option<Value>, blobs: BlobManifest }
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

- `hugr-replay` (`src/replay.rs`): [`replay`]`(trace) -> Replay { commands, log }` re-feeds the events into a fresh brain (mirroring the host driver loop, zero IO) and returns the reconstructed command sequence + folded log; [`verify`] does that and asserts the reconstructed log equals the recorded log (`TraceError::ReplayMismatch` otherwise) **and**, for traces carrying a recorded `commands` sequence, that the reconstructed commands equal the recorded ones in order, bit-for-bit (`TraceError::CommandMismatch` reports the first divergent index; a commandless old trace falls back to log-only comparison) — the Phase 3 exit criterion. The host `Recorder` captures the ordered `Command`s the driver drains from `brain.poll()` and `Engine::trace()` carries them; the resume path re-derives commands from its re-fold so a resumed session (even from a pre-commands trace) re-saves a self-consistent sequence. Because the brain *branches* on some of the policy's pure decisions (`needs_permission`, `is_background`), faithful reconstruction needs the *same* policy: `StaticPolicy` is now `Serialize`/`Deserialize`, the trace gained an opaque `policy: Option<Value>` field (`Trace::with_policy`), and `replay`/`verify`/`Inspector` decode it (`replay_with_policy`/`verify_with_policy` accept a custom one; fall back to the default when absent/undecodable). [`Inspector`] wraps the same reconstruction as a step-through debugger: `step()` feeds the next recorded event and reports the commands it produced + the log tail it appended; `run()` collects every `Step`. All public types are `#[non_exhaustive]` with constructors.
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
  - **Driver loop** (`host/engine.js`) — mirrors `engine.rs`: drain `poll()`, spawn one async task per op, merge all events into one ordered inbox, `submit()` (stamping a `Tick` first — the brain never reads a clock), repeat until nothing is in flight. Handles `StartModelCall`/`StartCapability`/`RequestPermission`/`Cancel`/`Emit`/`Done`, first-class cancellation (an `AbortController` per op; a **Stop** button injects `UserAbort`), and the permission round-trip (auto-approve toggle = the CLI's `-y`). The browser host also records the exact submitted `Tick`+event stream so the side panel can export a JSONL trace envelope alongside the folded durable log.
  - **Model adapter** (`host/model.js`) — the exact analogue of `openai.rs` but `fetch` + streamed `ReadableStream` SSE: builds the chat-completions body from the canonical `ModelRequest`, streams text deltas live, assembles tool calls (with the same stable-id synthesis), and returns the consolidated `ModelOutput` + `Usage` (including router cost) in serde shape. An MV3 page with host permissions fetches the endpoint cross-origin, so there is **no backend of our own** — the Phase 4 "no server" property.
  - **Capabilities** (`host/tools.js`, `host/schemas.js`) — ordinary tools over `chrome.tabs` / `chrome.scripting`, **read + navigate only** (no click/type/form-submit by design): `list_tabs`, `get_current_page`, `get_page_text`, `get_page_links`, `get_page_outline`, `get_interactive_elements` (read-only, no permission), plus permission-gated `open_tab`, `navigate_tab`, `activate_tab`, `close_tab`, plus the agent-UX tools `ask_user_confirmation` and `show_plan`. Semantic errors (e.g. a privileged `chrome://` page that can't be injected) route back to the model as tool results (§5.4).
  - **Front-end** (`sidepanel.js` + `styles.css`) — a DOM renderer of the brain's `OutputEvent`s: streamed assistant text with dependency-free Markdown rendering (headings, lists, quotes, code blocks, links, tables, emphasis), reasoning, tool cards with collapsible results, plan cards, and interactive permission/confirmation cards; per-call token/cost metrics; header actions for starting a fresh in-panel chat and downloading the current trace as JSONL. Settings (`options.html`) persist the API key/base URL/model/temperature/auto-approve in `chrome.storage.local` (a browser has no env vars or token files, unlike `OpenAiAdapter::from_env`).
  - **Build** (`build-extension.sh`) — compiles `hugr-wasm` to `wasm32-unknown-unknown` and runs `wasm-bindgen --target web` into `extension/wasm/` (committed, so the extension loads with no build step); MV3's `content_security_policy` grants `'wasm-unsafe-eval'` so the module instantiates.
  - Docs: `extension/README.md` (install + an architecture-mapping table) and `extension/DEMOS.md` (eight lightweight demos — summarize a page, triage tabs, navigate-with-permission, multi-tab research, plan-then-confirm, describe-read-only, interrupt a turn, "same brain, prove it").

Tests (104 total across the workspace, +4): `hugr-wasm` unit tests over the native-testable `Core` — a user turn drives a `StartModelCall`, the log holds the `UserMessage`, the default policy constructs idle, and invalid event/policy JSON are clean errors. Plus out-of-band validation of the *shipped* artifact (WASM + generated glue) in Node: a full turn loop (`user → model → list_tabs → model resume → Done{EndTurn}`, 12 tools advertised) and the navigation permission round-trip (`navigate_tab → RequestPermission → Deny → model resumes`).

Deferred (still open for a future Phase 4 pass): the general `hugr-py` (PyO3) `poll`/`submit` host, the WASM *plugin* transport backend (wasmtime), size/cold-start benchmarking against §11, and browser-side trace persistence/resume (the side panel can export JSONL with events + log, but re-seeding a brain from a saved browser trace is not yet wired). `hugr-docs` now has a narrower product-level Python extension for one-question docs retrieval; that does not replace the general brain binding.

## Phase 6 — Sub-agents & forks ✅ (built before Phase 4, by request)

**Goal:** cheap, portable sub-agents built on log forking — a sub-agent is *not* a special subsystem, it is **another `hugr-core` instance** (ARCHITECTURE §13).

**Exit criterion — met:** a parent agent fans out to N child agents (fork-shared context), collects their results, and the whole tree replays deterministically from one recorded trace (`hugr-host/tests/end_to_end.rs::parent_fans_out_to_sub_agents_and_replays`).

Done:

- `hugr-core` — sub-agents as an op, forks as a log-prefix copy, all as *strategy*, not reducer hardcoding:
  - `Command::StartAgent { op, agent, config, seed }` — the brain emits this (instead of `StartCapability`) when the policy designates a tool as a sub-agent spawner. `agent` is the typed agent-kind name (serde-default for old traces); `config` is the model's opaque tool-call args, passed through **untouched** (the brain never injects keys into them, ARCHITECTURE §2.4); `seed` is the **forked log prefix** the child starts from.
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

- Durable checkpoints in the native host: `EngineBuilder::checkpoint(path, cadence)` implies recording and writes the current trace atomically during the run. `CheckpointCadence` is explicit: `OnCommand` saves when the brain emits `Command::Checkpoint`, `EveryEvent` saves after each host event submitted to the brain (the crash-safe mode that captures “op N is in flight” mid-turn), and `EveryNEvents(n)` trades write frequency for durability. `Trace::save_atomic` writes a sibling temp file and renames it into place, creating parent directories when needed. Checkpoint writes run in `spawn_blocking` off the driver loop and are single-flight (a checkpoint due mid-write marks dirty and rewrites the latest state when the writer finishes; a monotone generation stops a stale writer clobbering a newer snapshot), skip entirely when nothing changed, and flush synchronously on `session_end` and `Drop`.
- Resume-after-crash reconciliation: `EngineBuilder::resume(trace)` still re-folds the recorded event stream with zero IO, and now, if that fold reconstructs stale in-flight ops from a killed process, applies `CrashResumePolicy::CancelInflight` by appending recorded `OpCancelled` events under fresh injected `Tick`s before going live; the commands those reconcile submissions queue are drained and recorded, so a resumed engine starts quiescent (no stale `Done { Cancelled }` or pre-crash `StartModelCall` firing into the next live turn). That records the policy choice in the trace itself, so the grown trace remains replayable bit-for-bit. Idempotent re-issue is left as a future host policy; cancel is the conservative default.
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
