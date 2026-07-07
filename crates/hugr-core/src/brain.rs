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
use crate::model::{ContextPlan, ModelDelta, ModelOutput, ModelSelector, ToolCall, Usage};
use crate::policy::{AgentSeed, StaticPolicy, TurnPolicy};
use crate::primitives::{OpId, Value};
use crate::record::{LogEntry, OpMeta, OpOutcome, Record, SeqRange, SummaryCoverage};
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
    /// (ARCHITECTURE §14). A sub-agent (`Command::StartAgent`'s `seed`) or a
    /// resumed session starts from a copy of a log prefix; the brain re-derives
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
                mode,
                est_tokens,
            } => self.on_user_input(content, mode, est_tokens),
            Event::UserAbort => self.on_user_abort(),
            Event::CompactContext => self.on_compact_context(),
            Event::ModelOverride { selector } => {
                // Durable, not just in-memory: a pending override must survive
                // checkpoint/resume, and `BrainState` must stay a pure fold
                // over the log (ARCHITECTURE §3.1). `from_log` re-derives the
                // pending override from this record (see `Record::ModelOverride`).
                self.append(Record::ModelOverride {
                    selector: selector.clone(),
                });
                self.state.set_model_override(selector);
                self.checkpoint();
            }
            Event::HookFired {
                phase,
                name,
                result,
                est_tokens,
            } => {
                self.append(Record::Hook {
                    phase,
                    name,
                    result,
                    est_tokens,
                });
                self.checkpoint();
            }

            Event::ModelDelta { op, delta } => self.on_model_delta(op, delta),
            Event::ModelDone {
                op,
                output,
                usage,
                est_tokens,
            } => self.on_model_done(op, output, usage, est_tokens),
            Event::ModelError { op, error } => self.on_model_error(op, error),

            Event::CapabilityChunk { op, chunk } => {
                self.emit(OutputEvent::ToolChunk { op, chunk });
            }
            Event::CapabilityDone {
                op,
                result,
                version,
                est_tokens,
            } => self.on_capability_done(op, result, version, est_tokens),
            Event::CapabilityError {
                op,
                error,
                conflict,
                est_tokens,
            } => self.on_capability_error(op, error, conflict, est_tokens),

            Event::AgentDone {
                op,
                result,
                est_tokens,
            } => self.on_agent_done(op, result, est_tokens),
            Event::AgentError {
                op,
                error,
                est_tokens,
            } => self.on_agent_error(op, error, est_tokens),

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

    fn on_user_input(&mut self, content: Value, mode: SteerMode, est_tokens: u32) {
        self.append(Record::UserMessage {
            text: stringify(&content),
            est_tokens,
            steer: mode,
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

    /// A pure control-signal abort (ARCHITECTURE §4.6). While ops are in flight
    /// this latches `abort_requested`: the `Cancel` commands race each op's own
    /// terminal event, and whichever arrives first must still end the turn
    /// `Cancelled` without starting new work (ARCHITECTURE §4.3). An abort also
    /// supersedes a pending interrupt-resume — a steer followed by an abort
    /// must not become a surprise model turn. An idle abort stays a no-op.
    fn on_user_abort(&mut self) {
        self.state.set_pending_resume(false);
        if self.state.is_busy() {
            self.state.set_abort_requested(true);
            self.cancel_all_inflight();
        }
    }

    fn on_model_delta(&mut self, op: OpId, delta: ModelDelta) {
        // Deltas are transport only: accumulate cheaply for live UI and forward
        // a cosmetic event. Never written to the log (ARCHITECTURE §4.5).
        let is_compaction = matches!(
            self.state.get_op(op).map(|entry| &entry.kind),
            Some(OpKind::Compaction { .. })
        );
        match delta {
            ModelDelta::Text(t) => {
                self.state.buffer_model_text(op, &t);
                if !is_compaction {
                    self.emit(OutputEvent::ModelText { op, text: t });
                }
            }
            ModelDelta::Reasoning(t) => {
                if !is_compaction {
                    self.emit(OutputEvent::ModelReasoning { op, text: t });
                }
            }
            ModelDelta::ToolCallStart { id, name } => {
                if !is_compaction {
                    self.emit(OutputEvent::ToolCallStarted { op, id, name });
                }
            }
            ModelDelta::ToolCallArgsDelta { .. } | ModelDelta::ToolCallEnd { .. } => {}
        }
    }

    fn on_model_done(&mut self, op: OpId, output: ModelOutput, usage: Usage, est_tokens: u32) {
        if let Some((summary_of, est_tokens_in, tier, resume_turn)) = self.compaction_op(op) {
            self.on_compaction_done(
                op,
                output,
                usage,
                est_tokens,
                summary_of,
                est_tokens_in,
                tier,
                resume_turn,
            );
            return;
        }

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
                if self.state.pending_resume() {
                    // An interrupt's `Cancel` lost the race to this normal
                    // completion (ARCHITECTURE §4.6): the interrupting input is
                    // already logged, so consume the latch and start the fresh
                    // turn it asked for instead of ending.
                    self.start_model_turn();
                } else {
                    self.done(DoneReason::EndTurn);
                }
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
        // after a terminal `Done`. A pending interrupt-resume supersedes the
        // error: the interrupting input starts a fresh turn once ops drain.
        if self.background_running() {
            if !self.state.pending_resume() {
                self.state.set_deferred_error(Some(stringify(&error)));
            }
            return;
        }
        if self.state.pending_resume() {
            // An interrupt's `Cancel` lost the race to this error: consume the
            // latch and start the fresh turn it asked for (ARCHITECTURE §4.6).
            self.start_model_turn();
            return;
        }
        self.done(DoneReason::Error(stringify(&error)));
    }

    /// The shared tail of every tool-result-shaped resolution (capability,
    /// sub-agent, user answer, permission denial): record a version refresh if
    /// one rode along (ARCHITECTURE §7.3), append the *one* consolidated
    /// `ToolResult` record (ARCHITECTURE §4.5), end the op, and resume the turn.
    fn finish_tool_result(
        &mut self,
        op: OpId,
        result: Value,
        version: Option<VersionRef>,
        outcome: OpOutcome,
        est_tokens: u32,
    ) {
        if let Some(v) = &version {
            self.state
                .record_version(v.object.clone(), v.version.clone());
        }
        let (name, call_id) = self.tool_ids(op);
        self.append(Record::ToolResult {
            op,
            name,
            call_id,
            result,
            version,
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

    fn on_capability_done(
        &mut self,
        op: OpId,
        result: Value,
        version: Option<VersionRef>,
        est_tokens: u32,
    ) {
        self.finish_tool_result(op, result, version, OpOutcome::Ok, est_tokens);
    }

    fn on_capability_error(
        &mut self,
        op: OpId,
        error: Value,
        conflict: Option<VersionRef>,
        est_tokens: u32,
    ) {
        // A stale-edit conflict refreshes the read-set so the model's next edit
        // is stamped correctly; otherwise it is an ordinary error result fed
        // back to the model (ARCHITECTURE §5.4, §7.3).
        self.finish_tool_result(
            op,
            error.clone(),
            conflict,
            OpOutcome::Error(error),
            est_tokens,
        );
    }

    fn on_agent_done(&mut self, op: OpId, result: Value, est_tokens: u32) {
        // A sub-agent result returns to the parent as a tool-result-shaped value
        // the next model turn consumes (ARCHITECTURE §13.1/§14.3): the child's
        // digest flows back as *one* value; forks diverge, results flow back.
        self.finish_tool_result(op, result, None, OpOutcome::Ok, est_tokens);
    }

    fn on_agent_error(&mut self, op: OpId, error: Value, est_tokens: u32) {
        self.finish_tool_result(op, error.clone(), None, OpOutcome::Error(error), est_tokens);
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
                self.finish_tool_result(
                    op,
                    result.clone(),
                    None,
                    OpOutcome::Error(result),
                    est_tokens,
                );
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
                version: None,
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

        if self.state.pending_resume() && !self.state.is_busy() {
            // An interrupt (steer) is waiting for the in-flight ops to drain:
            // start the fresh turn now that they have (the partial work is
            // already logged, so the new turn's projection sees it).
            self.start_model_turn();
        } else if !self.state.is_busy() {
            // A host-initiated cancel with nothing to resume and no abort
            // latched: the turn is over, cancelled. Emit the terminal `Done`
            // once the last in-flight op has drained so the front-end sees it.
            self.done(DoneReason::Cancelled);
        }
    }

    fn on_compact_context(&mut self) {
        if self.state.is_busy() {
            self.emit(OutputEvent::Notice(
                "compaction skipped: operations are still in flight".to_string(),
            ));
            return;
        }
        let budget = self.policy.context_budget(&self.state);
        let plan = self.policy.project_context(self.state.log(), budget);
        if !self.start_selected_compaction(&plan, false) {
            self.emit(OutputEvent::Notice(
                "compaction skipped: no compactable context span".to_string(),
            ));
        }
    }

    // ========================================================================
    // Turn-loop helpers
    // ========================================================================

    /// Begin a model turn: ask the policy which model to call and how to project
    /// context, then emit the call.
    fn start_model_turn(&mut self) {
        // Starting a turn consumes any pending interrupt-resume: the projection
        // computed below already sees the interrupting input (ARCHITECTURE
        // §4.6), so the latch never outlives the resume it asked for.
        self.state.set_pending_resume(false);
        let budget = self.policy.context_budget(&self.state);
        let plan = self.policy.project_context(self.state.log(), budget);
        if self.should_compact(&plan, budget) && self.start_selected_compaction(&plan, true) {
            return;
        }

        let op = self.state.alloc_op();
        let selector = self
            .state
            .take_model_override()
            .unwrap_or_else(|| self.policy.choose_model(&self.state));
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

    /// Start a policy-selected compaction from a projection `plan` the caller
    /// already computed (turn-start reuses its own; manual compaction computes
    /// one) — avoids projecting the same log twice per decision.
    fn start_selected_compaction(&mut self, plan: &ContextPlan, resume_turn: bool) -> bool {
        let Some(target) = self.policy.select_compaction_span(self.state.log(), plan) else {
            return false;
        };
        self.start_compaction_turn(target.summary_of, target.est_tokens_in, resume_turn, plan);
        true
    }

    fn should_compact(
        &self,
        plan: &crate::model::ContextPlan,
        budget: crate::model::TokenBudget,
    ) -> bool {
        self.policy
            .compaction_high_water(&self.state, budget)
            .is_some_and(|high_water| plan.totals.used_tokens > high_water)
    }

    fn start_compaction_turn(
        &mut self,
        summary_of: SeqRange,
        est_tokens_in: u32,
        resume_turn: bool,
        plan: &ContextPlan,
    ) {
        let op = self.state.alloc_op();
        // Route the compaction model through the policy (ARCHITECTURE §2.5): the
        // reducer must not hardcode a selector. The per-turn model override is
        // deliberately *not* consumed here — it belongs to the real turn that
        // resumes after compaction.
        let _ = plan;
        let selector = self.policy.choose_model(&self.state);
        let request = self.policy.compaction_request(self.state.log(), summary_of);
        self.state.mark(
            op,
            OpKind::Compaction {
                selector: selector.clone(),
                summary_of,
                est_tokens_in,
                resume_turn,
                text_so_far: String::new(),
            },
        );
        self.state.push_command(Command::StartModelCall {
            op,
            model: selector,
            request,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn on_compaction_done(
        &mut self,
        op: OpId,
        output: ModelOutput,
        usage: Usage,
        est_tokens_out: u32,
        summary_of: SeqRange,
        est_tokens_in: u32,
        tier: ModelSelector,
        resume_turn: bool,
    ) {
        self.append(Record::Summary {
            op,
            text: summary_text(&output),
            summary_of,
            coverage: SummaryCoverage::Complete,
            tier,
            est_tokens_in,
            est_tokens_out,
        });
        self.end_op(op, OpOutcome::Ok, Some(usage));
        self.checkpoint();
        // A latched abort raced this compaction's completion: fold the summary
        // but do not resume — end the turn `Cancelled` once drained (§4.3).
        if self.resolve_abort_if_drained() {
            return;
        }
        if resume_turn {
            self.start_model_turn();
        }
    }

    fn compaction_op(&self, op: OpId) -> Option<(SeqRange, u32, ModelSelector, bool)> {
        match self.state.get_op(op).map(|entry| &entry.kind) {
            Some(OpKind::Compaction {
                selector,
                summary_of,
                est_tokens_in,
                resume_turn,
                ..
            }) => Some((*summary_of, *est_tokens_in, selector.clone(), *resume_turn)),
            _ => None,
        }
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
        // A policy-designated sub-agent (ARCHITECTURE §13/§14): fork the log
        // prefix per the seed strategy and hand it to the host as a child brain.
        // The brain owns the log, so resolving the fork is a pure operation here.
        if let Some(seed) = self.policy.agent_seed(&call.name) {
            let seed_log = self.resolve_seed(seed);
            // The agent *name* is typed on the command (the host branches on
            // which agent to run); the model's args pass through **untouched**
            // as the opaque config — the brain never edits opaque payloads
            // (ARCHITECTURE §2.4).
            self.state.mark(
                op,
                OpKind::Agent {
                    name: call.name.clone(),
                    call_id: call.id,
                },
            );
            self.state.push_command(Command::StartAgent {
                op,
                agent: call.name,
                config: call.args,
                seed: seed_log,
            });
            return;
        }
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
        mut args: Value,
        call_id: String,
        background: bool,
    ) {
        // Optimistic-concurrency stamping (ARCHITECTURE §7.3): declarative
        // schema metadata says which arg is the object key and where a mutating
        // call expects the last-seen version. The brain never hardcodes tool
        // names or parses paths; it only does opaque equality/table lookup.
        self.stamp_expected_version(&name, &mut args);
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

    fn stamp_expected_version(&self, name: &str, args: &mut Value) {
        let Some(versioning) = self.policy.capability_versioning(name) else {
            return;
        };
        let Some(expected_arg) = versioning.expected_version_arg else {
            return;
        };
        let Some(object) = args
            .get(&versioning.object_arg)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        else {
            return;
        };
        let Some(version) = self.state.versions().get(&object) else {
            return;
        };
        if let Some(map) = args.as_object_mut() {
            map.insert(expected_arg, Value::String(version.clone()));
        }
    }

    /// Resolve a sub-agent [`AgentSeed`] into the actual log prefix to fork
    /// (ARCHITECTURE §14). Pure: the brain owns the log. Copy-on-write is a host
    /// optimization; the contract is just "the child starts from these entries."
    fn resolve_seed(&self, seed: AgentSeed) -> Vec<LogEntry> {
        match seed {
            AgentSeed::Fresh => Vec::new(),
            AgentSeed::ForkFull => self.state.log().to_vec(),
            AgentSeed::ForkAt { seq } => self
                .state
                .log()
                .iter()
                .filter(|e| e.seq.0 <= seq)
                .cloned()
                .collect(),
        }
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
            version: None,
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
            Some(OpKind::Model { text_so_far, .. } | OpKind::Compaction { text_so_far, .. })
                if !text_so_far.is_empty() =>
            {
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

fn summary_text(output: &ModelOutput) -> String {
    if !output.text.is_empty() {
        return output.text.clone();
    }
    if output.tool_calls.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&output.tool_calls).unwrap_or_default()
    }
}
