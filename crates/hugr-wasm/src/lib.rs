//! Browser-specific Hugr host scaffolding.
//!
//! `hugr-wasm` is intentionally an edge crate: Chrome APIs, IndexedDB, and the
//! extension UI belong here, while `hugr-core` remains a pure reducer. The Rust
//! side currently exposes the browser capability schemas and a small WASM
//! surface for the extension shell; the actual Chrome API calls live in the
//! extension bridge files under `extension/`.

#![forbid(unsafe_code)]

mod capabilities;
mod config;

#[cfg(target_arch = "wasm32")]
mod exports;

pub use capabilities::{BrowserCapability, browser_capabilities, browser_tool_schemas};
pub use config::{BrowserAgentConfig, DEFAULT_BASE_URL, DEFAULT_MODEL};

#[cfg(target_arch = "wasm32")]
pub use exports::HugrWasm;
