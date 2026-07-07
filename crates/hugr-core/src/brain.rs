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
use crate::event::{Decision, Event};
use crate::model::{ContextPlan, ModelDelta, ModelOutput, ToolCall, Usage};
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

    /// Create a brain **seeded from an inherited log** — the fork primitive
    /// (ARCHITECTURE §14). A fork or a resumed session starts from a copy of a
    /// log prefix; the brain re-derives
    /// its state by folding it (§3.1). No IO: the recorded ops are not re-run,
    /// only re-folded to reconstruct `BrainState`.
    pub fn from_log(policy: Box<dyn TurnPolicy>, log: Vec<LogEntry>) -> Self {
        Self {
            state: BrainState::from_log(log),
            policy,
        }
    }

    /// Read-only access to the brain's derived state (log, op table, …).
    pub fn state(&self) -> &BrainState {
        &self.state
    }

    /// Inspect the context projection that the next normal model turn would
    /// render. Pure and synchronous: the same [`TurnPolicy`] hooks used by the
    /// reducer's turn-start path produce this plan.
    pub fn context_plan(&self) -> ContextPlan {
        let budget = self.policy.context_budget(&self.state);
        self.policy.project_context(self.state.log(), budget)
    }

    /// Drain the commands the brain wants the host to perform. Pure, instant.
    pub fn poll(&mut self) -> Vec<Command> {
        self.state.drain_commands()
    }

    /// Feed one event in. Pure, instant, no IO. The single entry point for all
    /// of the brain's logic.
    pub fn submit(&mut self, event: Event) {
        match event {
            Event::UserInput {
                content,
                est_tokens,
            } => self.on_user_input(content, est_tokens),
            Event::UserAbort => self.on_user_abort(),

            Event::ModelDelta { op, delta } => self.on_model_delta(op, delta),
            Event::ModelDone {
                op,
                output,
                usage,
                est_tokens,
            } => self.on_model_done(op, output, usage, est_tokens),
            Event::ModelError { op, error } => self.on_model_error(op, error),

            // Capability chunks are transport-only progress; nothing durable
            // and no reduced output — the host may render them itself.
            Event::CapabilityChunk { .. } => {}
            Event::CapabilityDone {
                op,
                result,
                est_tokens,
            } => self.on_capability_done(op, result, est_tokens),
            Event::CapabilityError {
                op,
                error,
                est_tokens,
            } => self.on_capability_error(op, error, est_tokens),

            Event::PermissionDecision {
                op,
                decision,
                est_tokens,
            } => self.on_permission_decision(op, decision, est_tokens),

            Event::OpCancelled { op } => self.on_op_cancelled(op),

            Event::Tick { now } => *self.state.now_mut() = now,
        }
    }

    // ========================================================================
    // Event handlers
    // ========================================================================

    fn on_user_input(&mut self, content: Value, est_tokens: u32) {
        self.append(Record::UserMessage {
            text: stringify(&content),
            est_tokens,
        });
        // Idle: start a turn immediately. Mid-turn input just queues: the next
        // turn boundary's projection sees the new message (ARCHITECTURE §4.6).
        if !self.state.is_busy() {
            self.start_model_turn();
        }
    }

    /// A pure control-signal abort (ARCHITECTURE §4.6). While ops are in flight
    /// this latches `abort_requested`: the `Cancel` commands race each op's own
    /// terminal event, and whichever arrives first must still end the turn
    /// `Cancelled` without starting new work (ARCHITECTURE §4.3). An idle abort
    /// stays a no-op.
    fn on_user_abort(&mut self) {
        if self.state.is_busy() {
            self.state.set_abort_requested(true);
            self.cancel_all_inflight();
        }
    }

    fn on_model_delta(&mut self, op: OpId, delta: ModelDelta) {
        // Deltas are transport only: accumulate cheaply for live UI and forward
        // a cosmetic event. Never written to the log (ARCHITECTURE §4.5).
        match delta {
            ModelDelta::Text(t) => {
                self.state.buffer_model_text(op, &t);
                self.emit(OutputEvent::ModelText { op, text: t });
            }
            // Reasoning and tool-call-start deltas produce no reduced output;
            // they only exist so adapters can stream uniformly.
            ModelDelta::Reasoning(_) | ModelDelta::ToolCallStart { .. } => {}
        }
    }

    fn on_model_done(&mut self, op: OpId, output: ModelOutput, usage: Usage, est_tokens: u32) {
        self.append(Record::ModelOutput {
            op,
            output: output.clone(),
            est_tokens,
        });
        self.end_op(op, OpOutcome::Ok, Some(usage));

        if self.state.abort_requested() {
            // A `UserAbort` raced this terminal event (ARCHITECTURE §4.3): the
            // op's `Cancel` is stale, but the abort must still win. Fold the
            // durable record but start no new work. Any requested tool calls
            // are never started; log a cancelled result for each so every
            // `tool_use` in the next projection still has a paired
            // `tool_result` (ARCHITECTURE §4.5).
            for call in output.tool_calls {
                self.cancel_unstarted_tool_call(call);
            }
            self.checkpoint();
            self.resolve_abort_if_drained();
            return;
        }

        if output.tool_calls.is_empty() {
            // A final answer with no tool calls ends the turn — unless a
            // background op is still running. In that case the turn isn't over:
            // when the background op finishes its result is folded in and a
            // fresh turn picks it up (ARCHITECTURE §6.3). We checkpoint either
            // way (the model output is durable) but defer `Done` until idle.
            self.checkpoint();
            if !self.background_running() {
                self.done(DoneReason::EndTurn);
            }
        } else {
            // The model wants tools: turn each call into an op. The brain routes;
            // it never interprets the args.
            for call in output.tool_calls {
                self.begin_tool_call(call);
            }
            // If every tool call this turn was a background op, nothing blocks
            // the turn — resume the model now so it streams *alongside* them
            // (ARCHITECTURE §6.3). Done once, after the whole fan-out, so a mix
            // of background + foreground calls still waits for the foreground.
            self.maybe_resume_model_turn();
        }
    }

    fn on_model_error(&mut self, op: OpId, error: Value) {
        // A transport error the host already gave up on. Record it and end the
        // turn; a richer policy could decide to retry/route differently.
        self.end_op(op, OpOutcome::Error(error.clone()), None);
        if self.resolve_abort_if_drained() {
            return;
        }
        // Mirror `on_model_done`: while a background op is still running the
        // turn is not over (ARCHITECTURE §4.2), so defer the terminal
        // `Done(Error)` until the last op drains rather than emitting commands
        // after a terminal `Done`.
        if self.background_running() {
            self.state.set_deferred_error(Some(stringify(&error)));
            return;
        }
        self.done(DoneReason::Error(stringify(&error)));
    }

    /// The shared tail of every tool-result-shaped resolution (capability,
    /// sub-agent, user answer, permission denial): append the *one*
    /// consolidated `ToolResult` record (ARCHITECTURE §4.5), end the op, and
    /// resume the turn.
    fn finish_tool_result(&mut self, op: OpId, result: Value, outcome: OpOutcome, est_tokens: u32) {
        let (name, call_id) = self.tool_ids(op);
        self.append(Record::ToolResult {
            op,
            name,
            call_id,
            result,
            est_tokens,
        });
        self.end_op(op, outcome, None);
        if self.resolve_abort_if_drained() {
            return;
        }
        if self.resolve_deferred_error_if_drained() {
            return;
        }
        self.maybe_resume_model_turn();
    }

    fn on_capability_done(&mut self, op: OpId, result: Value, est_tokens: u32) {
        self.finish_tool_result(op, result, OpOutcome::Ok, est_tokens);
    }

    fn on_capability_error(&mut self, op: OpId, error: Value, est_tokens: u32) {
        // A semantic tool failure is an ordinary error result fed back to the
        // model (ARCHITECTURE §5.4).
        self.finish_tool_result(op, error.clone(), OpOutcome::Error(error), est_tokens);
    }

    fn on_permission_decision(&mut self, op: OpId, decision: Decision, est_tokens: u32) {
        // Only an op actually awaiting permission may consume a decision. Peek
        // before removing: a stray/duplicate decision (e.g. a second `Allow`
        // after the capability already started, or a decision for an op that
        // already resolved) must be a no-op — never drop a live op from the
        // in-flight table (ARCHITECTURE §4.1).
        if !matches!(
            self.state.get_op(op).map(|entry| &entry.kind),
            Some(OpKind::AwaitingPermission { .. })
        ) {
            return;
        }
        match decision {
            Decision::Allow => {
                // A latched abort already sent this op a `Cancel`; do not start
                // the capability (no new work while aborting, ARCHITECTURE
                // §4.3) — the pending `OpCancelled` resolves the op instead.
                if self.state.abort_requested() {
                    return;
                }
                // Resume the stashed tool call, reusing the same op id.
                if let Some(op_state) = self.state.remove_op(op) {
                    if let OpKind::AwaitingPermission {
                        name,
                        args,
                        call_id,
                    } = op_state.kind
                    {
                        let background = self.policy.is_background(&name);
                        self.start_capability(op, name, args, call_id, background);
                        // A granted background op runs alongside the model:
                        // resume the turn now (no-op if other ops still block it,
                        // e.g. a sibling permission still pending).
                        if background {
                            self.maybe_resume_model_turn();
                        }
                    }
                }
            }
            Decision::Deny { reason } => {
                let result = json!({ "error": "permission_denied", "reason": reason });
                self.finish_tool_result(op, result.clone(), OpOutcome::Error(result), est_tokens);
            }
        }
    }

    fn on_op_cancelled(&mut self, op: OpId) {
        // Ignore a cancel confirmation for an op that already resolved. The host
        // aborts the task and emits `OpCancelled`, but the task may have queued
        // its real terminal event (e.g. `ModelDone`) a hair before the abort;
        // that event is folded first and removes the op. Without this guard the
        // stale `OpCancelled` would append a spurious `Cancelled` `OpEnded` and
        // break replay. Cancellation is idempotent (ARCHITECTURE §6.4).
        if self.state.get_op(op).is_none() {
            return;
        }

        // Log the partial work (e.g. "N tokens then cancelled") before removing
        // the op, so the trace never has an implicit gap (ARCHITECTURE §6.4).
        let partial = self.partial_of(op);
        // A cancelled *tool-shaped* op (capability / sub-agent / awaiting
        // permission) still owes the log a consolidated `ToolResult`: its
        // originating `tool_use` is projected from the `ModelOutput` record,
        // and chat formats require every tool_use to carry a paired
        // tool_result (ARCHITECTURE §4.5). Append the cancellation result
        // BEFORE the `OpEnded` so projection and replay stay well-formed —
        // without this, the next model request has a dangling tool_use and the
        // provider rejects it.
        if self
            .state
            .get_op(op)
            .is_some_and(|entry| entry.kind.call_id().is_some())
        {
            let (name, call_id) = self.tool_ids(op);
            let result = if partial.is_null() {
                json!({ "cancelled": true })
            } else {
                json!({ "cancelled": true, "partial": partial.clone() })
            };
            self.append(Record::ToolResult {
                op,
                name,
                call_id,
                result,
                est_tokens: 0,
            });
        }
        self.end_op(op, OpOutcome::Cancelled { partial }, None);

        if self.resolve_abort_if_drained() {
            return;
        }
        if self.resolve_deferred_error_if_drained() {
            return;
        }

        if !self.state.is_busy() {
            // A host-initiated cancel with nothing to resume and no abort
            // latched: the turn is over, cancelled. Emit the terminal `Done`
            // once the last in-flight op has drained so the front-end sees it.
            self.done(DoneReason::Cancelled);
        }
    }

    // ========================================================================
    // Turn-loop helpers
    // ========================================================================

    /// Begin a model turn: ask the policy which model to call and how to project
    /// context, then emit the call.
    fn start_model_turn(&mut self) {
        let budget = self.policy.context_budget(&self.state);
        let plan = self.policy.project_context(self.state.log(), budget);
        let op = self.state.alloc_op();
        let selector = self.policy.choose_model(&self.state);
        let request = plan.to_model_request();
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
    ///
    /// A **background** op is the subtle case: it does not block the turn while
    /// running, so the model may already have ended and the brain gone idle by
    /// the time it finishes. We must not start a fresh turn while a foreground
    /// op or a live model call is still going (that would double-call the model
    /// or strand the in-flight one); we only resume when the whole brain is
    /// otherwise idle, folding the background result in at the next boundary.
    fn maybe_resume_model_turn(&mut self) {
        // A latched abort or a deferred model error means the turn is ending,
        // not resuming (ARCHITECTURE §4.3): never start new work here.
        if self.state.abort_requested() || self.state.deferred_error().is_some() {
            return;
        }
        let blocked = self.state.inflight().values().any(|o| o.kind.blocks_turn());
        let model_running = self
            .state
            .inflight()
            .values()
            .any(|o| o.kind.is_model_call());
        if !blocked && !model_running {
            self.start_model_turn();
        }
    }

    /// Turn one model-requested tool call into an op: spawn a sub-agent (if the
    /// policy designates this capability as an agent), gate it on permission, or
    /// start it immediately.
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
            let background = self.policy.is_background(&call.name);
            self.start_capability(op, call.name, call.args, call.id, background);
        }
    }

    fn start_capability(
        &mut self,
        op: OpId,
        name: String,
        args: Value,
        call_id: String,
        background: bool,
    ) {
        self.state.mark(
            op,
            OpKind::Capability {
                name: name.clone(),
                call_id,
                background,
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

    /// Whether any background capability op is still running (ARCHITECTURE
    /// §4.2): while one is, the turn is not over and terminal `Done` is
    /// deferred until it resolves.
    fn background_running(&self) -> bool {
        self.state.inflight().values().any(|o| {
            matches!(
                o.kind,
                OpKind::Capability {
                    background: true,
                    ..
                }
            )
        })
    }

    /// Resolve a latched `UserAbort` (ARCHITECTURE §4.3/§4.6) after a terminal
    /// event folded: once the last in-flight op drains, emit the single
    /// terminal `Done(Cancelled)` and clear the latch. Returns `true` while
    /// the abort is latched — the caller must not resume the turn or start any
    /// new work.
    fn resolve_abort_if_drained(&mut self) -> bool {
        if !self.state.abort_requested() {
            return false;
        }
        if !self.state.is_busy() {
            self.state.set_abort_requested(false);
            self.done(DoneReason::Cancelled);
        }
        true
    }

    /// Resolve a deferred model-error `Done` (ARCHITECTURE §4.2): a model
    /// transport error with background ops still running defers its terminal
    /// `Done(Error)`; once the last op drains, emit it with the original
    /// reason. Returns `true` while a deferral is pending — the caller must
    /// not resume the turn.
    fn resolve_deferred_error_if_drained(&mut self) -> bool {
        if self.state.deferred_error().is_none() {
            return false;
        }
        if !self.state.is_busy() {
            if let Some(reason) = self.state.take_deferred_error() {
                self.done(DoneReason::Error(reason));
            }
        }
        true
    }

    /// A tool call the model requested but that will never start (its turn was
    /// aborted before fan-out, ARCHITECTURE §4.3). Log the same paired records
    /// a cancelled running tool gets — a cancelled `ToolResult` then the
    /// `OpEnded` bookkeeping — so the originating `tool_use` never dangles in
    /// the next projection (ARCHITECTURE §4.5).
    fn cancel_unstarted_tool_call(&mut self, call: ToolCall) {
        let op = self.state.alloc_op();
        self.append(Record::ToolResult {
            op,
            name: call.name,
            call_id: call.id,
            result: json!({ "cancelled": true }),
            est_tokens: 0,
        });
        self.end_op(
            op,
            OpOutcome::Cancelled {
                partial: Value::Null,
            },
            None,
        );
    }

    // ========================================================================
    // Bookkeeping
    // ========================================================================

    fn append(&mut self, record: Record) {
        let seq = crate::primitives::Seq(self.state.alloc_seq());
        let at = self.state.now();
        self.state.push_log(LogEntry::new(seq, at, record));
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
