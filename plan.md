# Huggr Roadmap Plan

This plan turns every item in `new_ideas.md` into either a detailed TODO or an entry in the Won't-do / Deferred sections. It is grounded in the codebase as of commit `875a9a2` — file references were verified against source. Tasks are grouped in phases ordered by dependency; tasks within a phase are mostly parallelizable unless a `Depends on` says otherwise.

Status legend: each task starts `[ ]`; flip to `[x]` when merged (code + tests + docs). A task is not done until `docs/` / `README.md` / `AGENTS.md` match reality (see the Doc-sync checklist at the end). Sizes: S (&lt;1 day), M (1–3 days), L (~1 week), XL (multi-week).

## Ground rules (checked against every task below)

- `huggr-core` stays sans-IO, pure, single-threaded, dependency-free beyond serde. None of the tasks below adds anything environmental to it; the only core changes in this plan are pure (new `ContextDisposition` variants, one `Record` variant for model-backed compaction, policy plumbing) and each ships with scripted determinism + replay tests.
- All nondeterminism stays injected as events. Anything that needs a model call (summarization) or IO (storage backends) lives in a host layer or rides the existing command/event cycle.
- The log stays the source of truth and traces stay immutable. Feedback, memory, and analytics are all sidecars or folds — never mutations of a stored trace. Compaction changes the *projection*, never the log.
- Narrow waist: new capabilities (memory, feedback-as-tool, traces_read) are ordinary `Capability` registrations with opaque args — zero core type changes. New manifest sections are open string sets where possible.
- Sandbox-by-registration: every new tool is granted in the manifest, jailed to a declared scope, and gets a threat-model note in `docs/concepts/security.md`. The library stays exec-free.
- One way to do each thing: where this plan adds a mechanism (e.g. storage traits), it replaces the old shape (concrete structs threaded everywhere) rather than living beside it.

### 4.4 `[ ]` Additional ideas (mine — proposed, each independently droppable)

- `[ ]` **Trace GC** (resolves the trace garbage collection open question) — S/M: `huggr traces gc <agent-dir> [--keep-days N | --keep-last N] [--dry-run]`; deletes only *leaf* traces (never a `depends_on` target — lineage stays intact), sweeps orphaned scratch dirs and unreferenced blobs (refcount by scanning trace blob manifests; shared store makes this a mark-and-sweep across all agents' traces). Explicit command only — no automatic deletion.
- `[ ]` **Eval harness** (`huggr eval`) — M: `evals.toml` beside the manifest (`[[case]] question / expect.path / expect.contains / expect.status / max_cost_micro_usd`); runs each case as a normal ask (live or against a recorded-response fake adapter for CI), reports pass/fail + cost table, exit code for CI. The natural companion to 4.1: insights propose, evals verify. Regression story: pin a case's trace and assert replay equivalence.
- `[ ]` **Anthropic-native provider adapter** — M: `huggr-providers::AnthropicAdapter` (Messages API streaming, tool use, same retry rules); proves the `ModelAdapter` seam with a second real implementation and unlocks non-OpenAI-compatible endpoints. Registered per tier via a `provider = "anthropic"` key on `[models.<tier>]` (open string, default `openai`).
- `[ ]` **Release pipeline** — M: tag-driven GitHub workflow — crates.io publish order (core → replay → host → providers → agent → toolkit), `huggr` CLI binaries (linux/macos artifacts), Python wheels (3.1), npm package (3.2). (Distinct from Deferred D2, which is HF-Hub-specific distribution.)
- `[ ]` **CI additions** — S: run the `#[ignore]`d conformance/build_cli suites in a nightly/weekly workflow (they're the real gates and currently never run in CI); add `cargo deny`/`audit`; extend the sans-IO canary with a `cargo tree -p huggr-core` allowlist check (catches non-wasm-visible deps too).
- `[ ]` **`code_exec` sandboxed capability** — L (already designed in `docs/reference/agents.md` tool library as the one future exec exception): pinned interpreter, cwd = scratchpad, no network, output caps; keep last in line — it's the highest-risk tool and nothing above depends on it.


## Deferred (the "at some point" items — listed, not planned)

- **D1 — Android surface** (idea 18): a JNI/UniFFI host around the same core; blocked on nothing architecturally (the wasm host proves the pattern) but no current need. Revisit after 3.2 (the mobile story likely reuses the TS/WASM work via React Native or a Kotlin host).
- **D2 — Huggr on the Hub** (ideas 19–22): store traces in HF buckets (a `TraceBackend` impl — 1.2 makes this a clean plugin, likely living outside this repo); run agents in HF Jobs/sandboxes; GitHub Action producing a binary per commit → bucket with xet dedup, commit-hash + tag/branch aliases. Prereqs all land in this plan (1.2 backends, release pipeline, `--stats`); the Hub pieces themselves stay out until wanted.

## Won't do (kept apart — would break the key rules, or explicitly excluded)

- **W1 — Real-time feedback consumption**: feedback (2.3) is never read during an ask and never alters a live session; analysis is offline (4.1). (Explicitly excluded in idea 1; also protects determinism and the one-way Ask/Answer door.)
- **W2 — Concrete Postgres / browser-localStorage / cloud storage backends in this repo**: 1.2 ships the traits + fs + in-memory reference impls (and 3.2 the IndexedDB TS impl); anything heavier is written *in an agent implementation* via `storage()` — that extensibility is the requirement in idea 8, not a Postgres driver dependency in the framework.
- **W3 — Compaction that rewrites the durable log**: "forget" only ever changes the projection (2.1); the log/trace stays append-only and immutable. Any design that summarizes-then-deletes records is rejected — it breaks replay, fork, and audit.
- **W4 — Model-backed summarization outside the event loop**: an adapter or host silently calling a model to compact (as the wasm POC's *shape* would suggest if generalized) hides an unrecorded model call from the trace; the only acceptable shape is 2.1b (a `StartModelCall` command + recorded `Record::ContextSummary`). The deterministic parts of the POC are absorbed by 2.1a instead.
- **W5 — Environmental anything in `huggr-core`**: no storage traits, Python/TS types, or async in core. All surfaces in this plan are hosts.
- **W6 — A `shell` tool / bespoke plugin protocol / second external-tool escape hatch**: unchanged; MCP remains the only external-process escape hatch, the library stays exec-free (`code_exec` in 4.4 is the designed, sandboxed exception and is not a shell).
- **W7 — Per-agent generated Python packages as the "Python API"**: the runtime API (3.1) does not replace or restore per-agent codegen beyond the existing `--surface python`; one generic runtime package, per the old plan's recommendation.
