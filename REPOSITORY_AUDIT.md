# Huggr repository technical audit

Audit date: 2026-07-17
Audited revision: `d4975ca` on `main`
Scope: repository structure, documentation, Rust workspace, Python and TypeScript bindings, Chrome extension example, examples, tests, build and release workflows, dependencies, and primary runtime paths.

## 1. Executive summary

Huggr has a clear architectural center: `huggr-core` is a small, synchronous reducer whose state is derived from timestamped events, while all nondeterminism and I/O remain in hosts. The implementation largely matches the design documents. `cargo tree -p huggr-core` contains only `serde` and `serde_json`; the WASM canary, replay tests, explicit tool registration, typed ask/answer contract, path-jail tests, and immutable trace lineage are good foundations. The documentation is unusually complete for a prototype and generally tracks the live code.

The audit found no Critical issue. It found one High-severity issue, thirteen Medium-severity issues, and three Low-severity issues. The highest risk is the development Chrome extension: untrusted page content can influence a model that receives auto-approved, all-site browser capabilities, including arbitrary HTTP(S) download and subsequent upload into another page. This creates a plausible prompt-injection path for relaying data from local or private network endpoints to an attacker-controlled site. The extension documentation acknowledges its broad development permissions, but the combination of permissions and tools is more dangerous than a broad host grant alone.

The most important reliability and scalability concerns are repeated full-trace cloning and serialization at every checkpoint, unbounded provider buffers and host event channels, incomplete cancellation in the JavaScript hosts, and non-transactional finalization of traces, blobs, and scratch state. These are unlikely to affect short examples, but they become material with long model streams, many tool turns, large scratch trees, stalled subprocesses, or provider misbehavior.

The test suite is behavior-oriented and broad, but several release gates are ignored and absent from CI. The normal workspace test command is environment-sensitive: six tests failed in the audited environment because a valid global model configuration changed the expected warning count. More seriously, ignored tests described as hermetic and offline used the real configured provider during this audit. One performed successful real requests, and another failed because the live provider returned success where the test expected an offline error. CI dependency resolution is also less reproducible than the Rust lockfile suggests: the TypeScript lockfile is ignored, CI runs `npm install`, Python test tools are unpinned, and GitHub Actions are referenced by mutable major-version tags.

### Five highest-value improvements

1. Remove auto-approved broad browser authority: require user approval for network, upload, write, submit, and tab-destructive actions; restrict destinations and add data-provenance rules.
2. Replace full-trace checkpoint rewrites with an append-only event/command journal plus periodic snapshots and a single compact final trace.
3. Add explicit time, byte, event, and channel bounds across provider streaming, host dispatch, MCP, scratch, and browser downloads.
4. Make all tests hermetic by isolating model catalog and credentials, then run conformance, CLI build, generated Python, TypeScript, and Chrome integration gates in CI.
5. Make finalization transactional across blobs, scratch, trace, and checkpoint state, with fault-injection tests for every commit boundary.

### Positive observations

- The pure reducer boundary is real rather than aspirational. `crates/huggr-core/Cargo.toml:10-14` contains only data dependencies, and the CI WASM build at `.github/workflows/ci.yml:114-125` reinforces it.
- The narrow waist is well chosen: the core branches on operation lifecycle and model output structure while capability arguments and results remain opaque JSON values.
- Replay and determinism are first-class. The repository has scripted command-sequence tests, concurrent operation tests, cancellation tests, and trace verification rather than only unit tests of helpers.
- Privileged tools are registered from explicit grants. Filesystem tools canonicalize roots and defend against traversal and symlink escape; restricted shell execution avoids shell syntax.
- Trace identifiers are reserved with `create_new`, completed traces are written atomically, and lineage is immutable rather than updated in place (`crates/huggr-agent/src/store.rs:277-328`).
- Provider tokens are not exposed in model cards and secret-bearing runtime options use redacted debug representations.
- The PyPI workflow uses OIDC trusted publishing instead of a long-lived package token (`.github/workflows/publish-huglet-docs.yml:38-53`).
- Documentation, examples, language bindings, and agent skills form a coherent developer journey and were mostly consistent with implementation.

## 2. Repository overview and architecture

### Major components

| Component | Responsibility | Important interactions |
| --- | --- | --- |
| `crates/huggr-core` | Pure single-threaded reducer, event fold, operation table, command queue, durable log, turn policy | Receives timestamped host events and emits commands; has no I/O or runtime dependency |
| `crates/huggr-host` | Tokio engine, model and capability registries, frontends, MCP stdio client, JSON-line framing | Drains core commands, starts asynchronous effects, feeds results and deltas back as events |
| `crates/huggr-providers` | OpenAI-compatible streaming adapter, request construction, retries, SSE parsing | Converts provider streams into `ModelDelta` events and one consolidated `ModelOutput` |
| `crates/huggr-replay` | Trace schema, replay and verification, atomic trace persistence, content-addressed blob store | Refeeds recorded envelopes through the core and compares commands/logs |
| `crates/huggr-agent` | Stable `Ask`/`Answer` surface, storage, scratch, blobs, feedback, limits, costs, child-agent tools | Assembles an engine per ask, loads a parent trace, checkpoints, and finalizes persistent state |
| `crates/huggr-toolkit` | Manifest parsing, tool implementations, model catalog resolution, scaffolding, build system, `huggr` CLI | Converts a huglet folder into a configured agent or generated standalone artifact |
| `crates/huggr-wasm` | JSON-oriented WASM binding around the core and session helper | Lets browser and Node hosts drive the same reducer without linking native host I/O |
| `crates/huggr-python` and `bindings/python` | PyO3 embedding plus typed Python API | Builds native agents from Python configuration and callable tools |
| `bindings/typescript` | Typed Node/browser host over WASM, OpenAI-compatible adapter, local persistence | Implements model/tool effects in JavaScript and persists portable traces |
| `examples/chrome-extension` | Manifest V3 browser host, side panel, Chrome capability implementation | Connects the WASM brain and model adapter to broad `chrome.*`, page, download, and upload operations |
| `examples/huglet-*`, `examples/hf-librarian` | Reference agents and end-to-end workflows | Exercise manifests, typed responses, tool grants, generated wheels, delegation, and traces |

### Primary control and data flow

```text
User Ask
  -> Agent validates/resolves parent trace and prepares scratch/checkpoint
  -> Engine submits UserInput envelope to Brain
  -> Brain folds event, asks TurnPolicy, queues StartModel/StartCapability/permission commands
  -> Host drains commands and runs provider or registered capability asynchronously
  -> Streaming deltas/chunks and final result return as timestamped events
  -> Brain updates live operation state and appends one consolidated durable Record
  -> Engine repeats until Finish
  -> Agent calculates metadata and persists trace, outbound blobs, and finalized scratch
  -> Answer returns status, structured response, trace id, blobs, and mandatory cost/token metadata
```

The architecture separates strategy from mechanics reasonably well. `TurnPolicy` owns context projection and permission decisions; the reducer owns lifecycle and routing; hosts own I/O. The main inconsistency is across hosts: the native Rust host submits model deltas to the reducer, while the TypeScript and vendored browser drivers emit text to their UI but submit only the consolidated model result. This weakens the promise that all surfaces drive the same event semantics, especially on interruption.

### Dependency and trust-boundary map

```text
Trusted operator inputs
  huggr.toml / SYSTEM.md / runtime arguments / model catalog / custom host code
      |
      v
Toolkit -> Agent -> Host -----------------------> local filesystem, subprocesses, MCP servers
                    |                                      ^
                    v                                      |
                 Pure Core -> requested tool calls -> registered grants and policy decisions
                    ^
                    |
External model provider <--- prompt/context ---> untrusted model output

Persistent local data: traces, checkpoints, scratch, memory, feedback, blob store

Browser-specific boundary:
untrusted web page -> content script/page snapshot -> model -> auto-approved Chrome capabilities
                                                        |-> every HTTP(S) origin
                                                        |-> tab/page mutations
                                                        `-> download store -> file upload into a page
```

The model, provider responses, capability arguments, MCP messages, page content, and imported trace files must be treated as untrusted. Manifests, explicit grants, custom Python/TypeScript callables, storage overrides, full shell access, MCP process definitions, and child-agent artifacts are trusted operator configuration. Local traces and logs can contain user prompts, model output, capability arguments/results, paths, and remote content; they are sensitive even though they do not contain resolved API keys by design.

### Important files

| File | Why it matters |
| --- | --- |
| `crates/huggr-core/src/brain.rs` | Central reducer and operation lifecycle |
| `crates/huggr-core/src/state.rs` | Fold-derived state and live buffers |
| `crates/huggr-core/src/policy.rs` | Pure strategy boundary and context/permission decisions |
| `crates/huggr-core/src/event.rs` and `command.rs` | Host/core protocol and narrow waist |
| `crates/huggr-host/src/engine.rs` | Native driver loop, concurrency, checkpoint triggers, trace capture |
| `crates/huggr-host/src/mcp.rs` | External-process protocol boundary |
| `crates/huggr-providers/src/openai.rs` | External HTTP, credentials, retries, and stream parsing |
| `crates/huggr-replay/src/trace.rs` | Portable trace contract and atomic serialization |
| `crates/huggr-agent/src/agent.rs` | Ask lifecycle, costs, storage, resume/fork, and finalization order |
| `crates/huggr-agent/src/store.rs` | Trace identity, immutability, listing, and checkpoints |
| `crates/huggr-agent/src/scratch.rs` | Per-lineage mutable filesystem state |
| `crates/huggr-toolkit/src/runtime.rs` | Manifest-to-runtime assembly and effective model catalog |
| `crates/huggr-toolkit/src/build.rs` | Standalone artifact generation and dependency resolution |
| `bindings/typescript/src/agent.ts` | Typed JavaScript host semantics |
| `bindings/typescript/agent_driver.js` | Generic browser driver vendored into the extension |
| `examples/chrome-extension/manifest.json` and `chrome_api.js` | Broadest application trust boundary and browser effects |
| `.github/workflows/ci.yml` | Actual verification and dependency installation policy |

### How to run, test, and build

The following commands match the implementation and documentation. Provider-backed asks require a model catalog and the provider API-key environment variable; tests intended to use mock providers should not require credentials.

```bash
# Full Rust quality gate
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo doc --workspace --no-deps

# Core invariant audit
cargo test -p huggr-core
cargo tree -p huggr-core
cargo build --target wasm32-unknown-unknown -p huggr-core

# Development CLI and standalone build
cargo run -p huggr-toolkit --bin huggr -- run examples/huglet-weather "What is the weather in Paris?"
cargo run -p huggr-toolkit --bin huggr -- build examples/huglet-weather --release

# Slow release-surface gates
cargo test -p huggr-toolkit --test conformance -- --ignored --nocapture
cargo test -p huggr-toolkit --test build_cli -- --ignored --nocapture
cargo test -p huggr-toolkit --test build_python -- --ignored --nocapture

# Python embedding
cd bindings/python
python -m venv .venv
. .venv/bin/activate
pip install maturin pytest mypy
maturin develop --release
python -m pytest tests -q
mypy python

# TypeScript and Chrome extension
cd bindings/typescript && npm install && npm test
cd ../.. && ./examples/chrome-extension/build.sh
```

Audit verification results: formatting passed; Clippy passed with warnings denied; the core dependency tree satisfied the sans-I/O rule; TypeScript tests passed 18/18; a fresh Python 3.12 environment built the native extension and passed 22/22 tests; Python type checking passed with a warning that the configured Python 3.9 target is unsupported by the installed mypy 2.3.0; the Chrome extension build passed; and the standalone Python build gate passed. The workspace test command failed six `huggr-toolkit` tests because the developer's valid global provider configuration removed the warning each test expected. The ignored conformance test passed but made live provider calls; the ignored CLI build gate failed after a real successful model response contradicted its expected offline error.

## 3. Top risks, ranked by severity

| Rank | ID | Severity | Finding |
| ---: | --- | --- | --- |
| 1 | HGR-001 | High | Auto-approved all-site browser tools enable a prompt-injection data relay across trust boundaries |
| 2 | HGR-002 | Medium | Every durable step clones and rewrites the full growing trace |
| 3 | HGR-003 | Medium | Provider buffers and host event channels have no effective bounds or timeouts |
| 4 | HGR-004 | Medium | JavaScript interruption rejects a wrapper but does not stop in-flight browser effects |
| 5 | HGR-005 | Medium | Trace, blob, scratch, and checkpoint finalization is not one recoverable transaction |
| 6 | HGR-006 | Medium | MCP startup and requests can wait forever while serializing all calls behind one lock |
| 7 | HGR-007 | Medium | TypeScript and browser hosts omit model deltas from the reducer event stream |
| 8 | HGR-008 | Medium | Ignored tests documented as offline can use real provider credentials and incur calls |
| 9 | HGR-009 | Medium | Normal workspace tests depend on the developer's model configuration |
| 10 | HGR-010 | Medium | Release surfaces and Chrome behavior are not exercised by the required CI gate |
| 11 | HGR-011 | Medium | JavaScript, Python, and workflow dependencies are resolved non-reproducibly |
| 12 | HGR-012 | Medium | Trace header listing reads every complete trace into memory |
| 13 | HGR-013 | Medium | Resume and fork synchronously copy the full scratch tree on an async worker |
| 14 | HGR-014 | Medium | Declared Python 3.9 support is neither executed in CI nor supported by the current unpinned type checker |
| 15 | HGR-015 | Low | A failed immutable trace save can leave an empty reserved file |
| 16 | HGR-016 | Low | YAML and Python native dependencies need a planned maintenance pass |
| 17 | HGR-017 | Low | The publish workflow does not verify the exact wheel before OIDC publication |

## 4. Detailed findings

### HGR-001: Auto-approved all-site browser tools enable a prompt-injection data relay across trust boundaries

**Severity:** High
**Category:** Security, architecture
**Status:** Confirmed capability chain; exploitation depends on the model following attacker-controlled page instructions.

**Evidence:** The extension grants `tabs`, `scripting`, and `webNavigation`, injects its content script on every HTTP(S) page, and grants every HTTP(S) origin (`examples/chrome-extension/manifest.json:19-40`). Its README states that every browser capability is auto-approved (`examples/chrome-extension/README.md:14`) and calls the all-site permissions a development setting (`examples/chrome-extension/README.md:66`). `downloadToLocalStore` performs `fetch(url)` on a model-provided URL without scheme, hostname, resolved-IP, redirect, or provenance checks (`examples/chrome-extension/chrome_api.js:151-183`). `uploadLocalFile` converts any stored blob to a data URL and injects it into a file input on a model-selected tab (`examples/chrome-extension/chrome_api.js:204-219`). Page text and snapshots are model inputs, and the same dispatcher exposes tab listing, page mutation, form submission, downloads, and uploads (`examples/chrome-extension/chrome_api.js:9-62`).

**Why it matters:** A malicious page can place prompt-injection text in content the agent reads. If the model follows it, the model can request an extension-context fetch from loopback, RFC1918, link-local, intranet, or cloud metadata endpoints, retain the response as a blob, open or select an attacker page, and place that blob into an upload input. The 5 MiB response cap limits size but does not prevent cross-origin data relay. Even without the download/upload chain, auto-approved page typing, clicking, submitting, tab closing, and all-tab enumeration give untrusted content a large ambient authority surface.

**Conditions:** A user installs the development extension, configures a provider, asks it to process attacker-controlled or compromised page content, and the model emits the relevant tool calls. Browser and target-page behavior must permit the requested fetch and upload. This is not a claim that every model will follow such an instruction.

**Recommendation:** Make risky operations approval-gated by default and separate read-only browsing from network, write, submit, upload, download, tab-close, and cross-tab capabilities. Replace static all-site permissions with optional per-origin grants. Validate URLs before and after redirects; block loopback, link-local, private, multicast, non-HTTP(S), credential-bearing, and disallowed-port destinations, and defend against DNS rebinding by validating resolved addresses. Track blob origin and sensitivity so data fetched from a private or different origin cannot be uploaded without an explicit user decision. Do not include download/upload capabilities in the default development grant. Add adversarial page-content tests that assert the host refuses private-network fetch and cross-origin relay even when the model requests it.

### HGR-002: Every durable step clones and rewrites the full growing trace

**Severity:** Medium
**Category:** Performance, scalability, durability
**Status:** Confirmed.

**Evidence:** Every checkpoint boundary marks the engine dirty (`crates/huggr-host/src/engine.rs:200-206`). `Engine::trace` clones the entire recorded event vector, durable log, and command vector (`crates/huggr-host/src/engine.rs:213-230`). `flush_checkpoint` then serializes that full trace atomically (`crates/huggr-host/src/engine.rs:233-248`). Filesystem-backed asks always create and attach a live checkpoint (`crates/huggr-agent/src/agent.rs:451-468`). Model deltas are transport events and are recorded by the native host, so long streamed answers increase the checkpoint payload even though deltas are not durable log records.

**Why it matters:** If a session has `n` durable boundaries and its trace grows roughly linearly, cumulative checkpoint work is quadratic in the number of events and commands. Atomic save also creates a second full file during each rewrite. Short huglets will not notice; long tool loops, verbose model streams, or frequent capability chunks can spend increasing CPU, allocation, and disk bandwidth on persistence and may exhaust storage temporarily.

**Conditions:** Filesystem-backed native agent, recording enabled, multiple model/tool completion boundaries, especially with large event payloads or streamed deltas.

**Recommendation:** Persist envelopes and emitted commands to an append-only per-ask journal, fsync at defined boundaries, and write compact reducer snapshots only periodically. On finalization, produce the portable immutable trace once. If the portable format must remain a single JSON object, keep the live checkpoint format distinct and incrementally appendable. Add a benchmark that measures bytes written and latency for 10, 100, and 1,000 tool turns and fails on superlinear growth.

### HGR-003: Provider buffers and host event channels have no effective bounds or timeouts

**Severity:** Medium
**Category:** Reliability, performance, security hardening
**Status:** Confirmed.

**Evidence:** `OpenAiAdapter::new` uses `reqwest::Client::new()` without connect, request, or idle-stream timeouts (`crates/huggr-providers/src/openai.rs:34-43`). Non-success bodies are consumed in full with `resp.text()` (`crates/huggr-providers/src/openai.rs:202-207`). The SSE parser extends a `Vec<u8>` until it sees a newline, with no line or cumulative-response limit (`crates/huggr-providers/src/openai.rs:275-318`). Consolidated text, reasoning, and tool arguments also grow with the response. The engine and both stream sinks use Tokio unbounded channels (`crates/huggr-host/src/engine.rs:114-122`, `crates/huggr-host/src/model.rs:35-68`, `crates/huggr-host/src/capability.rs:45-65`). Agent and Python event surfaces also use unbounded channels. Default agent limits may be unset, so they do not provide a universal guard.

**Why it matters:** A stalled endpoint can hold a request forever. A malicious or broken OpenAI-compatible endpoint can send an arbitrarily large error body, an SSE event without a newline, or deltas faster than the single-threaded reducer and frontend consume them. Memory then grows without backpressure. This is a denial-of-service risk when provider URLs are operator-configurable and a reliability risk under ordinary provider faults or verbose tools.

**Conditions:** Slow or hostile provider, connection that never closes, oversized response, high-rate delta/chunk source, slow frontend or reducer consumer, or an agent without explicit output limits.

**Recommendation:** Build the HTTP client with configurable connect, total-request, and idle-read timeouts. Cap error bodies, SSE line length, total streamed bytes, accumulated assistant text/reasoning, tool-call argument size, delta count, and capability chunk size/count. Replace unbounded event channels with bounded channels and asynchronous backpressure; coalesce UI text deltas where appropriate. When a bound is exceeded, abort the effect and submit a typed, auditable error. Exercise each bound with a fake provider and a flooding capability.

### HGR-004: JavaScript interruption rejects a wrapper but does not stop in-flight browser effects

**Severity:** Medium
**Category:** Correctness, reliability
**Status:** Confirmed.

**Evidence:** The typed TypeScript API gives custom tools an `AbortSignal`, but `abortable` only rejects its wrapper promise; a tool that ignores the signal continues (`bindings/typescript/src/agent.ts:196-214`, `bindings/typescript/src/agent.ts:406-412`). The generic browser driver calls `host.invokeCapability` without passing a signal, then uses `Promise.race` against an abort rejection (`bindings/typescript/agent_driver.js:170-177`, `bindings/typescript/agent_driver.js:329-337`). The Chrome capability dispatcher and its fetch, IndexedDB, content-script, navigation-wait, and page-mutation operations accept no common cancellation signal (`examples/chrome-extension/chrome_api.js:9-70`, `examples/chrome-extension/chrome_api.js:151-219`).

**Why it matters:** The trace and UI can report an interrupted ask while the underlying effect later completes. A download may still be stored, a page operation may still mutate state, or a listener/timer may remain live. The completed side effect is not represented by the terminal trace, which undermines auditability and surprises users who treat interrupt as a stop command.

**Conditions:** User interrupts while a capability is pending and the capability does not natively observe the signal. Browser-host capabilities always meet the latter condition because no signal is passed.

**Recommendation:** Make cancellation part of the host capability contract. Pass one `AbortSignal` through the generic driver into every browser capability; cancel fetch readers, remove event listeners and timers, and ensure content-script requests can be abandoned safely. Distinguish operations that cannot be rolled back and require confirmation before starting them. Do not write the terminal trace until running operations have acknowledged cancellation or have been explicitly marked detached. Add a delayed-side-effect test proving that no file, navigation, or mutation appears after interruption.

### HGR-005: Trace, blob, scratch, and checkpoint finalization is not one recoverable transaction

**Severity:** Medium
**Category:** Correctness, durability
**Status:** Confirmed failure window.

**Evidence:** The agent first stores the completed immutable trace (`crates/huggr-agent/src/agent.rs:575-595`), then sweeps outbound blobs (`:597-598`), then finalizes scratch under the new trace ID (`:600-601`), and finally removes the live checkpoint (`:602-604`). Any error or crash after trace persistence returns an infrastructure failure while leaving some subset of a completed trace, checkpoint, outbound files, and finalized scratch. On resume, missing parent scratch is silently replaced with an empty directory (`crates/huggr-agent/src/scratch.rs:203-214`).

**Why it matters:** A caller can receive an error even though a completed immutable trace is visible. A later resume of that trace may start with missing scratch state, producing behavior that cannot be reconstructed from the trace and without an explicit corruption signal. Retained checkpoints can also make one logical ask appear both completed and interrupted.

**Conditions:** Disk full, permission failure, blob-store error, cross-filesystem rename problem, process crash, or host termination during the finalization window.

**Recommendation:** Stage outbound blobs and scratch under transaction-specific temporary names, validate them, then commit a small completion marker or trace header last. Alternatively, persist the trace initially as incomplete and atomically promote it only after scratch and blob references are durable. Record enough state to resume or roll back an interrupted commit. Treat a missing finalized scratch directory for a parent that is expected to have one as an explicit integrity error rather than silently seeding empty state. Add fault-injection tests after each write/rename boundary.

### HGR-006: MCP startup and requests can wait forever while serializing all calls behind one lock

**Severity:** Medium
**Category:** Reliability, operations
**Status:** Confirmed.

**Evidence:** One async mutex owns the MCP child's stdin, stdout, ID counter, and process handle (`crates/huggr-host/src/mcp.rs:124-134`). A request holds that mutex across write and an unbounded read loop until a matching numeric ID arrives (`crates/huggr-host/src/mcp.rs:201-226`). There is no startup, initialization, tool-list, request, line-length, or idle timeout. `load_stdio` must connect and list tools before it can return capabilities (`crates/huggr-host/src/mcp.rs:266-270`), so an ask-level execution timeout does not necessarily protect agent construction.

**Why it matters:** A child that starts but never answers can hang agent assembly or a tool call indefinitely. One stuck request blocks every later request and notification on the same client. A server that emits endless unmatched IDs or a line without a newline can also hold memory and the lock.

**Conditions:** Misconfigured, crashed, malicious, or protocol-incompatible MCP server; child writes logs to stdout; lost response; or long-running tool beyond operator expectations.

**Recommendation:** Add configurable startup, request, and idle timeouts plus a maximum JSON-line size. Run one reader task that dispatches responses by ID instead of holding an I/O mutex across the response wait. Monitor child exit, reject all pending requests when it exits, and kill/restart it on timeout according to policy. Support protocol notifications explicitly and validate the full JSON-RPC envelope. Test hung initialization, endless notifications, oversized lines, child exit, and one timed-out request followed by another call.

### HGR-007: TypeScript and browser hosts omit model deltas from the reducer event stream

**Severity:** Medium
**Category:** Correctness, architectural consistency
**Status:** Confirmed.

**Evidence:** The TypeScript `WasmSession` interface has methods for model completion and error but no model-delta submission (`bindings/typescript/src/agent.ts:33-47`). Its model loop yields `text_delta` UI events and submits only the final consolidated output (`bindings/typescript/src/agent.ts:157-187`). The generic browser driver likewise forwards text to UI/persistence callbacks and later calls only `submit_model_done` (`bindings/typescript/agent_driver.js:125-158`). Generated WASM declarations expose no `submit_model_delta` method. In contrast, the core buffers `ModelDelta::Text` for live operation state (`crates/huggr-core/src/brain.rs:139-153`), native model sinks submit every delta, and cancellation tests assert partial output from buffered deltas.

**Why it matters:** A JavaScript-hosted interruption or model failure loses partial assistant text from reducer state and trace semantics. Portable traces from different hosts do not contain equivalent input event streams even though replay verification may still pass for the events that are present. This weakens cross-surface conformance and makes interrupted browser/Node sessions less inspectable than native sessions.

**Conditions:** TypeScript, Node, or Chrome host; model produces text deltas; ask is inspected, failed, or interrupted before successful consolidation.

**Recommendation:** Expose model-delta submission on `AgentSession`/WASM and feed each text, reasoning, and tool-call-start delta through the reducer before emitting UI events. Keep deltas non-durable in the core log as designed, but include submitted envelopes in portable traces consistently. Add a cross-host test that interrupts mid-model and compares partial state, command sequence, and replay behavior with the native host.

### HGR-008: Ignored tests documented as offline can use real provider credentials and incur calls

**Severity:** Medium
**Category:** Test safety, operations, cost control
**Status:** Confirmed during this audit.

**Evidence:** The conformance suite says “No network” and assumes every ask receives an error from an unreachable model (`crates/huggr-toolkit/tests/conformance.rs:9-15`). The CLI build fixture explicitly says it remains hermetic even with a developer catalog and key and embeds an unreachable test provider (`crates/huggr-toolkit/tests/build_cli.rs:19-36`); it later asserts that an ask returns `status: "error"` (`:140-148`). The runtime intentionally allows an existing host/global catalog to replace an embedded build snapshot. In the audited environment, the conformance test made successful real provider calls and passed; the real CLI build test made a real successful provider call and failed because the result was not an error.

**Why it matters:** A developer or CI operator running a test documented as offline can leak fixture prompts to an external provider, consume quota, incur cost, and get nondeterministic results. The test does not prove the intended fallback behavior when a global catalog silently changes its provider.

**Conditions:** Slow ignored gate is run on a machine with a valid Huggr global catalog and provider credentials.

**Recommendation:** Give tests an explicit isolated catalog/home path and clear or override every relevant environment variable in the spawned process. Prefer a local mock HTTP server whose requests are asserted, rather than an unreachable port. Add a test-only “do not read global catalog” option to generated artifacts if necessary. Fail immediately if a test request targets a non-loopback host. Update comments only after the isolation is enforced.

### HGR-009: Normal workspace tests depend on the developer's model configuration

**Severity:** Medium
**Category:** Test reliability, developer experience
**Status:** Confirmed during this audit.

**Evidence:** Runtime assembly resolves API tokens from options or real provider environment variables and emits a warning only when the result is empty (`crates/huggr-toolkit/src/runtime.rs:388-400`). Multiple tests build agents and assert `warnings.len() == 1` without isolating provider configuration, for example `crates/huggr-toolkit/src/runtime.rs:1203-1219`, `:1222-1244`, `:1285-1300`, and `:1400-1414`. `cargo test --workspace --all-targets` failed six such tests in the audited environment because a valid global model setup produced zero warnings. The same revision's recent clean GitHub CI runs were green, confirming environment dependence rather than a general compile failure.

**Why it matters:** The repository's primary documented test command is not reliable on the exact developer machines most likely to have Huggr configured. Failures obscure real regressions and encourage skipping the full suite.

**Conditions:** A configured provider token or catalog satisfies the test's default provider mapping.

**Recommendation:** Inject environment and catalog readers into runtime assembly, and use deterministic test maps. Assert the warning's semantic content only in a test that explicitly supplies an empty token. For tests unrelated to credentials, pass an explicit test token or suppress warning collection. Run a CI matrix once with an empty environment and once with a populated fake catalog to prevent recurrence.

### HGR-010: Release surfaces and Chrome behavior are not exercised by the required CI gate

**Severity:** Medium
**Category:** Testing, CI/CD
**Status:** Confirmed.

**Evidence:** The required CI workflow runs Rust unit/integration tests, builds examples, builds the extension, tests Python on one version, and tests TypeScript (`.github/workflows/ci.yml:11-125`). The conformance, real CLI build, and generated Python build suites are `#[ignore]` and are not invoked. The README states that generated Python, typed Node/browser, and Chrome surfaces are not in the conformance gate (`README.md:155-161`). The Chrome job only runs `build.sh`; it has no behavioral test of permission, persistence, interruption, or capability dispatch. `cargo test` for the excluded `huggr-python` crate runs zero Rust tests; behavior is covered only through Python.

**Why it matters:** The most deployment-specific code can regress while the required pull-request checks remain green. The browser surface also owns the broadest security boundary but receives only compilation coverage.

**Conditions:** Changes to generated shims, catalog embedding/override, wheel packaging, WASM glue, generic browser driver, extension persistence, or Chrome capability implementation.

**Recommendation:** Add a hermetic release-surface CI job, possibly scheduled or change-path-triggered, that runs conformance plus real CLI and generated Python builds against a local mock provider. Extend conformance to typed Node/browser and generated Python. Use a headless Chrome integration suite for capability registration, permission decisions, cancellation, private-network rejection, and persistence. Add at least a native smoke test around the PyO3 boundary. Cache detached build targets to control runtime.

### HGR-011: JavaScript, Python, and workflow dependencies are resolved non-reproducibly

**Severity:** Medium
**Category:** Dependencies, supply chain, operations
**Status:** Confirmed configuration weakness; no dependency compromise was found.

**Evidence:** The TypeScript `package-lock.json` is deliberately ignored (`.gitignore:24-28`), package ranges use carets (`bindings/typescript/package.json:30-34`), and CI runs `npm install` (`.github/workflows/ci.yml:107-112`). Python CI installs unconstrained current versions of maturin, pytest, and mypy (`.github/workflows/ci.yml:78-89`), while the publish workflow independently installs unconstrained maturin (`.github/workflows/publish-huglet-docs.yml:24-30`). GitHub Actions use mutable major tags such as `actions/checkout@v4`, `Swatinem/rust-cache@v2`, and `taiki-e/install-action@v2`. Ignored detached builds updated registry indexes and resolved current compatible packages during the audit.

**Why it matters:** Identical commits can compile and test against different dependency graphs over time. A new transitive release can break CI or generated artifacts without a repository change, and mutable action tags broaden the workflow trust surface. This also makes incident reproduction difficult.

**Conditions:** Upstream package or action release, yanked package, changed registry metadata, or compromised mutable tag.

**Recommendation:** Commit the TypeScript lockfile and use `npm ci`. Pin Python development/build tools through a constraints or lock file and install the same maturin version in CI and release. Pin third-party actions to full commit SHAs, with a dependency updater responsible for reviewable bumps. Use `cargo --locked` for release and detached builds where possible. Add scheduled Rust and npm advisory scans; no `cargo-audit` or `cargo-deny` tool was installed in the audited environment, so known-vulnerability status was not verified.

### HGR-012: Trace header listing reads every complete trace into memory

**Severity:** Medium
**Category:** Performance, scalability, misleading abstraction
**Status:** Confirmed.

**Evidence:** `TraceStore::head` is documented as reading a header “without loading its events,” but calls `std::fs::read(path)` and then deserializes `HeadOnly` from the entire byte buffer (`crates/huggr-agent/src/store.rs:341-350`). `list` calls `head` for every completed trace and checkpoint (`crates/huggr-agent/src/store.rs:353-414`). Avoiding allocation of the event vector does not avoid reading or allocating the full file bytes.

**Why it matters:** Listing traces, building lineage views, and analytics pay I/O and peak allocations proportional to the total size of all trace files, rather than the number of headers. A long-running agent with many large traces can make a nominally cheap audit operation slow or memory-heavy.

**Conditions:** Many traces, large event/tool payloads, or repeated `traces`/`stats` operations.

**Recommendation:** Persist a compact sidecar header atomically with each trace, or maintain an append-only/indexed metadata store rebuildable from trace files. Validate sidecar identity against the trace ID and update it in the same completion protocol. A streaming JSON parser would reduce peak allocation but still scans the full object; it is only an interim fix. Add a benchmark that lists thousands of large synthetic traces and asserts bytes read scale with header size.

### HGR-013: Resume and fork synchronously copy the full scratch tree on an async worker

**Severity:** Medium
**Category:** Performance, scalability, async correctness
**Status:** Confirmed.

**Evidence:** The async filesystem scratch backend calls synchronous `remove_dir_all`, `create_dir_all`, recursive `read_dir`, and `fs::copy` directly (`crates/huggr-agent/src/scratch.rs:192-238`, `:491-525`). Each resume or fork copies every regular file from the parent's finalized scratch except the top-level outbound directory. No byte/file quota is enforced at this boundary.

**Why it matters:** Large accumulated scratch state blocks a Tokio worker and duplicates the full tree for every lineage edge. A sequence of resumes with growing state can create quadratic cumulative disk I/O and storage; parallel asks can starve unrelated asynchronous work.

**Conditions:** Filesystem scratch backend, resume/fork, large or numerous files, long lineage, or concurrent agents on a small Tokio pool.

**Recommendation:** Move blocking filesystem work to `spawn_blocking` immediately. Longer term, use copy-on-write snapshots, reflinks where available, or immutable content-addressed files plus a small per-trace manifest. Enforce configurable scratch file-count and byte quotas and reject symlinks/special files explicitly. Benchmark deep and wide scratch trees under concurrent resumes.

### HGR-014: Declared Python 3.9 support is neither executed in CI nor supported by the current unpinned type checker

**Severity:** Medium
**Category:** Compatibility, developer experience
**Status:** Confirmed support-process mismatch; runtime incompatibility was not proven.

**Evidence:** The package declares `requires-python = ">=3.9"` and configures strict mypy for Python 3.9 (`bindings/python/pyproject.toml:5-24`). CI tests only Python 3.12 and installs the latest mypy (`.github/workflows/ci.yml:65-89`). In a clean audit environment, mypy 2.3.0 completed but warned that Python 3.9 is unsupported and that at least 3.10 must be used.

**Why it matters:** Python 3.9 syntax, wheel loading, ABI, dependency, and typing regressions can ship without detection, while the type checker no longer validates the declared target. Users receive a broader compatibility promise than the release process proves.

**Conditions:** Python 3.9 user or a future source/dependency change that assumes a newer Python version.

**Recommendation:** Decide whether Python 3.9 remains a supported target. If yes, run Python 3.9 plus a current version in CI, pin a mypy release that can model 3.9, and smoke-test the built abi3 wheel on both interpreters. If not, raise `requires-python`, PyO3 abi3 settings, documentation, generated wrappers, and CI together.

### HGR-015: A failed immutable trace save can leave an empty reserved file

**Severity:** Low
**Category:** Reliability, storage hygiene
**Status:** Confirmed error path.

**Evidence:** `TraceStore::put` reserves the final path with `create_new`, drops the empty file, and calls `trace.save_atomic(...)?` without cleanup on error (`crates/huggr-agent/src/store.rs:298-328`). The analogous checkpoint reservation explicitly removes its file if `save_atomic` fails (`crates/huggr-agent/src/store.rs:258-263`). Listing skips corrupt entries with a warning, so the placeholder can remain indefinitely and force later identical content to a suffixed ID.

**Why it matters:** Disk or permission failures leave corrupt namespace entries, noisy listings, and false hash collisions. The completed-trace directory is no longer composed solely of immutable valid traces.

**Conditions:** Serialization, temporary-file, fsync, or rename failure after path reservation.

**Recommendation:** On save failure, remove only the exact placeholder this call reserved and return the original error, preserving any cleanup error as context. Prefer a reservation strategy that does not expose an empty final-name file. Add a deterministic injected-save-failure test and a crash-recovery cleanup pass for zero-length entries.

### HGR-016: YAML and Python native dependencies need a planned maintenance pass

**Severity:** Low
**Category:** Dependency maintenance
**Status:** Confirmed staleness; not a vulnerability claim.

**Evidence:** The workspace pins `serde_yaml = "0.9"` (`Cargo.toml:30-35`), whose published crate is marked deprecated. The Python native binding uses PyO3 0.23.5 while the registry had materially newer releases at audit time. `cargo tree -d` reported no duplicate package versions, which is positive. A Rust advisory scan could not be run because neither `cargo-audit` nor `cargo-deny` was installed.

**Why it matters:** Deprecated parsers and aging native-binding layers receive less maintenance and make later Rust/Python upgrades larger. This evidence does not establish an exploitable issue in the resolved versions.

**Conditions:** Future compiler/Python upgrade, parser bug, or security advisory affecting the current versions.

**Recommendation:** Evaluate a maintained YAML implementation or a narrower configuration format and add parser corpus tests before migration. Plan incremental PyO3 upgrades with the Python-version matrix and generated-wheel tests. Add automated advisory and license-policy checks, review results rather than auto-failing on untriaged advisories, and document accepted exceptions.

### HGR-017: The publish workflow does not verify the exact wheel before OIDC publication

**Severity:** Low
**Category:** Release engineering
**Status:** Confirmed workflow gap.

**Evidence:** The manual workflow checks out source, installs current maturin, builds the `huglet-docs` wheel, uploads it, and publishes that artifact in a separate job (`.github/workflows/publish-huglet-docs.yml:17-53`). It does not install the produced wheel, run an import/describe/ask smoke test, record hashes, or require the repository's test gates inside the publishing workflow. OIDC and environment protection are good controls, but they authenticate publication rather than validate the artifact.

**Why it matters:** A packaging-only defect, wrong bundled manifest, missing native symbol, or build-environment drift can reach PyPI even if source CI was previously green. Manual dispatch can also occur at a commit whose checks are not enforced by the workflow itself.

**Conditions:** Release from an unverified commit or a defect specific to the generated wheel/build image.

**Recommendation:** Build with pinned tools, install the exact wheel into clean Python 3.9 and current-Python environments, run import, typed response, `describe`, and mock-provider ask smoke tests, and publish only the hash-identified artifact that passed. Protect the `pypi` environment with required reviewers and restrict dispatch to tags or validated commits.

## 5. Testing gaps

The suite is strong around core behavior: scripted reducer transitions, deterministic replay, concurrent operations, cancellation races, path jail behavior, provider fixtures, engine end-to-end flows, trace lineage, manifests, and typed language APIs. Assertions frequently pin commands and wire shapes, which is more valuable than raw line coverage.

The highest-priority missing behaviors are:

- Adversarial Chrome integration: prompt injection, private-network URL rejection, blob-origin enforcement, destructive-action approval, cancellation with delayed effects, and persistence recovery.
- Bounded-resource behavior: oversized SSE lines/error bodies/tool arguments, stream flood, slow consumer, MCP oversized line, scratch quota, and trace growth.
- Persistence fault injection: failure after checkpoint reservation, trace write, blob sweep, scratch staging/finalize, and checkpoint removal.
- Cross-host interrupted-model conformance, including partial output and identical reducer commands for Rust, TypeScript, and browser hosts.
- Hermetic model-catalog precedence tests in subprocesses with both empty and populated fake user configuration.
- Generated artifact smoke tests for CLI, MCP, typed Python wheel, and Node/browser package as required or scheduled CI.
- Python support-matrix tests and installation of the exact built abi3 wheel, not only `maturin develop` on 3.12.
- Trace listing and checkpoint scalability benchmarks that assert asymptotic behavior and peak memory.

No coverage report is produced, and no mutation or fuzz target is present for high-risk parsers such as SSE, JSON-line MCP framing, manifests, trace import, or provider tool-call assembly. Coverage percentage alone is not the remedy; targeted fuzzing and failure-path tests would have higher value.

## 6. Security review

The confirmed security finding is HGR-001. It is High because the browser host bridges untrusted page content, an instruction-following model, broad cross-origin network access, persistent blobs, and page upload without a human authorization boundary. HGR-003 and HGR-006 are denial-of-service hardening issues when endpoints or MCP processes are untrusted or faulty. HGR-008 is a test-time data disclosure and cost risk because real credentials can be used unexpectedly.

Native authorization design is otherwise sound for the stated local toolkit model. Capabilities are registered from grants; the reducer does not invent privileged built-ins; restricted shell avoids shell parsing; filesystem roots are operator-selected and canonicalized; delegation does not intentionally widen grants; model tokens are resolved in the host; and MCP/full shell/child agents are documented as operating-system escape hatches. There is no network server authentication surface in the core product: CLI JSON and MCP stdio are local process interfaces. Any deployment that wraps them in HTTP would need its own authentication, authorization, rate limiting, tenant isolation, and storage segregation.

Trace, scratch, feedback, memory, and blob directories inherit operating-system account protections. They can contain sensitive prompts, page content, file contents, tool arguments/results, provider errors, and lineage. The repository does not encrypt them or enforce retention, which is acceptable for a local prototype but should be explicit for shared or regulated hosts. Provider error text and tool results can also reach logs/answers; consumers should avoid treating them as safe display markup.

Dependency vulnerability status is unverified. Registry version checks and `cargo tree -d` were performed, but no advisory scanner was available and no npm audit result is a required CI artifact. This report does not claim that the current lockfile contains a known vulnerability.

## 7. Performance and scalability review

HGR-002 is the largest algorithmic issue: checkpoint I/O grows quadratically with session length. HGR-012 makes trace listings proportional to total trace bytes, and HGR-013 copies scratch state per lineage edge while blocking the async executor. HGR-003 allows provider and capability streams to outgrow consumers. Together these mean the system's current happy path is well suited to the stated goal of small huglets but lacks guardrails when “small” assumptions are violated.

The reducer itself respects the intended O(1)-ish event-handler design: deltas append to in-flight buffers and durable records consolidate logical messages. Concurrency stays in the host and operation tasks are indexed by ID. Cost metadata is derived after the turn rather than repeatedly recalculated. These are good choices.

Scalability work should measure: events and bytes per ask, checkpoint bytes written, peak event-channel depth, provider time to first/last byte, MCP request latency/timeouts, trace-list bytes read, scratch bytes copied, and finalization duration. There is currently no structured observability for these operational quantities beyond trace contents, notices, and aggregate cost/token/tool statistics.

## 8. Maintainability and developer-experience review

The crate layering and naming are clear, and the repository resists speculative abstraction. The core protocol types are small, policy is pluggable, and the language surfaces mirror the ask/answer contract. Documentation is task-oriented and the examples are meaningful rather than toy-only. Formatting and Clippy were clean.

The maintenance pressure is concentrated at synchronization boundaries: four public model tiers must remain aligned across Rust, manifests, generated artifacts, Python, TypeScript, examples, docs, and skills; JavaScript files are both authored and vendored/generated; Rust wire types must mirror Python/TypeScript types; and generated release surfaces are slow enough to be ignored. The repository documents these obligations well, but CI does not fully enforce them. HGR-007 is an example of semantic drift even though types compile.

Local setup is mostly discoverable, but the nominal `cargo test` command fails on configured developer machines (HGR-009), Python tool versions drift (HGR-014), and slow gates unexpectedly use real services (HGR-008). These issues make the safe developer path less predictable than the documentation suggests. A single hermetic `just verify` or `cargo xtask verify` entry point could set isolated homes, run fast checks, and offer explicit `verify-release` and `verify-live` tiers without hiding expensive behavior.

Configuration defaults favor prototype flexibility: limits and timeouts can be absent, an existing runtime catalog overrides embedded mappings, and custom storage/tool implementations are trusted. Those are defensible tradeoffs, but effective limits, catalog source, credential-resolution status, and enabled escape hatches should be exposed in one redacted diagnostic view before execution.

## 9. Recommended remediation plan

### Immediate fixes

1. Disable extension download/upload and destructive/write actions by default; add approval prompts and reject private/local network destinations before the next extension distribution.
2. Isolate all tests from the real global catalog, home, and credentials; add a loopback-only assertion to mock-provider tests.
3. Add provider/MCP connect, request, idle, body, line, and accumulated-output limits with conservative defaults and explicit errors.
4. Fix immutable trace reservation cleanup and surface missing parent scratch as an integrity error.
5. Commit the npm lockfile, use `npm ci`, pin Python build/test tools, and pin workflow actions by commit SHA.

### Near-term improvements

1. Add a hermetic release-surface CI job and headless Chrome security/cancellation tests.
2. Propagate `AbortSignal` through browser capabilities, wait for cancellation acknowledgement, and submit model deltas through WASM on JavaScript hosts.
3. Stage blob and scratch outputs and introduce a recoverable completion marker so an answer commits all persistent state coherently.
4. Bound event channels and add load tests for provider floods, capability chunks, and slow frontends.
5. Add Python 3.9/current testing or formally raise the supported minimum; verify built wheels in clean interpreters.
6. Add advisory, license, and dependency-update automation with reviewed exceptions.

### Longer-term refactors

1. Separate live recovery persistence from the portable final trace: use an append-only journal, periodic snapshots, and one final compaction.
2. Store trace metadata in compact sidecars or a rebuildable index so listing and analytics do not scan full traces.
3. Replace recursive scratch copies with immutable content-addressed manifests or copy-on-write snapshots and enforce quotas.
4. Split browser capabilities into explicit privilege classes with provenance-aware information-flow policy rather than one auto-approved dispatcher.
5. Build a cross-language conformance harness that replays the same scripted provider/tool stream through native Rust, Python, TypeScript/Node, and browser hosts and compares commands, logs, traces, interruption state, and answers.

## 10. Open questions and assumptions

- Is the Chrome extension strictly an unsafe development demonstration, or is it intended for eventual end-user distribution? The severity remains High for anyone who installs and uses it, but the remediation schedule depends on distribution intent.
- Should a runtime host catalog always override the catalog embedded at build time? The current behavior supports operator control but makes a self-contained artifact and tests less predictable. A locked-catalog mode may be needed.
- What durability guarantee is intended for a returned error after a trace has already been stored? The current API does not distinguish “answer completed but state finalization failed” from a wholly failed ask.
- Is scratch state required for every resumable completed trace, or is missing scratch intentionally equivalent to empty scratch? Documentation describes scratch as lineage state, so this audit assumes silent loss is not intended.
- Are provider endpoints and MCP servers considered fully trusted operator infrastructure? Bounds and timeouts are still reliability requirements, but stronger sandboxing is needed if untrusted users can choose them.
- Is Python 3.9 still a product requirement? The abi3 and packaging configuration say yes; CI behavior does not verify it.
- Are immutable traces expected to remain small enough for whole-file JSON forever? The current design and “small huglet” goal suggest this may be an accepted prototype tradeoff, but live checkpoint rewriting becomes expensive before final trace portability itself is a problem.
- Branch protection, environment-review rules, external secret scanning, repository dependency alerts, and published-package consumers were not inspectable from the local checkout and were not verified.
- The audit did not perform live exploitation against private endpoints, publish artifacts, inspect user trace contents, or run destructive browser actions. It did run ignored tests, which unexpectedly contacted the configured real model provider as described in HGR-008.
- Known-vulnerability status is unverified because advisory tooling was not installed. No vulnerability is inferred solely from a package being old or deprecated.
