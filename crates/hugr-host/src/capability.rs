//! The uniform capability (tool) interface and its registry.
//!
//! There are **no privileged built-ins**: fs and http are ordinary
//! [`Capability`]s, exactly like an external MCP tool would be. The brain only
//! ever emits `StartCapability { name, args }`; the host looks the name up here.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use hugr_core::{Event, OpId, ToolSchema, Value};
use tokio::sync::mpsc::UnboundedSender;

/// A host-provided tool. Streaming-capable: it may emit chunks via the
/// [`ChunkSink`] (e.g. lines of stdout) before returning a final result.
#[async_trait]
pub trait Capability: Send + Sync {
    /// The capability name the model/brain refers to.
    fn name(&self) -> &str;

    /// The JSON-schema advertised to the model for this tool.
    fn schema(&self) -> ToolSchema;

    /// Whether invoking this capability should go through a permission check.
    /// Read-only tools override this to `false`; mutating/effectful tools keep
    /// the safe default of `true`.
    fn requires_permission(&self) -> bool {
        true
    }

    /// Whether this capability runs in the **background**: it does not block
    /// the model turn, so the model keeps streaming while the op runs. Defaults
    /// to `false` (foreground: the turn waits for the result).
    fn runs_in_background(&self) -> bool {
        false
    }

    /// Run the tool. `Ok(result)` and `Err(error)` are both routed back to the
    /// model as a tool result (an error is a *semantic* result the model can
    /// react to) — return `Err` only for tool-level failures, not transport
    /// issues.
    async fn invoke(&self, args: Value, sink: &ChunkSink) -> Result<Value, Value>;
}

/// Lets a capability stream intermediate chunks (transport only) back to the
/// brain as `CapabilityChunk` events while it runs.
///
/// `Clone` is cheap (it clones the op id + an `Arc`-backed sender) so a wrapper
/// (e.g. an MCP capability bridging an external process stream) can
/// move an emitter into a closure.
#[derive(Clone)]
pub struct ChunkSink {
    op: OpId,
    tx: UnboundedSender<Event>,
}

impl ChunkSink {
    pub(crate) fn new(op: OpId, tx: UnboundedSender<Event>) -> Self {
        Self { op, tx }
    }

    /// Emit one streamed chunk (e.g. a line of stdout).
    pub fn chunk(&self, chunk: Value) {
        let _ = self.tx.send(Event::CapabilityChunk { op: self.op, chunk });
    }
}

/// Maps capability names to their implementations.
///
/// `Clone` is cheap (it clones `Arc`s) so a sub-agent runner can reuse — or
/// [`subset`](CapabilityRegistry::subset) — the parent's tools.
#[derive(Clone, Default)]
pub struct CapabilityRegistry {
    map: HashMap<String, Arc<dyn Capability>>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a capability under its own [`Capability::name`].
    pub fn register(&mut self, capability: Arc<dyn Capability>) {
        self.map.insert(capability.name().to_string(), capability);
    }

    /// Look a capability up by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Capability>> {
        self.map.get(name).cloned()
    }

    /// A registry restricted to an allowlist of capability names — the tools a
    /// sub-agent may use. `None` returns a clone of the whole registry (the
    /// child inherits every tool).
    pub fn subset(&self, allow: Option<&std::collections::HashSet<String>>) -> CapabilityRegistry {
        match allow {
            None => self.clone(),
            Some(allow) => CapabilityRegistry {
                map: self
                    .map
                    .iter()
                    .filter(|(name, _)| allow.contains(*name))
                    .map(|(name, cap)| (name.clone(), cap.clone()))
                    .collect(),
            },
        }
    }

    /// The schemas of all registered capabilities (advertised to the model).
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.map.values().map(|c| c.schema()).collect()
    }

    /// The names of capabilities that require a permission round-trip.
    pub fn permissioned_names(&self) -> Vec<String> {
        self.map
            .values()
            .filter(|c| c.requires_permission())
            .map(|c| c.name().to_string())
            .collect()
    }

    /// The names of capabilities that run in the background (do not block the
    /// model turn).
    pub fn background_names(&self) -> Vec<String> {
        self.map
            .values()
            .filter(|c| c.runs_in_background())
            .map(|c| c.name().to_string())
            .collect()
    }
}
