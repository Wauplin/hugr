//! Loading plugins as capabilities (ARCHITECTURE §8).
//!
//! A plugin is surfaced through the **same** [`Capability`] interface as the
//! built-in shell/fs/http — there are no privileged built-ins, and equally no
//! privileged plugins. [`load`] queries a [`PluginTransport`] for its tools and
//! wraps each as a [`PluginCapability`] the host registers like any other. The
//! brain never knows a tool came from a plugin: it just emits
//! `StartCapability { name, args }`.

use std::sync::Arc;

use async_trait::async_trait;
use baton_core::{ToolSchema, Value};
use baton_plugin_abi::{PluginError, PluginSink, PluginTransport, SubprocessPlugin};

use crate::capability::{Capability, ChunkSink};

/// One tool provided by a plugin, adapted to the host [`Capability`] interface.
///
/// It holds the shared [`PluginTransport`] and the single tool's schema; invoking
/// it forwards to the transport, bridging the host's [`ChunkSink`] to the
/// plugin's [`PluginSink`] so streamed chunks reach the brain.
pub struct PluginCapability {
    transport: Arc<dyn PluginTransport>,
    schema: ToolSchema,
    requires_permission: bool,
    runs_in_background: bool,
}

impl PluginCapability {
    /// Wrap one plugin tool. Plugins are third-party/effectful, so they require
    /// permission by default (the host can relax it, see
    /// [`with_permission`](Self::with_permission)).
    pub fn new(transport: Arc<dyn PluginTransport>, schema: ToolSchema) -> Self {
        Self {
            transport,
            schema,
            requires_permission: true,
            runs_in_background: false,
        }
    }

    /// Override whether this plugin tool goes through a permission check.
    pub fn with_permission(mut self, requires: bool) -> Self {
        self.requires_permission = requires;
        self
    }

    /// Mark this plugin tool as a background op (does not block the model turn).
    pub fn with_background(mut self, background: bool) -> Self {
        self.runs_in_background = background;
        self
    }
}

#[async_trait]
impl Capability for PluginCapability {
    fn name(&self) -> &str {
        &self.schema.name
    }

    fn schema(&self) -> ToolSchema {
        self.schema.clone()
    }

    fn requires_permission(&self) -> bool {
        self.requires_permission
    }

    fn runs_in_background(&self) -> bool {
        self.runs_in_background
    }

    async fn invoke(&self, args: Value, sink: &ChunkSink) -> Result<Value, Value> {
        // Bridge the host sink to the plugin sink so streamed chunks flow to the
        // brain as `CapabilityChunk`s. `ChunkSink` is cheap to clone (op + sender).
        let host_sink = sink.clone();
        let plugin_sink = PluginSink::new(move |chunk| host_sink.chunk(chunk));
        self.transport
            .invoke(&self.schema.name, args, &plugin_sink)
            .await
    }
}

/// Load a plugin over `transport`: `describe` its tools and wrap each as a
/// [`PluginCapability`]. Register the returned capabilities on the
/// [`EngineBuilder`](crate::EngineBuilder) like any other.
///
/// All tools default to requiring permission (third-party/effectful); adjust per
/// tool with [`PluginCapability::with_permission`] if needed.
pub async fn load(
    transport: Arc<dyn PluginTransport>,
) -> Result<Vec<Arc<dyn Capability>>, PluginError> {
    let tools = transport.describe().await?;
    Ok(tools
        .into_iter()
        .map(|schema| {
            Arc::new(PluginCapability::new(transport.clone(), schema)) as Arc<dyn Capability>
        })
        .collect())
}

/// Convenience: load a **subprocess** plugin from a program path (and optional
/// arguments). Spawns the program to `describe` its tools.
///
/// ```no_run
/// # async fn run() -> Result<(), baton_plugin_abi::PluginError> {
/// let tools = baton_host::plugins::load_subprocess("my-plugin", ["--flag"]).await?;
/// # let _ = tools; Ok(()) }
/// ```
pub async fn load_subprocess(
    program: impl Into<std::ffi::OsString>,
    args: impl IntoIterator<Item = impl Into<std::ffi::OsString>>,
) -> Result<Vec<Arc<dyn Capability>>, PluginError> {
    let mut plugin = SubprocessPlugin::new(program);
    for arg in args {
        plugin = plugin.arg(arg);
    }
    load(Arc::new(plugin)).await
}
