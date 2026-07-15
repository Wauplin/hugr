//! The per-lineage scratchpad end-to-end.
//!
//! Drives the real tokio [`Engine`] through [`Agent::ask`] with a scripted
//! mock model that emits `scratch_write` / `scratch_read` tool calls, so the
//! ungated, jailed scratch capabilities are exercised through the real engine.
//! Asserts the three exit criteria:
//! - a note written in one ask is re-read across a **resumed** ask;
//! - nothing escapes the scratch root (`../` and absolute paths are rejected);
//! - a fork's writes never leak into its sibling (copy-on-fork isolation).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use huggr_agent::{Agent, Ask, TraceId, TraceStore};
use huggr_core::{ModelOutput, ModelRequest, ModelSelector, Record, ToolCall, Usage};
use huggr_host::{Clock, ModelAdapter, ModelSink};
use serde_json::{Value, json};

/// A scripted model: each call pops the next queued [`ModelOutput`]. A tool-call
/// output drives a tool round-trip; a text output ends the turn.
struct MockModel {
    outputs: Mutex<VecDeque<ModelOutput>>,
}

struct BlockAfterTool {
    first: Mutex<Option<ModelOutput>>,
    blocked: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

#[async_trait]
impl ModelAdapter for BlockAfterTool {
    async fn call(
        &self,
        _request: ModelRequest,
        _sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        let first = self.first.lock().unwrap().take();
        if let Some(output) = first {
            return Ok((output, Usage::new(1, 1)));
        }
        if let Some(blocked) = self.blocked.lock().unwrap().take() {
            let _ = blocked.send(());
        }
        std::future::pending().await
    }
}

impl MockModel {
    fn new<I: IntoIterator<Item = ModelOutput>>(outputs: I) -> Arc<Self> {
        Arc::new(Self {
            outputs: Mutex::new(outputs.into_iter().collect()),
        })
    }
}

#[async_trait]
impl ModelAdapter for MockModel {
    async fn call(
        &self,
        _request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        let output = self
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("mock ran out of scripted outputs"))?;
        if !output.text.is_empty() {
            sink.text(output.text.clone());
        }
        Ok((output, Usage::new(1, 1)))
    }
}

fn deterministic_clock() -> Clock {
    let counter = Arc::new(AtomicU64::new(1));
    Arc::new(move || counter.fetch_add(1, Ordering::SeqCst))
}

fn agent(store: TraceStore, outputs: Vec<ModelOutput>) -> Agent {
    {
        let mut agent = Agent::new("scratch-agent", "0.1.0", store);
        agent
            .models
            .push((ModelSelector::named("medium"), MockModel::new(outputs)));
        agent.system_prompt = Some("You take notes.".into());
        agent.clock = Some(deterministic_clock());
        agent
    }
}

fn write_call(id: &str, path: &str, content: &str) -> ToolCall {
    ToolCall::new(
        id,
        "scratch_write",
        json!({ "path": path, "content": content }),
    )
}

fn read_call(id: &str, path: &str) -> ToolCall {
    ToolCall::new(id, "scratch_read", json!({ "path": path }))
}

/// All tool results recorded under `name` in the trace stored at `id`.
fn tool_results(store: &TraceStore, id: &TraceId, name: &str) -> Vec<Value> {
    let trace = store.get(id).unwrap();
    trace
        .log
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::ToolResult {
                name: n, result, ..
            } if n == name => Some(result.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn note_written_in_one_ask_is_reread_across_a_resumed_ask() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = agent(
        store.clone(),
        vec![
            // ask 1: write a note, then answer.
            ModelOutput::tool_calls(vec![write_call("c1", "note.txt", "remember: 42")]),
            ModelOutput::text("saved"),
            // ask 2 (resumed): read the note back, then answer.
            ModelOutput::tool_calls(vec![read_call("c2", "note.txt")]),
            ModelOutput::text("recalled"),
        ],
    );

    let first = agent.ask(Ask::new("take a note")).await.unwrap();
    let second = agent
        .ask(Ask {
            trace_id: Some(first.trace_id.clone()),
            ..Ask::new("what did I note?")
        })
        .await
        .unwrap();

    // The resumed ask's scratch_read returned the note written in the first ask.
    let reads = tool_results(&store, &second.trace_id, "scratch_read");
    assert_eq!(reads.len(), 1, "one scratch_read in the resumed turn");
    assert_eq!(reads[0]["content"], json!("remember: 42"));
}

#[tokio::test]
async fn interrupted_ask_resumes_completed_tool_and_scratch_state() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let (blocked_tx, blocked_rx) = tokio::sync::oneshot::channel();
    let mut interrupted = Agent::new("scratch-agent", "0.1.0", store.clone());
    interrupted.models.push((
        ModelSelector::named("medium"),
        Arc::new(BlockAfterTool {
            first: Mutex::new(Some(ModelOutput::tool_calls(vec![write_call(
                "c1", "note.txt", "survived",
            )]))),
            blocked: Mutex::new(Some(blocked_tx)),
        }),
    ));
    interrupted.clock = Some(deterministic_clock());

    let task = tokio::spawn(async move { interrupted.ask(Ask::new("write then wait")).await });
    blocked_rx.await.expect("second model call started");
    task.abort();
    let _ = task.await;

    let heads = store.list().unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].status, "interrupted");
    let checkpoint = heads[0].trace_id.clone();
    let trace = store.get(&checkpoint).unwrap();
    assert_eq!(
        trace
            .log
            .iter()
            .filter(|entry| matches!(&entry.record, Record::ToolResult { name, .. } if name == "scratch_write"))
            .count(),
        1
    );
    huggr_replay::verify(&trace).expect("live checkpoint replays");

    let resumed = agent(
        store.clone(),
        vec![
            ModelOutput::tool_calls(vec![read_call("c2", "note.txt")]),
            ModelOutput::text("recovered"),
        ],
    );
    let answer = resumed
        .ask(Ask {
            trace_id: Some(checkpoint),
            ..Ask::new("continue")
        })
        .await
        .unwrap();

    let reads = tool_results(&store, &answer.trace_id, "scratch_read");
    assert_eq!(reads[0]["content"], json!("survived"));
    assert_eq!(
        tool_results(&store, &answer.trace_id, "scratch_write").len(),
        1,
        "the completed tool step was not repeated"
    );
}

#[tokio::test]
async fn nothing_escapes_the_scratch_root() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = agent(
        store.clone(),
        vec![
            ModelOutput::tool_calls(vec![
                write_call("c1", "../escape.txt", "pwn"),
                write_call("c2", "/tmp/huggr-escape.txt", "pwn"),
            ]),
            ModelOutput::text("done"),
        ],
    );

    let answer = agent.ask(Ask::new("try to escape")).await.unwrap();

    // Both escape attempts came back as tool-level errors (semantic results),
    // not writes.
    let writes = tool_results(&store, &answer.trace_id, "scratch_write");
    assert_eq!(writes.len(), 2, "both escape attempts recorded a result");
    for result in &writes {
        assert!(
            result.get("error").is_some(),
            "escape attempt must be rejected: {result}"
        );
    }

    // And nothing was actually written outside the jail.
    assert!(!dir.path().join("escape.txt").exists());
    assert!(!std::path::Path::new("/tmp/huggr-escape.txt").exists());
}

#[tokio::test]
async fn fork_writes_do_not_leak_into_the_sibling() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = agent(
        store.clone(),
        vec![
            // parent: seed an ancestor note.
            ModelOutput::tool_calls(vec![write_call("p", "parent.txt", "P")]),
            ModelOutput::text("ok"),
            // fork A: overwrite shared.txt with A's content.
            ModelOutput::tool_calls(vec![write_call("a", "shared.txt", "from-A")]),
            ModelOutput::text("ok"),
            // fork B: overwrite shared.txt with B's content.
            ModelOutput::tool_calls(vec![write_call("b", "shared.txt", "from-B")]),
            ModelOutput::text("ok"),
            // resume A: read shared.txt + the ancestor note.
            ModelOutput::tool_calls(vec![
                read_call("ra1", "shared.txt"),
                read_call("ra2", "parent.txt"),
            ]),
            ModelOutput::text("ok"),
            // resume B: read shared.txt.
            ModelOutput::tool_calls(vec![read_call("rb1", "shared.txt")]),
            ModelOutput::text("ok"),
        ],
    );

    let parent = agent.ask(Ask::new("start")).await.unwrap();
    // Fork the same parent twice → two independent copy-on-fork subtrees.
    let fork_a = agent
        .ask(Ask {
            trace_id: Some(parent.trace_id.clone()),
            ..Ask::new("branch A")
        })
        .await
        .unwrap();
    let fork_b = agent
        .ask(Ask {
            trace_id: Some(parent.trace_id.clone()),
            ..Ask::new("branch B")
        })
        .await
        .unwrap();

    let read_a = agent
        .ask(Ask {
            trace_id: Some(fork_a.trace_id.clone()),
            ..Ask::new("recall A")
        })
        .await
        .unwrap();
    let read_b = agent
        .ask(Ask {
            trace_id: Some(fork_b.trace_id.clone()),
            ..Ask::new("recall B")
        })
        .await
        .unwrap();

    // Each branch sees only its own write to shared.txt — the sibling's write
    // never leaked in through copy-on-fork isolation.
    let reads_a = tool_results(&store, &read_a.trace_id, "scratch_read");
    assert_eq!(reads_a[0]["content"], json!("from-A"));
    // And the ancestor's note is visible across the fork.
    assert_eq!(reads_a[1]["content"], json!("P"));

    let reads_b = tool_results(&store, &read_b.trace_id, "scratch_read");
    assert_eq!(reads_b[0]["content"], json!("from-B"));
}

struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn tempdir() -> TempDir {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!("huggr-scratch-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    TempDir { path }
}
