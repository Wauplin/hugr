//! `BrainState`: the derived state the reducer folds the log into.
//!
//! Everything here is rebuildable by replaying the log; the state exists so the
//! hot path (handling a delta, deciding the next command) is cheap.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::command::Command;
use crate::model::ModelSelector;
use crate::primitives::{OpId, Timestamp, Value};
use crate::record::LogEntry;

/// The brain's working state. Derived from [`log`](BrainState::log); never the
/// source of truth itself.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BrainState {
    /// Append-only source of truth.
    log: Vec<LogEntry>,
    /// Next sequence number to assign.
    next_seq: u64,
    /// Next op id to assign.
    next_op: u64,
    /// Every started, not-yet-finished op. A `BTreeMap` on purpose: iteration
    /// order leaks into emitted commands (e.g. the `Cancel` fan-out on abort),
    /// and replay must be deterministic — a hash map would emit them in random
    /// order.
    inflight: BTreeMap<OpId, InflightOp>,
    /// Commands queued for the host to drain via [`Brain::poll`](crate::Brain::poll).
    #[serde(skip)]
    outbox: Vec<Command>,
    /// Latest injected time.
    now: Timestamp,
    /// Set when a `UserAbort` arrived while ops were in flight. The abort's
    /// `Cancel` commands race each op's own terminal event; while latched,
    /// terminal events fold their records but start no new work, and the single
    /// terminal `Done(Cancelled)` is emitted once the last in-flight op drains.
    #[serde(default)]
    abort_requested: bool,
    /// A model transport error whose terminal `Done(Error)` is deferred while
    /// background ops are still running (mirrors the `Done(EndTurn)` deferral):
    /// emitted once the last op drains.
    #[serde(default)]
    deferred_error: Option<String>,
}

impl BrainState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Rebuild state from an inherited log — the fork/seed primitive: the log
    /// is taken verbatim and the counters/clock derived from it. Nothing is in
    /// flight (a consolidated prefix has no open ops).
    pub(crate) fn from_log(log: Vec<LogEntry>) -> Self {
        let next_seq = log.last().map(|e| e.seq.0 + 1).unwrap_or(0);
        let now = log.last().map(|e| e.at).unwrap_or_default();
        let next_op = log
            .iter()
            .filter_map(|e| e.record.op_id())
            .map(|op| op.0)
            .max()
            .map(|max| max + 1)
            .unwrap_or(0);
        Self {
            log,
            next_seq,
            next_op,
            now,
            ..Self::default()
        }
    }

    /// The append-only log — the durable source of truth.
    pub fn log(&self) -> &[LogEntry] {
        &self.log
    }

    /// The latest injected timestamp.
    pub fn now(&self) -> Timestamp {
        self.now
    }

    /// Number of operations currently in flight.
    pub fn inflight_len(&self) -> usize {
        self.inflight.len()
    }

    /// Whether any op is in flight.
    pub fn is_busy(&self) -> bool {
        !self.inflight.is_empty()
    }

    /// The in-flight op table (ordered by op id, so iteration is deterministic).
    pub fn inflight(&self) -> &BTreeMap<OpId, InflightOp> {
        &self.inflight
    }

    pub(crate) fn now_mut(&mut self) -> &mut Timestamp {
        &mut self.now
    }

    pub(crate) fn alloc_op(&mut self) -> OpId {
        let id = OpId(self.next_op);
        self.next_op += 1;
        id
    }

    pub(crate) fn alloc_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    pub(crate) fn push_command(&mut self, cmd: Command) {
        self.outbox.push(cmd);
    }

    pub(crate) fn drain_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.outbox)
    }

    pub(crate) fn push_log(&mut self, entry: LogEntry) {
        self.log.push(entry);
    }

    pub(crate) fn mark(&mut self, op: OpId, kind: OpKind) {
        self.inflight.insert(op, InflightOp::new(self.now, kind));
    }

    pub(crate) fn get_op(&self, op: OpId) -> Option<&InflightOp> {
        self.inflight.get(&op)
    }

    pub(crate) fn buffer_model_text(&mut self, op: OpId, text: &str) {
        if let Some(entry) = self.inflight.get_mut(&op) {
            if let OpKind::Model { text_so_far, .. } = &mut entry.kind {
                text_so_far.push_str(text)
            }
        }
    }

    pub(crate) fn remove_op(&mut self, op: OpId) -> Option<InflightOp> {
        self.inflight.remove(&op)
    }

    /// The in-flight op ids, in ascending op-id order — deterministic, because
    /// this order leaks into emitted `Cancel` commands.
    pub(crate) fn inflight_op_ids(&self) -> Vec<OpId> {
        self.inflight.keys().copied().collect()
    }

    pub(crate) fn abort_requested(&self) -> bool {
        self.abort_requested
    }

    pub(crate) fn set_abort_requested(&mut self, v: bool) {
        self.abort_requested = v;
    }

    pub(crate) fn deferred_error(&self) -> Option<&String> {
        self.deferred_error.as_ref()
    }

    pub(crate) fn set_deferred_error(&mut self, reason: Option<String>) {
        self.deferred_error = reason;
    }

    pub(crate) fn take_deferred_error(&mut self) -> Option<String> {
        self.deferred_error.take()
    }
}

/// An entry in the in-flight op table: live scratch space for an op that has
/// started but not yet ended. Construct via [`InflightOp::new`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct InflightOp {
    /// When the op started (from the injected clock), for latency accounting.
    pub started_at: Timestamp,
    pub kind: OpKind,
}

impl InflightOp {
    pub fn new(started_at: Timestamp, kind: OpKind) -> Self {
        Self { started_at, kind }
    }
}

/// The kind of an in-flight op. Rebuildable by folding the log.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum OpKind {
    /// A model call. Accumulates streamed text for live UI; the consolidated
    /// `ModelDone` is authoritative for logic.
    Model {
        selector: ModelSelector,
        text_so_far: String,
    },
    /// A capability (tool) invocation in progress. `background` ops do **not**
    /// block the model turn: the turn resumes while they keep running, so a
    /// model stream and a long task can run simultaneously.
    Capability {
        name: String,
        call_id: String,
        background: bool,
    },
    /// A tool call awaiting a permission decision before it can start.
    AwaitingPermission {
        name: String,
        args: Value,
        call_id: String,
    },
}

impl OpKind {
    /// The model selector, if this is a model op (for [`OpMeta`](crate::OpMeta)).
    pub(crate) fn selector(&self) -> Option<ModelSelector> {
        match self {
            OpKind::Model { selector, .. } => Some(selector.clone()),
            _ => None,
        }
    }

    /// The capability (or sub-agent) name, if this op has one.
    pub(crate) fn capability_name(&self) -> Option<&str> {
        match self {
            OpKind::Capability { name, .. } | OpKind::AwaitingPermission { name, .. } => Some(name),
            _ => None,
        }
    }

    /// The originating model `tool_call` id, if this op has one.
    pub(crate) fn call_id(&self) -> Option<&str> {
        match self {
            OpKind::Capability { call_id, .. } | OpKind::AwaitingPermission { call_id, .. } => {
                Some(call_id)
            }
            _ => None,
        }
    }

    /// Whether this op is something a model turn is waiting on (a foreground
    /// tool, a pending permission, or a sub-agent) — as opposed to a model op
    /// or a **background** capability. Used to decide when to resume the turn:
    /// a background op runs *alongside* the model stream, so it must not hold
    /// the turn open.
    pub(crate) fn blocks_turn(&self) -> bool {
        match self {
            OpKind::Capability { background, .. } => !background,
            OpKind::AwaitingPermission { .. } => true,
            OpKind::Model { .. } => false,
        }
    }

    pub(crate) fn is_model_call(&self) -> bool {
        matches!(self, OpKind::Model { .. })
    }
}
