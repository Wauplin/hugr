//! The model-adapter interface and its registry.
//!
//! A model call is "an effect the host provides", registered much like a
//! capability. The brain names a logical [`ModelSelector`]; the registry
//! resolves it to a concrete adapter. The adapter streams deltas through a
//! [`ModelSink`] and returns the consolidated output + usage.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use huggr_core::{Event, ModelDelta, ModelOutput, ModelRequest, ModelSelector, OpId, Usage};
use tokio::sync::mpsc::UnboundedSender;

/// Translates the canonical [`ModelRequest`] to/from a concrete provider.
///
/// **Streaming is the only mode.** An adapter must request a streamed response
/// and emit deltas through the [`ModelSink`] *as they arrive* (so front-ends can
/// render live), then return the consolidated [`ModelOutput`] + [`Usage`] once
/// the response completes. There is deliberately no non-streaming variant.
///
/// Transport errors (429, timeouts, 5xx) should be retried *inside* the
/// adapter; only return `Err` once the adapter has genuinely given up.
#[async_trait]
pub trait ModelAdapter: Send + Sync {
    async fn call(
        &self,
        request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)>;
}

/// Lets an adapter stream model deltas (transport only) back to the brain as
/// `ModelDelta` events while a completion is in flight.
pub struct ModelSink {
    op: OpId,
    tx: UnboundedSender<Event>,
}

impl ModelSink {
    /// Wrap an op id and an event sender. The [`Engine`](crate::Engine)
    /// constructs these for adapters; it is also public so adapter authors can
    /// unit-test their streaming logic against a channel.
    pub fn new(op: OpId, tx: UnboundedSender<Event>) -> Self {
        Self { op, tx }
    }

    /// A chunk of assistant text.
    pub fn text(&self, text: impl Into<String>) {
        self.delta(ModelDelta::Text(text.into()));
    }

    /// A chunk of model reasoning/thinking.
    pub fn reasoning(&self, text: impl Into<String>) {
        self.delta(ModelDelta::Reasoning(text.into()));
    }

    /// The model started emitting a tool call (id + name known).
    pub fn tool_call_start(&self, id: impl Into<String>, name: impl Into<String>) {
        self.delta(ModelDelta::ToolCallStart {
            id: id.into(),
            name: name.into(),
        });
    }

    fn delta(&self, delta: ModelDelta) {
        let _ = self.tx.send(Event::ModelDelta { op: self.op, delta });
    }
}

/// Maps logical [`ModelSelector`]s to concrete adapters.
///
/// `Clone` is cheap (it clones `Arc`s) and lets a huglet runner reuse the
/// parent's model registry on its own task.
#[derive(Clone, Default)]
pub struct ModelRegistry {
    map: HashMap<ModelSelector, Arc<dyn ModelAdapter>>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, selector: ModelSelector, adapter: Arc<dyn ModelAdapter>) {
        self.map.insert(selector, adapter);
    }

    pub fn get(&self, selector: &ModelSelector) -> Option<Arc<dyn ModelAdapter>> {
        self.map.get(selector).cloned()
    }
}
