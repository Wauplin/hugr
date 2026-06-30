//! Shared helpers for the integration tests.
//!
//! Compiled into each test binary; not every binary uses every helper, so the
//! dead-code lint is silenced here rather than per-item.
#![allow(dead_code)]

use baton_core::{Brain, Command, Event, ModelOutput, ToolCall, Usage};
use serde_json::json;

/// Run a whole script of events through a brain, collecting every command the
/// brain emits (across all `poll()`s), in order. This is the deterministic
/// "function" we assert on: same events in → same commands out.
pub fn run_script(brain: &mut Brain, events: Vec<Event>) -> Vec<Command> {
    let mut commands = Vec::new();
    // Drain anything queued before the first event (none, normally).
    commands.extend(brain.poll());
    for event in events {
        brain.submit(event);
        commands.extend(brain.poll());
    }
    commands
}

/// A consolidated model output that ends the turn with plain text.
pub fn text_output(text: &str) -> ModelOutput {
    ModelOutput::text(text)
}

/// A consolidated model output that requests a single tool call.
pub fn tool_output(call_id: &str, name: &str, args: serde_json::Value) -> ModelOutput {
    ModelOutput::tool_calls(vec![ToolCall::new(call_id, name, args)])
}

/// A throwaway usage value for tests.
pub fn usage() -> Usage {
    Usage::new(10, 20)
}

/// Keep only the commands a host acts on, dropping cosmetic `Emit`s. Most
/// assertions care about the *effectful* command sequence.
pub fn effectful(commands: &[Command]) -> Vec<&Command> {
    commands
        .iter()
        .filter(|c| !matches!(c, Command::Emit(_)))
        .collect()
}

/// Convenience for a user message event with the default steer mode.
pub fn user(text: &str) -> Event {
    Event::UserInput {
        content: json!(text),
        mode: baton_core::SteerMode::Queue,
    }
}
