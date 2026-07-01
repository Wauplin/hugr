//! # baton-wasm — the browser/JS binding
//!
//! Phase 4 (portability): the **same** sans-IO brain ([`baton_core`]) compiled
//! to WebAssembly and exposed to a JavaScript host. This is the whole payoff of
//! the design (ARCHITECTURE §10/§11, ROADMAP Phase 4): the brain never did IO,
//! so it drops into a browser tab / Chrome extension with **no backend** — the
//! host (fetch-based model adapter, DOM front-end, tab capabilities) is the only
//! thing that differs from the native CLI.
//!
//! ## The binding is deliberately trivial
//!
//! The entire core ↔ host contract is two enums plus two methods
//! (ARCHITECTURE §2). Every [`Event`](baton_core::Event) and
//! [`Command`](baton_core::Command) is `serde`-serializable, so the JS boundary
//! is just **JSON in, JSON out** — no hand-written type marshalling. A JS host:
//!
//! ```js
//! import init, { BatonBrain } from "./baton_wasm.js";
//! await init();
//! const brain = new BatonBrain(JSON.stringify(policyConfig));
//! brain.submit(JSON.stringify({ Tick: { now: Date.now() } }));
//! brain.submit(JSON.stringify({ UserInput: { content: "hi", mode: "Queue" } }));
//! for (const cmd of JSON.parse(brain.poll())) host.perform(cmd);
//! ```
//!
//! `submit`/`poll` are synchronous and pure — exactly the native contract
//! (`brain.rs`). All concurrency (streaming model calls, tab tools, timers)
//! lives in the JS host, merged into the single ordered event stream the brain
//! consumes one event at a time (ARCHITECTURE §4.2).
//!
//! ## Structure
//!
//! [`Core`] holds the pure logic (JSON string in / JSON string out, `String`
//! errors) so it is unit-testable on the native target. [`BatonBrain`] is the
//! `#[wasm_bindgen]` wrapper that only adds the JS marshalling (the
//! wasm-bindgen string intrinsics abort on non-wasm targets, so the tests
//! exercise `Core`, never the wrapper).

use baton_core::{Brain, Command, Event, StaticPolicy};
use wasm_bindgen::prelude::*;

/// The pure binding logic, target-independent and native-testable. Every method
/// speaks JSON strings and returns a `String` error, so no wasm intrinsics are
/// involved — this is ordinary Rust wrapping [`Brain`].
pub struct Core {
    inner: Brain,
}

impl Core {
    /// Build from a JSON-serialized [`StaticPolicy`] (see [`BatonBrain::new`]).
    pub fn from_policy_json(policy_json: &str) -> Result<Core, String> {
        let policy: StaticPolicy =
            serde_json::from_str(policy_json).map_err(|e| format!("invalid policy JSON: {e}"))?;
        Ok(Core {
            inner: Brain::new(Box::new(policy)),
        })
    }

    /// Build with the default [`StaticPolicy`] (no tools, no permissions).
    pub fn default_policy() -> Core {
        Core {
            inner: Brain::with_default_policy(),
        }
    }

    /// Feed one JSON [`Event`] in (mirrors `Brain::submit`).
    pub fn submit(&mut self, event_json: &str) -> Result<(), String> {
        let event: Event =
            serde_json::from_str(event_json).map_err(|e| format!("invalid event JSON: {e}"))?;
        self.inner.submit(event);
        Ok(())
    }

    /// Drain queued commands as a JSON array (mirrors `Brain::poll`).
    pub fn poll(&mut self) -> Result<String, String> {
        let commands: Vec<Command> = self.inner.poll();
        serde_json::to_string(&commands).map_err(|e| format!("serializing commands: {e}"))
    }

    /// Number of in-flight ops (the host's turn-completion test).
    pub fn inflight_len(&self) -> usize {
        self.inner.state().inflight_len()
    }

    /// The durable, consolidated log as JSON — the source of truth (§3.1).
    pub fn log_json(&self) -> Result<String, String> {
        serde_json::to_string(self.inner.state().log()).map_err(|e| format!("serializing log: {e}"))
    }
}

/// A [`Brain`] wrapped for JavaScript. Construct one with a serialized
/// [`StaticPolicy`], then drive it with [`submit`](BatonBrain::submit) /
/// [`poll`](BatonBrain::poll) exactly as the native host's driver loop does.
#[wasm_bindgen]
pub struct BatonBrain {
    core: Core,
}

#[wasm_bindgen]
impl BatonBrain {
    /// Create a brain from a JSON-serialized [`StaticPolicy`] — the same policy
    /// the native [`EngineBuilder`](baton_host) assembles (model selector,
    /// advertised tools, permissioned set, system prompt). The brain branches on
    /// the policy's pure decisions (`needs_permission`, `is_background`,
    /// `agent_seed`), so the host must configure it up front (ARCHITECTURE §2.5).
    #[wasm_bindgen(constructor)]
    pub fn new(policy_json: &str) -> Result<BatonBrain, JsError> {
        #[cfg(feature = "console_error_panic_hook")]
        console_error_panic_hook::set_once();

        let core = Core::from_policy_json(policy_json).map_err(|e| JsError::new(&e))?;
        Ok(BatonBrain { core })
    }

    /// Create a brain with the default [`StaticPolicy`] (no tools, no
    /// permissions) — handy for a bare "chat only" host.
    #[wasm_bindgen(js_name = withDefaultPolicy)]
    pub fn with_default_policy() -> BatonBrain {
        #[cfg(feature = "console_error_panic_hook")]
        console_error_panic_hook::set_once();

        BatonBrain {
            core: Core::default_policy(),
        }
    }

    /// Feed one [`Event`] in, as JSON. Pure, instant, no IO — the single entry
    /// point for all of the brain's logic (mirrors `Brain::submit`). The host is
    /// responsible for stamping a `Tick` before each logical event, exactly like
    /// the native engine (ARCHITECTURE §6.1) — the brain never reads a clock.
    pub fn submit(&mut self, event_json: &str) -> Result<(), JsError> {
        self.core.submit(event_json).map_err(|e| JsError::new(&e))
    }

    /// Drain the commands the brain wants performed, as a JSON array of
    /// [`Command`]s. Pure, instant (mirrors `Brain::poll`).
    pub fn poll(&mut self) -> Result<String, JsError> {
        self.core.poll().map_err(|e| JsError::new(&e))
    }

    /// Number of operations currently in flight. The host's driver loop uses
    /// this to decide when a turn is complete (nothing left in flight), exactly
    /// like the native engine's `drive_to_idle` (ARCHITECTURE §2.3).
    #[wasm_bindgen(js_name = inflightLen)]
    pub fn inflight_len(&self) -> usize {
        self.core.inflight_len()
    }

    /// The durable, consolidated log as JSON — the source of truth
    /// (ARCHITECTURE §3.1). A host can persist this (e.g. to
    /// `chrome.storage`/IndexedDB) as a trace and re-seed a fresh brain from it
    /// later. `BrainState` is never serialized directly; it is always a fold
    /// over this log.
    #[wasm_bindgen(js_name = logJson)]
    pub fn log_json(&self) -> Result<String, JsError> {
        self.core.log_json().map_err(|e| JsError::new(&e))
    }
}

/// The `baton-wasm` version this binding was built from, exposed so the JS host
/// can display / assert it. Cheap and handy for a "which brain am I running?"
/// line in a demo.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The binding is a thin JSON shell over `Brain`; the substantive reducer
    // behaviour is tested in `baton-core`. Here we pin the JSON boundary via the
    // native-testable `Core` (the `#[wasm_bindgen]` wrapper's string intrinsics
    // abort off wasm, so it is exercised in the browser, not in `cargo test`).
    #[test]
    fn user_input_drives_a_model_call_over_the_json_boundary() {
        let policy = serde_json::json!({
            "model": { "Named": "big" },
            "tools": [],
            "permissioned": [],
            "background": [],
            "agents": [],
            "params": {},
            "system": null
        });
        let mut core = Core::from_policy_json(&policy.to_string()).expect("valid policy");

        core.submit(r#"{ "Tick": { "now": 1 } }"#).unwrap();
        core.submit(r#"{ "UserInput": { "content": "hello", "mode": "Queue" } }"#)
            .unwrap();

        let commands = core.poll().unwrap();
        assert!(
            commands.contains("StartModelCall"),
            "expected a StartModelCall, got: {commands}"
        );
        assert_eq!(core.inflight_len(), 1, "one model op in flight");

        // The log already holds the user message (the durable truth).
        let log = core.log_json().unwrap();
        assert!(log.contains("UserMessage"), "log: {log}");
    }

    #[test]
    fn default_policy_constructs_and_is_idle() {
        let mut core = Core::default_policy();
        core.submit(r#"{ "Tick": { "now": 1 } }"#).unwrap();
        assert_eq!(core.inflight_len(), 0);
    }

    #[test]
    fn invalid_event_json_is_a_clean_error() {
        let mut core = Core::default_policy();
        let err = core.submit("{ not json").unwrap_err();
        assert!(err.contains("invalid event JSON"), "err: {err}");
    }

    #[test]
    fn invalid_policy_json_is_a_clean_error() {
        let err = Core::from_policy_json("not json")
            .err()
            .expect("should error");
        assert!(err.contains("invalid policy JSON"), "err: {err}");
    }
}
