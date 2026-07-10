//! The durable log: what actually gets persisted.
//!
//! The log is the **source of truth**; [`BrainState`](crate::BrainState) is a
//! fold over it. One [`Record`] is appended per *logical* thing — a user
//! message, a consolidated model output, a tool result, an op ending — never
//! one per streaming delta. That keeps traces comparable in size to a normal
//! message history.

use serde::{Deserialize, Serialize};

use crate::model::{ModelOutput, ModelSelector, Usage};
use crate::primitives::{OpId, Seq, Timestamp, Value};

/// One ordered, timestamped entry in the append-only log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LogEntry {
    /// Host-assigned global order (also the replay key).
    pub seq: Seq,
    /// From the latest injected [`Tick`](crate::Event::Tick), never a syscall.
    pub at: Timestamp,
    pub record: Record,
}

impl LogEntry {
    pub fn new(seq: Seq, at: Timestamp, record: Record) -> Self {
        Self { seq, at, record }
    }
}

/// The persisted forms of state-changing facts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Record {
    /// A conversational message from the user.
    UserMessage {
        text: String,
        /// Host-provided approximate token count. The brain stores/sums this
        /// value but never tokenizes content itself.
        #[serde(default)]
        est_tokens: u32,
    },

    /// A consolidated model output (the authoritative result of a model call).
    /// Phase 0 stores it inline; later phases may store a [`Value`] blob ref.
    ModelOutput {
        op: OpId,
        output: ModelOutput,
        /// Host/provider-provided approximate token count for this content.
        #[serde(default)]
        est_tokens: u32,
    },

    /// A tool/capability result fed back into the conversation. (Also used for
    /// denials and conflicts, which are just error-shaped results to the model.)
    ToolResult {
        op: OpId,
        /// The capability name.
        name: String,
        /// The originating model `tool_call` id. Providers require a tool result
        /// to reference the exact id of the call it answers, so projection uses
        /// this (not the op id) to correlate the two.
        call_id: String,
        result: Value,
        /// Host-provided approximate token count for this content.
        #[serde(default)]
        est_tokens: u32,
    },

    /// An operation ended; carries per-op metadata (timing, cost, selector) so
    /// latency and spend are queryable from the trace itself.
    OpEnded {
        op: OpId,
        outcome: OpOutcome,
        meta: OpMeta,
    },
}

impl Record {
    /// The op this record refers to, if any. Used to reconstruct the next op id
    /// when seeding a forked child log, so the child's new ops don't collide
    /// with ids already present in the inherited prefix.
    pub fn op_id(&self) -> Option<OpId> {
        match self {
            Record::ModelOutput { op, .. }
            | Record::ToolResult { op, .. }
            | Record::OpEnded { op, .. } => Some(*op),
            Record::UserMessage { .. } => None,
        }
    }

    /// Host-recorded token estimate for durable content projected to a model.
    /// `None` means bookkeeping only.
    pub fn content_est_tokens(&self) -> Option<u32> {
        match self {
            Record::UserMessage { est_tokens, .. }
            | Record::ModelOutput { est_tokens, .. }
            | Record::ToolResult { est_tokens, .. } => Some(*est_tokens),
            Record::OpEnded { .. } => None,
        }
    }
}

/// How an op ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum OpOutcome {
    Ok,
    Error(Value),
    /// Cancelled/interrupted; `partial` preserves whatever was produced so far.
    Cancelled {
        partial: Value,
    },
}

/// Per-op metadata recorded when an op ends. Timing matters as much as cost.
/// `extra` is an opaque bag (provider request-id, cache info, retries, …) the
/// brain stores but never interprets.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OpMeta {
    pub started_at: Timestamp,
    pub ended_at: Timestamp,
    /// Which logical model (for model ops).
    pub model: Option<ModelSelector>,
    /// Tokens / cost, when applicable.
    pub usage: Option<Usage>,
    pub extra: Value,
}
