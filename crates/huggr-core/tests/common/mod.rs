//! Shared helpers for the integration tests.
//!
//! Compiled into each test binary; not every binary uses every helper, so the
//! dead-code lint is silenced here rather than per-item.
#![allow(dead_code)]

use huggr_core::{Brain, Command, Envelope, Event, ModelOutput, Timestamp, ToolCall, Usage};
use serde_json::json;

/// Stamp an event with a fixed test timestamp. Command-logic tests don't care
/// about time; time-sensitive tests build explicit [`Envelope`]s instead.
pub fn stamp(event: Event) -> Envelope {
    Envelope::new(Timestamp::default(), event)
}

/// Run a whole script of events through a brain, collecting every command the
/// brain emits (across all `poll()`s), in order. This is the deterministic
/// "function" we assert on: same events in → same commands out.
pub fn run_script(brain: &mut Brain, events: Vec<Event>) -> Vec<Command> {
    run_envelopes(brain, events.into_iter().map(stamp).collect())
}

/// [`run_script`] over explicitly time-stamped envelopes.
pub fn run_envelopes(brain: &mut Brain, envelopes: Vec<Envelope>) -> Vec<Command> {
    let mut commands = Vec::new();
    // Drain anything queued before the first envelope (none, normally).
    commands.extend(brain.poll());
    for envelope in envelopes {
        brain.submit(envelope);
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

/// Convenience for a user message event.
pub fn user(text: &str) -> Event {
    Event::UserInput {
        content: json!(text),
        est_tokens: 1,
    }
}

/// Fold the same script into two fresh brains and assert identical commands and
/// an identical durable log.
pub fn assert_deterministic_replay(
    make_brain: impl Fn() -> Brain,
    script: impl Fn() -> Vec<Event>,
) {
    let mut a = make_brain();
    let commands_a = run_script(&mut a, script());

    let mut b = make_brain();
    let commands_b = run_script(&mut b, script());

    assert_eq!(
        commands_a, commands_b,
        "re-feeding the identical event stream must yield identical commands"
    );
    assert_eq!(
        a.state().log(),
        b.state().log(),
        "re-feeding the identical event stream must yield an identical log"
    );
}
