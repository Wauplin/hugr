use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use huggr_agent::{Agent, Ask, FeedbackError, FsFeedbackStore, TraceId, TraceStore};
use huggr_core::{ModelOutput, ModelRequest, ModelSelector, Usage};
use huggr_host::{ModelAdapter, ModelSink};
use serde_json::json;

struct MockModel {
    outputs: Mutex<VecDeque<ModelOutput>>,
}

impl MockModel {
    fn new(outputs: Vec<ModelOutput>) -> Arc<Self> {
        Arc::new(Self {
            outputs: Mutex::new(outputs.into()),
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
            .ok_or_else(|| anyhow::anyhow!("mock ran out of outputs"))?;
        if !output.text.is_empty() {
            sink.text(output.text.clone());
        }
        Ok((output, Usage::new(1, 1)))
    }
}

#[tokio::test]
async fn feedback_is_append_only_sidecar_keyed_to_trace() {
    let dir = tempdir();
    let feedback_dir = dir.path().join("feedback");
    let mut agent = Agent::new("feedback-agent", "0.1.0", TraceStore::new(dir.path()));
    agent.set_feedback_store(FsFeedbackStore::new(&feedback_dir));
    agent.models.push((
        ModelSelector::named("medium"),
        MockModel::new(vec![ModelOutput::text("done")]),
    ));
    agent.system_prompt = Some("answer".into());

    let answer = agent.ask(Ask::new("question")).await.unwrap();
    let first = agent
        .feedback(answer.trace_id.clone(), json!({ "score": 1 }))
        .await
        .unwrap();
    let second = agent
        .feedback(answer.trace_id.clone(), json!({ "note": "useful" }))
        .await
        .unwrap();

    let entries = agent.feedback_for(&answer.trace_id).await.unwrap();
    assert_eq!(entries, vec![first, second]);
    assert_eq!(
        std::fs::read_to_string(feedback_dir.join(format!("{}.jsonl", answer.trace_id)))
            .unwrap()
            .lines()
            .count(),
        2
    );
    let trace = agent.trace_backend().get(&answer.trace_id).await.unwrap();
    assert!(
        !serde_json::to_string(&trace).unwrap().contains("useful"),
        "feedback is not embedded in immutable traces"
    );

    let err = agent
        .feedback(TraceId::new("missing-trace"), json!({ "score": 0 }))
        .await
        .unwrap_err();
    assert!(matches!(err, FeedbackError::UnknownTrace(id) if id.as_str() == "missing-trace"));
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
    let path = std::env::temp_dir().join(format!(
        "huggr-agent-feedback-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    TempDir { path }
}
