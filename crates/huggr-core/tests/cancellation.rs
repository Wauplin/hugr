//! First-class cancellation: a `Cancel` command aborts an in-flight op, the host
//! confirms with `OpCancelled`, and the brain logs the *partial* work as a
//! `Cancelled` outcome. These tests pin the command sequence and assert
//! deterministic replay.

mod common;

use common::*;
use huggr_core::{
    Brain, Command, DoneReason, Event, ModelDelta, OpId, OpOutcome, Record, StaticPolicy, Value,
};
use serde_json::json;

/// A policy that runs `shell` in the background (does not block the turn).
fn background_shell_policy() -> StaticPolicy {
    StaticPolicy::default().with_background(["shell".to_string()])
}

/// The partial text preserved for a cancelled model op, if any.
fn cancelled_partial(log: &[huggr_core::LogEntry], op: OpId) -> Option<Value> {
    log.iter().find_map(|e| match &e.record {
        Record::OpEnded {
            op: o,
            outcome: OpOutcome::Cancelled { partial },
            ..
        } if *o == op => Some(partial.clone()),
        _ => None,
    })
}

/// A model stream produces a few tokens, then the user aborts. The host aborts
/// the task and confirms with `OpCancelled`; the brain records the partial text
/// and ends the turn `Cancelled`.
#[test]
fn stream_some_tokens_then_cancel_records_the_partial() {
    let mut brain = Brain::with_default_policy();

    let commands = run_script(
        &mut brain,
        vec![
            user("write a poem"),
            // The model streams a couple of tokens (transport only; not logged).
            Event::ModelDelta {
                op: OpId(0),
                delta: ModelDelta::Text("Hello, ".into()),
            },
            Event::ModelDelta {
                op: OpId(0),
                delta: ModelDelta::Text("wor".into()),
            },
            // User hits ESC: a pure abort. The brain asks the host to cancel
            // every in-flight op.
            Event::UserAbort,
            // The host aborted the model task and confirms it.
            Event::OpCancelled { op: OpId(0) },
        ],
    );

    let effectful = effectful(&commands);
    assert!(
        matches!(
            effectful.as_slice(),
            [
                Command::StartModelCall { op: OpId(0), .. },
                // UserAbort → cancel the in-flight model op.
                Command::Cancel { op: OpId(0) },
                // OpCancelled → the turn is over, cancelled.
                Command::Done {
                    reason: DoneReason::Cancelled
                },
            ]
        ),
        "unexpected command sequence: {effectful:#?}"
    );

    // The partial output ("N tokens then cancelled") is preserved in the log.
    let partial = cancelled_partial(brain.state().log(), OpId(0));
    assert_eq!(partial, Some(json!("Hello, wor")));
}

/// Replay: re-feeding the identical event stream (stream N tokens, then cancel)
/// to a fresh brain yields identical commands AND an identical durable log — the
/// partial is reproduced before the cancel, deterministically.
#[test]
fn cancellation_replay_is_deterministic() {
    let script = || {
        vec![
            user("write a poem"),
            Event::ModelDelta {
                op: OpId(0),
                delta: ModelDelta::Text("Hello, ".into()),
            },
            Event::ModelDelta {
                op: OpId(0),
                delta: ModelDelta::Text("wor".into()),
            },
            Event::UserAbort,
            Event::OpCancelled { op: OpId(0) },
        ]
    };

    let mut a = Brain::with_default_policy();
    let commands_a = run_script(&mut a, script());

    let mut b = Brain::with_default_policy();
    let commands_b = run_script(&mut b, script());

    assert_eq!(
        commands_a, commands_b,
        "stream-then-cancel must replay to identical commands"
    );
    assert_eq!(
        a.state().log(),
        b.state().log(),
        "stream-then-cancel must replay to an identical log (partial then cancelled)"
    );

    // And the log really does contain the partial then the Cancelled outcome.
    assert_eq!(
        cancelled_partial(a.state().log(), OpId(0)),
        Some(json!("Hello, wor"))
    );
}

/// A stale terminal event racing a **host-initiated** cancel (no `UserAbort`
/// in this script): the host aborts the task, but the task had already queued
/// its real `ModelDone` a hair earlier. The brain folds the `ModelDone` first
/// (op resolves `Ok`, turn ends `EndTurn`), then the now-stale `OpCancelled`
/// arrives — it must be a no-op (idempotent), not append a spurious `Cancelled`
/// `OpEnded` that would corrupt the log / break replay. The *user-abort*
/// flavour of this race is different — the abort latch makes the terminal
/// event end the turn `Cancelled` — and is pinned separately below.
#[test]
fn stale_op_cancelled_after_done_is_ignored() {
    let mut brain = Brain::with_default_policy();

    let commands = run_script(
        &mut brain,
        vec![
            user("hi"),
            Event::ModelDelta {
                op: OpId(0),
                delta: ModelDelta::Text("done".into()),
            },
            // The real terminal event lands first (op completes Ok, turn ends).
            Event::ModelDone {
                op: OpId(0),
                output: text_output("done"),
                usage: usage(),
                est_tokens: 1,
            },
            // ...then the late cancel confirmation for the same op arrives.
            Event::OpCancelled { op: OpId(0) },
        ],
    );

    let effectful = effectful(&commands);
    assert!(
        matches!(
            effectful.as_slice(),
            [
                Command::StartModelCall { op: OpId(0), .. },
                Command::Checkpoint,
                Command::Done {
                    reason: DoneReason::EndTurn
                },
                // The stale OpCancelled produced NO further command.
            ]
        ),
        "stale OpCancelled should be a no-op: {effectful:#?}"
    );

    // Exactly one OpEnded, with the Ok outcome — no spurious Cancelled entry.
    let op_ends: Vec<&OpOutcome> = brain
        .state()
        .log()
        .iter()
        .filter_map(|e| match &e.record {
            Record::OpEnded { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect();
    assert_eq!(op_ends.len(), 1);
    assert!(matches!(op_ends[0], OpOutcome::Ok));
}

/// Cancelling one background op while the model is still streaming must NOT end
/// the turn: the model op is still in flight, so the brain stays busy. Proves
/// the terminal `Done { Cancelled }` only fires once the *last* op drains.
#[test]
fn cancelling_a_background_op_mid_stream_does_not_end_the_turn() {
    let mut brain = Brain::new(Box::new(background_shell_policy()));

    let commands = run_script(
        &mut brain,
        vec![
            user("build and chat"),
            // Model kicks off a background shell (op 1); the turn resumes into a
            // second model call (op 2) — both now in flight.
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "cargo build" })),
                usage: usage(),
                est_tokens: 1,
            },
            // Cancel just the background shell op while the model (op 2) streams.
            Event::OpCancelled { op: OpId(1) },
            // The model finishes; with the background op gone and nothing else in
            // flight, the turn ends normally (EndTurn, not Cancelled).
            Event::ModelDone {
                op: OpId(2),
                output: text_output("All set."),
                usage: usage(),
                est_tokens: 1,
            },
        ],
    );

    let effectful = effectful(&commands);
    assert!(
        matches!(
            effectful.as_slice(),
            [
                Command::StartModelCall { op: OpId(0), .. },
                Command::StartCapability { op: OpId(1), .. },
                Command::StartModelCall { op: OpId(2), .. },
                // No Done after the background cancel — the model op still runs.
                Command::Checkpoint,
                Command::Done {
                    reason: DoneReason::EndTurn
                },
            ]
        ),
        "unexpected command sequence: {effectful:#?}"
    );

    // The cancelled background op was logged as Cancelled (with a null partial,
    // since it produced no model text).
    assert_eq!(
        cancelled_partial(brain.state().log(), OpId(1)),
        Some(Value::Null)
    );
}

// ============================================================================
// Cancelled tool calls leave a *paired* tool_result in the log
// ============================================================================

/// The plain-abort flavour: `UserAbort` during a foreground tool. The paired
/// cancelled `ToolResult` is logged and the turn ends `Cancelled` (no resume).
#[test]
fn user_abort_during_tool_call_logs_a_paired_cancelled_tool_result() {
    let mut brain = Brain::with_default_policy();

    let commands = run_script(
        &mut brain,
        vec![
            user("run the build"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "cargo build" })),
                usage: usage(),
                est_tokens: 1,
            },
            Event::UserAbort,
            Event::OpCancelled { op: OpId(1) },
        ],
    );

    let effectful = effectful(&commands);
    assert!(
        matches!(
            effectful.as_slice(),
            [
                Command::StartModelCall { op: OpId(0), .. },
                Command::StartCapability { op: OpId(1), .. },
                Command::Cancel { op: OpId(1) },
                Command::Done {
                    reason: DoneReason::Cancelled
                },
            ]
        ),
        "unexpected command sequence: {effectful:#?}"
    );

    // The cancelled tool call is paired in the log.
    assert!(
        brain.state().log().iter().any(|e| matches!(
            &e.record,
            Record::ToolResult { op: OpId(1), call_id, result, .. }
                if call_id == "call-1" && result == &json!({ "cancelled": true })
        )),
        "the aborted tool call must log a cancelled ToolResult"
    );
}

/// Replay: the abort-during-tool script re-fed to a fresh brain yields
/// identical commands and an identical log (paired ToolResult included).
#[test]
fn cancelled_tool_result_replay_is_deterministic() {
    let script = || {
        vec![
            user("run the build"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "cargo build" })),
                usage: usage(),
                est_tokens: 1,
            },
            Event::UserAbort,
            Event::OpCancelled { op: OpId(1) },
        ]
    };
    assert_deterministic_replay(Brain::with_default_policy, script);
}

// ============================================================================
// The abort latch: UserAbort must win the race against terminal events
// ============================================================================

/// `Command::Cancel` races the op's own terminal event: here the `ModelDone`
/// lands first. Without the abort latch the brain would end the turn
/// `EndTurn` (the abort cancels nothing); with it, the record is folded but
/// no new work starts and the turn ends `Cancelled` exactly once.
#[test]
fn user_abort_racing_a_model_done_still_ends_cancelled() {
    let mut brain = Brain::with_default_policy();

    let commands = run_script(
        &mut brain,
        vec![
            user("hi"),
            // User hits ESC while the model streams...
            Event::UserAbort,
            // ...but the model's real terminal event beat the Cancel.
            Event::ModelDone {
                op: OpId(0),
                output: text_output("done"),
                usage: usage(),
                est_tokens: 1,
            },
            // The stale cancel confirmation is still a no-op.
            Event::OpCancelled { op: OpId(0) },
        ],
    );

    let effectful = effectful(&commands);
    assert!(
        matches!(
            effectful.as_slice(),
            [
                Command::StartModelCall { op: OpId(0), .. },
                Command::Cancel { op: OpId(0) },
                // The consolidated output is durable (checkpointed), but the
                // abort wins: Cancelled, not EndTurn — and exactly once.
                Command::Checkpoint,
                Command::Done {
                    reason: DoneReason::Cancelled
                },
            ]
        ),
        "unexpected command sequence: {effectful:#?}"
    );

    // The model output was still folded into the durable log.
    assert!(
        brain
            .state()
            .log()
            .iter()
            .any(|e| matches!(&e.record, Record::ModelOutput { op: OpId(0), .. }))
    );
}

/// The same race, but the raced `ModelDone` requests tool calls: the latched
/// abort must suppress the fan-out (no `StartCapability`), while still pairing
/// each never-started `tool_use` with a synthesized cancelled `ToolResult` so
/// the next projection stays well-formed.
#[test]
fn user_abort_racing_a_tool_fanout_starts_no_tools() {
    let mut brain = Brain::with_default_policy();

    let commands = run_script(
        &mut brain,
        vec![
            user("run the build"),
            Event::UserAbort,
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "cargo build" })),
                usage: usage(),
                est_tokens: 1,
            },
            Event::OpCancelled { op: OpId(0) },
        ],
    );

    let effectful = effectful(&commands);
    assert!(
        matches!(
            effectful.as_slice(),
            [
                Command::StartModelCall { op: OpId(0), .. },
                Command::Cancel { op: OpId(0) },
                // NO StartCapability: the abort suppressed the fan-out.
                Command::Checkpoint,
                Command::Done {
                    reason: DoneReason::Cancelled
                },
            ]
        ),
        "unexpected command sequence: {effectful:#?}"
    );

    // The never-started call still got a paired cancelled ToolResult.
    assert!(
        brain.state().log().iter().any(|e| matches!(
            &e.record,
            Record::ToolResult { call_id, result, .. }
                if call_id == "call-1" && result == &json!({ "cancelled": true })
        )),
        "the suppressed tool call must log a cancelled ToolResult"
    );
}

/// The race on the tool side: the abort's `Cancel` targets a foreground
/// capability, but its real `CapabilityDone` lands first. The result is folded
/// (a durable ToolResult), yet the latched abort must prevent the resume — the
/// turn ends `Cancelled` once the last op drains, and the stale `OpCancelled`
/// stays a no-op.
#[test]
fn user_abort_racing_a_capability_done_does_not_resume() {
    let mut brain = Brain::with_default_policy();

    let commands = run_script(
        &mut brain,
        vec![
            user("run the build"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "cargo build" })),
                usage: usage(),
                est_tokens: 1,
            },
            Event::UserAbort,
            // The tool's real terminal event beat the Cancel.
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "exit_code": 0 }),
                est_tokens: 1,
            },
            Event::OpCancelled { op: OpId(1) },
        ],
    );

    let effectful = effectful(&commands);
    assert!(
        matches!(
            effectful.as_slice(),
            [
                Command::StartModelCall { op: OpId(0), .. },
                Command::StartCapability { op: OpId(1), .. },
                Command::Cancel { op: OpId(1) },
                // NO resume (StartModelCall) — the abort wins, exactly once.
                Command::Done {
                    reason: DoneReason::Cancelled
                },
            ]
        ),
        "unexpected command sequence: {effectful:#?}"
    );

    // The real tool result was still folded into the durable log.
    assert!(brain.state().log().iter().any(|e| matches!(
        &e.record,
        Record::ToolResult { op: OpId(1), result, .. } if result == &json!({ "exit_code": 0 })
    )));
}

/// Replay: the abort-vs-terminal-event races re-fed to fresh brains yield
/// identical commands and identical logs.
#[test]
fn abort_race_replay_is_deterministic() {
    let model_done_race = || {
        vec![
            user("hi"),
            Event::UserAbort,
            Event::ModelDone {
                op: OpId(0),
                output: text_output("done"),
                usage: usage(),
                est_tokens: 1,
            },
            Event::OpCancelled { op: OpId(0) },
        ]
    };
    assert_deterministic_replay(Brain::with_default_policy, model_done_race);

    let fanout_race = || {
        vec![
            user("run the build"),
            Event::UserAbort,
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "cargo build" })),
                usage: usage(),
                est_tokens: 1,
            },
            Event::OpCancelled { op: OpId(0) },
        ]
    };
    assert_deterministic_replay(Brain::with_default_policy, fanout_race);

    let capability_done_race = || {
        vec![
            user("run the build"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "cargo build" })),
                usage: usage(),
                est_tokens: 1,
            },
            Event::UserAbort,
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "exit_code": 0 }),
                est_tokens: 1,
            },
            Event::OpCancelled { op: OpId(1) },
        ]
    };
    assert_deterministic_replay(Brain::with_default_policy, capability_done_race);
}

/// Aborting with several ops in flight must fan out `Cancel` commands in a
/// deterministic order (ascending op id): the in-flight table's iteration
/// order leaks into the command stream, and replay/trace-verify compares
/// command sequences bit-for-bit.
#[test]
fn abort_with_multiple_inflight_ops_cancels_in_op_id_order() {
    use huggr_core::{ModelOutput, ToolCall};

    let make_brain = || Brain::new(Box::new(background_shell_policy()));
    let script = || {
        vec![
            user("run two long builds"),
            // The model fans out two background shells (ops 1 and 2); nothing
            // blocks the turn, so it resumes as a fresh model call (op 3).
            Event::ModelDone {
                op: OpId(0),
                output: ModelOutput::tool_calls(vec![
                    ToolCall::new("call-1", "shell", json!({ "cmd": "cargo build" })),
                    ToolCall::new("call-2", "shell", json!({ "cmd": "cargo test" })),
                ]),
                usage: usage(),
                est_tokens: 1,
            },
            // Three ops in flight (two shells + the resumed model). Abort.
            Event::UserAbort,
        ]
    };

    let mut brain = make_brain();
    let commands = run_script(&mut brain, script());
    let cancelled: Vec<OpId> = commands
        .iter()
        .filter_map(|command| match command {
            Command::Cancel { op } => Some(*op),
            _ => None,
        })
        .collect();
    assert_eq!(
        cancelled,
        vec![OpId(1), OpId(2), OpId(3)],
        "Cancel fan-out must be in ascending op-id order, never hash order"
    );

    // And the whole path replays bit-for-bit across fresh brains.
    assert_deterministic_replay(make_brain, script);
}
