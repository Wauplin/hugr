//! Limits enforcement end-to-end (ROADMAP T3.1).
//!
//! Drives the real tokio [`Engine`] through [`Agent::ask`] with a scripted
//! mock model and a no-op tool, so each declared limit is exercised through the
//! real engine and asserts the exit criteria:
//! - each limit (`max_model_calls`, `max_turns`, `max_cost_micro_usd`,
//!   `timeout_ms`) triggers cleanly as an error answer with a typed
//!   `Answer.extra` reason and a persisted `trace_id`;
//! - the partial trace still replays bit-for-bit (`hugr_replay::verify`);
//! - with no limits set, behavior is unchanged (a normal success answer).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use hugr_agent::{Agent, AgentLimits, AnswerStatus, Ask, Pricing, TraceStore};
use hugr_core::{ModelOutput, ModelRequest, ModelSelector, ToolCall, ToolSchema, Usage};
use hugr_host::{Capability, ChunkSink, Clock, ModelAdapter, ModelSink};
use serde_json::{Value, json};

/// A model that never stops on its own: every call requests the `noop` tool, so
/// the turn loops until a limit refuses a call. Usage is a fixed 7 in / 3 out
/// per call, so cost is predictable under a known price sheet.
struct LoopingModel {
    calls: AtomicU64,
}

impl LoopingModel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicU64::new(0),
        })
    }
}

#[async_trait]
impl ModelAdapter for LoopingModel {
    async fn call(
        &self,
        _request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let id = format!("call-{n}");
        sink.tool_call_start(id.clone(), "noop");
        sink.tool_call_end(id.clone());
        let output = ModelOutput::tool_calls(vec![ToolCall::new(id, "noop", json!({}))]);
        Ok((output, Usage::new(7, 3)))
    }
}

/// A model that answers with plain text after a `tokio` sleep long enough to
/// blow a short wall-clock timeout.
struct SlowModel;

#[async_trait]
impl ModelAdapter for SlowModel {
    async fn call(
        &self,
        _request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        sink.text("late answer");
        Ok((ModelOutput::text("late answer"), Usage::new(7, 3)))
    }
}

/// A model that answers immediately with plain text (no tools).
struct FastModel;

#[async_trait]
impl ModelAdapter for FastModel {
    async fn call(
        &self,
        _request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        sink.text("done");
        Ok((ModelOutput::text("done"), Usage::new(7, 3)))
    }
}

/// A non-gated no-op tool that lets the model loop indefinitely.
struct NoopTool;

#[async_trait]
impl Capability for NoopTool {
    fn name(&self) -> &str {
        "noop"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new("noop", "does nothing", json!({ "type": "object" }))
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, _args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        Ok(json!({ "ok": true }))
    }
}

fn deterministic_clock() -> Clock {
    let counter = Arc::new(AtomicU64::new(1));
    Arc::new(move || counter.fetch_add(1, Ordering::SeqCst))
}

fn looping_agent(store: TraceStore, limits: AgentLimits) -> Agent {
    Agent::builder("limit-agent", "0.1.0", store)
        .model(ModelSelector::named("medium"), LoopingModel::new())
        .capability(Arc::new(NoopTool))
        .system_prompt("You loop.")
        .clock(deterministic_clock())
        .pricing(Pricing::new().with_tier("medium", 2.0, 5.0))
        .limits(limits)
        .build()
}

/// The `{limit, value}` object placed on `Answer.extra` by a limit trip.
fn limit_reason(answer: &hugr_agent::Answer) -> (String, u64) {
    let obj = answer
        .extra
        .get("limit_exceeded")
        .expect("limit trip sets extra.limit_exceeded");
    (
        obj["limit"].as_str().unwrap().to_string(),
        obj["value"].as_u64().unwrap(),
    )
}

#[tokio::test]
async fn max_model_calls_trips_cleanly_and_the_partial_trace_replays() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = looping_agent(store.clone(), AgentLimits::new().with_max_model_calls(2));

    let answer = agent.ask(Ask::new("go")).await.unwrap();

    assert_eq!(answer.status, AnswerStatus::Error);
    assert_eq!(limit_reason(&answer), ("max_model_calls".to_string(), 2));
    // Exactly two model calls were billed before the third was refused.
    assert_eq!(answer.metadata.model_calls, 2);

    let head = store.head(&answer.trace_id).unwrap();
    assert_eq!(head.status, "error");
    hugr_replay::verify(&store.get(&answer.trace_id).unwrap()).unwrap();
}

#[tokio::test]
async fn max_turns_trips_cleanly() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = looping_agent(store.clone(), AgentLimits::new().with_max_turns(1));

    let answer = agent.ask(Ask::new("go")).await.unwrap();

    assert_eq!(answer.status, AnswerStatus::Error);
    assert_eq!(limit_reason(&answer), ("max_turns".to_string(), 1));
    assert_eq!(answer.metadata.model_calls, 1);
    hugr_replay::verify(&store.get(&answer.trace_id).unwrap()).unwrap();
}

#[tokio::test]
async fn max_cost_trips_after_the_running_total_crosses_the_bound() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    // Each call costs 7*2 + 3*5 = 29 micro-USD. With a 40 bound: call 1 runs
    // (total 29 < 40), call 2 runs (total 58), call 3 is refused (58 >= 40).
    let agent = looping_agent(store.clone(), AgentLimits::new().with_max_cost_micro_usd(40));

    let answer = agent.ask(Ask::new("go")).await.unwrap();

    assert_eq!(answer.status, AnswerStatus::Error);
    assert_eq!(limit_reason(&answer), ("max_cost_micro_usd".to_string(), 40));
    assert_eq!(answer.metadata.model_calls, 2);
    assert_eq!(answer.metadata.cost_micro_usd, 58);
    hugr_replay::verify(&store.get(&answer.trace_id).unwrap()).unwrap();
}

#[tokio::test]
async fn timeout_trips_and_persists_a_replayable_partial_trace() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = Agent::builder("slow-agent", "0.1.0", store.clone())
        .model(ModelSelector::named("medium"), Arc::new(SlowModel))
        .system_prompt("You are slow.")
        .clock(deterministic_clock())
        .limits(AgentLimits::new().with_timeout_ms(50))
        .build();

    let answer = agent.ask(Ask::new("go")).await.unwrap();

    assert_eq!(answer.status, AnswerStatus::Error);
    assert_eq!(limit_reason(&answer), ("timeout_ms".to_string(), 50));

    let head = store.head(&answer.trace_id).unwrap();
    assert_eq!(head.status, "error");
    // The partial trace (a model call still in flight when the timeout fired)
    // replays bit-for-bit.
    hugr_replay::verify(&store.get(&answer.trace_id).unwrap()).unwrap();
}

#[tokio::test]
async fn no_limits_leaves_behavior_unchanged() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = Agent::builder("plain-agent", "0.1.0", store.clone())
        .model(ModelSelector::named("medium"), Arc::new(FastModel))
        .system_prompt("You answer.")
        .clock(deterministic_clock())
        .build();

    let answer = agent.ask(Ask::new("go")).await.unwrap();

    assert_eq!(answer.status, AnswerStatus::Success);
    assert_eq!(answer.message, "done");
    assert!(answer.extra.is_null(), "no limit trip → no extra reason");
    assert_eq!(answer.metadata.model_calls, 1);
    hugr_replay::verify(&store.get(&answer.trace_id).unwrap()).unwrap();
}

// --- tiny tempdir helper (no external dev-dep) ---------------------------

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
    let path = std::env::temp_dir().join(format!("hugr-agent-limits-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    TempDir { path }
}
