//! Phase 0 exit criterion #2: deterministic replay. Feeding the same event
//! stream to a fresh brain twice yields identical commands — the brain is a
//! pure fold over the event stream (ARCHITECTURE §6).

mod common;

use baton_core::{Brain, Event, OpId, Timestamp};
use common::*;
use serde_json::json;

/// Build a representative script that exercises ticks, deltas (transport-only),
/// a tool round-trip, and a final answer.
fn representative_script() -> Vec<Event> {
    use baton_core::ModelDelta;
    vec![
        Event::Tick {
            now: Timestamp(1_000),
        },
        user("summarize the repo"),
        // Streamed deltas — transport only, must not change the command logic.
        Event::ModelDelta {
            op: OpId(0),
            delta: ModelDelta::Text("Let me ".into()),
        },
        Event::ModelDelta {
            op: OpId(0),
            delta: ModelDelta::Text("look.".into()),
        },
        Event::Tick {
            now: Timestamp(1_200),
        },
        Event::ModelDone {
            op: OpId(0),
            output: tool_output("c1", "shell", json!({ "cmd": "ls" })),
            usage: usage(),
        },
        Event::CapabilityChunk {
            op: OpId(1),
            chunk: json!("a.txt\n"),
        },
        Event::CapabilityDone {
            op: OpId(1),
            result: json!({ "stdout": "a.txt" }),
            version: None,
        },
        Event::Tick {
            now: Timestamp(1_500),
        },
        Event::ModelDone {
            op: OpId(2),
            output: text_output("One file: a.txt."),
            usage: usage(),
        },
    ]
}

/// Same events, two fresh brains → identical command streams (including the
/// cosmetic `Emit`s, which are themselves deterministic).
#[test]
fn replay_yields_identical_commands() {
    let script = representative_script();

    let mut brain_a = Brain::with_default_policy();
    let commands_a = run_script(&mut brain_a, script.clone());

    let mut brain_b = Brain::with_default_policy();
    let commands_b = run_script(&mut brain_b, script);

    assert_eq!(
        commands_a, commands_b,
        "replaying the same event stream must yield identical commands"
    );
}

/// The same brain instance, re-run, is also stable across the run boundary:
/// folding a script then folding it again into a fresh brain matches.
#[test]
fn replay_is_stable_across_instances() {
    let script = representative_script();

    let mut first = Brain::with_default_policy();
    let commands_first = run_script(&mut first, script.clone());

    // A second, independent fold of the identical stream.
    let mut second = Brain::with_default_policy();
    let commands_second = run_script(&mut second, script);

    assert_eq!(commands_first, commands_second);

    // And the durable log derived from the fold is identical too.
    assert_eq!(first.state().log(), second.state().log());
}

/// Deltas are transport-only: a stream *with* deltas produces the same durable
/// log as the same stream *without* them (ARCHITECTURE §4.5). Only cosmetic
/// `Emit` commands differ.
#[test]
fn deltas_do_not_affect_the_log() {
    use baton_core::ModelDelta;

    let with_deltas = vec![
        user("hi"),
        Event::ModelDelta {
            op: OpId(0),
            delta: ModelDelta::Text("hello".into()),
        },
        Event::ModelDone {
            op: OpId(0),
            output: text_output("hello"),
            usage: usage(),
        },
    ];
    let without_deltas = vec![
        user("hi"),
        Event::ModelDone {
            op: OpId(0),
            output: text_output("hello"),
            usage: usage(),
        },
    ];

    let mut a = Brain::with_default_policy();
    run_script(&mut a, with_deltas);

    let mut b = Brain::with_default_policy();
    run_script(&mut b, without_deltas);

    assert_eq!(
        a.state().log(),
        b.state().log(),
        "deltas must not appear in or alter the durable log"
    );
}

/// A trace (the consolidated log) round-trips through JSON unchanged — the
/// substrate Phase 3 builds on.
#[test]
fn log_is_serializable() {
    let mut brain = Brain::with_default_policy();
    run_script(&mut brain, representative_script());

    let log = brain.state().log();
    let json = serde_json::to_string(log).expect("log serializes");
    let restored: Vec<baton_core::LogEntry> =
        serde_json::from_str(&json).expect("log deserializes");

    assert_eq!(log, restored.as_slice());
}
