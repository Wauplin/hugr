//! `BrainState`: the derived state the reducer folds the log into.
//!
//! Everything here is **rebuildable by replaying the log** — the log is the
//! truth (ARCHITECTURE §3.1). The state exists so the hot path (handling a
//! delta, deciding the next command) is cheap.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::command::Command;
use crate::model::ModelSelector;
use crate::primitives::{ObjectKey, OpId, Timestamp, Value};
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
    /// Every started, not-yet-finished op.
    inflight: HashMap<OpId, InflightOp>,
    /// Commands queued for the host to drain via [`Brain::poll`](crate::Brain::poll).
    #[serde(skip)]
    outbox: Vec<Command>,
    /// Latest injected time (ARCHITECTURE §6.1).
    now: Timestamp,
    /// Generic optimistic-concurrency read-set: last-seen version per object,
    /// folded from capability results (ARCHITECTURE §7.3). Opaque keys/values.
    versions: HashMap<ObjectKey, String>,
    /// Set when an interrupt cancelled in-flight ops and a fresh turn must start
    /// once they drain.
    pending_resume: bool,
}

impl BrainState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    // --- read-only accessors (for hosts, tooling and tests) ------------------

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

    /// The in-flight op table.
    pub fn inflight(&self) -> &HashMap<OpId, InflightOp> {
        &self.inflight
    }

    /// The optimistic-concurrency read-set (last-seen version per object).
    pub fn versions(&self) -> &HashMap<ObjectKey, String> {
        &self.versions
    }

    // --- mutation helpers, used only by the reducer --------------------------

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
        self.inflight.insert(
            op,
            InflightOp {
                started_at: self.now,
                kind,
            },
        );
    }

    pub(crate) fn get_op(&self, op: OpId) -> Option<&InflightOp> {
        self.inflight.get(&op)
    }

    pub(crate) fn buffer_model_text(&mut self, op: OpId, text: &str) {
        if let Some(InflightOp {
            kind: OpKind::Model { text_so_far, .. },
            ..
        }) = self.inflight.get_mut(&op)
        {
            text_so_far.push_str(text);
        }
    }

    pub(crate) fn remove_op(&mut self, op: OpId) -> Option<InflightOp> {
        self.inflight.remove(&op)
    }

    pub(crate) fn inflight_op_ids(&self) -> Vec<OpId> {
        self.inflight.keys().copied().collect()
    }

    pub(crate) fn record_version(&mut self, object: ObjectKey, version: String) {
        self.versions.insert(object, version);
    }

    pub(crate) fn pending_resume(&self) -> bool {
        self.pending_resume
    }

    pub(crate) fn set_pending_resume(&mut self, v: bool) {
        self.pending_resume = v;
    }
}

/// An entry in the in-flight op table: live scratch space for an op that has
/// started but not yet ended.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InflightOp {
    /// When the op started (from the injected clock), for latency accounting.
    pub started_at: Timestamp,
    pub kind: OpKind,
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
    /// A capability (tool) invocation in progress.
    Capability { name: String, call_id: String },
    /// A tool call awaiting a permission decision before it can start.
    AwaitingPermission {
        name: String,
        args: Value,
        call_id: String,
    },
    /// A pending `AskUser` awaiting the user's answer.
    AwaitingUser,
    /// A sub-agent (full handling in Phase 6).
    Agent,
}

impl OpKind {
    /// The model selector, if this is a model op (for [`OpMeta`](crate::OpMeta)).
    pub(crate) fn selector(&self) -> Option<ModelSelector> {
        match self {
            OpKind::Model { selector, .. } => Some(selector.clone()),
            _ => None,
        }
    }

    /// The capability name, if this op has one.
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

    /// Whether this op is something a model turn is waiting on (a tool, a
    /// pending permission, a sub-agent, or a user answer) — as opposed to a
    /// model op. Used to decide when to resume the turn.
    pub(crate) fn blocks_turn(&self) -> bool {
        matches!(
            self,
            OpKind::Capability { .. }
                | OpKind::AwaitingPermission { .. }
                | OpKind::Agent
                | OpKind::AwaitingUser
        )
    }
}
