//! Pins the `Ask`/`Answer` wire contract.
//!
//! Three layers of pinning:
//! 1. serde round-trips for minimal and fully-populated values;
//! 2. exact wire-JSON snapshots (field names and status strings are the
//!    contract — renaming a field is a breaking change and must fail here);
//! 3. the committed JSON schema files stay structurally in lock-step with the
//!    Rust types (property sets match what serde actually emits).

use huggr_agent::{
    Answer, AnswerMeta, Ask, BlobHandle, BlobRef, STATUS_ERROR, STATUS_SUCCESS, TraceId,
};
use serde_json::{Value, json};

fn full_ask() -> Ask {
    Ask {
        question: "Which expenses violate policy?".into(),
        trace_id: Some(TraceId::new("tr-parent")),
        blobs: vec![
            BlobHandle {
                blob_ref: BlobRef::Bytes {
                    base64: "aGVsbG8=".into(),
                },
                media_type: "text/plain".into(),
                name: Some("note.txt".into()),
            },
            BlobHandle {
                blob_ref: BlobRef::Sha256 {
                    sha256: "ab".repeat(32),
                },
                media_type: "application/pdf".into(),
                name: None,
            },
        ],
        skills: vec!["./skills/policy-review".into()],
        extra: json!({"caller": "orchestrator-1"}),
    }
}

fn full_answer() -> Answer {
    let metadata = AnswerMeta {
        duration_ms: 1234,
        cost_micro_usd: 43,
        tokens_in: 1700,
        tokens_out: 350,
        model_calls: 3,
        tool_calls: 3,
    };
    Answer {
        status: STATUS_SUCCESS.to_string(),
        response: json!({
            "response": {
                "summary": "Two expenses exceed the hotel cap."
            },
            "related_documents": ["travel.md"]
        }),
        trace_id: TraceId::new("tr-child"),
        blobs: vec![BlobHandle {
            blob_ref: BlobRef::Path {
                path: "report.md".into(),
            },
            media_type: "text/markdown".into(),
            name: None,
        }],
        metadata,
        extra: json!({"caller_visible": true}),
    }
}

#[test]
fn contract_round_trips_serde() {
    let minimal_ask = Ask::new("q");
    let re: Ask = serde_json::from_str(&serde_json::to_string(&minimal_ask).unwrap()).unwrap();
    assert_eq!(minimal_ask, re);

    let ask = full_ask();
    let re: Ask = serde_json::from_str(&serde_json::to_string(&ask).unwrap()).unwrap();
    assert_eq!(ask, re);

    let answer = full_answer();
    let re: Answer = serde_json::from_str(&serde_json::to_string(&answer).unwrap()).unwrap();
    assert_eq!(answer, re);
}

#[test]
fn minimal_ask_wire_form_is_question_only() {
    // Optional fields are omitted, not null/empty — the minimal wire form is
    // exactly {"question": ...}.
    assert_eq!(
        serde_json::to_value(Ask::new("q")).unwrap(),
        json!({"question": "q"})
    );
}

#[test]
fn trace_ids_reject_path_components_on_deserialization() {
    let error = serde_json::from_value::<Ask>(json!({
        "question": "q",
        "trace_id": "../outside"
    }))
    .unwrap_err();
    assert!(error.to_string().contains("trace id"));
}

#[test]
fn full_wire_snapshots_are_pinned() {
    // Field names and status strings ARE the contract. If this test fails, you
    // changed the wire format: bump/version the committed schemas instead.
    assert_eq!(
        serde_json::to_value(full_ask()).unwrap(),
        json!({
            "question": "Which expenses violate policy?",
            "trace_id": "tr-parent",
            "blobs": [
                {
                    "ref": {"kind": "bytes", "base64": "aGVsbG8="},
                    "media_type": "text/plain",
                    "name": "note.txt"
                },
                {
                    "ref": {"kind": "sha256", "sha256": "ab".repeat(32)},
                    "media_type": "application/pdf"
                }
            ],
            "skills": ["./skills/policy-review"],
            "extra": {"caller": "orchestrator-1"}
        })
    );

    assert_eq!(
        serde_json::to_value(full_answer()).unwrap(),
        json!({
            "status": "success",
            "response": {
                "response": {
                    "summary": "Two expenses exceed the hotel cap."
                },
                "related_documents": ["travel.md"]
            },
            "trace_id": "tr-child",
            "blobs": [
                {
                    "ref": {"kind": "path", "path": "report.md"},
                    "media_type": "text/markdown"
                }
            ],
            "metadata": {
                "duration_ms": 1234,
                "cost_micro_usd": 43,
                "tokens_in": 1700,
                "tokens_out": 350,
                "model_calls": 3,
                "tool_calls": 3
            },
            "extra": {"caller_visible": true}
        })
    );
}

#[test]
fn errors_are_answers_with_mandatory_zeroed_meta() {
    // An error before any model call still serializes full accounting.
    let answer = Answer {
        status: STATUS_ERROR.to_string(),
        response: json!({"error": "model endpoint unreachable"}),
        trace_id: TraceId::new("tr-err"),
        ..Answer::default()
    };
    let wire = serde_json::to_value(&answer).unwrap();
    assert_eq!(wire["status"], "error");
    assert_eq!(wire["metadata"]["cost_micro_usd"], 0);
}

#[test]
fn sparse_wire_forms_keep_loading() {
    // Forward compat inside the current contract: serde defaults let sparse
    // JSON deserialize, but renamed fields are intentionally breaking.
    let ask: Ask = serde_json::from_value(json!({"question": "q"})).unwrap();
    assert!(ask.trace_id.is_none() && ask.blobs.is_empty() && ask.extra.is_null());

    let answer: Answer = serde_json::from_value(json!({
        "status": "success",
        "response": {"text": "m"},
        "trace_id": "t",
        "metadata": {
            "duration_ms": 1, "cost_micro_usd": 0, "tokens_in": 0, "tokens_out": 0,
            "model_calls": 0, "tool_calls": 0
        }
    }))
    .unwrap();
    assert!(answer.blobs.is_empty());
}

const ASK_SCHEMA: &str = include_str!("../schemas/ask.schema.json");
const ANSWER_SCHEMA: &str = include_str!("../schemas/answer.schema.json");

fn property_names(schema_object: &Value) -> Vec<String> {
    let mut names: Vec<String> = schema_object["properties"]
        .as_object()
        .expect("schema object has properties")
        .keys()
        .cloned()
        .collect();
    names.sort();
    names
}

fn sorted_keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

#[test]
fn committed_schemas_match_the_rust_types() {
    let ask_schema: Value = serde_json::from_str(ASK_SCHEMA).unwrap();
    let answer_schema: Value = serde_json::from_str(ANSWER_SCHEMA).unwrap();

    // The fully-populated wire forms emit exactly the properties the schemas
    // declare — a field added to the types without updating the schema (or
    // vice versa) fails here.
    assert_eq!(
        property_names(&ask_schema),
        sorted_keys(&serde_json::to_value(full_ask()).unwrap())
    );
    let full_answer_wire = serde_json::to_value(full_answer()).unwrap();
    assert_eq!(
        property_names(&answer_schema),
        sorted_keys(&full_answer_wire)
    );
    assert_eq!(
        property_names(&answer_schema["$defs"]["answer_meta"]),
        sorted_keys(&full_answer_wire["metadata"])
    );
    assert_eq!(
        property_names(&ask_schema["$defs"]["blob_handle"]),
        sorted_keys(&serde_json::to_value(full_ask()).unwrap()["blobs"][0])
    );

    // Required fields and status strings are pinned.
    assert_eq!(ask_schema["required"], json!(["question"]));
    assert_eq!(
        answer_schema["required"],
        json!(["status", "response", "trace_id", "metadata"])
    );
    assert_eq!(
        answer_schema["properties"]["status"]["enum"],
        json!(["success", "error"])
    );
    // The three blob-ref variants, tagged by "kind".
    let kinds: Vec<&str> = ask_schema["$defs"]["blob_ref"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["properties"]["kind"]["const"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["bytes", "path", "sha256"]);
}
