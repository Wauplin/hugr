//! Replay & inspector tests (P3-3): re-feed a trace's recorded event stream
//! into a fresh brain and assert the reconstruction is deterministic and the
//! recorded log matches bit-for-bit — the Phase 3 exit criterion. Also covers
//! the step-through [`Inspector`].

use hugr_core::{Command, Event, ModelOutput, OpId, Timestamp, ToolCall, Usage, Value};
use hugr_replay::{Inspector, Trace, TraceError, replay, verify};
use serde_json::json;

/// The ordered host→brain event stream of a realistic Phase 1/2 session:
/// `user → model (tool call) → tool result → model → done`, with the injected
/// `Tick`s the host stamps before each event (the recorder captures both).
fn session_events() -> Vec<Event> {
    let tick = |n| Event::Tick { now: Timestamp(n) };
    vec![
        // user message
        tick(1),
        Event::UserInput {
            content: json!("run echo hi"),
            est_tokens: 1,
        },
        // model asks for a shell tool call (op 0 is the first model call)
        tick(2),
        Event::ModelDone {
            op: OpId(0),
            output: ModelOutput::tool_calls(vec![ToolCall::new(
                "call_1",
                "shell",
                json!({ "cmd": "echo hi" }),
            )]),
            usage: Usage::new(10, 2),
            est_tokens: 2,
        },
        // shell op (op 1) returns
        tick(3),
        Event::CapabilityDone {
            op: OpId(1),
            result: json!({ "stdout": "hi\n", "exit": 0 }),
            est_tokens: 1,
        },
        // model's final answer (op 2), no tool calls → turn ends
        tick(4),
        Event::ModelDone {
            op: OpId(2),
            output: ModelOutput::text("It printed: hi"),
            usage: Usage::new(20, 5),
            est_tokens: 5,
        },
    ]
}

/// Build a trace whose `log` is exactly what folding `events` produces, so
/// `verify` has a faithful recording to check against (this is what the host
/// recorder captures: events as input, the brain's resulting log as the truth).
fn faithful_trace() -> Trace {
    let events = session_events();
    // Derive the consolidated log by replaying once (the recorder reads it from
    // the live brain at save time).
    let derived = replay(&Trace::new(events.clone(), vec![], Some(1)));
    Trace::new(events, derived.log, Some(1))
}

#[test]
fn replay_is_deterministic_bit_for_bit() {
    let trace = faithful_trace();

    let first = replay(&trace);
    let second = replay(&trace);

    // The whole point: identical event stream → identical commands AND log.
    assert_eq!(
        first.commands, second.commands,
        "re-feeding the same events must yield identical commands"
    );
    assert_eq!(first.log, second.log, "and an identical reconstructed log");

    // The command sequence is the expected agentic turn loop.
    let kinds: Vec<&str> = first
        .commands
        .iter()
        .map(|c| match c {
            Command::StartModelCall { .. } => "StartModelCall",
            Command::StartCapability { .. } => "StartCapability",
            Command::RequestPermission { .. } => "RequestPermission",
            Command::Emit(_) => "Emit",
            Command::Checkpoint => "Checkpoint",
            Command::Done { .. } => "Done",
            _ => "other",
        })
        .collect();
    // user → model call → (tool call) → tool → model call → checkpoint → done.
    assert_eq!(kinds.first(), Some(&"StartModelCall"));
    assert!(kinds.contains(&"StartCapability"));
    assert_eq!(kinds.last(), Some(&"Done"));
}

#[test]
fn verify_passes_for_a_faithful_trace() {
    let trace = faithful_trace();
    let replay = verify(&trace).expect("a faithfully recorded trace must verify");
    assert_eq!(replay.log, trace.log);
    assert!(!replay.commands.is_empty());
}

/// A faithful trace that *also* carries the recorded command sequence (as the
/// host recorder now does): `verify` compares commands bit-for-bit in addition
/// to the log, and still passes for a genuine recording.
#[test]
fn verify_compares_recorded_commands_and_passes() {
    let events = session_events();
    let derived = replay(&Trace::new(events.clone(), vec![], Some(1)));
    // Attach BOTH the derived log and the derived command sequence — exactly the
    // shape `Engine::trace()` now produces.
    let trace = Trace::new(events, derived.log, Some(1)).with_commands(derived.commands.clone());
    assert!(!trace.commands.is_empty(), "the trace carries commands");

    let replayed = verify(&trace).expect("a faithful trace with commands must verify");
    assert_eq!(
        replayed.commands, trace.commands,
        "verify reconstructed the exact recorded command sequence"
    );
}

/// The regression this whole feature exists for: a trace whose recorded
/// `commands` disagree with what re-feeding its events re-emits must FAIL
/// verification — command-order divergence that never touches the log is now
/// caught (§6.3).
#[test]
fn verify_rejects_a_divergent_command_sequence() {
    let events = session_events();
    let derived = replay(&Trace::new(events.clone(), vec![], Some(1)));

    // Craft a recording whose command sequence differs from what replay
    // re-emits: drop the last command. The log still matches bit-for-bit, so
    // the OLD log-only check would wave this through — only the command
    // comparison catches it.
    let mut tampered_commands = derived.commands.clone();
    tampered_commands.pop().expect("session emitted commands");
    let trace = Trace::new(events, derived.log, Some(1)).with_commands(tampered_commands);

    let err = verify(&trace).expect_err("divergent command sequence must fail verification");
    match err {
        TraceError::CommandMismatch {
            recorded,
            reconstructed,
            ..
        } => {
            assert_eq!(
                recorded + 1,
                reconstructed,
                "replay re-emits the dropped command"
            );
        }
        other => panic!("expected CommandMismatch, got {other:?}"),
    }
}

/// A reordered command sequence (same length, same set, wrong order) is exactly
/// the `HashMap`-ordered-cancel-all class of bug — and it, too, must fail.
#[test]
fn verify_rejects_reordered_commands() {
    let events = session_events();
    let derived = replay(&Trace::new(events.clone(), vec![], Some(1)));
    assert!(
        derived.commands.len() >= 2,
        "need at least two commands to reorder"
    );
    let mut reordered = derived.commands.clone();
    reordered.swap(0, 1);
    let trace = Trace::new(events, derived.log, Some(1)).with_commands(reordered);
    let err = verify(&trace).expect_err("reordered commands must fail verification");
    assert!(
        matches!(err, TraceError::CommandMismatch { .. }),
        "expected CommandMismatch, got {err:?}"
    );
}

/// Back-compat: an OLD-style trace with no recorded `commands` (empty via serde
/// default) still verifies — `verify` falls back to the log-only comparison.
#[test]
fn verify_falls_back_to_log_only_for_old_traces() {
    let trace = faithful_trace();
    assert!(
        trace.commands.is_empty(),
        "faithful_trace models a pre-commands recording"
    );
    // No commands recorded, but the log is faithful → log-only fallback passes.
    verify(&trace).expect("an old commandless trace verifies via the log-only fallback");

    // And a commandless trace whose LOG is wrong still fails (the fallback is
    // not a blanket pass).
    let mut broken = faithful_trace();
    broken.log.pop();
    assert!(matches!(
        verify(&broken).unwrap_err(),
        TraceError::ReplayMismatch { .. }
    ));
}

#[test]
fn verify_rejects_a_tampered_log() {
    let mut trace = faithful_trace();
    // Corrupt the recorded log: drop its last entry. Replay rebuilds the full
    // log from the events, so the lengths now disagree.
    trace.log.pop();
    let err = verify(&trace).unwrap_err();
    assert!(
        matches!(err, TraceError::ReplayMismatch { .. }),
        "expected ReplayMismatch, got {err:?}"
    );
}

#[test]
fn replay_round_trips_through_disk() {
    let trace = faithful_trace();
    let dir = std::env::temp_dir().join(format!("hugr-replay-disk-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.trace.json");
    trace.save(&path).unwrap();

    // Load from disk and replay → reconstruct the same commands + log.
    let loaded = Trace::load(&path).unwrap();
    let from_disk = verify(&loaded).expect("disk-loaded trace must replay bit-for-bit");
    let in_memory = replay(&trace);
    assert_eq!(from_disk.commands, in_memory.commands);
    assert_eq!(from_disk.log, in_memory.log);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn inspector_steps_through_every_event() {
    let trace = faithful_trace();
    let mut inspector = Inspector::new(&trace);
    assert_eq!(inspector.len(), trace.events.len());
    assert!(!inspector.is_empty());

    let mut step_count = 0;
    let mut all_commands = Vec::new();
    let mut all_appended = Vec::new();
    let mut last_index = None;
    while let Some(step) = inspector.step() {
        // Steps are reported in order, one per recorded event.
        assert_eq!(step.index, step_count);
        if let Some(prev) = last_index {
            assert_eq!(step.index, prev + 1);
        }
        last_index = Some(step.index);
        all_commands.extend(step.commands);
        all_appended.extend(step.appended);
        step_count += 1;
    }

    assert_eq!(step_count, trace.events.len(), "one step per event");
    assert_eq!(inspector.position(), trace.events.len());
    assert!(
        inspector.step().is_none(),
        "exhausted inspector yields None"
    );

    // Stepwise reconstruction equals the whole-stream replay: the per-step
    // commands and appended log entries, concatenated, are the full session.
    let whole = replay(&trace);
    assert_eq!(all_commands, whole.commands);
    assert_eq!(all_appended, whole.log);
    // And the appended entries are exactly the recorded log (bit-for-bit).
    assert_eq!(all_appended, trace.log);
}

#[test]
fn inspector_run_collects_all_steps() {
    let trace = faithful_trace();
    let steps = Inspector::new(&trace).run();
    assert_eq!(steps.len(), trace.events.len());
    // The user-input event is the one that first appends a log entry and emits
    // the opening StartModelCall.
    let opening = steps
        .iter()
        .find(|s| matches!(s.event, Event::UserInput { .. }))
        .expect("there is a user-input step");
    assert!(
        opening
            .commands
            .iter()
            .any(|c| matches!(c, Command::StartModelCall { .. })),
        "user input opens a model turn"
    );
}

#[test]
fn empty_trace_replays_to_nothing() {
    let trace = Trace::new(vec![], vec![], None);
    let replay = verify(&trace).expect("empty trace verifies trivially");
    assert!(replay.commands.is_empty());
    assert!(replay.log.is_empty());
    let _ = Value::Null; // silence unused import if the rest changes
}
