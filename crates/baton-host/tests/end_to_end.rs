//! End-to-end Phase 1 test: a genuine multi-turn session driven through the
//! real tokio [`Engine`] loop, using a scripted mock model adapter and the
//! *real* shell capability. This exercises the whole path
//! (user → model → tool → model → done) without a network or API key.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use baton_core::{
    DoneReason, ModelOutput, ModelRequest, ModelSelector, OutputEvent, Record, ToolCall, Usage,
};
use baton_host::capabilities::Shell;
use baton_host::policy::DenyAll;
use baton_host::{Engine, Frontend, ModelAdapter, ModelSink, Policy};
use serde_json::json;

/// A scripted model: each `call` pops the next queued output and records the
/// request it was given (so tests can assert on the projection).
struct MockModel {
    responses: Mutex<VecDeque<ModelOutput>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl MockModel {
    fn new(responses: impl IntoIterator<Item = ModelOutput>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ModelAdapter for MockModel {
    async fn call(
        &self,
        request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        self.requests.lock().unwrap().push(request);
        let output = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("mock ran out of scripted responses"))?;
        if !output.text.is_empty() {
            sink.text(output.text.clone()); // stream it, like a real adapter
        }
        Ok((output, Usage::new(1, 1)))
    }
}

/// A front-end that captures streamed assistant text for assertions.
#[derive(Clone, Default)]
struct Capture {
    text: Arc<Mutex<String>>,
    done: Arc<Mutex<Vec<DoneReason>>>,
}

impl Frontend for Capture {
    fn on_output(&mut self, event: &OutputEvent) {
        if let OutputEvent::ModelText { text, .. } = event {
            self.text.lock().unwrap().push_str(text);
        }
    }
    fn on_notice(&mut self, _message: &str) {}
    fn on_done(&mut self, reason: &DoneReason) {
        self.done.lock().unwrap().push(reason.clone());
    }
}

fn deterministic_clock() -> baton_host::Clock {
    let counter = Arc::new(AtomicU64::new(1));
    Arc::new(move || counter.fetch_add(1, Ordering::SeqCst))
}

fn count_tool_results(log: &[baton_core::LogEntry]) -> Vec<(String, serde_json::Value)> {
    log.iter()
        .filter_map(|e| match &e.record {
            Record::ToolResult { name, result, .. } => Some((name.clone(), result.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn multi_turn_session_with_real_shell() {
    let capture = Capture::default();

    // Turn 1 needs two model calls (tool, then final). Turn 2 needs one.
    let model = MockModel::new([
        ModelOutput::tool_calls(vec![ToolCall::new(
            "call-1",
            "shell",
            json!({ "cmd": "echo hello-from-baton" }),
        )]),
        ModelOutput::text("The shell printed the greeting."),
        ModelOutput::text("Anything else?"),
    ]);

    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), model.clone())
        .capability(Arc::new(Shell))
        .system_prompt("You are a test agent.")
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .build();

    // Turn 1: model calls shell, then answers.
    engine.user_turn("greet me using the shell".into()).await;
    // Turn 2: a follow-up, proving the session is multi-turn.
    engine.user_turn("thanks".into()).await;

    // The real shell ran and its stdout flowed back as a tool result.
    let tool_results = count_tool_results(engine.brain().state().log());
    assert_eq!(tool_results.len(), 1, "expected exactly one tool result");
    let (name, result) = &tool_results[0];
    assert_eq!(name, "shell");
    assert!(
        result["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("hello-from-baton"),
        "shell stdout not captured: {result}"
    );

    // Both turns reached EndTurn.
    let dones = capture.done.lock().unwrap();
    assert_eq!(dones.len(), 2);
    assert!(dones.iter().all(|d| matches!(d, DoneReason::EndTurn)));

    // The streamed assistant text from all model calls was rendered.
    let text = capture.text.lock().unwrap().clone();
    assert!(text.contains("The shell printed the greeting."));
    assert!(text.contains("Anything else?"));

    // The mock saw the system prompt and the advertised shell tool in its
    // projected request (proving builder → StaticPolicy wiring).
    let first_request = &model.requests.lock().unwrap()[0];
    assert_eq!(first_request.tools.len(), 1);
    assert_eq!(first_request.tools[0].name, "shell");
    assert!(matches!(
        first_request.blocks.first().map(|b| b.role),
        Some(baton_core::Role::System)
    ));
}

#[tokio::test]
async fn denied_permission_routes_error_back_to_model() {
    let capture = Capture::default();

    let model = MockModel::new([
        // Model wants to run a (sensitive) shell command...
        ModelOutput::tool_calls(vec![ToolCall::new(
            "call-1",
            "shell",
            json!({ "cmd": "rm -rf /" }),
        )]),
        // ...but after the denial comes back, it gives a safe final answer.
        ModelOutput::text("Okay, I won't run that."),
    ]);

    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), model)
        .capability(Arc::new(Shell))
        .policy(Arc::new(DenyAll) as Arc<dyn Policy>)
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .build();

    engine.user_turn("delete everything".into()).await;

    // The tool never ran for real; the denial was fed back as a tool result.
    let tool_results = count_tool_results(engine.brain().state().log());
    assert_eq!(tool_results.len(), 1);
    let (_, result) = &tool_results[0];
    assert_eq!(result["error"], json!("permission_denied"));

    let text = capture.text.lock().unwrap().clone();
    assert!(text.contains("Okay, I won't run that."));
}
