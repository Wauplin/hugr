//! Generic WASM bindings around the Huggr brain for browser/JS hosts.
//!
//! `huggr-wasm` contains no Chrome APIs and bakes in no prompt or manifest: it
//! exposes the sans-IO `huggr-core` brain (submit/poll over JSON) plus the
//! browser tool schemas that form the model⇄browser contract. Everything
//! host-specific — capability implementations, storage, UI — lives in the JS
//! host that drives it (`bindings/typescript/` is the generic driver;
//! `examples/chrome-extension/` is one concrete host).

#![forbid(unsafe_code)]

mod capabilities;
mod config;

#[cfg(target_arch = "wasm32")]
mod exports;
#[cfg(target_arch = "wasm32")]
mod session;

pub use capabilities::{BrowserCapability, browser_capabilities, browser_tool_schemas};
pub use config::{BrowserAgentConfig, DEFAULT_BASE_URL, DEFAULT_MODEL};

#[cfg(target_arch = "wasm32")]
pub use exports::HuggrWasm;
#[cfg(target_arch = "wasm32")]
pub use session::{AgentSession, verify_trace_json};
