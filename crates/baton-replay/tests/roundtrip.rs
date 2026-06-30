//! Round-trip tests for the trace container (P3-1 DONE criteria).
//!
//! Serialize a realistic Phase 1/2 session — the ordered event stream + the
//! durable log — to disk, read it back, and assert byte-for-byte equality of
//! the reconstructed [`Trace`]. This is the property replay/resume (P3-3/P3-4)
//! build on: a trace persisted today reconstructs the same session later.

use baton_core::{
    Decision, Event, LogEntry, ModelOutput, OpId, Record, Seq, SteerMode, Timestamp, ToolCall,
    Usage,
};
use baton_replay::{BlobManifest, BlobRef, FORMAT_VERSION, Trace};
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
        },
        Event::ModelDelta {
            op: OpId(1),
            delta: baton_core::ModelDelta::Text("Sure, ".to_string()),
        },
        Event::ModelDone {
            op: OpId(1),
            output: ModelOutput::tool_calls(vec![ToolCall::new(
                "call_1",
                "shell",
                json!({ "cmd": "echo hi" }),
            )]),
            usage: Usage::new(42, 8),
        },
        Event::PermissionDecision {
            op: OpId(2),
            decision: Decision::Allow,
        },
        Event::CapabilityChunk {
            op: OpId(2),
            chunk: json!("hi\n"),
        },
        Event::CapabilityDone {
            op: OpId(2),
            result: json!({ "stdout": "hi\n", "exit": 0 }),
            version: None,
        },
        Event::ModelDone {
            op: OpId(3),
            output: ModelOutput::text("It printed: hi"),
            usage: Usage::new(60, 5),
        },
    ]
}

/// A representative consolidated log (one record per logical message/tool-result).
fn sample_log() -> Vec<LogEntry> {
    vec![
        LogEntry {
            seq: Seq(0),
            at: Timestamp(1_000),
            record: Record::UserMessage {
                text: "run `echo hi` and tell me the output".to_string(),
            },
        },
        LogEntry {
            seq: Seq(1),
            at: Timestamp(1_010),
            record: Record::ModelOutput {
                op: OpId(1),
                output: ModelOutput::tool_calls(vec![ToolCall::new(
                    "call_1",
                    "shell",
                    json!({ "cmd": "echo hi" }),
                )]),
            },
        },
        LogEntry {
            seq: Seq(2),
            at: Timestamp(1_020),
            record: Record::ToolResult {
                op: OpId(2),
                name: "shell".to_string(),
                call_id: "call_1".to_string(),
                result: json!({ "stdout": "hi\n", "exit": 0 }),
            },
        },
        // `OpEnded` carries `OpMeta`, which is `#[non_exhaustive]` (no struct
        // literal from outside its crate). Build it from JSON — this also pins
        // the real on-the-wire shape the trace persists.
        LogEntry {
            seq: Seq(3),
            at: Timestamp(1_021),
            record: serde_json::from_value(json!({
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
        },
        LogEntry {
            seq: Seq(4),
            at: Timestamp(1_030),
            record: Record::ModelOutput {
                op: OpId(3),
                output: ModelOutput::text("It printed: hi"),
            },
        },
        LogEntry {
            seq: Seq(5),
            at: Timestamp(1_031),
            record: serde_json::from_value(json!({
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
        },
    ]
}

#[test]
fn write_then_load_roundtrips_a_session_to_disk() {
    let trace = Trace::new(sample_events(), sample_log(), Some(1_000));

    let dir = std::env::temp_dir().join(format!("baton-replay-test-{}", std::process::id()));
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
fn rejects_unsupported_future_version() {
    // Hand-craft a trace JSON claiming a far-future format version.
    let bytes = serde_json::to_vec(&json!({
        "meta": {
            "codename": "baton-trace",
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
        matches!(err, baton_replay::TraceError::UnsupportedVersion { .. }),
        "expected UnsupportedVersion, got {err:?}"
    );
}
