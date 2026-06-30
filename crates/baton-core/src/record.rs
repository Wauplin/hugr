//! The durable log: what actually gets persisted.
//!
//! The log is the **source of truth**; [`BrainState`](crate::BrainState) is a
//! fold over it. One [`Record`] is appended per *logical* thing — a user
//! message, a consolidated model output, a tool result, an op ending — never
//! one per streaming delta (ARCHITECTURE §4.5). That keeps traces comparable in
//! size to a normal message history.

use serde::{Deserialize, Serialize};

use crate::model::{ModelOutput, ModelSelector, Usage};
use crate::primitives::{OpId, Seq, Timestamp, Value};

/// One ordered, timestamped entry in the append-only log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Host-assigned global order (also the replay key).
    pub seq: Seq,
    /// From the latest injected [`Tick`](crate::Event::Tick), never a syscall.
    pub at: Timestamp,
    pub record: Record,
}

/// The persisted forms of state-changing facts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Record {
    /// A conversational message from the user.
    UserMessage { text: String },

    /// A consolidated model output (the authoritative result of a model call).
    /// Phase 0 stores it inline; later phases may store a [`Value`] blob ref.
    ModelOutput { op: OpId, output: ModelOutput },

    /// A tool/capability result fed back into the conversation. (Also used for
    /// denials and conflicts, which are just error-shaped results to the model.)
    ToolResult {
        op: OpId,
        name: String,
        result: Value,
    },

    /// An operation ended; carries per-op metadata (timing, cost, selector) so
    /// latency and spend are queryable from the trace itself (ARCHITECTURE §4.1).
    OpEnded {
        op: OpId,
        outcome: OpOutcome,
        meta: OpMeta,
    },
}

/// How an op ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum OpOutcome {
    Ok,
    Error(Value),
    /// Cancelled/interrupted; `partial` preserves whatever was produced so far
    /// (ARCHITECTURE §6.4) — never an implicit gap.
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
