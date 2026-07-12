//! Front-ends consume the brain's [`OutputEvent`] stream plus host lifecycle
//! hooks.
//!
//! Rendering is never inside the core; any number of front-ends can subscribe.
//! The huglet runtime runs silently (its product is the `Answer` + trace),
//! so the engine's default front-end is a no-op.

use huggr_core::{Decision, DoneReason, ModelSelector, OpId, OutputEvent, Usage, Value};

/// Renders the agent's output and activity. The [`Engine`](crate::Engine) calls
/// these as it drains commands and observes the event stream. Every method has
/// a default no-op, so a minimal front-end only implements what it cares about.
#[allow(unused_variables)]
pub trait Frontend: Send {
    /// A cosmetic output event from the brain (streamed text, tool chunks, …).
    fn on_output(&mut self, event: &OutputEvent) {}

    /// A host-level notice (free-form).
    fn on_notice(&mut self, message: &str) {}

    /// A model call is starting.
    fn on_model_start(&mut self, op: OpId, selector: &ModelSelector) {}

    /// A model call finished; carries its token usage.
    fn on_model_end(&mut self, op: OpId, usage: &Usage) {}

    /// A capability (tool) is about to run, with its arguments.
    fn on_tool_start(&mut self, op: OpId, name: &str, args: &Value) {}

    /// A capability finished. `is_error` marks a tool-level failure (the result
    /// is the error payload).
    fn on_tool_end(&mut self, op: OpId, name: &str, result: &Value, is_error: bool) {}

    /// A permission request was decided.
    fn on_permission(&mut self, capability: &str, decision: &Decision) {}

    /// The turn reached a terminal state.
    fn on_done(&mut self, reason: &DoneReason) {}

    /// The whole session is finishing (one-shot run, or interactive exit). A
    /// front-end can render accumulated totals here.
    fn on_session_end(&mut self) {}
}
