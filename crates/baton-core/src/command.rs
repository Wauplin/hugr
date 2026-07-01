//! Commands: the brain → host half of the contract.
//!
//! A [`Command`] is something the brain wants the host to do. Every *effectful*
//! command carries an [`OpId`] so its results can be correlated by the matching
//! [`Event`](crate::Event) and the work can be cancelled. `#[non_exhaustive]`
//! so adding a variant is not a breaking change for hosts (ARCHITECTURE §2.4).

use serde::{Deserialize, Serialize};

use crate::model::{ModelRequest, ModelSelector};
use crate::primitives::{OpId, Value};
use crate::record::LogEntry;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Command {
    /// Start a model completion. `model` is a logical selector the host
    /// resolves (ARCHITECTURE §5.3); the host streams deltas back as events.
    StartModelCall {
        op: OpId,
        model: ModelSelector,
        request: ModelRequest,
    },

    /// Invoke a host capability (a tool). Covers shell, fs, http, plugins —
    /// there are no privileged built-ins. `args` is opaque to the brain.
    StartCapability { op: OpId, name: String, args: Value },

    /// Spawn a **sub-agent**: a child `baton-core` instance the host runs as an
    /// op (ARCHITECTURE §13). `config` is opaque to the brain (the model's
    /// tool-call args — the host interprets the child's prompt/model/tools).
    /// `seed` is the child's initial log — a **fork** of the parent's log
    /// prefix (ARCHITECTURE §14), resolved here from the parent's log by the
    /// [`TurnPolicy`](crate::TurnPolicy)'s
    /// [`agent_seed`](crate::TurnPolicy::agent_seed); it is empty for an
    /// isolated (`Fresh`) child. The child's result returns via
    /// [`Event::AgentDone`](crate::Event::AgentDone).
    StartAgent {
        op: OpId,
        config: Value,
        seed: Vec<LogEntry>,
    },

    /// Ask the user a free-form question. Answered by
    /// [`Event::UserAnswer`](crate::Event::UserAnswer).
    AskUser { op: OpId, prompt: UserPrompt },

    /// Request permission for a pending action; the host's policy decides and
    /// replies with [`Event::PermissionDecision`](crate::Event::PermissionDecision).
    RequestPermission {
        op: OpId,
        request: PermissionRequest,
    },

    /// Abort an in-flight operation (HTTP request, process, …). The host
    /// confirms with [`Event::OpCancelled`](crate::Event::OpCancelled).
    Cancel { op: OpId },

    /// A cosmetic / observability event for front-ends. Side-effect-free for
    /// durable state — never folded into the log.
    Emit(OutputEvent),

    /// Persist the current durable state (a checkpoint for resume). Cheap:
    /// the log is append-only, so this usually just flushes new entries.
    Checkpoint,

    /// The turn/session reached a terminal state.
    Done { reason: DoneReason },
}

/// A free-form question for the user. Kept minimal in Phase 0; `detail` is an
/// opaque blob a richer front-end can interpret.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UserPrompt {
    pub message: String,
    pub detail: Value,
}

/// A request for the host's policy to decide. Carries a typed outcome channel
/// (the `op`) but an opaque `detail` the policy interprets (ARCHITECTURE §2.4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PermissionRequest {
    /// The capability whose invocation is being gated.
    pub capability: String,
    /// The (opaque) arguments the capability would be invoked with.
    pub args: Value,
}

/// Why a turn/session ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DoneReason {
    /// The model produced a final answer with no tool calls.
    EndTurn,
    /// The session was cancelled/aborted.
    Cancelled,
    /// A terminal error.
    Error(String),
}

/// Cosmetic output for front-ends. Multiple front-ends can subscribe; rendering
/// is never inside the core (ARCHITECTURE §9).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum OutputEvent {
    /// A chunk of streamed assistant text (for live rendering).
    ModelText { op: OpId, text: String },
    /// A chunk of streamed model reasoning/thinking.
    ModelReasoning { op: OpId, text: String },
    /// The model began a tool call (id + name known before args complete).
    ToolCallStarted { op: OpId, id: String, name: String },
    /// A streamed chunk from a capability (e.g. a line of stdout).
    ToolChunk { op: OpId, chunk: Value },
    /// A free-form notice for logs/status lines.
    Notice(String),
}
