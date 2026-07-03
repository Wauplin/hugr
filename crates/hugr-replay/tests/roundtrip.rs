//! Round-trip tests for the trace container (P3-1 DONE criteria).
//!
//! Serialize a realistic Phase 1/2 session — the ordered event stream + the
//! durable log — to disk, read it back, and assert byte-for-byte equality of
//! the reconstructed [`Trace`]. This is the property replay/resume (P3-3/P3-4)
//! build on: a trace persisted today reconstructs the same session later.

use hugr_core::{
    Decision, Event, LogEntry, ModelOutput, OpId, Record, Seq, SteerMode, Timestamp, ToolCall,
    Usage,
};
use hugr_replay::{BlobManifest, BlobRef, FORMAT_VERSION, Trace};
use serde_json::json;

/// A representative event stream mirroring a Phase 1/2 session:
/// user → model (with a tool call) → tool result → model → done, with a tick,
/// a permission decision, and streaming deltas (transport) interleaved.
fn sample_events() -> Vec<Event> {
    vec![
        Event::Tick {
            now: Timestamp(1_000),
        },
        Event::UserInput {
            content: json!("run `echo hi` and tell me the output"),
            mode: SteerMode::Queue,
            est_tokens: 1,
        },
        Event::ModelDelta {
            op: OpId(1),
            delta: hugr_core::ModelDelta::Text("Sure, ".to_string()),
        },
        Event::ModelDone {
            op: OpId(1),
            output: ModelOutput::tool_calls(vec![ToolCall::new(
                "call_1",
                "shell",
                json!({ "cmd": "echo hi" }),
            )]),
            usage: Usage::new(42, 8),
            est_tokens: 8,
        },
        Event::PermissionDecision {
            op: OpId(2),
            decision: Decision::Allow,
            est_tokens: 1,
        },
        Event::CapabilityChunk {
            op: OpId(2),
            chunk: json!("hi\n"),
        },
        Event::CapabilityDone {
            op: OpId(2),
            result: json!({ "stdout": "hi\n", "exit": 0 }),
            version: None,
            est_tokens: 1,
        },
        Event::ModelDone {
            op: OpId(3),
            output: ModelOutput::text("It printed: hi"),
            usage: Usage::new(60, 5),
            est_tokens: 5,
        },
    ]
}

/// A representative consolidated log (one record per logical message/tool-result).
fn sample_log() -> Vec<LogEntry> {
    vec![
        LogEntry::new(
            Seq(0),
            Timestamp(1_000),
            Record::UserMessage {
                text: "run `echo hi` and tell me the output".to_string(),
                est_tokens: 10,
                steer: SteerMode::Queue,
            },
        ),
        LogEntry::new(
            Seq(1),
            Timestamp(1_010),
            Record::ModelOutput {
                op: OpId(1),
                output: ModelOutput::tool_calls(vec![ToolCall::new(
                    "call_1",
                    "shell",
                    json!({ "cmd": "echo hi" }),
                )]),
                est_tokens: 8,
            },
        ),
        LogEntry::new(
            Seq(2),
            Timestamp(1_020),
            Record::ToolResult {
                op: OpId(2),
                name: "shell".to_string(),
                call_id: "call_1".to_string(),
                result: json!({ "stdout": "hi\n", "exit": 0 }),
                version: None,
                est_tokens: 8,
            },
        ),
        // `OpEnded` carries `OpMeta`, which is `#[non_exhaustive]` (no struct
        // literal from outside its crate). Build it from JSON — this also pins
        // the real on-the-wire shape the trace persists.
        LogEntry::new(
            Seq(3),
            Timestamp(1_021),
            serde_json::from_value(json!({
                "OpEnded": {
                    "op": 2,
                    "outcome": "Ok",
                    "meta": {
                        "started_at": 1_011,
                        "ended_at": 1_021,
                        "model": null,
                        "usage": null,
                        "extra": {}
                    }
                }
            }))
            .unwrap(),
        ),
        LogEntry::new(
            Seq(4),
            Timestamp(1_030),
            Record::ModelOutput {
                op: OpId(3),
                output: ModelOutput::text("It printed: hi"),
                est_tokens: 5,
            },
        ),
        LogEntry::new(
            Seq(5),
            Timestamp(1_031),
            serde_json::from_value(json!({
                "OpEnded": {
                    "op": 3,
                    "outcome": "Ok",
                    "meta": {
                        "started_at": 1_022,
                        "ended_at": 1_031,
                        "model": { "Named": "big" },
                        "usage": { "input_tokens": 60, "output_tokens": 5, "extra": null },
                        "extra": { "cost": 0.0003, "cost_source": "router" }
                    }
                }
            }))
            .unwrap(),
        ),
    ]
}

#[test]
fn write_then_load_roundtrips_a_session_to_disk() {
    let trace = Trace::new(sample_events(), sample_log(), Some(1_000));

    let dir = std::env::temp_dir().join(format!("hugr-replay-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.trace.json");

    trace.save(&path).unwrap();
    let loaded = Trace::load(&path).unwrap();

    // The DONE criterion: full event stream + log round-trips equal.
    assert_eq!(trace, loaded);
    assert_eq!(loaded.events, sample_events());
    assert_eq!(loaded.log, sample_log());
    assert_eq!(loaded.meta.format_version, FORMAT_VERSION);
    assert_eq!(loaded.meta.created_at, Some(1_000));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn in_memory_json_roundtrip_is_exact() {
    let trace = Trace::new(sample_events(), sample_log(), Some(1_000));
    let bytes = trace.to_json().unwrap();
    let back = Trace::from_json(&bytes).unwrap();
    assert_eq!(trace, back);
}

#[test]
fn empty_session_roundtrips() {
    let trace = Trace::new(vec![], vec![], None);
    let bytes = trace.to_json().unwrap();
    let back = Trace::from_json(&bytes).unwrap();
    assert_eq!(trace, back);
    assert!(back.events.is_empty());
    assert!(back.log.is_empty());
    assert_eq!(back.meta.created_at, None);
}

#[test]
fn blob_manifest_roundtrips() {
    let mut blobs = BlobManifest::new();
    blobs.push(BlobRef::new("sha256:abc", 12_000, "text/plain"));
    blobs.push(BlobRef::new("sha256:def", 4, "application/json"));

    let trace = Trace::with_blobs(sample_events(), sample_log(), Some(1_000), blobs.clone());
    let back = Trace::from_json(&trace.to_json().unwrap()).unwrap();
    assert_eq!(back.blobs, blobs);
    assert_eq!(back.blobs.refs.len(), 2);
}

#[test]
fn recorded_commands_roundtrip() {
    use hugr_core::{Command, DoneReason};
    let commands = vec![
        Command::Checkpoint,
        Command::Done {
            reason: DoneReason::EndTurn,
        },
    ];
    let trace =
        Trace::new(sample_events(), sample_log(), Some(1_000)).with_commands(commands.clone());
    let back = Trace::from_json(&trace.to_json().unwrap()).unwrap();
    assert_eq!(back.commands, commands, "commands round-trip through JSON");
    assert_eq!(back, trace, "the whole trace round-trips exactly");
}

/// Back-compat: a trace JSON with NO `commands` key (an old recording) still
/// deserializes, defaulting to an empty command sequence (serde default).
#[test]
fn old_json_without_commands_field_deserializes() {
    let bytes = serde_json::to_vec(&json!({
        "meta": {
            "codename": "hugr-trace",
            "format_version": FORMAT_VERSION,
            "created_at": null
        },
        "events": [],
        "log": [],
        "blobs": { "refs": [] }
    }))
    .unwrap();
    let trace = Trace::from_json(&bytes).expect("old commandless JSON must still parse");
    assert!(
        trace.commands.is_empty(),
        "a missing commands field defaults to empty"
    );
}

/// A trace with no commands does not emit a `commands` key at all, so a
/// recording made without command capture stays byte-identical to the
/// pre-`commands` on-disk format (skip_serializing_if).
#[test]
fn empty_commands_are_omitted_from_json() {
    let trace = Trace::new(sample_events(), sample_log(), Some(1_000));
    let json = String::from_utf8(trace.to_json().unwrap()).unwrap();
    assert!(
        !json.contains("\"commands\""),
        "empty commands must not appear in serialized JSON"
    );
}

#[test]
fn rejects_unsupported_future_version() {
    // Hand-craft a trace JSON claiming a far-future format version.
    let bytes = serde_json::to_vec(&json!({
        "meta": {
            "codename": "hugr-trace",
            "format_version": FORMAT_VERSION + 99,
            "created_at": null
        },
        "events": [],
        "log": [],
        "blobs": { "refs": [] }
    }))
    .unwrap();
    let err = Trace::from_json(&bytes).unwrap_err();
    assert!(
        matches!(err, hugr_replay::TraceError::UnsupportedVersion { .. }),
        "expected UnsupportedVersion, got {err:?}"
    );
}

/// A tiny but *real* recorded session: a fresh brain under the default
/// `StaticPolicy` folds a `Tick` + `UserInput`, with the drained commands and
/// resulting log captured — exactly what a recording host produces. Used to
/// build valid nested child traces without a live host.
fn tiny_session_trace(prompt: &str) -> Trace {
    use hugr_core::{Brain, StaticPolicy};

    let policy = StaticPolicy::default();
    let policy_json = serde_json::to_value(&policy).unwrap();
    let mut brain = Brain::new(Box::new(policy));
    let events = vec![
        Event::Tick { now: Timestamp(1) },
        Event::UserInput {
            content: json!(prompt),
            mode: SteerMode::Queue,
            est_tokens: 1,
        },
    ];
    let mut commands = Vec::new();
    for event in &events {
        brain.submit(event.clone());
        commands.extend(brain.poll());
    }
    Trace::new(events, brain.state().log().to_vec(), Some(1))
        .with_commands(commands)
        .with_policy(policy_json)
}

/// Back-compat both ways: a trace without children serializes with **no**
/// `children` key (byte-stable with the pre-`children` format), and old trace
/// JSON lacking the field still loads (serde default) and verifies.
#[test]
fn traces_without_children_stay_byte_stable_and_old_json_loads() {
    let trace = tiny_session_trace("hello");
    let json = String::from_utf8(trace.to_json().unwrap()).unwrap();
    assert!(
        !json.contains("\"children\""),
        "an empty children list must be skipped from the serialized JSON"
    );

    // Round-trip: the parsed trace has (empty) children and equals the original.
    let reparsed = Trace::from_json(json.as_bytes()).unwrap();
    assert!(reparsed.children.is_empty());
    assert_eq!(reparsed, trace);
    hugr_replay::verify(&reparsed).expect("a childless trace verifies as before");
}

/// The nesting is recursive — children can have children (grandchildren, depth
/// 2). The in-process host cannot spawn grandchildren today (child policies
/// advertise no agent tools), so the recursion is pinned here at the
/// serde + verify level: a hand-assembled parent → child → grandchild tree
/// round-trips through JSON and disk, `verify()` recurses through both levels,
/// and corrupting the *grandchild* fails verification with a nested
/// `ChildMismatch` naming each level's op.
#[test]
fn nested_child_traces_round_trip_and_verify_recursively() {
    use hugr_core::OpId;
    use hugr_replay::{ChildTrace, TraceError, test_support::TempDir};

    let grandchild = ChildTrace::new(OpId(7), "task", Vec::new(), tiny_session_trace("leaf"));
    let child_trace = tiny_session_trace("middle").with_children(vec![grandchild]);
    let child = ChildTrace::new(OpId(3), "task", Vec::new(), child_trace);
    let parent = tiny_session_trace("root").with_children(vec![child]);

    // Serde handles the recursion: JSON and disk round-trips are lossless.
    let reparsed = Trace::from_json(&parent.to_json().unwrap()).unwrap();
    assert_eq!(reparsed, parent);
    let dir = TempDir::new("nested-children");
    let path = dir.path().join("nested.trace.json");
    parent.save(&path).unwrap();
    let reloaded = Trace::load(&path).unwrap();
    assert_eq!(reloaded, parent);
    assert_eq!(reloaded.children[0].trace.children[0].op, OpId(7));

    // verify() recurses through both levels.
    hugr_replay::verify(&reloaded).expect("the depth-2 tree verifies recursively");

    // Corrupting the grandchild's recorded log fails the whole verification,
    // and the error names both levels: child op 3 wrapping grandchild op 7.
    let mut corrupted = parent.clone();
    corrupted.children[0].trace.children[0].trace.log.pop();
    let err = hugr_replay::verify(&corrupted).expect_err("a corrupted grandchild must fail");
    match err {
        TraceError::ChildMismatch { op: 3, source, .. } => match *source {
            TraceError::ChildMismatch { op: 7, source, .. } => {
                assert!(matches!(*source, TraceError::ReplayMismatch { .. }));
            }
            other => panic!("expected nested ChildMismatch for op 7, got: {other:?}"),
        },
        other => panic!("expected ChildMismatch for op 3, got: {other:?}"),
    }
}
