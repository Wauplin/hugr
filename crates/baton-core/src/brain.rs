//! The brain: `poll()` + `submit()` + the reducer.
//!
//! This is the entire integration surface a host needs:
//!
//! ```text
//!     loop {
//!         for cmd in brain.poll() { host.perform(cmd); }   // drain commands
//!         let event = host.next_event().await;             // the only await (host-side)
//!         brain.submit(event);                             // pure, instant, no IO
//!     }
//! ```
//!
//! `poll()` and `submit()` are synchronous and pure — a WASM/Python/JS binding
//! calls them directly. All the agentic control flow lives in [`Brain::submit`].

use serde_json::json;

use crate::command::{Command, DoneReason, OutputEvent, PermissionRequest};
use crate::event::{Decision, Event, SteerMode, VersionRef};
use crate::model::{ModelDelta, ModelOutput, ToolCall, Usage};
use crate::policy::{StaticPolicy, TurnPolicy};
use crate::primitives::{OpId, Value};
use crate::record::{LogEntry, OpMeta, OpOutcome, Record};
use crate::state::{BrainState, OpKind};

/// The pure, sans-IO agent core. Construct one with a [`TurnPolicy`], feed it
/// [`Event`]s with [`submit`](Brain::submit), and drain [`Command`]s with
/// [`poll`](Brain::poll).
pub struct Brain {
    state: BrainState,
    policy: Box<dyn TurnPolicy>,
}

impl Brain {
    /// Create a brain with a custom [`TurnPolicy`].
    pub fn new(policy: Box<dyn TurnPolicy>) -> Self {
        Self {
            state: BrainState::new(),
            policy,
        }
    }

    /// Create a brain with the default [`StaticPolicy`] (trivial pass-through
    /// projection, no permissions, no tools).
    pub fn with_default_policy() -> Self {
        Self::new(Box::new(StaticPolicy::default()))
    }

    /// Read-only access to the brain's derived state (log, op table, …).
    pub fn state(&self) -> &BrainState {
        &self.state
    }

    /// Drain the commands the brain wants the host to perform. Pure, instant.
    pub fn poll(&mut self) -> Vec<Command> {
        self.state.drain_commands()
    }

    /// Feed one event in. Pure, instant, no IO. The single entry point for all
    /// of the brain's logic.
    pub fn submit(&mut self, event: Event) {
        match event {
            Event::UserInput { content, mode } => self.on_user_input(content, mode),
            Event::UserAbort => self.cancel_all_inflight(),

            Event::ModelDelta { op, delta } => self.on_model_delta(op, delta),
            Event::ModelDone { op, output, usage } => self.on_model_done(op, output, usage),
            Event::ModelError { op, error } => self.on_model_error(op, error),

            Event::CapabilityChunk { op, chunk } => {
                self.emit(OutputEvent::ToolChunk { op, chunk });
            }
            Event::CapabilityDone {
                op,
                result,
                version,
            } => self.on_capability_done(op, result, version),
            Event::CapabilityError {
                op,
                error,
                conflict,
            } => self.on_capability_error(op, error, conflict),

            Event::AgentDone { op, result } => self.on_agent_done(op, result),
            Event::AgentError { op, error } => self.on_agent_error(op, error),

            Event::UserAnswer { op, answer } => self.on_user_answer(op, answer),
            Event::PermissionDecision { op, decision } => self.on_permission_decision(op, decision),

            Event::OpCancelled { op } => self.on_op_cancelled(op),

            Event::Tick { now } => *self.state.now_mut() = now,
        }
    }

    // ========================================================================
    // Event handlers
    // ========================================================================

    fn on_user_input(&mut self, content: Value, mode: SteerMode) {
        self.append(Record::UserMessage {
            text: stringify(&content),
        });

        if !self.state.is_busy() {
            // Idle: start a turn immediately, regardless of mode.
            self.start_model_turn();
            return;
        }

        match mode {
            // Append now; the next turn boundary picks it up (its projection
            // sees the new message). No new mechanism needed.
            SteerMode::Queue | SteerMode::AppendAndContinue => {}
            // Cancel in-flight ops; once they drain (partial work logged first),
            // a fresh turn starts that sees both the partial work and the input.
            SteerMode::Interrupt => {
                self.state.set_pending_resume(true);
                self.cancel_all_inflight();
            }
        }
    }

    fn on_model_delta(&mut self, op: OpId, delta: ModelDelta) {
        // Deltas are transport only: accumulate cheaply for live UI and forward
        // a cosmetic event. Never written to the log (ARCHITECTURE §4.5).
        match &delta {
            ModelDelta::Text(t) => {
                self.state.buffer_model_text(op, t);
                self.emit(OutputEvent::ModelText {
                    op,
                    text: t.clone(),
                });
            }
            ModelDelta::Reasoning(t) => {
                self.emit(OutputEvent::ModelReasoning {
                    op,
                    text: t.clone(),
                });
            }
            ModelDelta::ToolCallStart { id, name } => {
                self.emit(OutputEvent::ToolCallStarted {
                    op,
                    id: id.clone(),
                    name: name.clone(),
                });
            }
            ModelDelta::ToolCallArgsDelta { .. } | ModelDelta::ToolCallEnd { .. } => {}
        }
    }

    fn on_model_done(&mut self, op: OpId, output: ModelOutput, usage: Usage) {
        self.append(Record::ModelOutput {
            op,
            output: output.clone(),
        });
        self.end_op(op, OpOutcome::Ok, Some(usage));

        if output.tool_calls.is_empty() {
            // A final answer with no tool calls ends the turn.
            self.checkpoint();
            self.done(DoneReason::EndTurn);
        } else {
            // The model wants tools: turn each call into an op. The brain routes;
            // it never interprets the args.
            for call in output.tool_calls {
                self.begin_tool_call(call);
            }
        }
    }

    fn on_model_error(&mut self, op: OpId, error: Value) {
        // A transport error the host already gave up on. Record it and end the
        // turn; a richer policy could decide to retry/route differently.
        self.end_op(op, OpOutcome::Error(error.clone()), None);
        self.done(DoneReason::Error(stringify(&error)));
    }

    fn on_capability_done(&mut self, op: OpId, result: Value, version: Option<VersionRef>) {
        if let Some(v) = version {
            self.state.record_version(v.object, v.version);
        }
        let (name, call_id) = self.tool_ids(op);
        self.append(Record::ToolResult {
            op,
            name,
            call_id,
            result,
        });
        self.end_op(op, OpOutcome::Ok, None);
        self.maybe_resume_model_turn();
    }

    fn on_capability_error(&mut self, op: OpId, error: Value, conflict: Option<VersionRef>) {
        // A stale-edit conflict refreshes the read-set so the model's next edit
        // is stamped correctly; otherwise it is an ordinary error result fed
        // back to the model (ARCHITECTURE §5.4, §7.3).
        if let Some(v) = conflict {
            self.state.record_version(v.object, v.version);
        }
        let (name, call_id) = self.tool_ids(op);
        self.append(Record::ToolResult {
            op,
            name,
            call_id,
            result: error.clone(),
        });
        self.end_op(op, OpOutcome::Error(error), None);
        self.maybe_resume_model_turn();
    }

    fn on_agent_done(&mut self, op: OpId, result: Value) {
        // A sub-agent result returns to the parent as a tool-result-shaped value
        // (ARCHITECTURE §13.1). Full sub-agent support lands in Phase 6.
        let (name, call_id) = self.tool_ids(op);
        self.append(Record::ToolResult {
            op,
            name,
            call_id,
            result,
        });
        self.end_op(op, OpOutcome::Ok, None);
        self.maybe_resume_model_turn();
    }

    fn on_agent_error(&mut self, op: OpId, error: Value) {
        let (name, call_id) = self.tool_ids(op);
        self.append(Record::ToolResult {
            op,
            name,
            call_id,
            result: error.clone(),
        });
        self.end_op(op, OpOutcome::Error(error), None);
        self.maybe_resume_model_turn();
    }

    fn on_user_answer(&mut self, op: OpId, answer: Value) {
        // The answer to an `AskUser` becomes a tool-result-shaped value the next
        // model turn consumes.
        let (name, call_id) = self.tool_ids(op);
        self.append(Record::ToolResult {
            op,
            name,
            call_id,
            result: answer,
        });
        self.end_op(op, OpOutcome::Ok, None);
        self.maybe_resume_model_turn();
    }

    fn on_permission_decision(&mut self, op: OpId, decision: Decision) {
        match decision {
            Decision::Allow => {
                // Resume the stashed tool call, reusing the same op id.
                if let Some(op_state) = self.state.remove_op(op) {
                    if let OpKind::AwaitingPermission {
                        name,
                        args,
                        call_id,
                    } = op_state.kind
                    {
                        self.start_capability(op, name, args, call_id);
                    }
                }
            }
            Decision::Deny { reason } => {
                let (name, call_id) = self.tool_ids(op);
                let result = json!({ "error": "permission_denied", "reason": reason });
                self.append(Record::ToolResult {
                    op,
                    name,
                    call_id,
                    result: result.clone(),
                });
                self.end_op(op, OpOutcome::Error(result), None);
                self.maybe_resume_model_turn();
            }
        }
    }

    fn on_op_cancelled(&mut self, op: OpId) {
        let partial = self.partial_of(op);
        self.end_op(op, OpOutcome::Cancelled { partial }, None);

        // If an interrupt is waiting for the in-flight ops to drain, start the
        // fresh turn now that they have (so the partial work is already logged).
        if self.state.pending_resume() && !self.state.is_busy() {
            self.state.set_pending_resume(false);
            self.start_model_turn();
        }
    }

    // ========================================================================
    // Turn-loop helpers
    // ========================================================================

    /// Begin a model turn: ask the policy which model to call and how to project
    /// context, then emit the call.
    fn start_model_turn(&mut self) {
        let op = self.state.alloc_op();
        let selector = self.policy.choose_model(&self.state);
        let request = self.policy.project_context(self.state.log());
        self.state.mark(
            op,
            OpKind::Model {
                selector: selector.clone(),
                text_so_far: String::new(),
            },
        );
        self.state.push_command(Command::StartModelCall {
            op,
            model: selector,
            request,
        });
    }

    /// After a tool/agent op resolves, resume the model turn once nothing the
    /// turn is waiting on remains in flight.
    fn maybe_resume_model_turn(&mut self) {
        let still_waiting = self.state.inflight().values().any(|o| o.kind.blocks_turn());
        if !still_waiting {
            self.start_model_turn();
        }
    }

    /// Turn one model-requested tool call into an op: either gate it on
    /// permission, or start it immediately.
    fn begin_tool_call(&mut self, call: ToolCall) {
        let op = self.state.alloc_op();
        if self.policy.needs_permission(&call.name) {
            self.state.mark(
                op,
                OpKind::AwaitingPermission {
                    name: call.name.clone(),
                    args: call.args.clone(),
                    call_id: call.id.clone(),
                },
            );
            self.state.push_command(Command::RequestPermission {
                op,
                request: PermissionRequest {
                    capability: call.name,
                    args: call.args,
                },
            });
        } else {
            self.start_capability(op, call.name, call.args, call.id);
        }
    }

    fn start_capability(&mut self, op: OpId, name: String, args: Value, call_id: String) {
        // Seam for optimistic-concurrency stamping (ARCHITECTURE §7.3): when a
        // capability's schema declares it mutates a versioned object, the brain
        // would stamp `expected_version` from `self.state.versions()` here. The
        // declarative schema metadata that drives it arrives in a later phase;
        // for Phase 0 args are forwarded verbatim.
        self.state.mark(
            op,
            OpKind::Capability {
                name: name.clone(),
                call_id,
            },
        );
        self.state
            .push_command(Command::StartCapability { op, name, args });
    }

    fn cancel_all_inflight(&mut self) {
        for op in self.state.inflight_op_ids() {
            self.state.push_command(Command::Cancel { op });
        }
    }

    // ========================================================================
    // Bookkeeping
    // ========================================================================

    fn append(&mut self, record: Record) {
        let seq = crate::primitives::Seq(self.state.alloc_seq());
        let at = self.state.now();
        self.state.push_log(LogEntry { seq, at, record });
    }

    /// End an op: build its [`OpMeta`] from the in-flight entry, remove it, and
    /// append an `OpEnded` record so latency/cost are queryable from the trace.
    fn end_op(&mut self, op: OpId, outcome: OpOutcome, usage: Option<Usage>) {
        let now = self.state.now();
        let (started_at, model) = match self.state.get_op(op) {
            Some(entry) => (entry.started_at, entry.kind.selector()),
            None => (now, None),
        };
        self.state.remove_op(op);
        let meta = OpMeta {
            started_at,
            ended_at: now,
            model,
            usage,
            extra: json!(null),
        };
        self.append(Record::OpEnded { op, outcome, meta });
    }

    /// The capability name and originating model `tool_call` id of an in-flight
    /// op, with placeholders if unknown.
    fn tool_ids(&self, op: OpId) -> (String, String) {
        match self.state.get_op(op) {
            Some(o) => (
                o.kind.capability_name().unwrap_or("<unknown>").to_string(),
                o.kind.call_id().unwrap_or_default().to_string(),
            ),
            None => ("<unknown>".to_string(), String::new()),
        }
    }

    /// Whatever a cancelled op produced so far (e.g. partial model text).
    fn partial_of(&self, op: OpId) -> Value {
        match self.state.get_op(op).map(|o| &o.kind) {
            Some(OpKind::Model { text_so_far, .. }) if !text_so_far.is_empty() => {
                Value::String(text_so_far.clone())
            }
            _ => Value::Null,
        }
    }

    fn emit(&mut self, event: OutputEvent) {
        self.state.push_command(Command::Emit(event));
    }

    fn checkpoint(&mut self) {
        self.state.push_command(Command::Checkpoint);
    }

    fn done(&mut self, reason: DoneReason) {
        self.state.push_command(Command::Done { reason });
    }
}

/// Render opaque user content to text for the log record. A string is taken
/// verbatim; anything else is JSON-encoded.
fn stringify(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
