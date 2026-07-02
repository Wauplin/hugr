//! The transport seam: how the host talks to a plugin, independent of *where*
//! the plugin runs (ARCHITECTURE §8).
//!
//! [`PluginTransport`] is the single trait a host depends on. The subprocess
//! transport ([`SubprocessPlugin`](crate::SubprocessPlugin)) implements it over
//! stdio today; a WASM component transport slots in behind the same trait later
//! (the `wasm` feature scaffolds it). Because the host only ever sees this trait,
//! adding a transport touches no host code.

use async_trait::async_trait;
use hugr_core::{ToolSchema, Value};

/// A loaded plugin the host can query and invoke. One transport instance may back
/// several tools (a plugin can `describe` many).
///
/// `Send + Sync` so the host can share it across op tasks (each tool call is an
/// op). Mirrors the host's `Capability` bounds.
#[async_trait]
pub trait PluginTransport: Send + Sync {
    /// Ask the plugin which tools it provides. Each returned [`ToolSchema`]
    /// becomes an ordinary host capability (no privileged built-ins, §8).
    async fn describe(&self) -> Result<Vec<ToolSchema>, PluginError>;

    /// Invoke one of the plugin's tools. Intermediate progress is streamed
    /// through `sink`; the call resolves to the terminal result.
    ///
    /// Returns `Ok(value)` for a successful result and `Err(value)` for a
    /// **semantic** tool error (both are routed back to the model as a tool
    /// result, §5.4). Transport-level failures (spawn/protocol/IO) are mapped to
    /// `Err(value)` too, so a broken plugin surfaces to the model rather than
    /// crashing the turn.
    async fn invoke(&self, name: &str, args: Value, sink: &PluginSink) -> Result<Value, Value>;
}

/// Lets a plugin transport forward streamed chunks back to the host without
/// depending on the host's own `ChunkSink`. The host constructs one that bridges
/// to its brain event stream.
pub struct PluginSink {
    emit: Box<dyn Fn(Value) + Send + Sync>,
}

impl PluginSink {
    /// Build a sink from a chunk-forwarding closure.
    pub fn new(emit: impl Fn(Value) + Send + Sync + 'static) -> Self {
        Self {
            emit: Box::new(emit),
        }
    }

    /// A no-op sink (drops chunks) — handy for tests and for `describe`.
    pub fn null() -> Self {
        Self::new(|_| {})
    }

    /// Forward one streamed chunk.
    pub fn chunk(&self, value: Value) {
        (self.emit)(value)
    }
}

/// Errors from loading or talking to a plugin. Only `describe` surfaces these
/// directly; `invoke` folds them into an `Err(Value)` so a failing plugin is a
/// semantic tool error the model can react to (§5.4).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PluginError {
    /// Spawning or doing IO with the plugin failed.
    #[error("plugin IO error: {0}")]
    Io(#[from] std::io::Error),

    /// The plugin sent something that isn't valid protocol JSON.
    #[error("plugin serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// The plugin violated the protocol (e.g. closed without a description).
    #[error("plugin protocol error: {0}")]
    Protocol(String),

    /// The plugin reports a newer, unknown ABI version.
    #[error("unsupported plugin protocol version {found} (host supports up to {supported})")]
    UnsupportedVersion { found: u32, supported: u32 },
}

impl From<crate::framing::FramingError> for PluginError {
    fn from(err: crate::framing::FramingError) -> Self {
        // Preserve the pre-framing error taxonomy: stream failures were `Io`,
        // malformed protocol JSON was `Serde`.
        match err {
            crate::framing::FramingError::Io(e) => PluginError::Io(e),
            crate::framing::FramingError::Json(e) => PluginError::Serde(e),
        }
    }
}
