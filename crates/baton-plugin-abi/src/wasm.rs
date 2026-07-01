//! WASM component-model transport — the roadmap's **primary** plugin ABI
//! (ARCHITECTURE §8.1), scaffolded behind the `wasm` feature.
//!
//! The seam is [`PluginTransport`]: a WASM plugin is a sandboxed component the
//! host loads and exposes as capabilities, exchanging the *same* [`protocol`]
//! messages a subprocess plugin does — only the transport differs. The full
//! wasmtime + component-model wiring lands with the Phase 4 portability work
//! (compiling `baton-core` to WASM); until then this type is a placeholder that
//! documents the shape and lets host/CLI code compile against the real trait.
//!
//! [`protocol`]: crate::protocol
//! [`PluginTransport`]: crate::PluginTransport

use std::path::PathBuf;

use async_trait::async_trait;
use baton_core::{ToolSchema, Value};
use serde_json::json;

use crate::transport::{PluginError, PluginSink, PluginTransport};

/// A plugin backed by a sandboxed WASM component (Phase 4/5). Placeholder: it
/// implements [`PluginTransport`] so it is a drop-in for the subprocess
/// transport, but every call reports "not yet implemented" until the wasmtime
/// backend is wired in.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct WasmPlugin {
    /// Path to the `.wasm` component module the host would instantiate.
    pub module: PathBuf,
}

impl WasmPlugin {
    /// Reference a WASM component module by path (not yet instantiated).
    pub fn new(module: impl Into<PathBuf>) -> Self {
        Self {
            module: module.into(),
        }
    }
}

const UNIMPLEMENTED: &str = "WASM plugin transport is not yet implemented; it lands with Phase 4 (portability). \
     Use the subprocess transport for now.";

#[async_trait]
impl PluginTransport for WasmPlugin {
    async fn describe(&self) -> Result<Vec<ToolSchema>, PluginError> {
        Err(PluginError::Protocol(UNIMPLEMENTED.to_string()))
    }

    async fn invoke(&self, _name: &str, _args: Value, _sink: &PluginSink) -> Result<Value, Value> {
        Err(json!({ "error": "wasm_unimplemented", "message": UNIMPLEMENTED }))
    }
}
