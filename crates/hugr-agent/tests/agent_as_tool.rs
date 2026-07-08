//! Agents as tools (ARCHITECTURE §20.5, ROADMAP T3.8).
//!
//! A parent agent grants a child agent as an ordinary `agent_child` tool. The
//! parent's model delegates a sub-question; the child answers; and we assert:
//! - the child's `Answer` is the tool result and the parent reaches its own
//!   final answer;
//! - the child's cost/tokens/calls fold into the parent's `AnswerMeta` (§18.4);
//! - a follow-up via the child's returned `trace_id` resumes the child thread;
//! - the recorded parent trace replays bit-for-bit (`verify()`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hugr_agent::{
    Agent, AgentToolResolver, AgentToolSpec, Ask, Pricing, STATUS_SUCCESS, TraceStore,
};
use hugr_core::{ModelOutput, ModelRequest, ModelSelector, ToolCall, Usage};
use hugr_host::{Clock, ModelAdapter, ModelSink};
use serde_json::{Value, json};

/// A scripted model returning queued outputs with a fixed per-call usage.
struct MockModel {
    outputs: Mutex<VecDeque<ModelOutput>>,
    usage: Usage,
}

impl MockModel {
    fn new(outputs: Vec<ModelOutput>, usage: Usage) -> Arc<Self> {
        Arc::new(Self {
            outputs: Mutex::new(outputs.into()),
            usage,
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
        Ok((output, self.usage.clone()))
    }
}

fn clock() -> Clock {
    let counter = Arc::new(AtomicU64::new(1));
    Arc::new(move || counter.fetch_add(1, Ordering::SeqCst))
}

#[tokio::test]
async fn parent_delegates_to_child_and_folds_cost() {
    let parent_dir = tempdir();
    let child_dir = tempdir();

    // Child: one text answer, usage 5 in / 2 out. Priced medium 2/5 →
    // 5*2 + 2*5 = 20 microUSD.
    let child = Arc::new({
        let mut agent = Agent::new("child", "0.1.0", TraceStore::new(child_dir.path()));
        agent.models.push((
            ModelSelector::named("medium"),
            MockModel::new(
                vec![
                    ModelOutput::text("child says hi"),
                    ModelOutput::text("child follow-up"),
                ],
                Usage::new(5, 2),
            ),
        ));
        agent.system_prompt = Some("child".into());
        agent.clock = Some(clock());
        agent.pricing = Pricing::new().with_tier("medium", 2.0, 5.0);
        agent
    });

    // Track child trace ids the resolver produces (for the follow-up assertion).
    let child_ids = Arc::new(Mutex::new(Vec::new()));
    let resolver: AgentToolResolver = {
        let child = child.clone();
        let ids = child_ids.clone();
        Arc::new(move |ask: Ask| {
            let child = child.clone();
            let ids = ids.clone();
            Box::pin(async move {
                let answer = child.ask(ask).await.map_err(|e| e.to_string())?;
                ids.lock().unwrap().push(answer.trace_id.clone());
                Ok(answer)
            })
        })
    };

    // Parent: first turn calls agent_child, second turn answers. Usage 7/3 per
    // call, priced medium 2/5 → 2 * (7*2 + 3*5) = 58 microUSD across 2 calls.
    let parent = {
        let mut agent = Agent::new("parent", "0.1.0", TraceStore::new(parent_dir.path()));
        agent.models.push((
            ModelSelector::named("medium"),
            MockModel::new(
                vec![
                    ModelOutput::tool_calls(vec![ToolCall::new(
                        "c1",
                        "agent_child",
                        json!({ "question": "sub-question" }),
                    )]),
                    ModelOutput::text("parent done"),
                ],
                Usage::new(7, 3),
            ),
        ));
        agent.system_prompt = Some("parent".into());
        agent.clock = Some(clock());
        agent.pricing = Pricing::new().with_tier("medium", 2.0, 5.0);
        agent.agent_tools.push(AgentToolSpec::new(
            "agent_child",
            "answers child questions",
            resolver,
        ));
        agent
    };

    let answer = parent.ask(Ask::new("delegate please")).await.unwrap();

    assert_eq!(answer.status, STATUS_SUCCESS);
    assert_eq!(text_response(&answer.response), "parent done");

    // Cost folds: parent 58 + child 20 = 78; tokens 14+5 in, 6+2 out; model
    // calls 2 (parent) + 1 (child) = 3; the child was actually invoked once.
    assert_eq!(
        answer.metadata.cost_micro_usd, 78,
        "child cost must fold in"
    );
    assert_eq!(answer.metadata.tokens_in, 19);
    assert_eq!(answer.metadata.tokens_out, 8);
    assert_eq!(answer.metadata.model_calls, 3);
    // The single merged `medium` tier line carries both parent and child spend.

    // A child trace was produced; a follow-up via its trace_id resumes it.
    let child_id = child_ids.lock().unwrap()[0].clone();
    let follow_up = child
        .ask(Ask {
            trace_id: Some(child_id.clone()),
            ..Ask::new("and more?")
        })
        .await
        .unwrap();
    assert_eq!(follow_up.status, STATUS_SUCCESS);
    assert_eq!(text_response(&follow_up.response), "child follow-up");
    assert_ne!(
        follow_up.trace_id, child_id,
        "resume writes a new child trace"
    );
    assert_eq!(
        child.store().head(&follow_up.trace_id).unwrap().depends_on,
        Some(child_id),
        "the follow-up depends on the child trace the parent delegated to"
    );

    // The recorded parent trace replays bit-for-bit (the child is not re-run —
    // its Answer is a recorded tool result).
    let parent_trace = parent.store().get(&answer.trace_id).unwrap();
    hugr_replay::verify(&parent_trace).unwrap();
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
    let path =
        std::env::temp_dir().join(format!("hugr-agent-agenttool-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    TempDir { path }
}

fn text_response(response: &Value) -> &str {
    response["text"].as_str().unwrap()
}
