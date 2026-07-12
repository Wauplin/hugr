//! Multiple concurrent ops: a model stream and a **background** capability op
//! run simultaneously; the brain reduces their interleaved events one at a
//! time, atomically. The host provides the concurrency; the brain just keys
//! everything by `OpId`.
//!
//! These tests pin the resulting command sequence and assert deterministic
//! replay over the interleaved stream.

mod common;

use common::*;
use huggr_core::{
    Brain, Command, DoneReason, Event, ModelDelta, ModelOutput, OpId, StaticPolicy, ToolCall,
};
use serde_json::json;

/// A policy that runs `shell` in the background (does not block the turn) and
/// gates nothing on permission.
fn background_shell_policy() -> StaticPolicy {
    StaticPolicy::default().with_background(["shell".to_string()])
}

/// The model kicks off a background `shell` op, then the turn resumes
/// immediately into a second model call instead of waiting for the shell. Their
/// events interleave, and a final turn sees the shell result and ends.
#[test]
fn model_stream_and_background_shell_run_concurrently() {
    let mut brain = Brain::new(Box::new(background_shell_policy()));

    let commands = run_script(
        &mut brain,
        vec![
            // User asks → model call op 0.
            user("build and summarize"),
            // Model asks for a background shell op (op 1). Because `shell` is a
            // background capability, the turn resumes into another model call
            // (op 2) WITHOUT waiting for op 1.
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "cargo build" })),
                usage: usage(),
                est_tokens: 1,
            },
            // Now op 1 (shell) and op 2 (model) are both in flight. Their events
            // interleave in arrival order:
            Event::ModelDelta {
                op: OpId(2),
                delta: ModelDelta::Text("Working".into()),
            },
            Event::CapabilityChunk {
                op: OpId(1),
                chunk: json!("Compiling huggr...\n"),
            },
            Event::ModelDelta {
                op: OpId(2),
                delta: ModelDelta::Text(" on it".into()),
            },
            // The model (op 2) finishes first with a plain answer. The turn does
            // NOT end yet — the background shell op is still running.
            Event::ModelDone {
                op: OpId(2),
                output: text_output("Build kicked off."),
                usage: usage(),
                est_tokens: 1,
            },
            // The shell (op 1) exits — reacted to instantly via the event. Its
            // result is folded in and a fresh turn (op 3) picks it up.
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "exit_code": 0, "stdout": "Finished" }),
                est_tokens: 1,
            },
            // The final model call ends the turn.
            Event::ModelDone {
                op: OpId(3),
                output: text_output("Build finished successfully."),
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
                // Background shell starts (op 1)...
                Command::StartCapability { op: OpId(1), name, .. },
                // ...and the model turn resumes immediately (op 2) — concurrent.
                Command::StartModelCall { op: OpId(2), .. },
                // op 2 finishes but the turn stays open (shell still running):
                // checkpoint, but NO Done here.
                Command::Checkpoint,
                // Shell finishes → fresh turn (op 3) picks up its result.
                Command::StartModelCall { op: OpId(3), .. },
                Command::Checkpoint,
                Command::Done { reason: DoneReason::EndTurn },
            ] if name == "shell"
        ),
        "unexpected command sequence: {effectful:#?}"
    );
}

/// Re-feeding the identical interleaved stream to a fresh brain yields identical
/// commands and an identical durable log — the merge order is recorded, the
/// fold is pure.
#[test]
fn concurrent_ops_replay_is_deterministic() {
    let script = || {
        vec![
            user("build and summarize"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "cargo build" })),
                usage: usage(),
                est_tokens: 1,
            },
            Event::ModelDelta {
                op: OpId(2),
                delta: ModelDelta::Text("Working".into()),
            },
            Event::CapabilityChunk {
                op: OpId(1),
                chunk: json!("Compiling...\n"),
            },
            Event::ModelDone {
                op: OpId(2),
                output: text_output("Build kicked off."),
                usage: usage(),
                est_tokens: 1,
            },
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "exit_code": 0 }),
                est_tokens: 1,
            },
            Event::ModelDone {
                op: OpId(3),
                output: text_output("Done."),
                usage: usage(),
                est_tokens: 1,
            },
        ]
    };

    let mut a = Brain::new(Box::new(background_shell_policy()));
    let commands_a = run_script(&mut a, script());

    let mut b = Brain::new(Box::new(background_shell_policy()));
    let commands_b = run_script(&mut b, script());

    assert_eq!(
        commands_a, commands_b,
        "interleaved concurrent-op stream must replay to identical commands"
    );
    assert_eq!(
        a.state().log(),
        b.state().log(),
        "interleaved concurrent-op stream must replay to an identical log"
    );
}

/// A foreground tool call running at the same time as a background op: the turn
/// still waits for the *foreground* op (it blocks the turn) while the background
/// op runs independently. Proves `blocks_turn` discriminates correctly and that
/// two capability ops plus the model are all tracked concurrently by `OpId`.
#[test]
fn background_op_does_not_gate_a_concurrent_foreground_op() {
    // `shell` is background; `http` is an ordinary foreground (blocking) tool.
    let policy = StaticPolicy::default().with_background(["shell".to_string()]);
    let mut brain = Brain::new(Box::new(policy));

    let two_calls = ModelOutput::tool_calls(vec![
        ToolCall::new("a", "shell", json!({ "cmd": "sleep 1" })), // background → op 1
        ToolCall::new("b", "http", json!({ "url": "https://x" })), // foreground → op 2
    ]);

    let commands = run_script(
        &mut brain,
        vec![
            user("do two things"),
            Event::ModelDone {
                op: OpId(0),
                output: two_calls,
                usage: usage(),
                est_tokens: 1,
            },
            // The background shell finishes first — must NOT resume the model,
            // because the foreground http op (op 2) still blocks the turn.
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "exit_code": 0 }),
                est_tokens: 1,
            },
            // The foreground http finishes — now the model resumes (op 3).
            Event::CapabilityDone {
                op: OpId(2),
                result: json!({ "status": 200 }),
                est_tokens: 1,
            },
            Event::ModelDone {
                op: OpId(3),
                output: text_output("All done."),
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
                Command::StartCapability { op: OpId(2), .. },
                // Exactly one resume, and only after the FOREGROUND op resolved.
                Command::StartModelCall { op: OpId(3), .. },
                Command::Checkpoint,
                Command::Done {
                    reason: DoneReason::EndTurn
                },
            ]
        ),
        "unexpected command sequence: {effectful:#?}"
    );
}
