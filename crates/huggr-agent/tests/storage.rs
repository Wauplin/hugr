use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use huggr_agent::{
    Agent, Ask, BlobRef, MemBlobStore, MemScratch, MemTraceStore, STATUS_SUCCESS, StorageOverrides,
};
use huggr_core::{ModelOutput, ModelRequest, ModelSelector, Record, ToolCall, Usage};
use huggr_host::{Clock, ModelAdapter, ModelSink};
use serde_json::json;

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

#[tokio::test]
async fn memory_storage_runs_resume_and_blobs_without_framework_files() {
    let dir = tempdir();
    let trace_store = Arc::new(MemTraceStore::new());
    let blob_store = Arc::new(MemBlobStore::new());
    let scratch = Arc::new(MemScratch::new());
    let mut agent = Agent::with_storage(
        "mem-agent",
        "0.1.0",
        StorageOverrides::new(trace_store.clone(), blob_store.clone(), scratch),
    );
    agent.models.push((
        ModelSelector::named("medium"),
        MockModel::new(vec![
            ModelOutput::tool_calls(vec![
                write_call("c1", "note.txt", "remember: mem"),
                write_call("c2", "out/report.txt", "from memory storage"),
            ]),
            ModelOutput::text("saved"),
            ModelOutput::tool_calls(vec![read_call("c3", "note.txt")]),
            ModelOutput::text("recalled"),
        ]),
    ));
    agent.system_prompt = Some("Use scratch.".into());
    agent.clock = Some(deterministic_clock());

    let first = agent.ask(Ask::new("start")).await.unwrap();
    assert_eq!(first.status, STATUS_SUCCESS);
    assert_eq!(first.blobs.len(), 1);
    let BlobRef::Sha256 { sha256 } = &first.blobs[0].blob_ref else {
        panic!("outbound blob must be sha256");
    };
    assert_eq!(
        agent.blob_backend().get(sha256).await.unwrap(),
        b"from memory storage"
    );

    let second = agent
        .ask(Ask {
            trace_id: Some(first.trace_id.clone()),
            ..Ask::new("resume")
        })
        .await
        .unwrap();
    let trace = agent.trace_backend().get(&second.trace_id).await.unwrap();
    let reads: Vec<_> = trace
        .log
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::ToolResult { name, result, .. } if name == "scratch_read" => {
                Some(result.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(reads[0]["content"], json!("remember: mem"));

    let heads = agent.traces().await.unwrap();
    assert_eq!(heads.len(), 2);
    assert!(
        heads.iter().any(|head| head.trace_id == second.trace_id
            && head.depends_on == Some(first.trace_id.clone()))
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
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
    let mut path = std::env::temp_dir();
    path.push(format!(
        "huggr-agent-storage-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    TempDir { path }
}
