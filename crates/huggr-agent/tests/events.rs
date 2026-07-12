use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use huggr_agent::{Agent, AgentEvent, Ask, TraceStore};
use huggr_core::{ModelOutput, ModelRequest, ModelSelector, ToolCall, Usage};
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

#[tokio::test]
async fn ask_events_streams_lifecycle_and_final_answer() {
    let dir = tempdir();
    let mut agent = Agent::new("event-agent", "0.1.0", TraceStore::new(dir.path()));
    agent.models.push((
        ModelSelector::named("medium"),
        MockModel::new(vec![
            ModelOutput::tool_calls(vec![ToolCall::new(
                "c1",
                "scratch_write",
                json!({ "path": "note.txt", "content": "hello" }),
            )]),
            ModelOutput::text("done"),
        ]),
    ));
    agent.clock = Some(deterministic_clock());

    let (mut events, handle) = agent.ask_events(Ask::new("write a note"));
    let mut seen = Vec::new();
    while let Some(event) = events.recv().await {
        seen.push(event);
    }
    let answer = handle.await.unwrap().unwrap();

    assert!(matches!(seen.first(), Some(AgentEvent::AskStarted { .. })));
    assert!(seen.iter().any(|event| matches!(
        event,
        AgentEvent::ModelStarted { tier, .. } if tier == "medium"
    )));
    assert!(seen.iter().any(|event| matches!(
        event,
        AgentEvent::ToolStarted { name, .. } if name == "scratch_write"
    )));
    assert!(seen.iter().any(|event| matches!(
        event,
        AgentEvent::ToolEnded { name, is_error: false, .. } if name == "scratch_write"
    )));
    assert!(seen.iter().any(|event| matches!(
        event,
        AgentEvent::TextDelta { text, .. } if text == "done"
    )));
    assert!(
        seen.iter()
            .any(|event| matches!(event, AgentEvent::Done { .. }))
    );
    let ready = seen
        .iter()
        .find_map(|event| match event {
            AgentEvent::AnswerReady { answer } => Some(answer),
            _ => None,
        })
        .expect("stream includes final answer event");
    assert_eq!(ready.trace_id, answer.trace_id);
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
        "huggr-agent-events-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    TempDir { path }
}
