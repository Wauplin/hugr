//! End-to-end tests: genuine multi-turn sessions driven through the real tokio
//! [`Engine`] loop, using a scripted mock model adapter and a test capability.
//! This exercises the whole path (user → model → tool → model → done) without
//! a network or API key.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use huggr_core::{
    BrainState, BudgetPolicy, ContentPart, ContextPlan, DoneReason, LogEntry, ModelOutput,
    ModelRequest, ModelSelector, OpOutcome, OutputEvent, PolicyRegistry, Record, Role,
    StaticPolicy, ToolCall, ToolSchema, TurnPolicy, Usage, Value,
};
use huggr_host::mcp::{self, McpServerConfig};
use huggr_host::{Capability, ChunkSink, Engine, Frontend, ModelAdapter, ModelSink};
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

/// A simple in-process test tool: echoes back the `message` argument.
struct Echo;

#[async_trait]
impl Capability for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "echo",
            "Echo a message.",
            json!({ "type": "object", "properties": { "message": { "type": "string" } } }),
        )
    }
    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        Ok(json!({ "stdout": args["message"].as_str().unwrap_or_default() }))
    }
}

/// A front-end that captures streamed assistant text for assertions.
#[derive(Clone, Default)]
struct Capture {
    text: Arc<Mutex<String>>,
    done: Arc<Mutex<Vec<DoneReason>>>,
    /// Token usage observed at each model-call end.
    model_usage: Arc<Mutex<Vec<Usage>>>,
    /// Tool names observed at each tool-call end.
    tool_ends: Arc<Mutex<Vec<String>>>,
    /// Number of times the session-end hook fired.
    session_ends: Arc<Mutex<u32>>,
}

impl Frontend for Capture {
    fn on_output(&mut self, event: &OutputEvent) {
        if let OutputEvent::ModelText { text, .. } = event {
            self.text.lock().unwrap().push_str(text);
        }
    }
    fn on_model_end(&mut self, _op: huggr_core::OpId, usage: &Usage) {
        self.model_usage.lock().unwrap().push(usage.clone());
    }
    fn on_tool_end(
        &mut self,
        _op: huggr_core::OpId,
        name: &str,
        _result: &serde_json::Value,
        _is_error: bool,
    ) {
        self.tool_ends.lock().unwrap().push(name.to_string());
    }
    fn on_done(&mut self, reason: &DoneReason) {
        self.done.lock().unwrap().push(reason.clone());
    }
    fn on_session_end(&mut self) {
        *self.session_ends.lock().unwrap() += 1;
    }
}

fn deterministic_clock() -> huggr_host::Clock {
    let counter = Arc::new(AtomicU64::new(1));
    Arc::new(move || counter.fetch_add(1, Ordering::SeqCst))
}

struct CustomSelectorPolicy {
    selector: ModelSelector,
    projection: StaticPolicy,
}

impl CustomSelectorPolicy {
    fn new(selector: impl Into<String>) -> Self {
        Self {
            selector: ModelSelector::named(selector.into()),
            projection: StaticPolicy::default(),
        }
    }
}

impl TurnPolicy for CustomSelectorPolicy {
    fn choose_model(&self, _state: &BrainState) -> ModelSelector {
        self.selector.clone()
    }

    fn project_context(&self, log: &[LogEntry], budget: huggr_core::TokenBudget) -> ContextPlan {
        self.projection.project_context(log, budget)
    }

    fn needs_permission(&self, _capability: &str) -> bool {
        false
    }
}

fn decode_custom_policy(value: &Value) -> Option<Box<dyn TurnPolicy>> {
    let selector = value.get("model")?.as_str()?;
    Some(Box::new(CustomSelectorPolicy::new(selector)))
}

fn count_tool_results(log: &[huggr_core::LogEntry]) -> Vec<(String, serde_json::Value)> {
    log.iter()
        .filter_map(|e| match &e.record {
            Record::ToolResult { name, result, .. } => Some((name.clone(), result.clone())),
            _ => None,
        })
        .collect()
}

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}

#[tokio::test]
async fn multi_turn_session_with_tool_round_trip() {
    let capture = Capture::default();

    // Turn 1 needs two model calls (tool, then final). Turn 2 needs one.
    let model = MockModel::new([
        ModelOutput::tool_calls(vec![ToolCall::new(
            "call-1",
            "echo",
            json!({ "message": "hello-from-huggr" }),
        )]),
        ModelOutput::text("The tool printed the greeting."),
        ModelOutput::text("Anything else?"),
    ]);

    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), model.clone())
        .capability(Arc::new(Echo))
        .system_prompt("You are a test agent.")
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .build();

    // Turn 1: model calls the tool, then answers.
    engine.user_turn("greet me using the tool".into()).await;
    // Turn 2: a follow-up, proving the session is multi-turn.
    engine.user_turn("thanks".into()).await;

    // The tool ran and its output flowed back as a tool result.
    let tool_results = count_tool_results(engine.brain().state().log());
    assert_eq!(tool_results.len(), 1, "expected exactly one tool result");
    let (name, result) = &tool_results[0];
    assert_eq!(name, "echo");
    assert_eq!(result["stdout"], json!("hello-from-huggr"));

    // Both turns reached EndTurn.
    let dones = capture.done.lock().unwrap();
    assert_eq!(dones.len(), 2);
    assert!(dones.iter().all(|d| matches!(d, DoneReason::EndTurn)));

    // The streamed assistant text from all model calls was rendered.
    let text = capture.text.lock().unwrap().clone();
    assert!(text.contains("The tool printed the greeting."));
    assert!(text.contains("Anything else?"));

    // The mock saw the system prompt and the advertised tool in its projected
    // request (proving builder → StaticPolicy wiring), and the tool result was
    // paired with its call in the follow-up request.
    let requests = model.requests.lock().unwrap();
    let first_request = &requests[0];
    assert_eq!(first_request.tools.len(), 1);
    assert_eq!(first_request.tools[0].name, "echo");
    assert!(matches!(
        first_request.blocks.first().map(|b| b.role),
        Some(Role::System)
    ));
    let followup = requests.get(1).expect("follow-up model request");
    let assistant_idx = followup
        .blocks
        .iter()
        .position(|block| {
            block.role == Role::Assistant
                && block
                    .content
                    .iter()
                    .any(|part| matches!(part, ContentPart::ToolUse { id, .. } if id == "call-1"))
        })
        .expect("assistant tool-call block");
    assert_eq!(followup.blocks[assistant_idx + 1].role, Role::Tool);
    assert!(matches!(
        followup.blocks[assistant_idx + 1].content.as_slice(),
        [ContentPart::ToolResult { id, .. }] if id == "call-1"
    ));
}

#[tokio::test]
async fn live_checkpoint_tracks_each_completed_step_and_replays() {
    let model = MockModel::new([
        ModelOutput::tool_calls(vec![ToolCall::new(
            "call-1",
            "echo",
            json!({ "message": "durable" }),
        )]),
        ModelOutput::text("done"),
    ]);
    let dir =
        std::env::temp_dir().join(format!("huggr-host-live-checkpoint-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("live.json");
    let mut meta = huggr_host::huggr_replay::TraceMeta::default();
    meta.trace_id = Some("live-run".into());
    meta.status = Some("interrupted".into());

    let mut engine = Engine::builder()
        .model(ModelSelector::named("medium"), model)
        .capability(Arc::new(Echo))
        .clock(deterministic_clock())
        .checkpoint(&path, meta)
        .build();
    engine.user_turn("use echo".into()).await;

    let trace = huggr_host::Trace::load(&path).expect("checkpoint is readable");
    assert_eq!(trace.meta.trace_id.as_deref(), Some("live-run"));
    assert_eq!(count_tool_results(&trace.log).len(), 1);
    assert!(trace.log.iter().any(|entry| matches!(
        &entry.record,
        Record::ModelOutput { output, .. } if output.text == "done"
    )));
    huggr_host::huggr_replay::verify(&trace).expect("checkpoint replays bit-for-bit");
}

#[tokio::test]
async fn mcp_stdio_tool_runs_through_real_engine() {
    if !python3_available() {
        eprintln!("skipping MCP stdio test: python3 unavailable");
        return;
    }

    let server = r#"
import json, sys
for line in sys.stdin:
    if not line.strip():
        continue
    msg = json.loads(line)
    if "id" not in msg:
        continue
    method = msg.get("method")
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fake-mcp", "version": "0"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "Echo a message.", "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}, "required": ["message"]}}]}
    elif method == "tools/call":
        args = msg.get("params", {}).get("arguments", {})
        result = {"content": [{"type": "text", "text": "echo:" + str(args.get("message", ""))}], "isError": False}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "unknown method"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
"#;
    let caps = mcp::load_stdio(McpServerConfig::new("fake", "python3").args(["-u", "-c", server]))
        .await
        .expect("MCP server should describe its tools");

    let model = MockModel::new([
        ModelOutput::tool_calls(vec![ToolCall::new(
            "call-1",
            "mcp__fake__echo",
            json!({ "message": "hello" }),
        )]),
        ModelOutput::text("The MCP server echoed hello."),
    ]);
    let mut builder = Engine::builder()
        .model(ModelSelector::named("medium"), model.clone())
        .clock(deterministic_clock());
    for cap in caps {
        builder = builder.capability(cap);
    }
    let mut engine = builder.build();

    engine.user_turn("use the MCP echo tool".into()).await;

    let tool_results = count_tool_results(engine.brain().state().log());
    let (_, result) = tool_results
        .iter()
        .find(|(name, _)| name == "mcp__fake__echo")
        .expect("MCP tool result should be logged");
    assert_eq!(result["content"][0]["text"], "echo:hello");
    assert_eq!(
        model.requests.lock().unwrap()[0].tools[0].name,
        "mcp__fake__echo"
    );
}

/// A background capability whose `invoke` blocks until explicitly released, and
/// records when it started. Lets a test *prove* the model ran while this op was
/// still in flight (true overlap, not just "both ran eventually").
struct BlockingBackground {
    /// Fires once `invoke` has started (the op is in flight).
    started: tokio::sync::mpsc::UnboundedSender<()>,
    /// `invoke` returns only after this resolves (the test releases it).
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[async_trait]
impl Capability for BlockingBackground {
    fn name(&self) -> &str {
        "bg"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "bg",
            "A background op that blocks.",
            json!({ "type": "object" }),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    fn runs_in_background(&self) -> bool {
        true
    }
    async fn invoke(&self, _args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let _ = self.started.send(());
        let rx = self.release.lock().unwrap().take();
        if let Some(rx) = rx {
            let _ = rx.await;
        }
        Ok(json!({ "exit_code": 0, "stdout": "background done" }))
    }
}

/// A model whose *second* call signals it ran (proving it executed while the
/// background op was still blocked) and then releases the background op.
struct ConcurrentModel {
    calls: AtomicU64,
    /// Fires when the second model call runs — i.e. while `bg` is still in flight.
    model_ran_concurrently: tokio::sync::mpsc::UnboundedSender<()>,
    /// Releases the blocked background op (sent on the second model call).
    release_bg: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    responses: Mutex<VecDeque<ModelOutput>>,
}

#[async_trait]
impl ModelAdapter for ConcurrentModel {
    async fn call(
        &self,
        _request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("mock ran out of scripted responses"))?;
        // The second call is the one that runs *concurrently* with the still-
        // blocked background op: announce it, then release the background op.
        if n == 1 {
            let _ = self.model_ran_concurrently.send(());
            if let Some(tx) = self.release_bg.lock().unwrap().take() {
                let _ = tx.send(());
            }
        }
        if !output.text.is_empty() {
            sink.text(output.text.clone());
        }
        Ok((output, Usage::new(1, 1)))
    }
}

/// A model stream and a background op run **simultaneously** through the real
/// tokio engine. The model's first call starts a background op; the turn
/// resumes into a second model call *without waiting* for it; that second call
/// provably runs while the background op is still blocked, then releases it;
/// when the background op finishes, a final turn picks up its result and ends.
/// No polling/sleep anywhere — the engine reacts to events.
#[tokio::test]
async fn model_stream_runs_while_background_op_is_in_flight() {
    let capture = Capture::default();

    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (ran_tx, mut ran_rx) = tokio::sync::mpsc::unbounded_channel();

    let background = Arc::new(BlockingBackground {
        started: started_tx,
        release: Mutex::new(Some(release_rx)),
    });

    // Call 0: ask for the background op. Call 1: streams while bg is blocked,
    // releases it. Call 2: final answer after bg's result is folded in.
    let model = Arc::new(ConcurrentModel {
        calls: AtomicU64::new(0),
        model_ran_concurrently: ran_tx,
        release_bg: Mutex::new(Some(release_tx)),
        responses: Mutex::new(
            [
                ModelOutput::tool_calls(vec![ToolCall::new("call-1", "bg", json!({}))]),
                ModelOutput::text("Kicked it off in the background."),
                ModelOutput::text("Background work finished."),
            ]
            .into_iter()
            .collect(),
        ),
    });

    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), model)
        .capability(background)
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .build();

    engine
        .user_turn("do background work and keep talking".into())
        .await;

    // The background op started (was in flight)...
    started_rx.recv().await.expect("background op should start");
    // ...and the second model call ran while it was still blocked (true overlap).
    ran_rx
        .recv()
        .await
        .expect("model should run concurrently with the in-flight background op");

    // The turn completed: the background result was folded in and a final model
    // call ended the turn. Exactly one EndTurn (the deferred-done path).
    let dones = capture.done.lock().unwrap();
    assert_eq!(dones.len(), 1, "expected exactly one terminal Done");
    assert!(matches!(dones[0], DoneReason::EndTurn));

    // The background tool result is in the durable log.
    let tool_results = count_tool_results(engine.brain().state().log());
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].0, "bg");

    // Both the concurrent line and the final line were streamed.
    let text = capture.text.lock().unwrap().clone();
    assert!(text.contains("Kicked it off in the background."));
    assert!(text.contains("Background work finished."));
}

/// A model adapter that streams a few tokens, signals it has started streaming,
/// then awaits forever — so the only way its task ends is the engine aborting
/// it. Lets a test cancel an in-flight model stream for real.
struct HangingStreamModel {
    /// Fires once the adapter has streamed its tokens and is about to block.
    streaming: tokio::sync::mpsc::UnboundedSender<()>,
}

#[async_trait]
impl ModelAdapter for HangingStreamModel {
    async fn call(
        &self,
        _request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        // Stream partial text live (transport only; never logged) — this is the
        // "N tokens" that cancellation must preserve as the partial.
        sink.text("Hello, ".to_string());
        sink.text("wor".to_string());
        // Announce that we are mid-stream, then hang until the task is aborted.
        let _ = self.streaming.send(());
        std::future::pending::<()>().await;
        unreachable!("the engine aborts this task on cancel");
    }
}

/// Cancel an in-flight **model stream** cleanly through the real tokio engine.
/// The model streams a couple of tokens then hangs; a `UserAbort` injected via
/// [`Engine::event_sender`] (as a Ctrl-C handler would) makes the brain emit
/// `Cancel`, the engine aborts the task, and the brain logs the partial
/// ("Hello, wor") with a `Cancelled` outcome and ends `Cancelled`.
#[tokio::test]
async fn cancel_in_flight_model_stream_preserves_partial() {
    let capture = Capture::default();

    let (streaming_tx, mut streaming_rx) = tokio::sync::mpsc::unbounded_channel();
    let model = Arc::new(HangingStreamModel {
        streaming: streaming_tx,
    });

    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), model)
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .build();

    // Inject the abort the moment the model is mid-stream (from "outside" the
    // turn, like a signal handler), then let the turn drive to completion.
    let sender = engine.event_sender();
    tokio::spawn(async move {
        streaming_rx
            .recv()
            .await
            .expect("model should start streaming");
        assert!(sender.abort(), "abort should be accepted");
    });

    engine.user_turn("write a long poem".into()).await;

    // The turn ended Cancelled (not EndTurn, not Error).
    let dones = capture.done.lock().unwrap();
    assert_eq!(dones.len(), 1, "expected exactly one terminal Done");
    assert!(
        matches!(dones[0], DoneReason::Cancelled),
        "expected Cancelled, got {:?}",
        dones[0]
    );

    // The partial streamed text is preserved in the durable log as the model
    // op's Cancelled outcome — "N tokens then cancelled", never an empty gap.
    let partial = engine
        .brain()
        .state()
        .log()
        .iter()
        .find_map(|e| match &e.record {
            Record::OpEnded {
                outcome: OpOutcome::Cancelled { partial },
                ..
            } => Some(partial.clone()),
            _ => None,
        })
        .expect("a Cancelled op should be logged");
    assert_eq!(partial, json!("Hello, wor"));

    // No model output was ever consolidated (the stream never finished).
    let model_outputs = engine
        .brain()
        .state()
        .log()
        .iter()
        .filter(|e| matches!(e.record, Record::ModelOutput { .. }))
        .count();
    assert_eq!(
        model_outputs, 0,
        "a cancelled stream has no consolidated output"
    );
}

/// A background capability that blocks forever once started (until its task is
/// aborted), signalling when it is in flight. Proves a background op cancels
/// cleanly with no leaked work.
struct HangingBackground {
    started: tokio::sync::mpsc::UnboundedSender<()>,
}

#[async_trait]
impl Capability for HangingBackground {
    fn name(&self) -> &str {
        "bg"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "bg",
            "A background op that never finishes.",
            json!({ "type": "object" }),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    fn runs_in_background(&self) -> bool {
        true
    }
    async fn invoke(&self, _args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let _ = self.started.send(());
        std::future::pending::<()>().await;
        unreachable!("the engine aborts this task on cancel");
    }
}

/// A model whose first call requests a background op and whose later call(s)
/// stream a token then hang until aborted.
struct BackgroundThenHang {
    responses: Mutex<VecDeque<ModelOutput>>,
}

#[async_trait]
impl ModelAdapter for BackgroundThenHang {
    async fn call(
        &self,
        _request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        if let Some(output) = self.responses.lock().unwrap().pop_front() {
            return Ok((output, Usage::new(1, 1)));
        }
        // After the scripted first response, every resumed turn streams a token
        // then hangs — keeping a model op in flight alongside the background op.
        sink.text("thinking".to_string());
        std::future::pending::<()>().await;
        unreachable!("aborted on cancel");
    }
}

/// A **background** capability op cancels cleanly through the real engine. The
/// model kicks off a never-finishing background op and the turn resumes into a
/// second model call (concurrent). Once the background op is in flight, a
/// `UserAbort` aborts every in-flight task; the background op is logged
/// `Cancelled` and the turn ends `Cancelled` with no leaked work.
#[tokio::test]
async fn cancel_in_flight_background_op_cleanly() {
    let capture = Capture::default();

    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let background = Arc::new(HangingBackground {
        started: started_tx,
    });

    // Call 0 asks for the background op; call 1 streams alongside it then hangs
    // (so something is still in flight to cancel when we abort).
    let model = Arc::new(BackgroundThenHang {
        responses: Mutex::new(
            [ModelOutput::tool_calls(vec![ToolCall::new(
                "call-1",
                "bg",
                json!({}),
            )])]
            .into_iter()
            .collect(),
        ),
    });

    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), model)
        .capability(background)
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .build();

    let sender = engine.event_sender();
    tokio::spawn(async move {
        started_rx.recv().await.expect("background op should start");
        assert!(sender.abort(), "abort should be accepted");
    });

    engine.user_turn("kick off background work".into()).await;

    // The session ended Cancelled.
    let dones = capture.done.lock().unwrap();
    assert_eq!(dones.len(), 1);
    assert!(matches!(dones[0], DoneReason::Cancelled));

    // The background op was logged Cancelled (no leaked/never-resolved op).
    let cancelled = engine
        .brain()
        .state()
        .log()
        .iter()
        .filter(|e| {
            matches!(
                e.record,
                Record::OpEnded {
                    outcome: OpOutcome::Cancelled { .. },
                    ..
                }
            )
        })
        .count();
    assert!(
        cancelled >= 1,
        "the background op should be logged Cancelled"
    );

    // Nothing is left in flight — the engine fully drained.
    assert_eq!(engine.brain().state().inflight_len(), 0);
}

/// A model that streams its answer split into `chunk_size`-char pieces. Each
/// `sink.text` is a separate `ModelDelta` event the engine submits to the brain
/// — so the brain sees every delta (partial text complete), while the durable
/// log consolidates. `chunk_size == 0` streams the whole answer in one delta.
struct ChunkedModel {
    text: String,
    chunk_size: usize,
}

impl ChunkedModel {
    fn new(text: &str, chunk_size: usize) -> Arc<Self> {
        Arc::new(Self {
            text: text.to_string(),
            chunk_size,
        })
    }
}

#[async_trait]
impl ModelAdapter for ChunkedModel {
    async fn call(
        &self,
        _request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        let chars: Vec<char> = self.text.chars().collect();
        let step = if self.chunk_size == 0 {
            chars.len().max(1)
        } else {
            self.chunk_size
        };
        for piece in chars.chunks(step) {
            let s: String = piece.iter().collect();
            sink.text(s); // one ModelDelta per chunk
        }
        Ok((ModelOutput::text(self.text.clone()), Usage::new(1, 1)))
    }
}

/// Run a one-shot turn against a `ChunkedModel` with the given chunk size and
/// return `(durable log, rendered text)`.
async fn run_chunked(answer: &str, chunk_size: usize) -> (Vec<huggr_core::LogEntry>, String) {
    let capture = Capture::default();
    let model = ChunkedModel::new(answer, chunk_size);
    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), model)
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .build();
    engine.user_turn("tell me something".into()).await;
    let log = engine.brain().state().log().to_vec();
    let text = capture.text.lock().unwrap().clone();
    (log, text)
}

/// Keep only the *logical* records (user/model/tool) — the consolidated content
/// the durable trace is about. (`OpEnded` carries timestamps whose count tracks
/// the number of injected ticks, which differs with delta count; the consolidated
/// records do not, and they are what replay keys off.)
fn logical_records(log: &[huggr_core::LogEntry]) -> Vec<Record> {
    log.iter()
        .filter(|e| {
            matches!(
                e.record,
                Record::UserMessage { .. } | Record::ModelOutput { .. } | Record::ToolResult { .. }
            )
        })
        .map(|e| e.record.clone())
        .collect()
}

/// Deltas are transport, never durable: exactly **one** consolidated `Record`
/// per message lands in the log, so the log (and thus replay) is identical
/// regardless of how the stream was chunked.
#[tokio::test]
async fn streamed_deltas_never_reach_the_durable_log() {
    let answer = "The quick brown fox jumps over the lazy dog.";

    // Same answer streamed three ways: per-character (worst-case churn), in
    // 5-char chunks, and as a single delta.
    let (log_per_char, text_a) = run_chunked(answer, 1).await;
    let (log_chunks, text_b) = run_chunked(answer, 5).await;
    let (log_one, text_c) = run_chunked(answer, 0).await;

    // 1. The user sees identical text no matter how it was chunked.
    assert_eq!(text_a, answer);
    assert_eq!(text_b, answer);
    assert_eq!(text_c, answer);

    // 2. The consolidated logical records are byte-for-byte identical across all
    //    three chunkings — exactly one `ModelOutput` per call, no per-delta
    //    entries. This is what makes replay bit-for-bit independent of batching.
    let logical_a = logical_records(&log_per_char);
    let logical_b = logical_records(&log_chunks);
    let logical_c = logical_records(&log_one);
    assert_eq!(logical_a, logical_b);
    assert_eq!(logical_a, logical_c);

    // 3. The log holds exactly one consolidated model output — never one record
    //    per delta.
    for (label, log) in [
        ("per-char", &log_per_char),
        ("chunks", &log_chunks),
        ("one", &log_one),
    ] {
        let model_outputs = log
            .iter()
            .filter(|e| matches!(e.record, Record::ModelOutput { .. }))
            .count();
        assert_eq!(
            model_outputs, 1,
            "{label}: expected exactly one consolidated ModelOutput, no per-delta entries"
        );
        let output = log.iter().find_map(|e| match &e.record {
            Record::ModelOutput { output, .. } => Some(output.clone()),
            _ => None,
        });
        assert_eq!(
            output.map(|o| o.text),
            Some(answer.to_string()),
            "{label}: consolidated output text must equal the full answer"
        );
    }
}

/// Record a real session through the engine, save it to a trace, reload it,
/// replay it through a fresh brain, and assert the reconstructed command
/// sequence + log is byte-identical to the original.
#[tokio::test]
async fn record_then_replay_reconstructs_the_session_bit_for_bit() {
    let capture = Capture::default();

    // A session with a tool op: model calls the tool, then gives a final answer.
    let model = MockModel::new([
        ModelOutput::tool_calls(vec![ToolCall::new(
            "call-1",
            "echo",
            json!({ "message": "replay-me" }),
        )]),
        ModelOutput::text("The tool printed replay-me."),
    ]);

    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), model)
        .capability(Arc::new(Echo))
        .system_prompt("You are a test agent.")
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .record(true)
        .build();

    engine.user_turn("greet me using the tool".into()).await;
    engine.session_end();

    // The live session's durable log (the truth we will replay against).
    let live_log = engine.brain().state().log().to_vec();
    assert!(!live_log.is_empty());

    // Save the recorded trace to disk and reload it.
    let dir = std::env::temp_dir().join(format!("huggr-host-replay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.trace.json");
    engine.save_trace(&path).expect("recording was enabled");
    let trace = huggr_host::Trace::load(&path).expect("reload the trace");

    // The recorded log matches the live log exactly (recorder read it from the
    // brain at save time; no desync).
    assert_eq!(trace.log, live_log, "recorded log must equal the live log");

    // The recorder captured the live command sequence too, so `verify` checks
    // command ordering (not just the log) bit-for-bit.
    assert!(
        !trace.commands.is_empty(),
        "the recorder captured the live command sequence"
    );

    // Replay through a FRESH brain (no engine, no IO): the reconstructed log AND
    // command sequence are byte-identical to the original recording. (`verify`
    // fails if either diverges.)
    let replay = huggr_host::huggr_replay::verify(&trace)
        .expect("replay must reconstruct the recorded log AND commands bit-for-bit");
    assert_eq!(
        replay.log, live_log,
        "replayed log == live log, bit-for-bit"
    );
    assert_eq!(
        replay.commands, trace.commands,
        "replayed commands == the live-recorded commands, bit-for-bit"
    );

    // Determinism: a second replay yields identical commands.
    let again = huggr_host::huggr_replay::replay(&trace);
    assert_eq!(
        replay.commands, again.commands,
        "re-feeding identical events must yield identical commands"
    );

    // The reconstructed command sequence is the real agentic turn loop: it
    // opens with a model call, runs the echo capability, and ends with Done.
    use huggr_core::Command;
    assert!(
        replay
            .commands
            .iter()
            .any(|c| matches!(c, Command::StartModelCall { .. })),
        "the opening model call was reconstructed"
    );
    assert!(
        replay
            .commands
            .iter()
            .any(|c| matches!(c, Command::StartCapability { name, .. } if name == "echo")),
        "the echo capability was invoked"
    );
    assert!(
        replay
            .commands
            .iter()
            .any(|c| matches!(c, Command::Done { .. })),
        "the session reaches Done"
    );

    // The step-through inspector walks the same session: every event is one
    // step, and the per-step appended log entries reassemble the full log.
    let mut inspector = huggr_host::Inspector::new(&trace);
    let mut stepped_log = Vec::new();
    let mut steps = 0;
    while let Some(step) = inspector.step() {
        stepped_log.extend(step.appended);
        steps += 1;
    }
    assert_eq!(steps, trace.events.len(), "one inspector step per event");
    assert_eq!(stepped_log, live_log, "stepwise log reassembles the truth");

    std::fs::remove_dir_all(&dir).ok();
}

/// Record a session, save it, then **resume** from the trace and add a NEW user
/// turn. The resumed engine rebuilds its brain from the trace's recorded events
/// with zero IO (the original model/tool calls are *not* re-run), continues
/// recording, and re-saving yields a trace whose log contains BOTH the original
/// records AND the new turn's — and which still replays bit-for-bit.
#[tokio::test]
async fn resume_from_trace_continues_the_session() {
    // Session 1: record an original session with a tool op, then save it.
    let model1 = MockModel::new([
        ModelOutput::tool_calls(vec![ToolCall::new(
            "call-1",
            "echo",
            json!({ "message": "resume-me" }),
        )]),
        ModelOutput::text("The tool printed resume-me."),
    ]);

    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), model1)
        .capability(Arc::new(Echo))
        .system_prompt("You are a test agent.")
        .frontend(Box::new(Capture::default()))
        .clock(deterministic_clock())
        .record(true)
        .build();

    engine.user_turn("greet me using the tool".into()).await;
    engine.session_end();

    let original_log = engine.brain().state().log().to_vec();
    let original_logical = logical_records(&original_log);
    assert!(!original_logical.is_empty());

    let dir = std::env::temp_dir().join(format!("huggr-host-resume-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.trace.json");
    engine.save_trace(&path).expect("recording was enabled");
    let saved = huggr_host::Trace::load(&path).expect("reload the trace");
    let original_event_count = saved.events.len();

    // Session 2: resume from the trace and add a NEW user turn.
    let capture2 = Capture::default();
    // A *fresh* model with only the new turn's responses: if resume re-ran the
    // recorded model calls this mock would be exhausted (proving no IO replay).
    let model2 = MockModel::new([ModelOutput::text("You're welcome!")]);

    let mut resumed = Engine::builder()
        .model(ModelSelector::named("big"), model2.clone())
        .capability(Arc::new(Echo))
        .system_prompt("You are a test agent.")
        .frontend(Box::new(capture2.clone()))
        .clock(deterministic_clock())
        .resume(saved.clone())
        .build();

    // The brain was rebuilt from the trace with no IO: the original log is fully
    // present *before* any new turn runs, and nothing is in flight.
    assert_eq!(
        resumed.brain().state().log(),
        original_log.as_slice(),
        "resumed brain reconstructs the original log before continuing"
    );
    assert_eq!(resumed.brain().state().inflight_len(), 0);
    // The recorded model was NOT re-invoked during the seed (its 0 requests).
    assert!(
        model2.requests.lock().unwrap().is_empty(),
        "resume must not re-run recorded model calls"
    );

    // Continue with a NEW turn.
    resumed.user_turn("thanks".into()).await;
    resumed.session_end();

    // The new turn's model call ran exactly once (the seed performed no IO).
    assert_eq!(
        model2.requests.lock().unwrap().len(),
        1,
        "only the new turn triggers a model call"
    );
    let text = capture2.text.lock().unwrap().clone();
    assert!(text.contains("You're welcome!"));

    // The grown log contains BOTH the original records AND the new turn's.
    let grown_log = resumed.brain().state().log().to_vec();
    let grown_logical = logical_records(&grown_log);
    assert!(
        grown_logical.len() > original_logical.len(),
        "the resumed session added records: {} → {}",
        original_logical.len(),
        grown_logical.len()
    );
    assert_eq!(
        &grown_logical[..original_logical.len()],
        original_logical.as_slice(),
        "the original records are preserved as a prefix of the grown log"
    );
    assert!(
        grown_logical.iter().any(|r| matches!(
            r,
            Record::UserMessage { text, .. } if text == "thanks"
        )),
        "the new user turn is in the grown log"
    );

    // Re-save the grown session: it still replays bit-for-bit.
    let path2 = dir.join("session.resumed.trace.json");
    resumed
        .save_trace(&path2)
        .expect("resume implies recording");
    let regrown = huggr_host::Trace::load(&path2).expect("reload the grown trace");

    // The grown trace carries the full event history (old + new), and its log is
    // the grown log (no desync).
    assert!(
        regrown.events.len() > original_event_count,
        "grown trace appends new events after the recorded ones"
    );
    assert_eq!(
        &regrown.events[..original_event_count],
        &saved.events[..],
        "the original event stream is the prefix of the grown one"
    );
    assert_eq!(regrown.log, grown_log, "saved log == live grown log");
    // The policy survived the round-trip (so replay branches identically).
    assert_eq!(
        regrown.policy, saved.policy,
        "the resumed trace carries the original policy"
    );

    // The whole grown session replays bit-for-bit through a fresh brain.
    let replay = huggr_host::huggr_replay::verify(&regrown)
        .expect("the resumed session must replay bit-for-bit");
    assert_eq!(replay.log, grown_log);

    std::fs::remove_dir_all(&dir).ok();
}

/// Regression: an **explicit** `.default_model(named("medium"))` must survive
/// later registrations — with tiers registered `[small, medium, big]`, the
/// first-registered fallback must not steal an explicit default that happens
/// to equal the built-in one, silently routing every turn to the small tier.
#[tokio::test]
async fn explicit_default_model_is_not_stolen_by_first_registration() {
    let small = MockModel::new([]);
    let medium = MockModel::new([ModelOutput::text("routed to medium")]);
    let big = MockModel::new([]);
    let capture = Capture::default();

    let mut engine = Engine::builder()
        .default_model(ModelSelector::named("medium"))
        .model(ModelSelector::named("small"), small.clone())
        .model(ModelSelector::named("medium"), medium.clone())
        .model(ModelSelector::named("big"), big.clone())
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .build();

    engine.user_turn("hello there".into()).await;

    assert_eq!(
        medium.requests.lock().unwrap().len(),
        1,
        "the explicit default tier handles the normal turn"
    );
    assert!(
        small.requests.lock().unwrap().is_empty(),
        "the first-registered tier must not steal the explicit default"
    );
    assert!(big.requests.lock().unwrap().is_empty());
    assert!(capture.text.lock().unwrap().contains("routed to medium"));
}

#[tokio::test]
async fn custom_policy_controls_model_and_verifies_through_registry() {
    let default = MockModel::new([]);
    let custom = MockModel::new([ModelOutput::text("custom route")]);
    let config = json!({ "kind": "custom-selector", "model": "custom" });

    let mut engine = Engine::builder()
        .record(true)
        .clock(deterministic_clock())
        .model(ModelSelector::named("default"), default.clone())
        .model(ModelSelector::named("custom"), custom.clone())
        .policy(
            Box::new(CustomSelectorPolicy::new("custom")),
            config.clone(),
        )
        .build();

    engine.user_turn("route this".into()).await;
    engine.session_end();

    assert!(default.requests.lock().unwrap().is_empty());
    assert_eq!(custom.requests.lock().unwrap().len(), 1);
    let trace = engine.trace().unwrap();
    assert_eq!(trace.policy, Some(config));

    let mut registry = PolicyRegistry::default();
    registry.register("custom-selector", decode_custom_policy);
    let replay = huggr_host::huggr_replay::verify_with_registry(&trace, &registry)
        .expect("custom policy trace verifies with its registry");
    assert_eq!(replay.log, trace.log);
}

#[tokio::test]
async fn budget_policy_wraps_default_policy_and_records_config() {
    let model = MockModel::new([ModelOutput::text("ok")]);
    let mut engine = Engine::builder()
        .record(true)
        .clock(deterministic_clock())
        .model(ModelSelector::named("medium"), model.clone())
        .budget_policy(
            BudgetPolicy::new(8)
                .with_trigger_tokens(8)
                .with_keep_recent_tokens(2)
                .with_max_block_tokens(2),
        )
        .build();

    engine
        .user_turn("long enough question to compact later".into())
        .await;
    engine.session_end();

    let trace = engine.trace().unwrap();
    let policy = trace.policy.as_ref().expect("recorded policy");
    assert_eq!(policy["kind"], "budget");
    assert_eq!(policy["budget_tokens"], 8);
    assert_eq!(policy["base"]["kind"], "static");
    huggr_host::huggr_replay::verify(&trace).expect("budget policy trace verifies");
}

#[tokio::test]
async fn summary_policy_records_summary_and_verifies() {
    let medium = MockModel::new([
        ModelOutput::text("first answer"),
        ModelOutput::text("final answer after summary"),
    ]);
    let summarizer = MockModel::new([ModelOutput::text("summary of the old turn")]);
    let mut engine = Engine::builder()
        .record(true)
        .clock(deterministic_clock())
        .model(ModelSelector::named("medium"), medium.clone())
        .model(ModelSelector::named("summarizer"), summarizer.clone())
        .budget_policy(
            BudgetPolicy::new(16)
                .with_trigger_tokens(16)
                .with_keep_recent_tokens(4)
                .with_summary_selector(ModelSelector::named("summarizer")),
        )
        .build();

    engine
        .user_turn("old context ".repeat(30).to_string())
        .await;
    engine.user_turn("use that context".into()).await;
    engine.session_end();

    assert_eq!(summarizer.requests.lock().unwrap().len(), 1);
    assert_eq!(medium.requests.lock().unwrap().len(), 2);
    let trace = engine.trace().unwrap();
    assert!(trace.log.iter().any(|entry| matches!(
        &entry.record,
        Record::ContextSummary { text, .. } if text == "summary of the old turn"
    )));
    huggr_host::huggr_replay::verify(&trace).expect("summarizing trace verifies");
}

/// Regression: a resumed engine must start **quiescent**. Resuming a trace
/// whose fold leaves an op in flight (a crash mid-model-call) reconciles it by
/// folding `OpCancelled` — and the commands that queues (a `Done { Cancelled }`,
/// potentially a stale `StartModelCall`) must be discarded at build time, not
/// fired at the start of the first live turn.
#[tokio::test]
async fn resume_after_crash_starts_quiescent() {
    // Craft a "crashed" trace by hand: a recorded user turn whose model call
    // never completed. Folding these events leaves the model op in flight —
    // exactly what a checkpoint written mid-call would contain.
    let events = vec![
        huggr_core::Event::Tick {
            now: huggr_core::Timestamp(1),
        },
        huggr_core::Event::UserInput {
            content: json!("start then crash"),
            est_tokens: 5,
        },
    ];
    let saved = huggr_host::Trace::new(events, Vec::new(), Some(1));

    let capture = Capture::default();
    let continued_model = MockModel::new([ModelOutput::text("continued after crash")]);
    let mut resumed = Engine::builder()
        .model(ModelSelector::named("medium"), continued_model.clone())
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .resume(saved)
        .build();

    // Crash resume cancels the stale in-flight op before going live.
    assert_eq!(resumed.brain().state().inflight_len(), 0);
    assert!(
        resumed.brain().state().log().iter().any(|entry| matches!(
            &entry.record,
            Record::OpEnded {
                outcome: OpOutcome::Cancelled { .. },
                ..
            }
        )),
        "resumed log records the stale op cancellation"
    );

    resumed.user_turn("continue".into()).await;

    // Exactly the live turn's Done fired — no spurious Done{Cancelled} left
    // over from the reconciled pre-crash turn.
    let dones = capture.done.lock().unwrap();
    assert_eq!(
        dones.len(),
        1,
        "only the live turn's Done fires, got {dones:?}"
    );
    assert!(matches!(dones[0], DoneReason::EndTurn), "got {dones:?}");

    // No stale pre-crash StartModelCall raced the new turn: the model ran
    // exactly once, for the new user input.
    assert_eq!(continued_model.requests.lock().unwrap().len(), 1);

    // The grown session still re-saves and replays bit-for-bit.
    let grown = resumed.trace().expect("resume implies recording");
    huggr_host::huggr_replay::verify(&grown).expect("grown crash trace replays");
}

/// A non-recording engine has no trace, and `save_trace` errors cleanly.
#[tokio::test]
async fn engine_without_recording_has_no_trace() {
    let model = MockModel::new([ModelOutput::text("hi")]);
    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), model)
        .frontend(Box::new(Capture::default()))
        .clock(deterministic_clock())
        .build();
    engine.user_turn("hello".into()).await;
    assert!(engine.trace().is_none(), "recording was not enabled");
    assert!(
        engine
            .save_trace("/tmp/should-not-exist.trace.json")
            .is_err()
    );
}

/// A capability that panics — the engine must map the panic to a tool error
/// instead of leaving the op in flight forever.
struct Panicker;

#[async_trait]
impl Capability for Panicker {
    fn name(&self) -> &str {
        "panicker"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new("panicker", "Always panics.", json!({ "type": "object" }))
    }
    async fn invoke(&self, _args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        panic!("boom");
    }
}

#[tokio::test]
async fn a_panicking_capability_resolves_as_a_tool_error_instead_of_hanging() {
    let capture = Capture::default();
    let model = MockModel::new([
        ModelOutput::tool_calls(vec![ToolCall::new("call-1", "panicker", json!({}))]),
        ModelOutput::text("Recovered from the tool failure."),
    ]);
    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), model)
        .capability(Arc::new(Panicker))
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .build();

    // Must terminate: before the panic mapping this hung on rx.recv() forever.
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.user_turn("use the tool".into()),
    )
    .await
    .expect("the turn must complete despite the panicking capability");

    let tool_results = count_tool_results(engine.brain().state().log());
    assert_eq!(tool_results.len(), 1);
    let (name, result) = &tool_results[0];
    assert_eq!(name, "panicker");
    assert!(
        result["error"]
            .as_str()
            .unwrap_or_default()
            .contains("boom"),
        "{result}"
    );
    let dones = capture.done.lock().unwrap();
    assert!(matches!(dones.last(), Some(DoneReason::EndTurn)));
}

/// A panicking model adapter must likewise resolve the op as a model error.
struct PanickingModel;

#[async_trait]
impl ModelAdapter for PanickingModel {
    async fn call(
        &self,
        _request: ModelRequest,
        _sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        panic!("adapter exploded");
    }
}

#[tokio::test]
async fn a_panicking_model_adapter_resolves_as_a_model_error() {
    let capture = Capture::default();
    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), Arc::new(PanickingModel))
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .build();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.user_turn("hi".into()),
    )
    .await
    .expect("the turn must complete despite the panicking adapter");
    let dones = capture.done.lock().unwrap();
    assert!(!dones.is_empty(), "the turn reached a Done command");
}

/// The registry advertises tools sorted by name, so the same agent definition
/// projects an identical tool ordering across processes (no HashMap-order
/// variance between runs).
#[test]
fn registry_schemas_are_sorted_by_name() {
    use huggr_host::CapabilityRegistry;

    struct Named(&'static str);
    #[async_trait]
    impl Capability for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema::new(self.0, "x", json!({ "type": "object" }))
        }
        async fn invoke(&self, _args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
            Ok(json!({}))
        }
    }

    let mut registry = CapabilityRegistry::new();
    for name in ["zebra", "apple", "mango", "banana"] {
        registry.register(Arc::new(Named(name)));
    }
    let names: Vec<_> = registry.schemas().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["apple", "banana", "mango", "zebra"]);
}
