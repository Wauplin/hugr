//! End-to-end Phase 1 test: a genuine multi-turn session driven through the
//! real tokio [`Engine`] loop, using a scripted mock model adapter and the
//! *real* shell capability. This exercises the whole path
//! (user → model → tool → model → done) without a network or API key.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hugr_core::{
    ContentPart, ContextDisposition, DoneReason, HookPhase, ModelOutput, ModelRequest,
    ModelSelector, OpOutcome, OutputEvent, Record, Role, ToolCall, ToolSchema, Usage, Value,
};
use hugr_host::capabilities::Shell;
use hugr_host::mcp::{self, McpServerConfig};
use hugr_host::policy::{AllowAll, AutoApprove, DenyAll};
use hugr_host::{
    Capability, CheckpointCadence, ChunkSink, CronExpr, Engine, Frontend, ModelAdapter, ModelSink,
    Policy, Schedule, TriggerTarget, spend_report,
};
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
    fn on_model_end(&mut self, _op: hugr_core::OpId, usage: &Usage) {
        self.model_usage.lock().unwrap().push(usage.clone());
    }
    fn on_tool_end(
        &mut self,
        _op: hugr_core::OpId,
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

fn deterministic_clock() -> hugr_host::Clock {
    let counter = Arc::new(AtomicU64::new(1));
    Arc::new(move || counter.fetch_add(1, Ordering::SeqCst))
}

fn count_tool_results(log: &[hugr_core::LogEntry]) -> Vec<(String, serde_json::Value)> {
    log.iter()
        .filter_map(|e| match &e.record {
            Record::ToolResult { name, result, .. } => Some((name.clone(), result.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn builtin_pre_tool_and_stop_hooks_are_recorded_in_trace() {
    let model = MockModel::new(vec![
        ModelOutput::tool_calls(vec![ToolCall::new(
            "call-1",
            "shell",
            json!({ "cmd": "printf hook-test" }),
        )]),
        ModelOutput::text("done"),
    ]);
    let mut engine = Engine::builder()
        .model(ModelSelector::named("medium"), model.clone())
        .capability(Arc::new(Shell))
        .policy(Arc::new(AllowAll))
        .clock(deterministic_clock())
        .record(true)
        .frontend(Box::new(Capture::default()))
        .build();

    engine.user_turn("run a command".to_string()).await;
    let trace = engine.trace().expect("recorded trace");
    let hooks: Vec<_> = trace
        .log
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::Hook { phase, name, .. } => Some((phase.clone(), name.clone())),
            _ => None,
        })
        .collect();

    assert!(hooks.contains(&(HookPhase::SessionStart, "builtin_session_start".to_string())));
    assert!(hooks.contains(&(HookPhase::PreTool, "builtin_pre_tool".to_string())));
    assert!(hooks.contains(&(HookPhase::PostTool, "builtin_post_tool".to_string())));
    assert!(hooks.contains(&(HookPhase::Stop, "builtin_stop".to_string())));

    let requests = model.requests.lock().unwrap();
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

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}

#[tokio::test]
async fn multi_turn_session_with_real_shell() {
    let capture = Capture::default();

    // Turn 1 needs two model calls (tool, then final). Turn 2 needs one.
    let model = MockModel::new([
        ModelOutput::tool_calls(vec![ToolCall::new(
            "call-1",
            "shell",
            json!({ "cmd": "echo hello-from-hugr" }),
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
            .contains("hello-from-hugr"),
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
        Some(hugr_core::Role::System)
    ));
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

#[tokio::test]
async fn context_plan_inspection_and_manual_compaction_feed_next_request() {
    let capture = Capture::default();
    let model = MockModel::new([
        ModelOutput::text("Old answer."),
        ModelOutput::text("Old turn summary."),
        ModelOutput::text("Next answer."),
    ]);

    let mut engine = Engine::builder()
        .model(ModelSelector::named("medium"), model.clone())
        .model(ModelSelector::named("small"), model.clone())
        .default_model(ModelSelector::named("medium"))
        .frontend(Box::new(capture))
        .clock(deterministic_clock())
        .build();

    engine
        .user_turn("old details that can be summarized".into())
        .await;
    let before = engine.context_plan();
    assert!(
        before
            .entries
            .iter()
            .any(|entry| matches!(entry.disposition, ContextDisposition::Included { .. }))
    );
    let before_used = before.totals.used_tokens;

    engine.compact_context().await;
    let after = engine.context_plan();
    assert!(
        after.totals.used_tokens < before_used,
        "manual compaction should reduce the planned request size"
    );
    assert!(
        after
            .entries
            .iter()
            .any(|entry| matches!(entry.disposition, ContextDisposition::Referenced { .. }))
    );
    assert!(
        after
            .entries
            .iter()
            .any(|entry| matches!(entry.disposition, ContextDisposition::Summarized { .. }))
    );

    engine.user_turn("next request".into()).await;
    let requests = model.requests.lock().unwrap();
    let next_request = requests.last().expect("next turn model request recorded");
    assert!(next_request.blocks.iter().any(|block| {
        block.content.iter().any(|part| {
            matches!(
                part,
                ContentPart::Ref { reference, .. } if reference == "log:0"
            )
        })
    }));
    assert!(next_request.blocks.iter().any(|block| {
        block.content.iter().any(|part| {
            matches!(
                part,
                ContentPart::Text(text) if text.contains("Old turn summary.")
            )
        })
    }));
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

struct JudgeModel {
    calls: Arc<Mutex<u32>>,
}

impl JudgeModel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Arc::new(Mutex::new(0)),
        })
    }
}

#[async_trait]
impl ModelAdapter for JudgeModel {
    async fn call(
        &self,
        request: ModelRequest,
        _sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        *self.calls.lock().unwrap() += 1;
        let body = request
            .blocks
            .iter()
            .flat_map(|b| &b.content)
            .map(|part| match part {
                hugr_core::ContentPart::Text(text) => text.as_str(),
                _ => "",
            })
            .collect::<String>();
        let verdict = if body.contains("rm -rf") {
            json!({ "safe": false, "reason": "destructive shell command" })
        } else {
            json!({ "safe": true, "reason": "benign bounded command" })
        };
        Ok((ModelOutput::text(verdict.to_string()), Usage::new(3, 2)))
    }
}

#[tokio::test]
async fn auto_approve_denies_risky_shell_and_replay_uses_recorded_verdict() {
    let capture = Capture::default();
    let judge = JudgeModel::new();
    let model = MockModel::new([
        ModelOutput::tool_calls(vec![ToolCall::new(
            "call-1",
            "shell",
            json!({ "cmd": "rm -rf ." }),
        )]),
        ModelOutput::text("I will choose a safer approach."),
    ]);

    let mut engine = Engine::builder()
        .model(ModelSelector::named("medium"), model)
        .capability(Arc::new(Shell))
        .policy(Arc::new(AutoApprove::new(judge.clone())) as Arc<dyn Policy>)
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .record(true)
        .build();

    engine.user_turn("clean the repo".into()).await;

    let tool_results = count_tool_results(engine.brain().state().log());
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].1["error"], json!("permission_denied"));
    assert_eq!(
        tool_results[0].1["reason"],
        json!("destructive shell command")
    );
    assert_eq!(*judge.calls.lock().unwrap(), 1);
    assert!(capture.text.lock().unwrap().contains("safer approach"));

    let trace = engine.trace().expect("recording enabled");
    hugr_replay::verify(&trace).expect("replay should reuse PermissionDecision events");
}

#[tokio::test]
async fn auto_approve_allows_benign_shell() {
    let judge = JudgeModel::new();
    let model = MockModel::new([
        ModelOutput::tool_calls(vec![ToolCall::new(
            "call-1",
            "shell",
            json!({ "cmd": "echo auto-approve-ok" }),
        )]),
        ModelOutput::text("The command was safe."),
    ]);

    let mut engine = Engine::builder()
        .model(ModelSelector::named("medium"), model)
        .capability(Arc::new(Shell))
        .policy(Arc::new(AutoApprove::new(judge.clone())) as Arc<dyn Policy>)
        .clock(deterministic_clock())
        .build();

    engine.user_turn("run a harmless command".into()).await;

    let tool_results = count_tool_results(engine.brain().state().log());
    assert_eq!(tool_results.len(), 1);
    assert!(
        tool_results[0].1["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("auto-approve-ok")
    );
    assert_eq!(*judge.calls.lock().unwrap(), 1);
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

    let spend = spend_report(engine.brain().state().log());
    let big = spend
        .tiers
        .iter()
        .find(|tier| tier.selector == ModelSelector::named("big"))
        .expect("big tier spend is queryable from OpMeta");
    assert_eq!(big.model_calls, 2);
    assert_eq!(big.input_tokens, 200);
    assert_eq!(big.output_tokens, 100);
    assert!((big.cost.unwrap_or_default() - 0.0020).abs() < f64::EPSILON);
    assert!(
        spend
            .recent_routing
            .iter()
            .any(|decision| decision.selector == ModelSelector::named("big")),
        "routing reasons should be queryable from the log"
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
async fn run_chunked(answer: &str, chunk_size: usize) -> (Vec<hugr_core::LogEntry>, String, u32) {
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
fn logical_records(log: &[hugr_core::LogEntry]) -> Vec<Record> {
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

/// P3-3 / Phase 3 exit criterion: record a real session through the engine,
/// save it to a trace, reload it, replay it through a fresh brain, and assert
/// the reconstructed command sequence + log is byte-identical to the original.
#[tokio::test]
async fn record_then_replay_reconstructs_the_session_bit_for_bit() {
    let capture = Capture::default();

    // A session with a tool op: model calls shell, then gives a final answer.
    let model = MockModel::new([
        ModelOutput::tool_calls(vec![ToolCall::new(
            "call-1",
            "shell",
            json!({ "cmd": "echo replay-me" }),
        )]),
        ModelOutput::text("The shell printed replay-me."),
    ]);

    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), model)
        .capability(Arc::new(Shell))
        .system_prompt("You are a test agent.")
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .record(true) // <-- capture the ordered event stream
        .build();

    engine.user_turn("greet me using the shell".into()).await;
    engine.session_end();

    // The live session's durable log (the truth we will replay against).
    let live_log = engine.brain().state().log().to_vec();
    assert!(!live_log.is_empty());

    // Save the recorded trace to disk and reload it.
    let dir = std::env::temp_dir().join(format!("hugr-host-replay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.trace.json");
    engine.save_trace(&path).expect("recording was enabled");
    let trace = hugr_host::Trace::load(&path).expect("reload the trace");

    // The recorded log matches the live log exactly (recorder read it from the
    // brain at save time; no desync).
    assert_eq!(trace.log, live_log, "recorded log must equal the live log");

    // Replay through a FRESH brain (no engine, no IO): the reconstructed log is
    // byte-identical to the original recording — the exit criterion.
    let replay = hugr_host::hugr_replay::verify(&trace)
        .expect("replay must reconstruct the recorded log bit-for-bit");
    assert_eq!(
        replay.log, live_log,
        "replayed log == live log, bit-for-bit"
    );

    // Determinism: a second replay yields identical commands.
    let again = hugr_host::hugr_replay::replay(&trace);
    assert_eq!(
        replay.commands, again.commands,
        "re-feeding identical events must yield identical commands"
    );

    // The reconstructed command sequence is the real agentic turn loop: it
    // opens with a model call, runs the shell capability, and ends with Done.
    use hugr_core::Command;
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
            .any(|c| matches!(c, Command::StartCapability { name, .. } if name == "shell")),
        "the shell capability was invoked"
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
    let mut inspector = hugr_host::Inspector::new(&trace);
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

/// P3-4 / Phase 3 exit criterion: record a session, save it, then **resume**
/// from the trace and add a NEW user turn. The resumed engine rebuilds its brain
/// from the trace's recorded events with zero IO (the original model/shell calls
/// are *not* re-run), continues recording, and re-saving yields a trace whose log
/// contains BOTH the original records AND the new turn's — and which still
/// replays bit-for-bit.
#[tokio::test]
async fn resume_from_trace_continues_the_session() {
    // --- Session 1: record an original session with a tool op, then save it. --
    let model1 = MockModel::new([
        ModelOutput::tool_calls(vec![ToolCall::new(
            "call-1",
            "shell",
            json!({ "cmd": "echo resume-me" }),
        )]),
        ModelOutput::text("The shell printed resume-me."),
    ]);

    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), model1)
        .capability(Arc::new(Shell))
        .system_prompt("You are a test agent.")
        .frontend(Box::new(Capture::default()))
        .clock(deterministic_clock())
        .record(true)
        .build();

    engine.user_turn("greet me using the shell".into()).await;
    engine.session_end();

    let original_log = engine.brain().state().log().to_vec();
    let original_logical = logical_records(&original_log);
    assert!(!original_logical.is_empty());

    let dir = std::env::temp_dir().join(format!("hugr-host-resume-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.trace.json");
    engine.save_trace(&path).expect("recording was enabled");
    let saved = hugr_host::Trace::load(&path).expect("reload the trace");
    let original_event_count = saved.events.len();

    // --- Session 2: resume from the trace and add a NEW user turn. ------------
    let capture2 = Capture::default();
    // A *fresh* model with only the new turn's responses: if resume re-ran the
    // recorded model calls this mock would be exhausted (proving no IO replay).
    let model2 = MockModel::new([ModelOutput::text("You're welcome!")]);

    let mut resumed = Engine::builder()
        .model(ModelSelector::named("big"), model2.clone())
        .capability(Arc::new(Shell))
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

    // --- Re-save the grown session: it still replays bit-for-bit. ------------
    let path2 = dir.join("session.resumed.trace.json");
    resumed
        .save_trace(&path2)
        .expect("resume implies recording");
    let regrown = hugr_host::Trace::load(&path2).expect("reload the grown trace");

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
    let replay = hugr_host::hugr_replay::verify(&regrown)
        .expect("the resumed session must replay bit-for-bit");
    assert_eq!(replay.log, grown_log);

    std::fs::remove_dir_all(&dir).ok();
}

/// A model call that starts, streams a partial delta, and then never returns.
/// Dropping its task simulates killing the process mid-turn.
struct PendingModel {
    started: tokio::sync::mpsc::UnboundedSender<()>,
}

#[async_trait]
impl ModelAdapter for PendingModel {
    async fn call(
        &self,
        _request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        sink.text("partial answer");
        let _ = self.started.send(());
        let () = std::future::pending().await;
        unreachable!("pending model never completes")
    }
}

async fn wait_for_checkpoint_with_delta(path: &std::path::Path) -> hugr_host::Trace {
    for _ in 0..100 {
        if let Ok(trace) = hugr_host::Trace::load(path) {
            if trace
                .events
                .iter()
                .any(|event| matches!(event, hugr_core::Event::ModelDelta { .. }))
            {
                return trace;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "checkpoint never captured a model delta at {}",
        path.display()
    );
}

/// Phase 7 exit criterion: a checkpoint written during an in-flight model call
/// can be loaded after a simulated crash. The resumed host records the
/// conservative crash policy (`OpCancelled` for the stale op), rebuilds to an
/// idle brain, and can continue with a new turn from the same trace.
#[tokio::test]
async fn durable_checkpoint_resumes_after_mid_turn_crash() {
    let dir = std::env::temp_dir().join(format!("hugr-host-crash-resume-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.trace.json");

    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let pending = Arc::new(PendingModel {
        started: started_tx,
    });

    let path_for_task = path.clone();
    let handle = tokio::spawn(async move {
        let mut engine = Engine::builder()
            .model(ModelSelector::named("big"), pending)
            .frontend(Box::new(Capture::default()))
            .clock(deterministic_clock())
            .checkpoint(path_for_task, CheckpointCadence::EveryEvent)
            .build();
        engine.user_turn("start then crash".into()).await;
    });

    started_rx.recv().await.expect("pending model should start");
    let saved = wait_for_checkpoint_with_delta(&path).await;
    handle.abort();

    let continued_model = MockModel::new([ModelOutput::text("continued after crash")]);
    let mut resumed = Engine::builder()
        .model(ModelSelector::named("big"), continued_model.clone())
        .frontend(Box::new(Capture::default()))
        .clock(deterministic_clock())
        .resume(saved)
        .checkpoint(path.clone(), CheckpointCadence::EveryEvent)
        .build();

    assert_eq!(
        resumed.brain().state().inflight_len(),
        0,
        "crash resume cancels stale in-flight ops before going live"
    );
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
    resumed.session_end();
    resumed.save_trace(&path).expect("checkpointed trace saves");

    let grown = hugr_host::Trace::load(&path).expect("reload grown crash trace");
    assert!(
        grown.log.iter().any(|entry| matches!(
            &entry.record,
            Record::UserMessage { text, .. } if text == "continue"
        )),
        "continued prompt is in the durable log"
    );
    hugr_host::hugr_replay::verify(&grown).expect("grown crash trace replays");

    std::fs::remove_dir_all(&dir).ok();
}

/// Phase 7 exit criterion: a host-side scheduled trigger fires a prompt into a
/// named persistent session. The second fire resumes the same trace and appends
/// a second user turn.
#[tokio::test]
async fn scheduled_trigger_fires_into_named_persistent_session() {
    let dir = std::env::temp_dir().join(format!("hugr-host-scheduler-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let model = MockModel::new([
        ModelOutput::text("first scheduled answer"),
        ModelOutput::text("second scheduled answer"),
    ]);
    let schedule = Schedule::new(
        CronExpr::parse("@every 1s").unwrap(),
        TriggerTarget::NamedPersistent {
            dir: dir.clone(),
            name: "nightly".to_string(),
        },
        "scheduled prompt",
    );

    for _ in 0..2 {
        hugr_host::fire_once(
            Engine::builder()
                .model(ModelSelector::named("big"), model.clone())
                .frontend(Box::new(Capture::default()))
                .clock(deterministic_clock()),
            &schedule,
        )
        .await
        .expect("scheduled fire succeeds");
    }

    let path = dir.join("nightly.trace.json");
    let trace = hugr_host::Trace::load(&path).expect("named session trace exists");
    let scheduled_messages = trace
        .log
        .iter()
        .filter(|entry| {
            matches!(
                &entry.record,
                Record::UserMessage { text, .. } if text == "scheduled prompt"
            )
        })
        .count();
    assert_eq!(scheduled_messages, 2);
    hugr_host::hugr_replay::verify(&trace).expect("scheduled trace replays");

    std::fs::remove_dir_all(&dir).ok();
}

/// A model that routes deterministically off the projected context (not off a
/// call counter), so it behaves identically no matter how the *concurrent*
/// sub-agent calls interleave. Three behaviours:
///
/// - context already has a tool result  → a final answer (the parent's join turn);
/// - last user text starts with "worker" → that child's answer (a sub-agent turn);
/// - otherwise                           → fan out to two `task` sub-agents.
struct RoutingModel;

#[async_trait]
impl ModelAdapter for RoutingModel {
    async fn call(
        &self,
        request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        use hugr_core::{ContentPart, Role};

        let has_tool_result = request.blocks.iter().any(|b| {
            b.content
                .iter()
                .any(|p| matches!(p, ContentPart::ToolResult { .. }))
        });
        let last_user_text = request
            .blocks
            .iter()
            .rev()
            .filter(|b| b.role == Role::User)
            .find_map(|b| {
                b.content.iter().find_map(|p| match p {
                    ContentPart::Text(t) => Some(t.clone()),
                    _ => None,
                })
            })
            .unwrap_or_default();

        let output = if has_tool_result {
            ModelOutput::text("Both workers finished.")
        } else if last_user_text.starts_with("worker") {
            ModelOutput::text(format!("{last_user_text} done"))
        } else {
            ModelOutput::tool_calls(vec![
                ToolCall::new("a", "task", json!({ "prompt": "worker-A" })),
                ToolCall::new("b", "task", json!({ "prompt": "worker-B" })),
            ])
        };
        if !output.text.is_empty() {
            sink.text(output.text.clone());
        }
        Ok((output, Usage::new(1, 1)))
    }
}

/// Phase 6 exit criterion: a parent agent fans out to N child agents (fork-shared
/// context), collects their results, and the whole tree replays deterministically
/// from one recorded trace. Driven through the **real** tokio engine — each child
/// is its own brain on its own task, reusing the parent's model registry.
#[tokio::test]
async fn parent_fans_out_to_sub_agents_and_replays() {
    use hugr_core::AgentSeed;

    let capture = Capture::default();
    let task_schema = ToolSchema::new(
        "task",
        "Delegate a unit of work to a sub-agent.",
        json!({ "type": "object", "properties": { "prompt": { "type": "string" } } }),
    );

    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), Arc::new(RoutingModel))
        // `task` is a sub-agent tool: invoking it spawns a child seeded with the
        // parent's full context (ForkFull).
        .agent(task_schema, AgentSeed::ForkFull)
        .frontend(Box::new(capture.clone()))
        .clock(deterministic_clock())
        .record(true)
        .build();

    engine.user_turn("fan out to two workers".into()).await;
    engine.session_end();

    // Both sub-agents returned, folded back as `task` tool results in the log.
    let tool_results = count_tool_results(engine.brain().state().log());
    assert_eq!(tool_results.len(), 2, "two sub-agents returned results");
    assert!(tool_results.iter().all(|(name, _)| name == "task"));
    let child_texts: Vec<String> = tool_results
        .iter()
        .filter_map(|(_, r)| r["text"].as_str().map(String::from))
        .collect();
    assert!(child_texts.contains(&"worker-A done".to_string()));
    assert!(child_texts.contains(&"worker-B done".to_string()));

    // The parent turn ended once, after both children joined.
    let dones = capture.done.lock().unwrap();
    assert_eq!(dones.len(), 1);
    assert!(matches!(dones[0], DoneReason::EndTurn));
    drop(dones);
    assert!(
        capture
            .text
            .lock()
            .unwrap()
            .contains("Both workers finished.")
    );

    // The whole tree replays bit-for-bit from the one parent trace: the recorded
    // `AgentDone` results drive the fold, so a fresh brain reconstructs the same
    // parent commands + log without re-running any child (ARCHITECTURE §13.3).
    let trace = engine.trace().expect("recording was enabled");
    let replay =
        hugr_host::hugr_replay::verify(&trace).expect("the agent tree must replay bit-for-bit");
    assert_eq!(replay.log, engine.brain().state().log());
    // The reconstruction shows the parent spawning two agents then finishing.
    use hugr_core::Command;
    let start_agents = replay
        .commands
        .iter()
        .filter(|c| matches!(c, Command::StartAgent { .. }))
        .count();
    assert_eq!(start_agents, 2, "the parent spawned two sub-agents");
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
