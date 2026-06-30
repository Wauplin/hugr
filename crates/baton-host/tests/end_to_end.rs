//! End-to-end Phase 1 test: a genuine multi-turn session driven through the
//! real tokio [`Engine`] loop, using a scripted mock model adapter and the
//! *real* shell capability. This exercises the whole path
//! (user → model → tool → model → done) without a network or API key.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use baton_core::{
    DoneReason, ModelOutput, ModelRequest, ModelSelector, OpOutcome, OutputEvent, Record, ToolCall,
    ToolSchema, Usage, Value,
};
use baton_host::capabilities::Shell;
use baton_host::policy::DenyAll;
use baton_host::{Capability, ChunkSink, Engine, Frontend, ModelAdapter, ModelSink, Policy};
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
    /// Token usage observed at each model-call end (drives metrics).
    model_usage: Arc<Mutex<Vec<Usage>>>,
    /// Tool names observed at each tool-call end.
    tool_ends: Arc<Mutex<Vec<String>>>,
    /// Number of times the session-end hook fired.
    session_ends: Arc<Mutex<u32>>,
    /// Number of `ModelText` render events the front-end actually received —
    /// lets a test prove the host coalesced many deltas into fewer renders.
    text_renders: Arc<Mutex<u32>>,
}

impl Frontend for Capture {
    fn on_output(&mut self, event: &OutputEvent) {
        if let OutputEvent::ModelText { text, .. } = event {
            self.text.lock().unwrap().push_str(text);
            *self.text_renders.lock().unwrap() += 1;
        }
    }
    fn on_notice(&mut self, _message: &str) {}
    fn on_model_end(&mut self, _op: baton_core::OpId, usage: &Usage) {
        self.model_usage.lock().unwrap().push(usage.clone());
    }
    fn on_tool_end(
        &mut self,
        _op: baton_core::OpId,
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

/// A scripted model that reports per-call **cost** in `Usage.extra` (mirroring a
/// real router adapter), so the metrics path can be exercised end-to-end.
struct CostModel {
    responses: Mutex<VecDeque<ModelOutput>>,
}

impl CostModel {
    fn new(responses: impl IntoIterator<Item = ModelOutput>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into_iter().collect()),
        })
    }
}

#[async_trait]
impl ModelAdapter for CostModel {
    async fn call(
        &self,
        _request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        let output = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("mock ran out of scripted responses"))?;
        if !output.text.is_empty() {
            sink.text(output.text.clone());
        }
        let usage =
            Usage::new(100, 50).with_extra(json!({ "cost": 0.0010, "cost_source": "router" }));
        Ok((output, usage))
    }
}

/// A one-shot run drives the metrics hooks through the real engine: each model
/// call surfaces token usage + cost via `on_model_end`, tool ends fire, and
/// `Engine::session_end` triggers the front-end's `on_session_end` exactly once.
#[tokio::test]
async fn metrics_flow_through_engine() {
    let capture = Capture::default();

    let model = CostModel::new([
        ModelOutput::tool_calls(vec![ToolCall::new(
            "call-1",
            "shell",
            json!({ "cmd": "echo metrics" }),
        )]),
        ModelOutput::text("Done."),
    ]);

    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), model)
        .capability(Arc::new(Shell))
        .system_prompt("You are a test agent.")
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .build();

    engine.user_turn("use the shell".into()).await;
    engine.session_end(); // one-shot run: emit the totals footer

    // Two model calls, each reporting tokens + cost in `Usage.extra`.
    let usage = capture.model_usage.lock().unwrap();
    assert_eq!(usage.len(), 2, "expected two model-call ends");
    for u in usage.iter() {
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 50);
        assert_eq!(u.extra.get("cost").and_then(|c| c.as_f64()), Some(0.0010));
    }

    // The shell tool ran and its completion was observed.
    let tools = capture.tool_ends.lock().unwrap();
    assert_eq!(tools.as_slice(), &["shell".to_string()]);

    // The session-end hook fired exactly once (the totals footer point).
    assert_eq!(*capture.session_ends.lock().unwrap(), 1);
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

/// P2-1 DONE criterion: a model stream and a background op run **simultaneously**
/// through the real tokio engine. The model's first call starts a background op;
/// the turn resumes into a second model call *without waiting* for it; that
/// second call provably runs while the background op is still blocked, then
/// releases it; when the background op finishes, a final turn picks up its
/// result and ends. No polling/sleep anywhere — the engine reacts to events.
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

/// P2-2 DONE criterion: cancel an in-flight **model stream** cleanly through the
/// real tokio engine. The model streams a couple of tokens then hangs; a
/// `UserAbort` injected via [`Engine::event_sender`] (as a Ctrl-C handler would)
/// makes the brain emit `Cancel`, the engine aborts the task, and the brain logs
/// the partial ("Hello, wor") with a `Cancelled` outcome and ends `Cancelled`.
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

/// P2-2 DONE criterion: a **background** capability op cancels cleanly through
/// the real engine. The model kicks off a never-finishing background op and the
/// turn resumes into a second model call (concurrent). Once the background op is
/// in flight, a `UserAbort` aborts every in-flight task; the background op is
/// logged `Cancelled` and the turn ends `Cancelled` with no leaked work.
#[tokio::test]
async fn cancel_in_flight_background_op_cleanly() {
    let capture = Capture::default();

    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let background = Arc::new(HangingBackground {
        started: started_tx,
    });

    // Call 0 asks for the background op; call 1 streams alongside it then hangs
    // (so something is still in flight to cancel when we abort).
    let (streaming_tx, _streaming_rx) = tokio::sync::mpsc::unbounded_channel();
    let model = Arc::new(BackgroundThenHang {
        calls: AtomicU64::new(0),
        streaming: streaming_tx,
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

/// A model whose first call requests a background op and whose later call(s)
/// stream a token then hang until aborted.
struct BackgroundThenHang {
    calls: AtomicU64,
    streaming: tokio::sync::mpsc::UnboundedSender<()>,
    responses: Mutex<VecDeque<ModelOutput>>,
}

#[async_trait]
impl ModelAdapter for BackgroundThenHang {
    async fn call(
        &self,
        _request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(output) = self.responses.lock().unwrap().pop_front() {
            return Ok((output, Usage::new(1, 1)));
        }
        // After the scripted first response, every resumed turn streams a token
        // then hangs — keeping a model op in flight alongside the background op.
        let _ = n;
        sink.text("thinking".to_string());
        let _ = self.streaming.send(());
        std::future::pending::<()>().await;
        unreachable!("aborted on cancel");
    }
}

/// A model that streams its answer split into `chunk_size`-char pieces (the
/// thing the host coalesces). Each `sink.text` is a separate `ModelDelta` event
/// the engine submits to the brain *and* feeds to the coalescer — so the brain
/// sees every delta (partial text complete), while the front-end render is
/// batched. `chunk_size == 0` streams the whole answer in one delta.
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
/// return `(durable log, rendered text, number of text-render calls)`.
async fn run_chunked(answer: &str, chunk_size: usize) -> (Vec<baton_core::LogEntry>, String, u32) {
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
    let renders = *capture.text_renders.lock().unwrap();
    (log, text, renders)
}

/// Keep only the *logical* records (user/model/tool) — the consolidated content
/// the durable trace is about. (`OpEnded` carries timestamps whose count tracks
/// the number of injected ticks, which differs with delta count; the consolidated
/// records do not, and they are what replay keys off — ARCHITECTURE §4.5.)
fn logical_records(log: &[baton_core::LogEntry]) -> Vec<Record> {
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

/// P2-3 DONE criterion: the host coalesces streamed deltas for the *render*, but
/// records exactly **one** consolidated `Record` per message — deltas never hit
/// the durable log, so the log (and thus replay) is identical regardless of how
/// the stream was chunked/batched.
#[tokio::test]
async fn delta_coalescing_keeps_recording_exact() {
    let answer = "The quick brown fox jumps over the lazy dog.";

    // Same answer streamed three ways: per-character (worst-case churn), in
    // 5-char chunks, and as a single delta.
    let (log_per_char, text_a, renders_a) = run_chunked(answer, 1).await;
    let (log_chunks, text_b, renders_b) = run_chunked(answer, 5).await;
    let (log_one, text_c, renders_c) = run_chunked(answer, 0).await;

    // 1. The user sees identical text no matter how it was chunked/coalesced.
    assert_eq!(text_a, answer);
    assert_eq!(text_b, answer);
    assert_eq!(text_c, answer);

    // 2. Coalescing actually batched the render: 44 per-character deltas became
    //    a single render (one contiguous text run, flushed once at turn end).
    assert!(
        renders_a < answer.chars().count() as u32,
        "per-char stream should be coalesced into fewer renders, got {renders_a}"
    );
    assert_eq!(renders_a, 1, "contiguous text coalesces to one render");
    assert_eq!(renders_b, 1);
    assert_eq!(renders_c, 1);

    // 3. The consolidated logical records are byte-for-byte identical across all
    //    three chunkings — exactly one `ModelOutput` per call, no per-delta
    //    entries. This is what makes replay bit-for-bit independent of batching.
    let logical_a = logical_records(&log_per_char);
    let logical_b = logical_records(&log_chunks);
    let logical_c = logical_records(&log_one);
    assert_eq!(logical_a, logical_b);
    assert_eq!(logical_a, logical_c);

    // 4. The log holds exactly one consolidated model output — never one record
    //    per delta (deltas are transport, never durable; ARCHITECTURE §4.5).
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
