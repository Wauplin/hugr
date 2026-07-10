//! Agent-wide memory tools.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hugr_agent::{Agent, Ask, FsMemory, TraceId, TraceStore};
use hugr_core::{ModelOutput, ModelRequest, ModelSelector, Record, ToolCall, Usage};
use hugr_host::{Clock, ModelAdapter, ModelSink};
use serde_json::{Value, json};

struct MockModel {
    outputs: Mutex<VecDeque<ModelOutput>>,
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

fn agent(
    root: &std::path::Path,
    store: TraceStore,
    outputs: Vec<ModelOutput>,
    readonly: bool,
) -> Agent {
    let mut agent = Agent::new("memory-agent", "0.1.0", store);
    agent
        .models
        .push((ModelSelector::named("medium"), MockModel::new(outputs)));
    agent.system_prompt = Some("Use memory when helpful.".into());
    agent.clock = Some(deterministic_clock());
    agent
        .capabilities
        .extend(FsMemory::new(root.join("memory"), readonly).capabilities());
    agent
}

fn write_call(id: &str, path: &str, content: &str) -> ToolCall {
    ToolCall::new(
        id,
        "memory_write",
        json!({ "path": path, "content": content }),
    )
}

fn read_call(id: &str, path: &str) -> ToolCall {
    ToolCall::new(id, "memory_read", json!({ "path": path }))
}

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
async fn memory_is_shared_across_fresh_asks() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path().join("traces"));
    let agent = agent(
        dir.path(),
        store.clone(),
        vec![
            ModelOutput::tool_calls(vec![write_call("w", "notes/fact.txt", "remember: 42")]),
            ModelOutput::text("saved"),
            ModelOutput::tool_calls(vec![read_call("r", "notes/fact.txt")]),
            ModelOutput::text("recalled"),
        ],
        false,
    );

    let first = agent.ask(Ask::new("remember this")).await.unwrap();
    let second = agent.ask(Ask::new("recall it")).await.unwrap();

    assert_ne!(first.trace_id, second.trace_id);
    let reads = tool_results(&store, &second.trace_id, "memory_read");
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0]["content"], json!("remember: 42"));
}

#[tokio::test]
async fn memory_rejects_escape_paths() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path().join("traces"));
    let agent = agent(
        dir.path(),
        store.clone(),
        vec![
            ModelOutput::tool_calls(vec![write_call("w", "../escape.txt", "nope")]),
            ModelOutput::text("done"),
        ],
        false,
    );

    let answer = agent.ask(Ask::new("try escape")).await.unwrap();
    let writes = tool_results(&store, &answer.trace_id, "memory_write");
    assert_eq!(writes.len(), 1);
    assert!(writes[0].get("error").is_some(), "{writes:?}");
    assert!(!dir.path().join("escape.txt").exists());
}

#[tokio::test]
async fn readonly_memory_rejects_writes() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path().join("traces"));
    let agent = agent(
        dir.path(),
        store.clone(),
        vec![
            ModelOutput::tool_calls(vec![write_call("w", "note.txt", "nope")]),
            ModelOutput::text("done"),
        ],
        true,
    );

    let answer = agent.ask(Ask::new("try write")).await.unwrap();
    let writes = tool_results(&store, &answer.trace_id, "memory_write");
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["error"], json!("memory is readonly"));
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
    let path = std::env::temp_dir().join(format!("hugr-memory-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    TempDir { path }
}
