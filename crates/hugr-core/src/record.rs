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

/// Inclusive range of log entries covered by a durable summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SeqRange {
    pub start: Seq,
    pub end: Seq,
}

impl SeqRange {
    pub fn new(start: Seq, end: Seq) -> Self {
        Self { start, end }
    }

    pub fn contains(&self, seq: Seq) -> bool {
        self.start <= seq && seq <= self.end
    }
}

/// How completely a summary covers its source span.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SummaryCoverage {
    Complete,
    Partial { reason: String },
}

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
    UserMessage {
        text: String,
        /// Host-provided approximate token count. The brain stores/sums this
        /// value but never tokenizes content itself (ARCHITECTURE §3.5).
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

    /// A durable, non-destructive compaction summary over an exact log span.
    /// The original records stay in the log; later projections use this record
    /// to evict covered entries to references (ARCHITECTURE §3.4).
    Summary {
        /// The model op that produced the summary.
        op: OpId,
        /// Human/model-readable summary text.
        text: String,
        /// Exact inclusive source span.
        summary_of: SeqRange,
        /// Whether the summary fully covers the span.
        coverage: SummaryCoverage,
        /// Tier used to produce the summary, e.g. `small`.
        tier: ModelSelector,
        /// Sum of host-recorded token estimates for the source span.
        #[serde(default)]
        est_tokens_in: u32,
        /// Host/provider estimate for the summary text.
        #[serde(default)]
        est_tokens_out: u32,
    },

    /// A skill was activated by a model-invoked skill descriptor. The full
    /// instructions are durable so replay/projection do not depend on the host
    /// rediscovering the skill bundle on disk.
    SkillActivated {
        id: String,
        title: String,
        summary: Option<String>,
        instructions: String,
        #[serde(default)]
        est_tokens: u32,
    },

    /// An operation ended; carries per-op metadata (timing, cost, selector) so
    /// latency and spend are queryable from the trace itself (ARCHITECTURE §4.1).
    OpEnded {
        op: OpId,
        outcome: OpOutcome,
        meta: OpMeta,
    },
}

impl Record {
    /// The op this record refers to, if any. Used to reconstruct the next op id
    /// when **seeding a forked child log** (ARCHITECTURE §14), so the child's new
    /// ops don't collide with ids already present in the inherited prefix.
    pub fn op_id(&self) -> Option<OpId> {
        match self {
            Record::ModelOutput { op, .. }
            | Record::ToolResult { op, .. }
            | Record::Summary { op, .. }
            | Record::OpEnded { op, .. } => Some(*op),
            Record::UserMessage { .. } | Record::SkillActivated { .. } => None,
        }
    }

    /// Host-recorded token estimate for durable content projected to a model.
    /// `None` means bookkeeping only.
    pub fn content_est_tokens(&self) -> Option<u32> {
        match self {
            Record::UserMessage { est_tokens, .. }
            | Record::ModelOutput { est_tokens, .. }
            | Record::ToolResult { est_tokens, .. } => Some(*est_tokens),
            Record::Summary { est_tokens_out, .. } => Some(*est_tokens_out),
            Record::SkillActivated { est_tokens, .. } => Some(*est_tokens),
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
    /// Why this selector was chosen, for trace-visible routing/spend analysis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingDecision>,
    /// Tokens / cost, when applicable.
    pub usage: Option<Usage>,
    pub extra: Value,
}

/// Trace-visible routing metadata for a model op (ROADMAP_2 B3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RoutingDecision {
    pub selector: ModelSelector,
    pub reasons: Vec<String>,
    /// Opaque snapshot of the pure routing inputs. The brain stores this for
    /// observability but does not interpret it after the decision is made.
    pub inputs: Value,
}

impl RoutingDecision {
    pub fn new(selector: ModelSelector, reasons: Vec<String>) -> Self {
        Self {
            selector,
            reasons,
            inputs: Value::Null,
        }
    }

    pub fn with_inputs(mut self, inputs: Value) -> Self {
        self.inputs = inputs;
        self
    }
}
