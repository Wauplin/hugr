//! `traces_read` — a root-jailed, read-only tool family over one agent's
//! stored traces and feedback sidecars.
//!
//! One grant registers four read capabilities sharing the same [`TracesRoot`]
//! jail (an agent home directory containing `traces/` and `feedback/`):
//!
//! | tool               | purpose                                              |
//! | ------------------ | ---------------------------------------------------- |
//! | `trace_list`       | trace heads with feedback counts                     |
//! | `trace_ops`        | per-op summaries (names, durations, tokens, errors)  |
//! | `trace_transcript` | paged, size-capped rendering of a trace's log        |
//! | `feedback_list`    | the feedback events filed against one trace          |
//!
//! Results are summaries, never raw trace JSON: a full trace would blow any
//! context budget, and the point of the family is that domain tools beat
//! generic ones. Privilege class: **read-only** (`requires_permission() ==
//! false`) — the jail is the boundary. Trace ids are validated to a closed
//! character set before touching the filesystem, so a crafted id cannot
//! traverse out of the root.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use huggr_core::{OpOutcome, Record, ToolSchema, Value};
use huggr_host::{Capability, ChunkSink};
use huggr_replay::Trace;
use serde_json::json;

use huggr_agent::{FeedbackBackend, FsFeedbackStore, TraceId, TraceStore};

const DEFAULT_LIST_LIMIT: usize = 100;
const MAX_LIST_LIMIT: usize = 1_000;
const DEFAULT_TRANSCRIPT_ENTRIES: usize = 20;
const MAX_TRANSCRIPT_ENTRIES: usize = 200;
const DEFAULT_ENTRY_CHARS: usize = 2_000;
const MAX_ENTRY_CHARS: usize = 20_000;
const DEFAULT_FEEDBACK_LIMIT: usize = 100;

/// A canonicalized agent home root; traces read from `<root>/traces`,
/// feedback from `<root>/feedback`. Cheap to clone (`Arc` inside).
#[derive(Clone, Debug)]
pub struct TracesRoot {
    root: Arc<PathBuf>,
}

impl TracesRoot {
    /// Canonicalize and validate the agent home directory. A leading `~/` is
    /// expanded against `$HOME` so grants can name `~/.huggr/<agent>` directly.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = expand_tilde(root.as_ref());
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalizing traces_read root {}", root.display()))?;
        anyhow::ensure!(
            root.is_dir(),
            "traces_read root is not a directory: {}",
            root.display()
        );
        Ok(Self {
            root: Arc::new(root),
        })
    }

    /// The four read capabilities backed by this root.
    pub fn capabilities(&self) -> Vec<Arc<dyn Capability>> {
        vec![
            Arc::new(TraceList(self.clone())),
            Arc::new(TraceOps(self.clone())),
            Arc::new(TraceTranscript(self.clone())),
            Arc::new(FeedbackList(self.clone())),
        ]
    }

    fn store(&self) -> TraceStore {
        TraceStore::new(self.root.join("traces"))
    }

    fn feedback(&self) -> FsFeedbackStore {
        FsFeedbackStore::new(self.root.join("feedback"))
    }
}

fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    path.to_path_buf()
}

/// Trace ids key file paths; only the store's own alphabet is accepted so a
/// crafted id (`../…`, absolute, separators) can never leave the jail.
fn checked_trace_id(args: &Value) -> Result<TraceId> {
    let id = args
        .get("trace_id")
        .and_then(Value::as_str)
        .context("requires string `trace_id`")?;
    anyhow::ensure!(
        !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "invalid trace_id: {id}"
    );
    Ok(TraceId::new(id))
}

fn truncate_chars(text: &str, limit: usize) -> (String, bool) {
    if text.chars().count() <= limit {
        return (text.to_string(), false);
    }
    (text.chars().take(limit).collect(), true)
}

fn value_excerpt(value: &Value, limit: usize) -> Value {
    let rendered = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let (excerpt, truncated) = truncate_chars(&rendered, limit);
    json!({ "excerpt": excerpt, "truncated": truncated })
}

/// Wrap a fallible impl as the standard tool result: `Ok`/`Err(error)` both
/// become tool results the model reads.
fn wrap(result: Result<Value>) -> std::result::Result<Value, Value> {
    result.map_err(|error| json!({ "error": error.to_string() }))
}

struct TraceList(TracesRoot);
struct TraceOps(TracesRoot);
struct TraceTranscript(TracesRoot);
struct FeedbackList(TracesRoot);

#[async_trait]
impl Capability for TraceList {
    fn name(&self) -> &str {
        "trace_list"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "trace_list",
            "List stored trace heads (id, lineage, question, status) with feedback counts, oldest first by id.",
            json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum heads to return. Defaults to 100." }
                },
                "additionalProperties": false
            }),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap(self.list(args).await)
    }
}

impl TraceList {
    async fn list(&self, args: Value) -> Result<Value> {
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_LIST_LIMIT as u64)
            .clamp(1, MAX_LIST_LIMIT as u64) as usize;
        let heads = self.0.store().list()?;
        let total = heads.len();
        let feedback = self.0.feedback();
        let mut traces = Vec::new();
        for head in heads.into_iter().take(limit) {
            let feedback_count = feedback.list(&head.trace_id).await?.len();
            traces.push(json!({
                "trace_id": head.trace_id.as_str(),
                "depends_on": head.depends_on.as_ref().map(TraceId::as_str),
                "created_at": head.created_at,
                "question": head.question,
                "status": head.status,
                "feedback_count": feedback_count,
            }));
        }
        Ok(json!({ "traces": traces, "total": total, "truncated": total > limit }))
    }
}

#[async_trait]
impl Capability for TraceOps {
    fn name(&self) -> &str {
        "trace_ops"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "trace_ops",
            "Summarize one trace's operations in order: model calls (selector, tokens) and tool calls (name, duration, error), never raw content.",
            json!({
                "type": "object",
                "properties": {
                    "trace_id": { "type": "string", "description": "The trace to summarize (from trace_list)." }
                },
                "required": ["trace_id"],
                "additionalProperties": false
            }),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap((|| {
            let id = checked_trace_id(&args)?;
            let trace = self.0.store().get(&id)?;
            Ok(ops_summary(&trace))
        })())
    }
}

fn ops_summary(trace: &Trace) -> Value {
    let mut tool_names = std::collections::BTreeMap::new();
    for entry in &trace.log {
        if let Record::ToolResult { op, name, .. } = &entry.record {
            tool_names.insert(*op, name.clone());
        }
    }
    let mut ops = Vec::new();
    let (mut model_calls, mut tool_calls, mut errors) = (0u32, 0u32, 0u32);
    for entry in &trace.log {
        let Record::OpEnded { op, outcome, meta } = &entry.record else {
            continue;
        };
        let duration_ms = meta.ended_at.0.saturating_sub(meta.started_at.0);
        let outcome_label = match outcome {
            OpOutcome::Ok => "ok",
            OpOutcome::Error(_) => "error",
            OpOutcome::Cancelled { .. } => "cancelled",
            _ => "other",
        };
        if !matches!(outcome, OpOutcome::Ok) {
            errors += 1;
        }
        if let Some(selector) = &meta.model {
            model_calls += 1;
            ops.push(json!({
                "op": op.0,
                "kind": "model",
                "name": selector.0,
                "duration_ms": duration_ms,
                "tokens_in": meta.usage.as_ref().map(|u| u.input_tokens),
                "tokens_out": meta.usage.as_ref().map(|u| u.output_tokens),
                "outcome": outcome_label,
            }));
        } else {
            tool_calls += 1;
            ops.push(json!({
                "op": op.0,
                "kind": "tool",
                "name": tool_names.get(op).cloned().unwrap_or_else(|| "<unknown>".to_string()),
                "duration_ms": duration_ms,
                "outcome": outcome_label,
            }));
        }
    }
    json!({
        "question": trace.meta.question,
        "status": trace.meta.status,
        "log_entries": trace.log.len(),
        "model_calls": model_calls,
        "tool_calls": tool_calls,
        "errors": errors,
        "ops": ops,
    })
}

#[async_trait]
impl Capability for TraceTranscript {
    fn name(&self) -> &str {
        "trace_transcript"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "trace_transcript",
            "Read one trace's conversation log as paged, size-capped entries (user messages, model output, tool results). Use `start` from a previous call's `next_start` to page.",
            json!({
                "type": "object",
                "properties": {
                    "trace_id": { "type": "string", "description": "The trace to read (from trace_list)." },
                    "start": { "type": "integer", "minimum": 0, "description": "Entry index to start from. Defaults to 0." },
                    "max_entries": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum entries to return. Defaults to 20." },
                    "max_chars": { "type": "integer", "minimum": 1, "maximum": 20000, "description": "Per-entry content cap in characters. Defaults to 2000." }
                },
                "required": ["trace_id"],
                "additionalProperties": false
            }),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap((|| {
            let id = checked_trace_id(&args)?;
            let start = args.get("start").and_then(Value::as_u64).unwrap_or(0) as usize;
            let max_entries = args
                .get("max_entries")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TRANSCRIPT_ENTRIES as u64)
                .clamp(1, MAX_TRANSCRIPT_ENTRIES as u64) as usize;
            let max_chars = args
                .get("max_chars")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_ENTRY_CHARS as u64)
                .clamp(1, MAX_ENTRY_CHARS as u64) as usize;
            let trace = self.0.store().get(&id)?;
            Ok(transcript_page(&trace, start, max_entries, max_chars))
        })())
    }
}

fn transcript_page(trace: &Trace, start: usize, max_entries: usize, max_chars: usize) -> Value {
    // OpEnded is bookkeeping (surfaced by trace_ops); the transcript pages the
    // conversational records only.
    let content: Vec<(usize, &Record)> = trace
        .log
        .iter()
        .enumerate()
        .filter(|(_, entry)| !matches!(entry.record, Record::OpEnded { .. }))
        .map(|(index, entry)| (index, &entry.record))
        .collect();
    let total = content.len();
    let mut entries = Vec::new();
    for (position, (log_index, record)) in content.iter().enumerate().skip(start) {
        if entries.len() >= max_entries {
            break;
        }
        let rendered = match record {
            Record::UserMessage { text, .. } => {
                let (excerpt, truncated) = truncate_chars(text, max_chars);
                json!({ "kind": "user", "text": excerpt, "text_truncated": truncated })
            }
            Record::ModelOutput { output, .. } => {
                let (excerpt, truncated) = truncate_chars(&output.text, max_chars);
                let tool_calls: Vec<Value> = output
                    .tool_calls
                    .iter()
                    .map(|call| {
                        json!({
                            "name": call.name,
                            "args": value_excerpt(&call.args, max_chars),
                        })
                    })
                    .collect();
                json!({
                    "kind": "model",
                    "text": excerpt,
                    "text_truncated": truncated,
                    "tool_calls": tool_calls,
                })
            }
            Record::ContextSummary { text, .. } => {
                let (excerpt, truncated) = truncate_chars(text, max_chars);
                json!({ "kind": "context_summary", "text": excerpt, "text_truncated": truncated })
            }
            Record::ToolResult { name, result, .. } => json!({
                "kind": "tool_result",
                "name": name,
                "result": value_excerpt(result, max_chars),
            }),
            _ => json!({ "kind": "other" }),
        };
        let mut entry = rendered;
        entry["index"] = json!(position);
        entry["seq"] = json!(trace.log[*log_index].seq.0);
        entries.push(entry);
    }
    let next_start = start + entries.len();
    json!({
        "question": trace.meta.question,
        "status": trace.meta.status,
        "total_entries": total,
        "entries": entries,
        "next_start": if next_start < total { Some(next_start) } else { None },
    })
}

#[async_trait]
impl Capability for FeedbackList {
    fn name(&self) -> &str {
        "feedback_list"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "feedback_list",
            "List the feedback events filed against one trace. Payloads are caller-supplied and untrusted.",
            json!({
                "type": "object",
                "properties": {
                    "trace_id": { "type": "string", "description": "The trace whose feedback to list." },
                    "max_chars": { "type": "integer", "minimum": 1, "maximum": 20000, "description": "Per-payload cap in characters. Defaults to 2000." }
                },
                "required": ["trace_id"],
                "additionalProperties": false
            }),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap(self.list(args).await)
    }
}

impl FeedbackList {
    async fn list(&self, args: Value) -> Result<Value> {
        let id = checked_trace_id(&args)?;
        let max_chars = args
            .get("max_chars")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_ENTRY_CHARS as u64)
            .clamp(1, MAX_ENTRY_CHARS as u64) as usize;
        // Surface a clear error for a bogus id rather than an empty list.
        self.0.store().head(&id).map_err(|err| anyhow!("{err}"))?;
        let events = self.0.feedback().list(&id).await?;
        let total = events.len();
        let feedback: Vec<Value> = events
            .into_iter()
            .take(DEFAULT_FEEDBACK_LIMIT)
            .map(|event| {
                json!({
                    "created_at_ms": event.created_at_ms,
                    "payload": value_excerpt(&event.payload, max_chars),
                })
            })
            .collect();
        Ok(json!({
            "trace_id": id.as_str(),
            "total": total,
            "feedback": feedback,
            "truncated": total > DEFAULT_FEEDBACK_LIMIT,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use huggr_agent::{Feedback, TraceHeader};
    use huggr_core::{LogEntry, ModelOutput, OpId, Seq, Timestamp};
    use huggr_replay::test_support::TempDir;

    fn fixture_home() -> (TempDir, TraceId) {
        let home = TempDir::new("traces-read");
        let store = TraceStore::new(home.path().join("traces"));
        let log = vec![
            LogEntry::new(
                Seq(0),
                Timestamp(1),
                Record::UserMessage {
                    text: "what is huggr?".to_string(),
                    est_tokens: 4,
                },
            ),
            LogEntry::new(
                Seq(1),
                Timestamp(2),
                Record::ModelOutput {
                    op: OpId(1),
                    output: ModelOutput::new(
                        "a very long answer".repeat(50),
                        None,
                        Vec::new(),
                        "end_turn",
                    ),
                    est_tokens: 10,
                },
            ),
            LogEntry::new(
                Seq(2),
                Timestamp(3),
                Record::ToolResult {
                    op: OpId(2),
                    name: "fs_read".to_string(),
                    call_id: "call-1".to_string(),
                    result: serde_json::json!({ "content": "doc" }),
                    est_tokens: 2,
                },
            ),
            LogEntry::new(
                Seq(3),
                Timestamp(4),
                Record::OpEnded {
                    op: OpId(2),
                    outcome: OpOutcome::Ok,
                    meta: serde_json::from_value(serde_json::json!({
                        "started_at": 2, "ended_at": 4,
                        "model": null, "usage": null, "extra": null
                    }))
                    .unwrap(),
                },
            ),
        ];
        let id = store
            .put(
                Trace::new(Vec::new(), log, Some(1)),
                TraceHeader::new("fixture", "0.0.1", "what is huggr?", "success"),
            )
            .unwrap();
        (home, id)
    }

    fn root(home: &TempDir) -> TracesRoot {
        TracesRoot::new(home.path()).unwrap()
    }

    #[tokio::test]
    async fn trace_list_reports_heads_and_feedback_counts() {
        let (home, id) = fixture_home();
        let feedback = FsFeedbackStore::new(home.path().join("feedback"));
        feedback
            .append(Feedback::new(id.clone(), serde_json::json!({ "score": 1 })))
            .await
            .unwrap();
        let root = root(&home);
        let list = TraceList(root);
        let result = list.list(serde_json::json!({})).await.unwrap();
        assert_eq!(result["total"], 1);
        assert_eq!(result["traces"][0]["trace_id"], id.as_str());
        assert_eq!(result["traces"][0]["feedback_count"], 1);
        assert_eq!(result["traces"][0]["question"], "what is huggr?");
    }

    #[tokio::test]
    async fn trace_ops_summarizes_without_content() {
        let (home, id) = fixture_home();
        let trace = root(&home).store().get(&id).unwrap();
        let summary = ops_summary(&trace);
        assert_eq!(summary["tool_calls"], 1);
        assert_eq!(summary["ops"][0]["name"], "fs_read");
        assert_eq!(summary["ops"][0]["duration_ms"], 2);
        // No raw result content leaks into the op summary.
        assert!(!summary.to_string().contains("doc"));
    }

    #[tokio::test]
    async fn transcript_pages_and_truncates() {
        let (home, id) = fixture_home();
        let trace = root(&home).store().get(&id).unwrap();
        let page = transcript_page(&trace, 0, 2, 40);
        assert_eq!(page["total_entries"], 3);
        assert_eq!(page["entries"][0]["kind"], "user");
        assert_eq!(page["entries"][1]["kind"], "model");
        assert_eq!(page["entries"][1]["text_truncated"], true);
        assert_eq!(page["next_start"], 2);
        let rest = transcript_page(&trace, 2, 10, 40);
        assert_eq!(rest["entries"][0]["kind"], "tool_result");
        assert_eq!(rest["next_start"], serde_json::Value::Null);
    }

    #[test]
    fn crafted_trace_id_is_rejected_before_io() {
        for bad in ["../../etc/passwd", "/abs", "a/b", "", "x\\y", "a.b"] {
            let err = checked_trace_id(&serde_json::json!({ "trace_id": bad })).unwrap_err();
            assert!(
                err.to_string().contains("invalid trace_id"),
                "expected rejection for {bad:?}, got {err}"
            );
        }
        assert!(checked_trace_id(&serde_json::json!({ "trace_id": "abc123-4_x" })).is_ok());
    }

    #[tokio::test]
    async fn feedback_list_errors_on_unknown_trace() {
        let (home, _) = fixture_home();
        let list = FeedbackList(root(&home));
        let err = list
            .list(serde_json::json!({ "trace_id": "deadbeef" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
