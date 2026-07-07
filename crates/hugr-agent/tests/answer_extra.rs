//! Structured answer extras (ROADMAP T3.4).
//!
//! With a declared answer-extra schema, a successful ask whose final message is
//! JSON has that value lifted into `Answer.extra` and validated against the
//! schema. A conforming extra yields no warnings; a violating one surfaces as
//! `Answer.warnings` **without** failing the ask (extra is never load-bearing).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hugr_agent::{Agent, AnswerStatus, Ask, TraceStore};
use hugr_core::{ModelOutput, ModelRequest, ModelSelector, Usage};
use hugr_host::{Clock, ModelAdapter, ModelSink};
use serde_json::json;

struct MockModel {
    replies: Mutex<VecDeque<String>>,
}

impl MockModel {
    fn new<I: IntoIterator<Item = &'static str>>(replies: I) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into_iter().map(String::from).collect()),
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
        let text = self.replies.lock().unwrap().pop_front().unwrap();
        sink.text(text.clone());
        Ok((ModelOutput::text(text), Usage::new(7, 3)))
    }
}

fn clock() -> Clock {
    let counter = Arc::new(AtomicU64::new(1));
    Arc::new(move || counter.fetch_add(1, Ordering::SeqCst))
}

fn agent_with_schema(store: TraceStore, reply: &'static str) -> Agent {
    {
        let mut agent = Agent::new("test-agent", "0.1.0", store);
        agent
            .models
            .push((ModelSelector::named("medium"), MockModel::new([reply])));
        agent.system_prompt = Some("Answer as JSON.".into());
        agent.clock = Some(clock());
        agent.answer_schema = Some(json!({
            "type": "object",
            "required": ["related_documents"],
            "properties": {
                "related_documents": { "type": "array", "items": { "type": "string" } }
            }
        }));
        agent
    }
}

#[tokio::test]
async fn conforming_json_message_is_lifted_into_extra_with_no_warnings() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let reply = r#"{"answer": "See the guide.", "related_documents": ["a.md", "b.md"]}"#;
    let agent = agent_with_schema(store, reply);

    let answer = agent.ask(Ask::new("q")).await.unwrap();

    assert_eq!(answer.status, AnswerStatus::Success);
    assert_eq!(answer.extra["related_documents"], json!(["a.md", "b.md"]));
    assert!(
        answer.warnings.is_empty(),
        "conforming extra → no warnings: {:?}",
        answer.warnings
    );
}

#[tokio::test]
async fn violating_extra_warns_but_does_not_fail() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    // Missing the required `related_documents` field.
    let reply = r#"{"answer": "no citations"}"#;
    let agent = agent_with_schema(store, reply);

    let answer = agent.ask(Ask::new("q")).await.unwrap();

    // The run still succeeds — the schema violation is advisory only.
    assert_eq!(answer.status, AnswerStatus::Success);
    assert_eq!(answer.warnings.len(), 1, "{:?}", answer.warnings);
    assert!(
        answer.warnings[0].contains("related_documents"),
        "{:?}",
        answer.warnings
    );
}

// --- tiny tempdir helper -------------------------------------------------

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
    let path = std::env::temp_dir().join(format!("hugr-agent-extra-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    TempDir { path }
}
